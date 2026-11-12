//! The messages between a Bulkhead node and its privileged helper,
//! `bulkhead-priv` (ROADMAP PS1, spike S36).
//!
//! The helper runs as root and parses what the node sends, so this crate
//! is small, forbids `unsafe`, and refuses anything outside its bounds
//! ([`Request::decode`] returns `None`). One message is one datagram on a
//! `SOCK_SEQPACKET` socket: a tag byte, then fields little-endian, strings
//! as a `u32` length and UTF-8 bytes, lists as a `u32` count and their
//! items. Descriptors travel beside a message as `SCM_RIGHTS`.
//!
//! The helper answers each request in order, with one [`Reply`]; between
//! answers it may send [`Reply::Exited`] for an instance that ended.

#![forbid(unsafe_code)]

/// The largest message either side sends or accepts.
pub const MAX_MESSAGE: usize = 64 << 10;
/// The longest string in a message: a path, an argument, a variable.
pub const MAX_STRING: usize = 4096;
/// The most arguments a spawn passes to the runner.
pub const MAX_ARGS: usize = 64;
/// The most environment variables a spawn sets.
pub const MAX_ENV: usize = 16;
/// The most directories a spawn binds into the instance's root.
pub const MAX_BINDS: usize = 32;
/// The most descriptors a spawn passes.
pub const MAX_FDS: usize = 10;
/// The environment variable a node started by the helper finds the
/// helper's socket in (a descriptor number).
pub const SOCKET_ENV: &str = "BULKHEAD_PRIV_FD";
/// The longest app name ([`is_app_name`]).
pub const MAX_APP_NAME: usize = 64;
/// The environment variables a spawn may set: the ones
/// `bulkhead-runner` and the app read (`bulkhead_native`'s `PRIV_ENV`,
/// `preopens::ENV` and `channel::SPIN_ENV`). The helper refuses any other,
/// such as `LD_PRELOAD`.
pub const RUNNER_ENV: [&str; 3] = ["BULKHEAD_PRIV", "BULKHEAD_PREOPENS", "BULKHEAD_SPIN_US"];
/// Descriptor numbers a spawn may place descriptors at: stdout, stderr
/// and 3 to this. The helper gives the instance `/dev/null` as stdin.
pub const MAX_FD_TARGET: u8 = 15;

/// Whether `name` is an app name as manifests allow it: 1 to
/// [`MAX_APP_NAME`] bytes of lowercase ASCII letters, digits, `-` and `_`,
/// starting with a letter or digit. The helper names users' bindings,
/// cgroups and directories after it, so nothing else gets through.
#[must_use]
pub fn is_app_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_APP_NAME
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// A directory to make visible in an instance's root, at its own path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// An absolute path, under one of the helper's bind roots.
    pub path: String,
    /// Whether the app may write there (a read-write volume).
    pub writable: bool,
}

/// Start an instance: `bulkhead-runner` (the helper's, from its own
/// configuration) under the app's user, with these arguments, variables,
/// directories and descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    /// The node's number for the instance's process, unique among those
    /// running; [`Reply::Exited`] names it.
    pub id: u64,
    /// The app, whose user the helper picks ([`is_app_name`]).
    pub app: String,
    /// Most memory, in bytes.
    pub memory_max: u64,
    /// Most processes and threads.
    pub pids_max: u32,
    /// The runner's arguments.
    pub args: Vec<String>,
    /// The runner's environment: names the helper allows only.
    pub env: Vec<(String, String)>,
    /// Directories the instance needs: its program's, its `priv/` files,
    /// its volumes.
    pub binds: Vec<Bind>,
    /// For each descriptor passed beside the message, in order, the
    /// number it gets in the instance.
    pub fds: Vec<u8>,
}

/// What the node asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Start an instance; answered [`Reply::Spawned`] or [`Reply::Refused`].
    Spawn(Spawn),
    /// Send `signal` (`SIGTERM`, `SIGKILL` or `SIGCONT`) to an instance's
    /// processes; answered [`Reply::Done`] or [`Reply::Refused`].
    Signal {
        /// The instance, as [`Spawn::id`] named it.
        id: u64,
        /// The signal's number on the helper's system.
        signal: i32,
    },
    /// Bind a TCP listener on a port the helper allows and pass it back;
    /// answered [`Reply::Bound`] with the listener beside it.
    Bind {
        /// The address and port, as `ip:port` (`[ip]:port` for IPv6).
        address: String,
    },
}

/// What the helper answers or reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The instance runs.
    Spawned {
        /// As in the request.
        id: u64,
        /// Its process id.
        pid: u32,
        /// Its cgroup directory (Linux), whose `cgroup.kill` the node may
        /// write; empty without one.
        cgroup: String,
        /// The user it runs as.
        uid: u32,
    },
    /// Done as asked.
    Done,
    /// The listener is passed beside this message.
    Bound,
    /// Not done, and why.
    Refused {
        /// What was wrong.
        reason: String,
    },
    /// Sent unasked: an instance's process ended. Before the helper
    /// removes what it made for it.
    Exited {
        /// As [`Spawn::id`] named it.
        id: u64,
        /// Its exit status, if it exited.
        code: Option<i32>,
        /// The signal that ended it, if one did.
        signal: Option<i32>,
        /// Whether its cgroup's memory limit killed it (Linux).
        memory_limit: bool,
    },
}

const T_SPAWN: u8 = 1;
const T_SIGNAL: u8 = 2;
const T_BIND: u8 = 3;
const T_SPAWNED: u8 = 0x81;
const T_DONE: u8 = 0x82;
const T_BOUND: u8 = 0x83;
const T_REFUSED: u8 = 0x84;
const T_EXITED: u8 = 0x85;

/// A message being written.
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    /// A length or count; one past `u32` cannot be sent and saturates,
    /// which the reader then refuses.
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.0.extend_from_slice(s.as_bytes());
    }
    fn opt_i32(&mut self, v: Option<i32>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.i32(v);
            }
            None => self.u8(0),
        }
    }
}

/// A message being read; every read checks what is left.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn count(&mut self, max: usize) -> Option<usize> {
        let n = usize::try_from(self.u32()?).ok()?;
        (n <= max).then_some(n)
    }
    fn str(&mut self) -> Option<String> {
        let n = self.count(MAX_STRING)?;
        let s = std::str::from_utf8(self.take(n)?).ok()?;
        // A NUL would cut a path or argument short where the kernel reads it.
        (!s.contains('\0')).then(|| s.to_string())
    }
    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn opt_i32(&mut self) -> Option<Option<i32>> {
        if self.bool()? {
            Some(Some(self.i32()?))
        } else {
            Some(None)
        }
    }
    fn end(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

impl Request {
    /// The message's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        match self {
            Self::Spawn(s) => {
                w.u8(T_SPAWN);
                w.u64(s.id);
                w.str(&s.app);
                w.u64(s.memory_max);
                w.u32(s.pids_max);
                w.len(s.args.len());
                for a in &s.args {
                    w.str(a);
                }
                w.len(s.env.len());
                for (k, v) in &s.env {
                    w.str(k);
                    w.str(v);
                }
                w.len(s.binds.len());
                for b in &s.binds {
                    w.str(&b.path);
                    w.u8(u8::from(b.writable));
                }
                w.len(s.fds.len());
                w.0.extend_from_slice(&s.fds);
            }
            Self::Signal { id, signal } => {
                w.u8(T_SIGNAL);
                w.u64(*id);
                w.i32(*signal);
            }
            Self::Bind { address } => {
                w.u8(T_BIND);
                w.str(address);
            }
        }
        w.0
    }

    /// The request in `bytes`, or `None` for anything malformed, over a
    /// limit, or with bytes left over.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_MESSAGE {
            return None;
        }
        let mut r = Reader(bytes);
        let request = match r.u8()? {
            T_SPAWN => {
                let id = r.u64()?;
                let app = r.str()?;
                let memory_max = r.u64()?;
                let pids_max = r.u32()?;
                let n = r.count(MAX_ARGS)?;
                let args = (0..n).map(|_| r.str()).collect::<Option<Vec<_>>>()?;
                let n = r.count(MAX_ENV)?;
                let env = (0..n)
                    .map(|_| Some((r.str()?, r.str()?)))
                    .collect::<Option<Vec<_>>>()?;
                let n = r.count(MAX_BINDS)?;
                let binds = (0..n)
                    .map(|_| {
                        Some(Bind {
                            path: r.str()?,
                            writable: r.bool()?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                let n = r.count(MAX_FDS)?;
                let fds = r.take(n)?.to_vec();
                Self::Spawn(Spawn {
                    id,
                    app,
                    memory_max,
                    pids_max,
                    args,
                    env,
                    binds,
                    fds,
                })
            }
            T_SIGNAL => Self::Signal {
                id: r.u64()?,
                signal: r.i32()?,
            },
            T_BIND => Self::Bind { address: r.str()? },
            _ => return None,
        };
        r.end()?;
        Some(request)
    }
}

impl Reply {
    /// The message's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        match self {
            Self::Spawned {
                id,
                pid,
                cgroup,
                uid,
            } => {
                w.u8(T_SPAWNED);
                w.u64(*id);
                w.u32(*pid);
                w.str(cgroup);
                w.u32(*uid);
            }
            Self::Done => w.u8(T_DONE),
            Self::Bound => w.u8(T_BOUND),
            Self::Refused { reason } => {
                w.u8(T_REFUSED);
                // A reason is the helper's own text; cut it to fit.
                let mut end = reason.len().min(MAX_STRING);
                while !reason.is_char_boundary(end) {
                    end -= 1;
                }
                w.str(&reason[..end]);
            }
            Self::Exited {
                id,
                code,
                signal,
                memory_limit,
            } => {
                w.u8(T_EXITED);
                w.u64(*id);
                w.opt_i32(*code);
                w.opt_i32(*signal);
                w.u8(u8::from(*memory_limit));
            }
        }
        w.0
    }

    /// The reply in `bytes`, or `None` for anything malformed.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_MESSAGE {
            return None;
        }
        let mut r = Reader(bytes);
        let reply = match r.u8()? {
            T_SPAWNED => Self::Spawned {
                id: r.u64()?,
                pid: r.u32()?,
                cgroup: r.str()?,
                uid: r.u32()?,
            },
            T_DONE => Self::Done,
            T_BOUND => Self::Bound,
            T_REFUSED => Self::Refused { reason: r.str()? },
            T_EXITED => Self::Exited {
                id: r.u64()?,
                code: r.opt_i32()?,
                signal: r.opt_i32()?,
                memory_limit: r.bool()?,
            },
            _ => return None,
        };
        r.end()?;
        Some(reply)
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
