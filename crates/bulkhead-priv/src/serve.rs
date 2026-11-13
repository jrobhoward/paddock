//! The helper's loop: synchronous, one thread, one `poll` over the node's
//! socket, the processes it started and a pipe of signals. It answers each
//! request in order and reports each exit as it happens, before it removes
//! what it made for that instance.
//!
//! When the node's socket closes or the node exits, the helper kills every
//! instance, waits for them (up to [`DRAIN`]), and exits with the node's
//! status. `SIGTERM` and `SIGINT` go on to the node as `SIGTERM`, `SIGHUP`
//! as `SIGHUP`, so the node stops its apps itself and then exits.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bulkhead_priv_wire::{Reply, Request, SOCKET_ENV, Spawn};
use rustix::event::{PollFd, PollFlags};

use crate::bindings::Bindings;
use crate::config::Config;
use crate::platform::{self, Instance, Node, State};
use crate::socket::{self, Received};
use crate::sys::Exec;
use crate::{check, share};

/// How long the helper waits for instances to end once the node has gone.
const DRAIN: Duration = Duration::from_secs(5);

/// The descriptor number the node finds the helper's socket at.
const NODE_SOCKET_FD: i32 = 3;

/// The environment the node gets from the helper's: these names, and
/// every `BULKHEAD_*` but the socket's, which the helper sets.
const NODE_ENV: [&str; 6] = ["PATH", "RUST_LOG", "RUST_BACKTRACE", "TZ", "LANG", "LC_ALL"];

/// How the helper meets its node.
#[derive(Debug)]
pub enum Mode {
    /// Start the node from the configuration.
    StartNode,
    /// Serve a node that is already connected on this socket (tests: the
    /// test is the node, and runs as root).
    Inherited(OwnedFd),
}

struct Helper {
    config: Config,
    st: State,
    bindings: Bindings,
    sock: OwnedFd,
    insts: BTreeMap<u64, Instance>,
    /// A stop signal came, and there is no node to pass it to.
    stopping: bool,
}

/// Run the helper; its exit status (the node's).
///
/// # Errors
///
/// Why it could not start.
pub fn run(config: Config, mode: Mode) -> Result<i32, String> {
    if !rustix::process::geteuid().is_root() {
        return Err("bulkhead-priv must run as root".into());
    }
    state_dir(&config)?;
    let bindings = Bindings::load(&config.state)?;
    let st = platform::init(&config)?;
    let signals = crate::sys::signal_pipe(&crate::child::HANDLED)
        .map_err(|e| format!("signal handlers: {e}"))?;
    let (sock, node) = match mode {
        Mode::Inherited(sock) => (sock, None),
        Mode::StartNode => {
            let (mine, theirs) = socket::pair().map_err(|e| format!("socketpair: {e}"))?;
            let exec = node_exec(&config).map_err(|e| format!("the node's command: {e}"))?;
            let node = platform::start_node(
                &config,
                &st,
                exec,
                vec![(theirs.as_raw_fd(), NODE_SOCKET_FD)],
            )
            .map_err(|e| format!("starting the node: {e}"))?;
            drop(theirs);
            eprintln!(
                "bulkhead-priv: the node runs as pid {} (user {})",
                node.pid, config.node_uid
            );
            (mine, Some(node))
        }
    };
    platform::fence_self()?;
    let mut helper = Helper {
        config,
        st,
        bindings,
        sock,
        insts: BTreeMap::new(),
        stopping: false,
    };
    Ok(helper.serve(node.as_ref(), &signals))
}

/// `state`: made 0700 if missing; it must be root's and closed to others.
fn state_dir(config: &Config) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let dir = &config.state;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(format!("{}: {e}", dir.display()));
        }
        _ => {}
    }
    let meta = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{}: must be a directory of root's, mode 0700",
            dir.display()
        ));
    }
    Ok(())
}

/// The node's program, arguments and environment.
fn node_exec(config: &Config) -> std::io::Result<Exec> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            NODE_ENV.contains(&k.as_str()) || (k.starts_with("BULKHEAD_") && k != SOCKET_ENV)
        })
        .collect();
    env.push((SOCKET_ENV.to_string(), NODE_SOCKET_FD.to_string()));
    Exec::new(&config.node.display().to_string(), &config.node_args, &env)
}

impl Helper {
    fn serve(&mut self, node: Option<&Node>, signals: &OwnedFd) -> i32 {
        let mut node_status = None;
        loop {
            let insts: Vec<&Instance> = self.insts.values().collect();
            let exit_fds: Vec<BorrowedFd<'_>> = platform::watched(&self.st, node, &insts);
            let mut fds = vec![
                PollFd::new(&self.sock, PollFlags::IN),
                PollFd::new(signals, PollFlags::IN),
            ];
            fds.extend(exit_fds.iter().map(|fd| PollFd::new(fd, PollFlags::IN)));
            if let Err(e) = rustix::event::poll(&mut fds, None)
                && e != rustix::io::Errno::INTR
            {
                eprintln!("bulkhead-priv: poll: {e}");
                break;
            }
            let ready: Vec<bool> = fds.iter().map(|f| !f.revents().is_empty()).collect();
            let sock_ready = ready[0];
            let sig_ready = ready[1];
            // Exits first, so an instance's exit is reported before the
            // answer to a request that came after it.
            let exits = platform::exits(&self.st, node, &insts, &ready[2..]);
            drop(insts);
            for (id, code, signal) in exits {
                match id {
                    None => node_status = Some((code, signal)),
                    Some(id) => self.ended(id, code, signal),
                }
            }
            if node_status.is_some() {
                eprintln!("bulkhead-priv: the node exited");
                break;
            }
            if sig_ready {
                self.on_signals(node, signals);
                if node.is_none() && self.stopping {
                    break;
                }
            }
            if sock_ready && !self.on_request() {
                eprintln!("bulkhead-priv: the node's socket closed");
                break;
            }
        }
        self.drain();
        match node_status.or_else(|| node.and_then(platform::wait_node)) {
            Some((Some(code), _)) => code,
            Some((_, Some(signal))) => 128 + signal,
            _ => 0,
        }
    }

    fn on_signals(&mut self, node: Option<&Node>, signals: &OwnedFd) {
        let mut buf = [0u8; 64];
        let n = rustix::io::read(signals, &mut buf).unwrap_or(0);
        for &s in &buf[..n] {
            let s = i32::from(s);
            let forward = if s == libc::SIGHUP {
                libc::SIGHUP
            } else {
                libc::SIGTERM
            };
            match node {
                Some(node) => {
                    if let Err(e) = platform::signal_node(node, forward) {
                        eprintln!("bulkhead-priv: signalling the node: {e}");
                    }
                }
                None if s != libc::SIGHUP => self.stopping = true,
                None => {}
            }
        }
    }

    /// Report an instance's exit, then remove what was made for it.
    fn ended(&mut self, id: u64, code: Option<i32>, signal: Option<i32>) {
        let memory_limit = self.insts.get(&id).is_some_and(platform::memory_limited);
        let reply = Reply::Exited {
            id,
            code,
            signal,
            memory_limit,
        };
        let _ = socket::send(self.sock.as_fd(), &reply.encode(), &[]);
        if let Some(inst) = self.insts.remove(&id) {
            platform::finish(&self.st, inst);
        }
    }

    /// Serve one request; `false` once the node's socket has closed.
    fn on_request(&mut self) -> bool {
        let (reply, give) = match socket::recv(self.sock.as_fd()) {
            Err(_) | Ok(Received::Closed) => return false,
            Ok(Received::Malformed) => (refused("a malformed message"), None),
            Ok(Received::Message(bytes, fds)) => match Request::decode(&bytes) {
                None => (refused("a malformed request"), None),
                Some(request) => self.answer(request, fds),
            },
        };
        let give: Vec<BorrowedFd<'_>> = give.iter().map(AsFd::as_fd).collect();
        socket::send(self.sock.as_fd(), &reply.encode(), &give).is_ok()
    }

    fn answer(&mut self, request: Request, fds: Vec<OwnedFd>) -> (Reply, Option<OwnedFd>) {
        match request {
            Request::Spawn(s) => match self.spawn(&s, fds) {
                Ok(reply) => (reply, None),
                Err(reason) => (Reply::Refused { reason }, None),
            },
            Request::Signal { id, signal } => {
                if !check::signal(signal) {
                    return (
                        refused(&format!("signal {signal} is not one the node may send")),
                        None,
                    );
                }
                let Some(inst) = self.insts.get(&id) else {
                    return (refused(&format!("no instance {id}")), None);
                };
                match platform::signal(inst, signal) {
                    Ok(()) => (Reply::Done, None),
                    Err(e) => (refused(&format!("signal: {e}")), None),
                }
            }
            Request::Share { app, bind } => match self.share(&app, &bind) {
                Ok(reply) => (reply, None),
                Err(reason) => (Reply::Refused { reason }, None),
            },
            Request::Bind { address } => {
                match check::bind_address(&self.config, &address).and_then(|addr| {
                    std::net::TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))
                }) {
                    Ok(listener) => (Reply::Bound, Some(OwnedFd::from(listener))),
                    Err(reason) => (Reply::Refused { reason }, None),
                }
            }
        }
    }

    /// `path` with links resolved, if it is a directory under the
    /// configuration's binds (checked again once resolved).
    fn bind_path(&self, path: &str) -> Result<PathBuf, String> {
        let resolved = std::fs::canonicalize(path).map_err(|e| format!("bind {path}: {e}"))?;
        if !check::under(&resolved, &self.config.binds) || !resolved.is_dir() {
            return Err(format!(
                "bind {path} is not a directory under the helper's binds"
            ));
        }
        Ok(resolved)
    }

    /// Give a directory the node wrote to its app (ROADMAP PS2), picking
    /// the app's user first if it has none yet.
    fn share(&mut self, app: &str, bind: &bulkhead_priv_wire::Bind) -> Result<Reply, String> {
        if !bulkhead_priv_wire::is_app_name(app) {
            return Err(format!("{app:?} is not an app name"));
        }
        let path = self.bind_path(&bind.path)?;
        let uid = self.app_user(app)?;
        share::share(&path, self.config.node_uid, uid, bind.writable)?;
        Ok(Reply::Done)
    }

    /// The app's user, picked and saved on its first use.
    fn app_user(&mut self, app: &str) -> Result<u32, String> {
        let (uid, new) = self.bindings.assign(app, &self.config.uids)?;
        if new {
            self.bindings
                .save(&self.config.state)
                .map_err(|e| format!("saving the app's user: {e}"))?;
            eprintln!("bulkhead-priv: app {app} gets user {uid}");
        }
        Ok(uid)
    }

    fn spawn(&mut self, s: &Spawn, fds: Vec<OwnedFd>) -> Result<Reply, String> {
        check::spawn(&self.config, s, fds.len())?;
        if self.insts.contains_key(&s.id) {
            return Err(format!("instance {} runs already", s.id));
        }
        let uid = self.app_user(&s.app)?;
        // Bound as they are: the node had each shared once, when it wrote
        // it (`Request::Share`), so a start walks nothing.
        let mut binds: Vec<(PathBuf, bool)> = Vec::new();
        for b in &s.binds {
            binds.push((self.bind_path(&b.path)?, b.writable));
        }
        let exec = Exec::new(&self.config.runner.display().to_string(), &s.args, &s.env)
            .map_err(|e| e.to_string())?;
        let placed: Vec<(i32, i32)> = fds
            .iter()
            .zip(&s.fds)
            .map(|(fd, t)| (fd.as_raw_fd(), i32::from(*t)))
            .collect();
        let inst = platform::spawn(&self.config, &self.st, s, uid, &binds, exec, placed)?;
        // The node's copies of the descriptors are the instance's now.
        drop(fds);
        let reply = Reply::Spawned {
            id: s.id,
            pid: inst.pid,
            cgroup: inst.cgroup(),
            uid,
        };
        self.insts.insert(s.id, inst);
        Ok(reply)
    }

    /// Kill every instance and wait, up to [`DRAIN`], for them to end.
    fn drain(&mut self) {
        if self.insts.is_empty() {
            return;
        }
        eprintln!("bulkhead-priv: ending {} instances", self.insts.len());
        let insts: Vec<&Instance> = self.insts.values().collect();
        platform::kill_all(&insts);
        drop(insts);
        let deadline = Instant::now() + DRAIN;
        while !self.insts.is_empty() && Instant::now() < deadline {
            let insts: Vec<&Instance> = self.insts.values().collect();
            let exit_fds = platform::watched(&self.st, None, &insts);
            let mut fds: Vec<PollFd<'_>> = exit_fds
                .iter()
                .map(|fd| PollFd::new(fd, PollFlags::IN))
                .collect();
            let timeout = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
            };
            let _ = rustix::event::poll(&mut fds, Some(&timeout));
            let ready: Vec<bool> = fds.iter().map(|f| !f.revents().is_empty()).collect();
            let exits = platform::exits(&self.st, None, &insts, &ready);
            drop(insts);
            for (id, code, signal) in exits {
                if let Some(id) = id {
                    self.ended(id, code, signal);
                }
            }
        }
    }
}

fn refused(reason: &str) -> Reply {
    Reply::Refused {
        reason: reason.to_string(),
    }
}
