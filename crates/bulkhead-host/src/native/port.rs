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
//! Except after a callback hinted idle (nothing queued behind it, and it
//! came longer than the spin after the frame before): then neither the
//! task nor the app spins until the next one (ROADMAP T1b).
//! What the app prints, on stdout or stderr, becomes log lines, as a
//! component's stderr does. Between fork and exec the child gets the
//! parent-death signal (`SIGKILL`), no core dumps, its memory limit (the
//! cgroup it starts in, or `RLIMIT_DATA` without one) and its channel. It
//! runs in a process group of its own, with an environment of
//! `BULKHEAD_PRIV` (the bundle's `priv/` files, when it has any),
//! `BULKHEAD_PREOPENS` (where each preopened directory is) and the spin
//! budget. A read-write volume is a `--write` directory for the runner.
//!
//! Native apps run on Linux only (`bulkhead-runner` is Linux only); elsewhere
//! [`Port::spawn`] refuses and a `Port` never exists.

use std::time::Duration;

use bulkhead_native::protocol::{FromApp, ToApp};

use crate::config::NativeConfig;
use crate::failover::RunUntil;
use crate::message::ExitReason;
use crate::native::Program;
use crate::output::CapturedOutput;
use crate::spec::InstanceSpec;

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
pub(crate) use linux::ProcessPort;

#[cfg(not(target_os = "linux"))]
pub(crate) use elsewhere::ProcessPort;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::os::fd::AsFd;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use bulkhead_bundle::manifest::{Capability, NativeProgram};
    use bulkhead_native::channel::{self, Channel, RING_CAP};
    use bulkhead_native::hot::FromAppHot;
    use bulkhead_native::protocol::{self, ToApp};
    use tokio::io::AsyncWriteExt;
    use tokio::net::unix::pipe;
    use tokio::process::{Child, Command};

    use super::{EXIT_GRACE, Incoming};
    use crate::config::NativeConfig;
    use crate::failover::RunUntil;
    use crate::message::ExitReason;
    use crate::native::memory::MemorySource;
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
    pub(crate) struct ProcessPort {
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
        /// The last callback was hinted idle (`hint_idle`), so the wait
        /// between callbacks does not spin.
        idle: bool,
        /// The budget of the wait in progress.
        cur_spin: Duration,
        /// When a frame last went either way: the task spins until
        /// `spin` after it.
        active: Instant,
        /// No other native instance ran on this task's worker during its
        /// last yield: the worker has nothing else to do (T9).
        worker_idle: bool,
        cgroup: Option<PathBuf>,
        /// For an instance bound to a lease, the task writing its
        /// deadlines to the runner's watchdog ([`bulkhead_native::lease`]),
        /// ended with the port.
        lease: Option<Renewals>,
        /// The protocol version the app said it speaks (`Hello`): frames
        /// newer than it are not sent.
        version: u16,
    }

    /// The task writing a leased port's deadlines; aborted on drop.
    #[derive(Debug)]
    struct Renewals(tokio::task::JoinHandle<()>);

    impl Drop for Renewals {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// The deadline the runner's watchdog enforces for `run_until`: its
    /// end on `CLOCK_MONOTONIC`, in nanoseconds ([`bulkhead_native::lease`]).
    pub(crate) fn deadline(run_until: RunUntil) -> u64 {
        use bulkhead_native::lease::{NO_DEADLINE, now_ns};
        match run_until {
            RunUntil::Always => NO_DEADLINE,
            RunUntil::Never => 0,
            RunUntil::Until(end) => {
                let left = end.saturating_duration_since(tokio::time::Instant::now());
                now_ns().saturating_add(u64::try_from(left.as_nanos()).unwrap_or(u64::MAX))
            }
        }
    }

    /// Write each change of `lease` to the watchdog's pipe until the
    /// runner has gone. A full pipe drops a deadline: the watchdog drains
    /// it whenever it runs, and the next change follows.
    async fn renew(mut lease: tokio::sync::watch::Receiver<RunUntil>, pipe: std::os::fd::OwnedFd) {
        loop {
            if lease.changed().await.is_err() {
                return;
            }
            let at = deadline(*lease.borrow_and_update());
            match rustix::io::write(&pipe, &at.to_le_bytes()) {
                Ok(_) | Err(rustix::io::Errno::AGAIN) => {}
                Err(_) => return,
            }
        }
    }

    /// The longest a wait watches the rings within one poll (T9): a task
    /// the idle check cannot see (not a native instance) waits up to this
    /// behind it. Watching until the end of the spin made requests a
    /// little cheaper and calls between 16 busy instances about 17% slower
    /// (16-core machine, 2026-11-04).
    const WATCH_SLICE: Duration = Duration::from_micros(5);

    /// Whether a callback is hinted idle: nothing queued behind it, and
    /// `since` the last frame longer than the `spin` (ROADMAP T1b).
    pub(crate) fn hinted_idle(nothing_queued: bool, since: Duration, spin: Duration) -> bool {
        nothing_queued && since > spin
    }

    /// Why an app speaking protocol `version` cannot be sent `frame`, if
    /// it is newer than that version.
    pub(crate) fn too_new(frame: &ToApp, version: u16) -> Option<String> {
        (frame.since() > version).then(|| {
            format!(
                "the app speaks protocol version {version}, which has no {} frame (version {})",
                to_app_name(frame),
                frame.since()
            )
        })
    }

    /// A frame's name, for errors.
    fn to_app_name(frame: &ToApp) -> &'static str {
        match frame {
            ToApp::Init { .. } => "init",
            ToApp::LoadSnapshot { .. } => "load-snapshot",
            ToApp::SaveSnapshot => "save-snapshot",
            ToApp::Call { .. } => "call",
            ToApp::Cast { .. } => "cast",
            ToApp::Info { .. } => "info",
            ToApp::Connection { .. } => "connection",
            ToApp::Terminate { .. } => "terminate",
            ToApp::Answer { .. } => "answer",
            ToApp::Ping { .. } => "ping",
            ToApp::Published { .. } => "published",
        }
    }

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

    impl ProcessPort {
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
            lease: Option<tokio::sync::watch::Receiver<RunUntil>>,
        ) -> Result<Self, String> {
            let runner = runner(config);
            let mut cmd = Command::new(&runner);
            cmd.arg(match program.kind {
                NativeProgram::Library => "--library",
                NativeProgram::Executable => "--executable",
            })
            .arg(&program.path);
            // A manifest's preopens stay in the spec when the operator
            // denies the capability: they count only with the grant.
            let preopens: &[crate::spec::Preopen] =
                if spec.granted.contains(&Capability::Filesystem) {
                    &spec.preopens
                } else {
                    &[]
                };
            for p in preopens {
                cmd.arg(if p.writable { "--write" } else { "--read" })
                    .arg(&p.host);
            }
            if config.landlock_optional {
                cmd.args(["--landlock", "best-effort"]);
            }
            // A lease's first deadline is in the pipe before the runner
            // starts; the rest follow from a task.
            let lease_pipe = match &lease {
                Some(rx) => {
                    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
                        .map_err(|e| format!("lease pipe: {e}"))?;
                    let first = deadline(*rx.borrow());
                    rustix::io::write(&write, &first.to_le_bytes())
                        .map_err(|e| format!("lease pipe: {e}"))?;
                    rustix::fs::fcntl_setfl(&write, rustix::fs::OFlags::NONBLOCK)
                        .map_err(|e| format!("lease pipe: {e}"))?;
                    cmd.arg(bulkhead_native::lease::FLAG);
                    Some((read, write))
                }
                None => None,
            };
            // The read end goes with the closure below, which `cmd` holds.
            let (lease_read, lease_write) = lease_pipe.unzip();
            cmd.env_clear();
            if let Some(p) = preopens.iter().find(|p| p.guest == crate::spec::PRIV_PATH) {
                cmd.env(protocol::PRIV_ENV, &p.host);
            }
            if !preopens.is_empty() {
                let preopens = bulkhead_native::preopens::encode(
                    preopens
                        .iter()
                        .map(|p| (p.guest.as_str(), p.host.as_path())),
                )
                .ok_or("a preopened directory's path is not UTF-8")?;
                cmd.env(bulkhead_native::preopens::ENV, preopens);
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
                    // Above the channel's descriptors before they move
                    // into place, then to its own; the copy closes at exec.
                    let lease = lease_read
                        .as_ref()
                        .map(|fd| {
                            rustix::io::fcntl_dupfd_cloexec(
                                fd,
                                bulkhead_native::lease::LEASE_FD + 1,
                            )
                        })
                        .transpose()?;
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
                    if let Some(lease) = &lease {
                        bulkhead_native::lease::install(lease.as_fd())?;
                    }
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
            let lease = lease
                .zip(lease_write)
                .map(|(rx, write)| Renewals(tokio::spawn(renew(rx, write))));
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
                idle: false,
                cur_spin: config.spin,
                active: Instant::now(),
                worker_idle: false,
                cgroup,
                lease,
                version: protocol::VERSION,
            })
        }

        /// Send a frame to the app: written into its ring now as far as it
        /// fits, the rest as the app reads. Dropped once the ring is
        /// closed.
        /// The app said it speaks protocol `version` (one the node
        /// accepts, [`protocol::accepts`]).
        /// Where this process's memory can be read.
        pub(crate) fn memory_source(&self) -> Option<MemorySource> {
            match &self.cgroup {
                Some(dir) => Some(MemorySource::Cgroup(dir.clone())),
                None => self.child.id().map(MemorySource::Process),
            }
        }

        pub(crate) fn set_version(&mut self, version: u16) {
            self.version = version;
        }

        pub(crate) fn send(&mut self, frame: &ToApp) {
            if self.closed {
                return;
            }
            // An older app cannot read a newer frame: its instance ends
            // with the reason rather than waiting on a frame it dropped.
            if let Some(why) = too_new(frame, self.version) {
                self.broken.get_or_insert(why);
                self.close();
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
        /// [`ProcessPort::send`] sends.
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
            self.cur_spin = if self.idle { Duration::ZERO } else { self.spin };
            self.next(false).await
        }

        /// Before a callback's frame is written: hint the app idle when
        /// nothing waits behind the callback and it came longer than the
        /// spin after the last frame, so neither side spins after it
        /// (ROADMAP T1b). An instance called back to back keeps spinning.
        pub(crate) fn hint_idle(&mut self, nothing_queued: bool) {
            let idle = hinted_idle(nothing_queued, self.active.elapsed(), self.spin);
            self.idle = idle;
            self.channel
                .to_app()
                .idle_hint()
                .store(u32::from(idle), Ordering::SeqCst);
        }

        /// [`ProcessPort::recv`] inside a callback, where nothing but the app's
        /// answer is expected soon: with `watch`, while the worker has
        /// nothing else to do, the wait watches the rings within the poll
        /// (T9). The caller passes `false` when it just woke another task,
        /// which would wait behind the watch.
        pub(crate) async fn recv_in_callback(&mut self, watch: bool) -> Option<Incoming> {
            self.cur_spin = self.spin;
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
            let spin = self.cur_spin;
            if self.active.elapsed() < spin {
                if watch && self.worker_idle && self.watch() {
                    return;
                }
                // Not watching, or the slice ended with the spin still on.
                if self.active.elapsed() < spin {
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
            self.reason(status)
        }

        /// Why the process ended, if it has already: for a lease's end,
        /// which the runner's watchdog may have enforced while the node
        /// did not run.
        pub(crate) fn exited_reason(&mut self) -> Option<ExitReason> {
            let status = self.child.try_wait().ok().flatten()?;
            Some(self.reason(Some(status)))
        }

        fn reason(&self, status: Option<std::process::ExitStatus>) -> ExitReason {
            if self.cgroup.as_deref().is_some_and(cgroup::oom_killed) {
                return ExitReason::Crashed("memory_limit".into());
            }
            let killed_exit = i32::from(bulkhead_native::lease::KILLED_EXIT);
            ExitReason::Crashed(match status {
                Some(s) => match (s.code(), s.signal()) {
                    (Some(code), _) if code == killed_exit && self.lease.is_some() => {
                        "killed_at_lease_end".into()
                    }
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
    use crate::failover::RunUntil;
    use crate::message::ExitReason;
    use crate::native::NativeCode;
    use crate::output::CapturedOutput;
    use crate::spec::InstanceSpec;
    use bulkhead_native::protocol::ToApp;

    /// No native process runs here: [`ProcessPort::spawn`] refuses, so no `ProcessPort`
    /// exists.
    #[derive(Debug)]
    pub(crate) struct ProcessPort(Infallible);

    impl ProcessPort {
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
            _lease: Option<tokio::sync::watch::Receiver<RunUntil>>,
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

        pub(crate) fn hint_idle(&mut self, _idle: bool) {}

        pub(crate) fn set_version(&mut self, _version: u16) {
            match self.0 {}
        }

        pub(crate) fn memory_source(&self) -> Option<crate::native::memory::MemorySource> {
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

        #[cfg(unix)]
        pub(crate) fn send_fd(
            &mut self,
            _fd: std::os::fd::BorrowedFd<'_>,
        ) -> Result<bulkhead_native::protocol::FdRef, String> {
            match self.0 {}
        }

        pub(crate) async fn exit_reason(&mut self) -> ExitReason {
            match self.0 {}
        }

        pub(crate) fn exited_reason(&mut self) -> Option<ExitReason> {
            match self.0 {}
        }

        pub(crate) async fn finish(&mut self) {
            match self.0 {}
        }
    }
}

/// A native instance's app: a process through `bulkhead-runner`, or, for
/// tests (cargo feature `test-apps`, ROADMAP Z9c), an app polled inside the
/// instance's own task ([`crate::in_process`]). Both speak the protocol's
/// frames, so the instance's lifecycle is the same.
// The in-process variant exists only with the test feature; a node runs
// processes only, so boxing the process port would cost every instance an
// allocation for a test's sake.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum Port {
    /// A process.
    Process(ProcessPort),
    /// An in-process test app.
    #[cfg(feature = "test-apps")]
    InProcess(crate::in_process::InProcessPort),
}

/// Call `$method` on whichever port this is.
macro_rules! each {
    ($self:ident, $port:ident => $e:expr) => {
        match $self {
            Port::Process($port) => $e,
            #[cfg(feature = "test-apps")]
            Port::InProcess($port) => $e,
        }
    };
}

impl Port {
    /// Start `program` for the instance `name` (its pid, as text): a
    /// process, or an in-process app.
    ///
    /// # Errors
    ///
    /// As [`ProcessPort::spawn`].
    pub(crate) fn spawn(
        config: &NativeConfig,
        program: &Program,
        spec: &InstanceSpec,
        name: &str,
        output: CapturedOutput,
        lease: Option<tokio::sync::watch::Receiver<RunUntil>>,
    ) -> Result<Self, String> {
        match program {
            Program::Process(code) => {
                ProcessPort::spawn(config, code, spec, name, output, lease).map(Port::Process)
            }
            #[cfg(feature = "test-apps")]
            Program::InProcess(app) => Ok(Port::InProcess(app.start())),
        }
    }

    pub(crate) fn set_version(&mut self, version: u16) {
        each!(self, p => p.set_version(version));
    }

    /// Where this instance's memory can be read: a process's. An in-process
    /// app has none of its own.
    pub(crate) fn memory_source(&self) -> Option<crate::native::memory::MemorySource> {
        match self {
            Self::Process(p) => p.memory_source(),
            #[cfg(feature = "test-apps")]
            Self::InProcess(_) => None,
        }
    }

    pub(crate) fn send(&mut self, frame: &ToApp) {
        each!(self, p => p.send(frame));
    }

    pub(crate) fn send_kv(&mut self, id: u64, reply: &bulkhead_native::hot::KvReply<'_>) {
        each!(self, p => p.send_kv(id, reply));
    }

    pub(crate) fn send_published(
        &mut self,
        version: &crate::services::keyvalue::publish::Version,
    ) -> Result<bulkhead_native::protocol::FdRef, String> {
        each!(self, p => p.send_published(version))
    }

    #[cfg(unix)]
    pub(crate) fn send_fd(
        &mut self,
        fd: std::os::fd::BorrowedFd<'_>,
    ) -> Result<bulkhead_native::protocol::FdRef, String> {
        each!(self, p => p.send_fd(fd))
    }

    pub(crate) async fn recv(&mut self) -> Option<Incoming> {
        each!(self, p => p.recv().await)
    }

    pub(crate) fn hint_idle(&mut self, nothing_queued: bool) {
        each!(self, p => p.hint_idle(nothing_queued));
    }

    pub(crate) async fn recv_in_callback(&mut self, watch: bool) -> Option<Incoming> {
        each!(self, p => p.recv_in_callback(watch).await)
    }

    pub(crate) fn kill(&mut self) {
        each!(self, p => p.kill());
    }

    pub(crate) async fn exit_reason(&mut self) -> ExitReason {
        each!(self, p => p.exit_reason().await)
    }

    pub(crate) fn exited_reason(&mut self) -> Option<ExitReason> {
        each!(self, p => p.exited_reason())
    }

    pub(crate) async fn finish(&mut self) {
        each!(self, p => p.finish().await);
    }
}

#[cfg(test)]
#[path = "port_tests.rs"]
mod port_tests;
