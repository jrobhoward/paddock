//! A native instance's process: `bulkhead-runner`, its channel and its
//! limits.
//!
//! Frames travel over the instance's channel (`bulkhead_native::channel`,
//! ROADMAP T1, spike S32): two rings in a memfd the app inherits, read and
//! written by the instance's task itself through a
//! [`Link`](super::link::Link), which has the spinning, sleeping and idle
//! hints of the waits.
//!
//! What the app prints, on stdout or stderr, becomes log lines. Between
//! fork and exec the child gets the
//! parent-death signal (`SIGKILL`), no core dumps, its memory limit (the
//! cgroup it starts in, or `RLIMIT_DATA` without one) and its channel. It
//! runs in a process group of its own, with an environment of
//! `BULKHEAD_PRIV` (the bundle's `priv/` files, when it has any),
//! `BULKHEAD_PREOPENS` (where each preopened directory is) and the spin
//! budget. A read-write volume is a `--write` directory for the runner.
//!
//! Native apps run on Linux and FreeBSD, the same code on both (what
//! differs is in [`crate::native::os`]); elsewhere [`Port::spawn`] refuses
//! and a `Port` never exists. A cgroup is Linux's alone.

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

/// The limit that holds an instance without a cgroup to `memory_max`.
/// Linux's `RLIMIT_DATA` counts private writable mappings, so every
/// allocation. FreeBSD's counts only `brk`, which its malloc does not use,
/// so there it is `RLIMIT_AS`: the whole address space, mapped libraries
/// and thread stacks included, a stricter count than memory in use, until
/// the privileged helper sets `rctl` rules (ROADMAP PS1, FB2).
#[cfg(target_os = "linux")]
const MEMORY_RESOURCE: rustix::process::Resource = rustix::process::Resource::Data;
#[cfg(target_os = "freebsd")]
const MEMORY_RESOURCE: rustix::process::Resource = rustix::process::Resource::As;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) use process::ProcessPort;

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
pub(crate) use elsewhere::ProcessPort;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod process {
    use std::ffi::{CString, OsString};
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::Stdio;

    use bulkhead_bundle::manifest::{Capability, NativeProgram};
    use bulkhead_native::channel::{self, Channel};
    use bulkhead_native::protocol::{self, ToApp};
    use tokio::io::AsyncWriteExt;
    use tokio::net::unix::pipe;
    use tokio::process::{Child, Command};

    use super::{EXIT_GRACE, Incoming};
    use crate::config::NativeConfig;
    use crate::failover::RunUntil;
    use crate::message::ExitReason;
    use crate::native::helper::PrivHelper;
    use crate::native::link::Link;
    use crate::native::memory::MemorySource;
    use crate::native::sender::{FdPass, SenderContext, Senders};
    use crate::native::{NativeCode, cgroup};
    use crate::output::CapturedOutput;
    use crate::spec::InstanceSpec;
    use bulkhead_native::protocol::{SenderChannel, SenderError};
    use std::sync::Arc;

    /// A running native instance's process.
    #[derive(Debug)]
    pub(crate) struct ProcessPort {
        proc: Proc,
        /// The channel's rings, bell and door.
        link: Link,
        /// The node's end of the descriptor socket (T3), shared with the
        /// senders' tasks.
        fds: Arc<FdPass>,
        /// The app's sender channels (ROADMAP AP1), once the instance has
        /// attached them; their tasks end with the port.
        senders: Option<Senders>,
        cgroup: Option<PathBuf>,
        /// The node made `cgroup` and removes it; the helper's own it
        /// removes itself.
        own_cgroup: bool,
        /// For an instance bound to a lease, the task writing its
        /// deadlines to the runner's watchdog ([`bulkhead_native::lease`]),
        /// ended with the port.
        lease: Option<Renewals>,
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

    /// The runner the node starts native apps through.
    fn runner(config: &NativeConfig) -> PathBuf {
        config.runner.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join("bulkhead-runner")))
                .unwrap_or_else(|| PathBuf::from("bulkhead-runner"))
        })
    }

    /// The process behind a port: the node's own child, or one the
    /// privileged helper started under the app's user ([`super::helper`]).
    #[derive(Debug)]
    enum Proc {
        Direct(Child),
        Helper(HelperProc),
    }

    /// An instance's process the helper started: its exit comes from the
    /// helper, and the node ends it through its cgroup (Linux) or the
    /// helper (S36, finding 1: the node cannot signal another user's
    /// process).
    #[derive(Debug)]
    struct HelperProc {
        helper: PrivHelper,
        id: u64,
        pid: u32,
        cgroup: Option<PathBuf>,
        exit: tokio::sync::watch::Receiver<Option<(std::process::ExitStatus, bool)>>,
    }

    impl HelperProc {
        fn kill(&self) {
            if self.exit.borrow().is_some() {
                return;
            }
            // `cgroup.kill` ends everything in the instance's cgroup, with
            // no capability and no round trip; the helper gave it to the
            // node.
            let killed = self
                .cgroup
                .as_ref()
                .is_some_and(|dir| std::fs::write(dir.join("cgroup.kill"), "1").is_ok());
            if !killed {
                self.helper.signal(self.id, libc::SIGKILL);
            }
        }
    }

    impl Drop for HelperProc {
        /// As `kill_on_drop` for the node's own child.
        fn drop(&mut self) {
            self.kill();
        }
    }

    impl Proc {
        fn id(&self) -> Option<u32> {
            match self {
                Self::Direct(c) => c.id(),
                Self::Helper(h) => (h.exit.borrow().is_none()).then_some(h.pid),
            }
        }

        fn start_kill(&mut self) {
            match self {
                Self::Direct(c) => {
                    let _ = c.start_kill();
                }
                Self::Helper(h) => h.kill(),
            }
        }

        async fn wait(&mut self) -> Option<std::process::ExitStatus> {
            match self {
                Self::Direct(c) => c.wait().await.ok(),
                Self::Helper(h) => loop {
                    if let Some((status, _)) = *h.exit.borrow_and_update() {
                        return Some(status);
                    }
                    if h.exit.changed().await.is_err() {
                        return None;
                    }
                },
            }
        }

        fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
            match self {
                Self::Direct(c) => c.try_wait().ok().flatten(),
                Self::Helper(h) => h.exit.borrow().map(|(status, _)| status),
            }
        }

        /// The helper said its memory limit ended it (its cgroup is gone
        /// by the time the node could read it).
        fn memory_limited(&self) -> bool {
            matches!(self, Self::Helper(h) if h.exit.borrow().is_some_and(|(_, oom)| oom))
        }
    }

    /// What the runner gets, either way it starts.
    struct Launch {
        args: Vec<OsString>,
        env: Vec<(&'static str, OsString)>,
        /// Directories the instance needs: the program's, then its
        /// preopens, and whether each is writable.
        dirs: Vec<(PathBuf, bool)>,
    }

    impl ProcessPort {
        /// Start `program` for the instance `name` (its pid, as text).
        /// Called on the runtime the instance's task runs on: the bell and
        /// the log pipe register with its I/O driver.
        ///
        /// # Errors
        ///
        /// Why the process could not start: no runner, a channel or a
        /// cgroup that could not be made, or the helper's refusal.
        pub(crate) async fn spawn(
            config: &NativeConfig,
            program: &NativeCode,
            spec: &InstanceSpec,
            name: &str,
            output: CapturedOutput,
            lease: Option<tokio::sync::watch::Receiver<RunUntil>>,
        ) -> Result<Self, String> {
            let mut launch = Launch {
                args: vec![
                    match program.kind {
                        NativeProgram::Library => "--library",
                        NativeProgram::Executable => "--executable",
                    }
                    .into(),
                    program.path.clone().into(),
                ],
                env: Vec::new(),
                dirs: program
                    .path
                    .parent()
                    .map(|d| (d.to_path_buf(), false))
                    .into_iter()
                    .collect(),
            };
            // A manifest's preopens stay in the spec when the operator
            // denies the capability: they count only with the grant.
            let preopens: &[crate::spec::Preopen] =
                if spec.granted.contains(&Capability::Filesystem) {
                    &spec.preopens
                } else {
                    &[]
                };
            for p in preopens {
                launch
                    .args
                    .push(if p.writable { "--write" } else { "--read" }.into());
                launch.args.push(p.host.clone().into());
                launch.dirs.push((p.host.clone(), p.writable));
            }
            if config.landlock_optional {
                launch.args.push("--landlock".into());
                launch.args.push("best-effort".into());
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
                    launch.args.push(bulkhead_native::lease::FLAG.into());
                    Some((read, write))
                }
                None => None,
            };
            let (lease_read, lease_write) = lease_pipe.unzip();
            if let Some(p) = preopens.iter().find(|p| p.guest == crate::spec::PRIV_PATH) {
                launch.env.push((protocol::PRIV_ENV, p.host.clone().into()));
            }
            if !preopens.is_empty() {
                let preopens = bulkhead_native::preopens::encode(
                    preopens
                        .iter()
                        .map(|p| (p.guest.as_str(), p.host.as_path())),
                )
                .ok_or("a preopened directory's path is not UTF-8")?;
                launch
                    .env
                    .push((bulkhead_native::preopens::ENV, preopens.into()));
            }
            launch.env.push((
                channel::SPIN_ENV,
                config.spin.as_micros().to_string().into(),
            ));
            let (channel, memfd) = Channel::create().map_err(|e| format!("channel: {e}"))?;
            let (bell, bell_app) = channel::bell().map_err(|e| format!("channel: {e}"))?;
            let (door_app, door) = channel::door().map_err(|e| format!("channel: {e}"))?;
            let (fds, fds_app) = channel::descriptors().map_err(|e| format!("channel: {e}"))?;
            let (log, log_app) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
                .map_err(|e| format!("log pipe: {e}"))?;
            let log_out = log_app.try_clone().map_err(|e| format!("log pipe: {e}"))?;
            let ends = AppEnds {
                stdout: log_out,
                stderr: log_app,
                memfd,
                bell: bell_app,
                door: door_app,
                fds: fds_app,
                lease: lease_read,
            };
            // Every end the app gets is moved into the start: once it
            // returns, the node holds none, so the bell's end of file
            // means the app has exited.
            let (proc, cgroup, own_cgroup) = match &config.helper {
                Some(helper) => {
                    let started = through_helper(helper, config, spec, launch, ends).await?;
                    let cgroup = started.cgroup.clone();
                    (Proc::Helper(started), cgroup, false)
                }
                None => {
                    let (child, cgroup) = directly(config, spec, name, launch, ends)?;
                    (Proc::Direct(child), cgroup, true)
                }
            };
            let lease = lease
                .zip(lease_write)
                .map(|(rx, write)| Renewals(tokio::spawn(renew(rx, write))));
            let link = Link::new(channel, bell, door, config.spin)?;
            let log = pipe::Receiver::from_owned_fd(log).map_err(|e| format!("log pipe: {e}"))?;
            tokio::spawn(copy_log(log, output));
            Ok(Self {
                proc,
                link,
                fds: Arc::new(FdPass::new(fds)),
                senders: None,
                cgroup,
                own_cgroup,
                lease,
            })
        }

        /// The app said it speaks protocol `version` (one the node
        /// accepts, [`protocol::accepts`]).
        pub(crate) fn set_version(&mut self, version: u16) {
            self.link.set_version(version);
        }

        /// Send a frame to the app ([`Link::send`]).
        pub(crate) fn send(&mut self, frame: &ToApp) {
            self.link.send(frame);
        }

        /// Answer a hot frame's keyvalue request ([`Link::send_kv`]).
        pub(crate) fn send_kv(&mut self, id: u64, reply: &bulkhead_native::hot::KvReply<'_>) {
            self.link.send_kv(id, reply);
        }

        /// The next thing from the app ([`Link::recv`]).
        pub(crate) async fn recv(&mut self) -> Option<Incoming> {
            self.link.recv().await
        }

        /// Hint the app idle before a callback ([`Link::hint_idle`]).
        pub(crate) fn hint_idle(&mut self, nothing_queued: bool) {
            self.link.hint_idle(nothing_queued);
        }

        /// The next thing from the app inside a callback
        /// ([`Link::recv_in_callback`]).
        pub(crate) async fn recv_in_callback(&mut self, watch: bool) -> Option<Incoming> {
            self.link.recv_in_callback(watch).await
        }

        /// Close the ring to the app: it exits once it has read what is
        /// there.
        pub(crate) fn close(&mut self) {
            self.link.close();
        }

        /// Where this process's memory can be read.
        pub(crate) fn memory_source(&self) -> Option<MemorySource> {
            match &self.cgroup {
                #[cfg(target_os = "linux")]
                Some(dir) => Some(MemorySource::Cgroup(dir.clone())),
                // The runtime refuses a cgroup off Linux.
                #[cfg(not(target_os = "linux"))]
                Some(_) => None,
                None => self.proc.id().map(MemorySource::Process),
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
            self.fds.send(fd)
        }

        /// Let the app open sender channels, served as `ctx`'s instance.
        pub(crate) fn attach_senders(&mut self, ctx: SenderContext) {
            self.senders = Some(Senders::new(ctx, self.fds.clone()));
        }

        /// Make a sender channel for the app (ROADMAP AP1).
        pub(crate) fn open_sender(&mut self) -> Result<SenderChannel, SenderError> {
            match &self.senders {
                Some(senders) => senders.open(),
                None => Err(SenderError::Other("no sender channels attached".into())),
            }
        }

        /// Kill the process now.
        pub(crate) fn kill(&mut self) {
            self.proc.start_kill();
        }

        /// Why the process ended, once it has: waited for within a grace
        /// period, then killed. Over its cgroup's `memory.max` is
        /// `memory_limit`.
        pub(crate) async fn exit_reason(&mut self) -> ExitReason {
            let status = match tokio::time::timeout(EXIT_GRACE, self.proc.wait()).await {
                Ok(Some(status)) => Some(status),
                _ => {
                    self.kill();
                    self.proc.wait().await
                }
            };
            self.reason(status)
        }

        /// Why the process ended, if it has already: for a lease's end,
        /// which the runner's watchdog may have enforced while the node
        /// did not run.
        pub(crate) fn exited_reason(&mut self) -> Option<ExitReason> {
            let status = self.proc.try_wait()?;
            Some(self.reason(Some(status)))
        }

        fn reason(&self, status: Option<std::process::ExitStatus>) -> ExitReason {
            if self.proc.memory_limited()
                || (self.own_cgroup && self.cgroup.as_deref().is_some_and(cgroup::oom_killed))
            {
                return ExitReason::Crashed("memory_limit".into());
            }
            let killed_exit = i32::from(bulkhead_native::lease::KILLED_EXIT);
            ExitReason::Crashed(match status {
                Some(s) => match (s.code(), s.signal()) {
                    (Some(code), _) if code == killed_exit && self.lease.is_some() => {
                        "killed_at_lease_end".into()
                    }
                    (Some(code), _) => format!("exited with status {code}"),
                    #[cfg(target_os = "linux")]
                    (_, Some(libc::SIGSYS)) => {
                        crate::native::syscalls::reason(self.link.refused_call())
                    }
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
            if tokio::time::timeout(EXIT_GRACE, self.proc.wait())
                .await
                .is_err()
            {
                self.kill();
                let _ = self.proc.wait().await;
            }
            if let Some(dir) = self.cgroup.take()
                && self.own_cgroup
            {
                cgroup::remove(&dir).await;
            }
        }
    }

    /// The ends of an instance's descriptors the app gets.
    struct AppEnds {
        stdout: OwnedFd,
        stderr: OwnedFd,
        memfd: OwnedFd,
        bell: OwnedFd,
        door: OwnedFd,
        fds: OwnedFd,
        lease: Option<OwnedFd>,
    }

    /// Start the runner as the node's own child: in a cgroup with
    /// `[native] cgroup`, else held by `RLIMIT_DATA` (`RLIMIT_AS` on
    /// FreeBSD).
    fn directly(
        config: &NativeConfig,
        spec: &InstanceSpec,
        name: &str,
        launch: Launch,
        ends: AppEnds,
    ) -> Result<(Child, Option<PathBuf>), String> {
        let runner = runner(config);
        let mut cmd = Command::new(&runner);
        cmd.args(&launch.args).env_clear();
        for (k, v) in &launch.env {
            cmd.env(k, v);
        }
        let AppEnds {
            stdout,
            stderr,
            memfd,
            bell: bell_app,
            door: door_app,
            fds: fds_app,
            lease: lease_read,
        } = ends;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
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
                        super::MEMORY_RESOURCE,
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
                        rustix::io::fcntl_dupfd_cloexec(fd, bulkhead_native::lease::LEASE_FD + 1)
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
        // `cmd` holds the child's ends: dropping it closes the node's
        // copies.
        drop(cmd);
        Ok((child, cgroup))
    }

    /// Ask the privileged helper to start the runner under the app's user,
    /// in its fences, with `ends` at the numbers the runner expects.
    async fn through_helper(
        helper: &PrivHelper,
        config: &NativeConfig,
        spec: &InstanceSpec,
        launch: Launch,
        ends: AppEnds,
    ) -> Result<HelperProc, String> {
        let text = |s: OsString| {
            s.into_string()
                .map_err(|s| format!("{} is not UTF-8, as the helper needs", s.display()))
        };
        let args = launch
            .args
            .into_iter()
            .map(text)
            .collect::<Result<_, _>>()?;
        let env = launch
            .env
            .into_iter()
            .map(|(k, v)| Ok((k.to_string(), text(v)?)))
            .collect::<Result<_, String>>()?;
        let binds = launch
            .dirs
            .into_iter()
            .map(|(path, writable)| {
                Ok(bulkhead_priv_wire::Bind {
                    path: text(path.into_os_string())?,
                    writable,
                })
            })
            .collect::<Result<_, String>>()?;
        let mut fds = vec![
            (ends.stdout, 1),
            (ends.stderr, 2),
            (ends.memfd, channel::MEMFD_FD),
            (ends.bell, channel::BELL_FD),
            (ends.door, channel::DOOR_FD),
            (ends.fds, channel::FDS_FD),
        ];
        if let Some(lease) = ends.lease {
            fds.push((lease, bulkhead_native::lease::LEASE_FD));
        }
        let (fds, targets): (Vec<OwnedFd>, Vec<u8>) = fds
            .into_iter()
            .map(|(fd, n)| (fd, u8::try_from(n).unwrap_or(u8::MAX)))
            .unzip();
        let request = bulkhead_priv_wire::Spawn {
            id: 0,
            app: spec.app.clone(),
            memory_max: spec.memory_max,
            pids_max: config.pids_max,
            args,
            env,
            binds,
            fds: targets,
        };
        let started = helper.spawn(request, fds).await?;
        Ok(HelperProc {
            helper: helper.clone(),
            id: started.id,
            pid: started.pid,
            cgroup: started.cgroup,
            exit: started.exit,
        })
    }

    /// The app's stdout and stderr, as log lines.
    async fn copy_log(mut log: pipe::Receiver, output: CapturedOutput) {
        let mut lines = std::pin::Pin::from(output.async_stream());
        let _ = tokio::io::copy(&mut log, &mut lines).await;
        let _ = lines.shutdown().await;
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
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
        /// Native apps run on Linux and FreeBSD only.
        ///
        /// # Errors
        ///
        /// Always.
        pub(crate) async fn spawn(
            _config: &NativeConfig,
            _program: &NativeCode,
            _spec: &InstanceSpec,
            _name: &str,
            _output: CapturedOutput,
            _lease: Option<tokio::sync::watch::Receiver<RunUntil>>,
        ) -> Result<Self, String> {
            Err("native apps run on Linux and FreeBSD only".into())
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

        pub(crate) fn open_sender(
            &mut self,
        ) -> Result<bulkhead_native::protocol::SenderChannel, bulkhead_native::protocol::SenderError>
        {
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
    pub(crate) async fn spawn(
        config: &NativeConfig,
        program: &Program,
        spec: &InstanceSpec,
        name: &str,
        output: CapturedOutput,
        lease: Option<tokio::sync::watch::Receiver<RunUntil>>,
    ) -> Result<Self, String> {
        match program {
            Program::Process(code) => ProcessPort::spawn(config, code, spec, name, output, lease)
                .await
                .map(Port::Process),
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

    /// Let the app open sender channels (ROADMAP AP1), served as `ctx`'s
    /// instance. An in-process test app has no channel to share.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(crate) fn attach_senders(&mut self, ctx: crate::native::sender::SenderContext) {
        match self {
            Port::Process(p) => p.attach_senders(ctx),
            #[cfg(feature = "test-apps")]
            Port::InProcess(_) => {}
        }
    }

    /// Make a sender channel for the app.
    pub(crate) fn open_sender(
        &mut self,
    ) -> Result<bulkhead_native::protocol::SenderChannel, bulkhead_native::protocol::SenderError>
    {
        match self {
            Port::Process(p) => p.open_sender(),
            #[cfg(feature = "test-apps")]
            Port::InProcess(_) => Err(bulkhead_native::protocol::SenderError::Unsupported),
        }
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
