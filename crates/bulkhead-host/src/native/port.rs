//! A native instance's process: `bulkhead-runner`, its channel and its
//! limits.
//!
//! Frames travel over the instance's channel (`bulkhead_native::channel`,
//! ROADMAP T1, spike S32): two rings in a memfd the app inherits, each
//! waiting side spinning for `[native] spin_us` before it sleeps. The
//! instance's task reads and writes the rings itself, with no hand-off to
//! another task; what does not fit in the ring to the app waits in the
//! port and goes as the app reads, so a stalled app never blocks the
//! task. Waiting for the app, the task spins (yielding to the runtime's
//! other tasks) until `spin_us` after the last frame, then sleeps on the
//! bell, a pipe the app writes a byte to when the node said it sleeps, and
//! whose end of file means the app has exited. Inside a callback, while no
//! other native instance runs on its worker, a turn of the spin watches the
//! rings within the poll for a few microseconds before it yields: each
//! yield of a task alone on its worker woke a parked worker for nothing
//! (ROADMAP T9). It spins between
//! callbacks too, yielding: a task kept runnable keeps its worker awake for
//! the instance's next message, which a call between instances needs
//! (ROADMAP T1), and a watch within the poll would not see the mailbox.
//! What the app prints, on stdout or stderr, becomes log lines, as a
//! component's stderr does. Between fork and exec the child gets the
//! parent-death signal (`SIGKILL`), no core dumps, its memory limit (the
//! cgroup it starts in, or `RLIMIT_DATA` without one) and its channel. It
//! runs in a process group of its own, with an environment of
//! `BULKHEAD_PRIV` (the bundle's `priv/` files, when it has any) and the
//! spin budget.
//!
//! Native apps run on Linux only (`bulkhead-runner` is Linux only); elsewhere
//! [`Port::spawn`] refuses and a `Port` never exists.

use std::time::Duration;

use bulkhead_native::protocol::FromApp;

/// Something from the app.
#[derive(Debug)]
pub(crate) enum Incoming {
    Frame(FromApp),
    /// A hot frame's keyvalue request (ROADMAP T7).
    Kv(KvCall),
    /// A frame that could not be read; the app is broken.
    Broken(String),
}

/// A keyvalue request from a hot frame, copied out of the app's ring.
#[derive(Debug)]
pub(crate) struct KvCall {
    pub(crate) id: u64,
    pub(crate) bucket: u32,
    pub(crate) op: bulkhead_native::hot::KvOp,
    pub(crate) key: String,
    pub(crate) value: Vec<u8>,
}

/// How long a process that ended its channel has to exit before it is
/// killed.
const EXIT_GRACE: Duration = Duration::from_secs(1);

#[cfg(target_os = "linux")]
pub(crate) use linux::Port;

#[cfg(not(target_os = "linux"))]
pub(crate) use elsewhere::Port;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::os::fd::AsFd;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use bulkhead_bundle::manifest::NativeProgram;
    use bulkhead_native::channel::{self, Channel, RING_CAP};
    use bulkhead_native::hot::FromAppHot;
    use bulkhead_native::protocol::{self, ToApp};
    use tokio::io::AsyncWriteExt;
    use tokio::net::unix::pipe;
    use tokio::process::{Child, Command};

    use super::{EXIT_GRACE, Incoming};
    use crate::config::NativeConfig;
    use crate::message::ExitReason;
    use crate::native::{NativeCode, cgroup};
    use crate::output::CapturedOutput;
    use crate::spec::InstanceSpec;

    /// The frames from the app, read from its ring and not yet decoded.
    #[derive(Debug)]
    struct Input {
        /// Bytes from `start` on are not decoded yet.
        buf: Vec<u8>,
        start: usize,
        /// No more frames: the app exited, or a frame was broken.
        done: bool,
    }

    /// A running native instance's process.
    #[derive(Debug)]
    pub(crate) struct Port {
        child: Child,
        channel: Channel,
        /// The bell's read end: a byte when the app woke the node, the end
        /// of file once it exited.
        bell: pipe::Receiver,
        /// The app's door's write end: a byte wakes it for a frame.
        door: std::os::fd::OwnedFd,
        /// The node's end of the descriptor socket (T3), and the id the
        /// next descriptor passed gets.
        fds: std::os::fd::OwnedFd,
        next_fd: u64,
        /// The app's end of the bell is closed: it exited.
        exited: bool,
        input: Input,
        /// Bytes for the app that did not fit in its ring yet, from
        /// `pending_at`.
        pending: Vec<u8>,
        /// A hot frame being written, kept between frames (T7).
        out: Vec<u8>,
        pending_at: usize,
        /// The ring to the app is closed: frames are dropped.
        closed: bool,
        /// The app broke its channel; said once, as the last thing read.
        broken: Option<String>,
        spin: Duration,
        /// When a frame last went either way: the task spins until
        /// `spin` after it.
        active: Instant,
        /// No other native instance ran on this task's worker during its
        /// last yield: the worker has nothing else to do (T9).
        worker_idle: bool,
        cgroup: Option<PathBuf>,
    }

    /// The longest a wait watches the rings within one poll (T9): a task
    /// the idle check cannot see (not a native instance) waits up to this
    /// behind it. Watching until the end of the spin made requests a
    /// little cheaper and calls between 16 busy instances about 17% slower
    /// (16-core machine, 2026-11-04).
    const WATCH_SLICE: Duration = Duration::from_micros(5);

    /// The largest hot-frame buffer a port keeps between frames.
    const KEPT_BUFFER: usize = 1 << 20;

    thread_local! {
        /// Yields of native instances' waits on this thread (T9).
        static TURNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Count a turn of a native wait on this thread: the thread (as the
    /// counter's address) and the count after it. Two turns of one task
    /// a count apart on the same thread mean no other native instance ran
    /// between them.
    pub(super) fn turn() -> (usize, u64) {
        TURNS.with(|turns| {
            let n = turns.get().wrapping_add(1);
            turns.set(n);
            (std::ptr::from_ref(turns) as usize, n)
        })
    }

    /// The runner the node starts native apps through.
    fn runner(config: &NativeConfig) -> PathBuf {
        config.runner.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join("bulkhead-runner")))
                .unwrap_or_else(|| PathBuf::from("bulkhead-runner"))
        })
    }

    impl Port {
        /// Start `program` for the instance `name` (its pid, as text).
        /// Called on the runtime the instance's task runs on: the bell and
        /// the log pipe register with its I/O driver.
        ///
        /// # Errors
        ///
        /// Why the process could not start: no runner, a channel or a
        /// cgroup that could not be made.
        pub(crate) fn spawn(
            config: &NativeConfig,
            program: &NativeCode,
            spec: &InstanceSpec,
            name: &str,
            output: CapturedOutput,
        ) -> Result<Self, String> {
            let runner = runner(config);
            let mut cmd = Command::new(&runner);
            cmd.arg(match program.kind {
                NativeProgram::Library => "--library",
                NativeProgram::Executable => "--executable",
            })
            .arg(&program.path);
            for p in &spec.preopens {
                cmd.arg("--read").arg(&p.host);
            }
            if config.landlock_optional {
                cmd.args(["--landlock", "best-effort"]);
            }
            cmd.env_clear();
            if let Some(p) = spec
                .preopens
                .iter()
                .find(|p| p.guest == crate::spec::PRIV_PATH)
            {
                cmd.env(protocol::PRIV_ENV, &p.host);
            }
            cmd.env(channel::SPIN_ENV, config.spin.as_micros().to_string());
            let (channel, memfd) = Channel::create().map_err(|e| format!("channel: {e}"))?;
            let (bell, bell_app) = channel::bell().map_err(|e| format!("channel: {e}"))?;
            let (door_app, door) = channel::door().map_err(|e| format!("channel: {e}"))?;
            let (fds, fds_app) = channel::descriptors().map_err(|e| format!("channel: {e}"))?;
            let (log, log_app) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
                .map_err(|e| format!("log pipe: {e}"))?;
            let log_out = log_app.try_clone().map_err(|e| format!("log pipe: {e}"))?;
            cmd.stdin(Stdio::null())
                .stdout(Stdio::from(log_out))
                .stderr(Stdio::from(log_app))
                .kill_on_drop(true)
                .process_group(0);
            let cgroup = config
                .cgroup
                .as_ref()
                .map(|root| cgroup::create(root, name, spec.memory_max, config.pids_max))
                .transpose()
                .map_err(|e| format!("cgroup: {e}"))?;
            let procs = cgroup
                .as_ref()
                .map(|dir| {
                    CString::new(
                        dir.join("cgroup.procs")
                            .into_os_string()
                            .into_encoded_bytes(),
                    )
                })
                .transpose()
                .map_err(|e| format!("cgroup: {e}"))?;
            let data_limit = cgroup.is_none().then_some(spec.memory_max);
            // SAFETY: between fork and exec the closure only makes system
            // calls through rustix, which neither allocate nor take locks;
            // `procs`, the memfd and the bell were made before the fork.
            unsafe {
                cmd.pre_exec(move || {
                    use rustix::process::{
                        Resource, Rlimit, Signal, set_parent_process_death_signal, setrlimit,
                    };
                    set_parent_process_death_signal(Some(Signal::KILL))?;
                    let none = Rlimit {
                        current: Some(0),
                        maximum: Some(0),
                    };
                    setrlimit(Resource::Core, none)?;
                    if let Some(bytes) = data_limit {
                        setrlimit(
                            Resource::Data,
                            Rlimit {
                                current: Some(bytes),
                                maximum: Some(bytes),
                            },
                        )?;
                    }
                    if let Some(procs) = &procs {
                        let fd = rustix::fs::open(
                            procs.as_c_str(),
                            rustix::fs::OFlags::WRONLY,
                            rustix::fs::Mode::empty(),
                        )?;
                        rustix::io::write(&fd, b"0")?;
                    }
                    channel::install(
                        memfd.as_fd(),
                        bell_app.as_fd(),
                        door_app.as_fd(),
                        fds_app.as_fd(),
                    )?;
                    Ok(())
                });
            }
            let child = cmd
                .spawn()
                .map_err(|e| format!("{}: {e}", runner.display()))?;
            // `cmd` holds the child's ends (the memfd, the bell's write end,
            // the log pipe's): dropping it closes the node's copies, so the
            // bell's end of file means the app has exited.
            drop(cmd);
            let bell = pipe::Receiver::from_owned_fd(bell).map_err(|e| format!("bell: {e}"))?;
            let log = pipe::Receiver::from_owned_fd(log).map_err(|e| format!("log pipe: {e}"))?;
            tokio::spawn(copy_log(log, output));
            Ok(Self {
                child,
                channel,
                bell,
                door,
                fds,
                next_fd: 1,
                exited: false,
                input: Input {
                    buf: Vec::with_capacity(RING_CAP),
                    start: 0,
                    done: false,
                },
                pending: Vec::new(),
                pending_at: 0,
                out: Vec::new(),
                closed: false,
                broken: None,
                spin: config.spin,
                active: Instant::now(),
                worker_idle: false,
                cgroup,
            })
        }

        /// Send a frame to the app: written into its ring now as far as it
        /// fits, the rest as the app reads. Dropped once the ring is
        /// closed.
        pub(crate) fn send(&mut self, frame: &ToApp) {
            if self.closed {
                return;
            }
            // A hot frame where the layouts cover it (T7), CBOR otherwise.
            let mut out = std::mem::take(&mut self.out);
            out.clear();
            if bulkhead_native::hot::write_to_app(&mut out, frame).is_some() {
                self.send_bytes(&out);
            } else {
                self.send_bytes(&protocol::encode(frame));
            }
            if out.capacity() <= KEPT_BUFFER {
                self.out = out;
            }
        }

        /// Answer a hot frame's keyvalue request, in a hot frame, as
        /// [`Port::send`] sends.
        pub(crate) fn send_kv(&mut self, id: u64, reply: &bulkhead_native::hot::KvReply<'_>) {
            if self.closed {
                return;
            }
            let mut out = std::mem::take(&mut self.out);
            out.clear();
            bulkhead_native::hot::write_kv_answer(&mut out, id, reply);
            self.send_bytes(&out);
            // A large value's buffer is not kept for the instance's life.
            if out.capacity() <= KEPT_BUFFER {
                self.out = out;
            }
        }

        fn send_bytes(&mut self, bytes: &[u8]) {
            self.active = Instant::now();
            if self.pending_at < self.pending.len() {
                self.pending.extend_from_slice(bytes);
                self.flush();
                return;
            }
            let ring = self.channel.to_app();
            let written = match ring.write(bytes) {
                Ok(n) => n,
                Err(e) => return self.corrupt(e),
            };
            channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
            if written < bytes.len() {
                self.pending.clear();
                self.pending.extend_from_slice(&bytes[written..]);
                self.pending_at = 0;
            }
        }

        /// Pass a published bucket's snapshot to the app, for the frame
        /// that names it.
        ///
        /// # Errors
        ///
        /// As [`Self::send_fd`].
        pub(crate) fn send_published(
            &mut self,
            version: &crate::services::keyvalue::publish::Version,
        ) -> Result<bulkhead_native::protocol::FdRef, String> {
            self.send_fd(version.memfd.as_fd())
        }

        /// Pass `fd` to the app, for a frame sent after this one to name.
        ///
        /// # Errors
        ///
        /// The socket refused it: full (an app that takes no
        /// descriptors), or the app gone.
        pub(crate) fn send_fd(
            &mut self,
            fd: std::os::fd::BorrowedFd<'_>,
        ) -> Result<bulkhead_native::protocol::FdRef, String> {
            let id = self.next_fd;
            channel::send_fd(self.fds.as_fd(), id, fd)
                .map_err(|e| format!("pass a descriptor: {e}"))?;
            self.next_fd += 1;
            Ok(bulkhead_native::protocol::FdRef(id))
        }

        /// Write what waits for the app's ring, as far as it fits.
        fn flush(&mut self) {
            if self.pending_at == self.pending.len() {
                return;
            }
            let ring = self.channel.to_app();
            match ring.write(&self.pending[self.pending_at..]) {
                Ok(0) => {}
                Ok(n) => {
                    self.pending_at += n;
                    channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
                    if self.pending_at == self.pending.len() {
                        self.pending.clear();
                        self.pending_at = 0;
                    }
                }
                Err(e) => self.corrupt(e),
            }
        }

        /// The app broke its channel: no more frames from it.
        fn corrupt(&mut self, e: channel::Corrupt) {
            if !self.input.done {
                self.input.done = true;
                self.broken = Some(e.to_string());
            }
            self.closed = true;
        }

        /// The next thing from the app; `None` once it has exited and
        /// everything it sent is read. Cancel safe: bytes read stay
        /// buffered for the next call.
        pub(crate) async fn recv(&mut self) -> Option<Incoming> {
            self.next(false).await
        }

        /// [`Port::recv`] inside a callback, where nothing but the app's
        /// answer is expected soon: with `watch`, while the worker has
        /// nothing else to do, the wait watches the rings within the poll
        /// (T9). The caller passes `false` when it just woke another task,
        /// which would wait behind the watch.
        pub(crate) async fn recv_in_callback(&mut self, watch: bool) -> Option<Incoming> {
            self.next(watch).await
        }

        async fn next(&mut self, watch: bool) -> Option<Incoming> {
            loop {
                if let Some(next) = self.take() {
                    return Some(next);
                }
                if self.input.done {
                    return None;
                }
                self.flush();
                match self.read() {
                    Ok(0) => {}
                    Ok(_) => {
                        self.active = Instant::now();
                        continue;
                    }
                    Err(e) => {
                        self.corrupt(e);
                        continue;
                    }
                }
                if self.exited {
                    self.input.done = true;
                    continue;
                }
                self.wait(watch).await;
            }
        }

        /// What the app sent, appended to the input; how many bytes.
        fn read(&mut self) -> Result<usize, channel::Corrupt> {
            let ring = self.channel.to_node();
            let input = &mut self.input;
            if input.start > 0 && input.start == input.buf.len() {
                input.buf.clear();
                input.start = 0;
            } else if input.start > RING_CAP {
                input.buf.drain(..input.start);
                input.start = 0;
            }
            // Only what is there: growing the buffer zeroes it.
            let available = ring.available()?;
            if available == 0 {
                return Ok(0);
            }
            let len = input.buf.len();
            input.buf.resize(len + available, 0);
            let n = ring.read(&mut input.buf[len..]);
            input.buf.truncate(len + *n.as_ref().unwrap_or(&0));
            if n.as_ref().is_ok_and(|n| *n > 0) {
                // The app may wait for the room just made.
                channel::wake_writer(ring.writer_waiting());
            }
            n
        }

        /// Wait for the app: spinning (yielding to the runtime's other
        /// tasks) until `spin_us` after the last frame, then asleep on the
        /// bell. Inside a callback (`watch`), on a worker whose last yield
        /// ran no other native instance, a turn of the spin first watches
        /// the rings within the poll.
        async fn wait(&mut self, watch: bool) {
            if self.active.elapsed() < self.spin {
                if watch && self.worker_idle && self.watch() {
                    return;
                }
                // Not watching, or the slice ended with the spin still on.
                if self.active.elapsed() < self.spin {
                    let before = turn();
                    tokio::task::yield_now().await;
                    self.worker_idle = turn() == (before.0, before.1 + 1);
                    return;
                }
            }
            let waiting_room = self.pending_at < self.pending.len();
            let (to_node, to_app) = (self.channel.to_node(), self.channel.to_app());
            to_node.reader_waiting().store(1, Ordering::SeqCst);
            if waiting_room {
                to_app.writer_waiting().store(1, Ordering::SeqCst);
            }
            std::sync::atomic::fence(Ordering::SeqCst);
            if !(to_node.is_readable() || waiting_room && to_app.has_room()) {
                // A wait for readiness, then a drain: the bell's bytes only
                // say "look", and its end of file says the app has gone.
                if self.bell.readable().await.is_ok() {
                    let mut rung = [0u8; 64];
                    loop {
                        match self.bell.try_read(&mut rung) {
                            Ok(0) => {
                                self.exited = true;
                                break;
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
            }
            to_node.reader_waiting().store(0, Ordering::SeqCst);
            to_app.writer_waiting().store(0, Ordering::SeqCst);
            self.active = Instant::now();
        }

        /// Watch the rings within the poll for up to [`WATCH_SLICE`], no
        /// later than the end of the spin: whether the app sent a frame, or
        /// made room the port waits for. Returning to tokio instead sends an idle worker's thread
        /// searching, and the search ends by waking a parked worker that
        /// finds nothing (ROADMAP T9).
        fn watch(&self) -> bool {
            let waiting_room = self.pending_at < self.pending.len();
            let (to_node, to_app) = (self.channel.to_node(), self.channel.to_app());
            let until = (Instant::now() + WATCH_SLICE).min(self.active + self.spin);
            loop {
                for _ in 0..16 {
                    if to_node.is_readable() || waiting_room && to_app.has_room() {
                        return true;
                    }
                    std::hint::spin_loop();
                }
                if Instant::now() >= until {
                    return false;
                }
            }
        }

        /// A whole frame from the input, if one is there.
        fn take(&mut self) -> Option<Incoming> {
            if let Some(why) = self.broken.take() {
                return Some(Incoming::Broken(why));
            }
            let input = &mut self.input;
            if input.done {
                return None;
            }
            let have = &input.buf[input.start..];
            let prefix: [u8; 4] = have.get(..4)?.try_into().ok()?;
            let len = match protocol::frame_len(prefix) {
                Ok(len) => len,
                Err(e) => {
                    input.done = true;
                    input.buf.clear();
                    input.start = 0;
                    return Some(Incoming::Broken(e.to_string()));
                }
            };
            let body = have.get(4..4 + len)?;
            let next = if bulkhead_native::hot::is_hot(body) {
                bulkhead_native::hot::parse_from_app(body).map(|hot| match hot {
                    FromAppHot::Kv(f) => Incoming::Kv(super::KvCall {
                        id: f.id,
                        bucket: f.bucket,
                        op: f.op,
                        key: f.key.to_string(),
                        value: f.value.to_vec(),
                    }),
                    FromAppHot::Frame(frame) => Incoming::Frame(frame),
                })
            } else {
                protocol::decode(body).map(Incoming::Frame)
            }
            .unwrap_or_else(|e| Incoming::Broken(e.to_string()));
            input.start += 4 + len;
            if matches!(next, Incoming::Broken(_)) {
                input.done = true;
                input.buf.clear();
                input.start = 0;
            }
            Some(next)
        }

        /// Close the ring to the app: it exits once it has read what is
        /// there.
        pub(crate) fn close(&mut self) {
            if !self.closed {
                self.flush();
                self.closed = true;
                let ring = self.channel.to_app();
                ring.close();
                channel::wake_reader(ring.reader_waiting(), self.door.as_fd());
            }
        }

        /// Kill the process now.
        pub(crate) fn kill(&mut self) {
            let _ = self.child.start_kill();
        }

        /// Why the process ended, once it has: waited for within a grace
        /// period, then killed. Over its cgroup's `memory.max` is
        /// `memory_limit`.
        pub(crate) async fn exit_reason(&mut self) -> ExitReason {
            let status = match tokio::time::timeout(EXIT_GRACE, self.child.wait()).await {
                Ok(Ok(status)) => Some(status),
                _ => {
                    self.kill();
                    self.child.wait().await.ok()
                }
            };
            if self.cgroup.as_deref().is_some_and(cgroup::oom_killed) {
                return ExitReason::Crashed("memory_limit".into());
            }
            ExitReason::Crashed(match status {
                Some(s) => match (s.code(), s.signal()) {
                    (Some(code), _) => format!("exited with status {code}"),
                    (_, Some(signal)) => format!("killed by signal {signal}"),
                    _ => "exited".into(),
                },
                None => "exited".into(),
            })
        }

        /// Wait for the process to exit after a stop (closing its ring,
        /// then killing it after the grace period), and remove its cgroup.
        /// Again does nothing more.
        pub(crate) async fn finish(&mut self) {
            self.close();
            if tokio::time::timeout(EXIT_GRACE, self.child.wait())
                .await
                .is_err()
            {
                self.kill();
                let _ = self.child.wait().await;
            }
            if let Some(dir) = self.cgroup.take() {
                cgroup::remove(&dir).await;
            }
        }
    }

    /// The app's stdout and stderr, as log lines.
    async fn copy_log(mut log: pipe::Receiver, output: CapturedOutput) {
        use wasmtime_wasi::cli::StdoutStream;
        let mut lines = std::pin::Pin::from(output.async_stream());
        let _ = tokio::io::copy(&mut log, &mut lines).await;
        let _ = lines.shutdown().await;
    }
}

#[cfg(not(target_os = "linux"))]
mod elsewhere {
    use std::convert::Infallible;

    use super::Incoming;
    use crate::config::NativeConfig;
    use crate::message::ExitReason;
    use crate::native::NativeCode;
    use crate::output::CapturedOutput;
    use crate::spec::InstanceSpec;
    use bulkhead_native::protocol::ToApp;

    /// No native process runs here: [`Port::spawn`] refuses, so no `Port`
    /// exists.
    #[derive(Debug)]
    pub(crate) struct Port(Infallible);

    impl Port {
        /// Native apps run on Linux only.
        ///
        /// # Errors
        ///
        /// Always.
        pub(crate) fn spawn(
            _config: &NativeConfig,
            _program: &NativeCode,
            _spec: &InstanceSpec,
            _name: &str,
            _output: CapturedOutput,
        ) -> Result<Self, String> {
            Err("native apps run on Linux only".into())
        }

        pub(crate) fn send(&mut self, _frame: &ToApp) {
            match self.0 {}
        }

        pub(crate) fn send_kv(&mut self, _id: u64, _reply: &bulkhead_native::hot::KvReply<'_>) {
            match self.0 {}
        }

        pub(crate) async fn recv(&mut self) -> Option<Incoming> {
            match self.0 {}
        }

        pub(crate) async fn recv_in_callback(&mut self, _watch: bool) -> Option<Incoming> {
            match self.0 {}
        }

        pub(crate) fn close(&mut self) {
            match self.0 {}
        }

        pub(crate) fn kill(&mut self) {
            match self.0 {}
        }

        pub(crate) fn send_published(
            &mut self,
            _version: &crate::services::keyvalue::publish::Version,
        ) -> Result<bulkhead_native::protocol::FdRef, String> {
            match self.0 {}
        }

        pub(crate) async fn exit_reason(&mut self) -> ExitReason {
            match self.0 {}
        }

        pub(crate) async fn finish(&mut self) {
            match self.0 {}
        }
    }
}

#[cfg(test)]
#[path = "port_tests.rs"]
mod port_tests;
