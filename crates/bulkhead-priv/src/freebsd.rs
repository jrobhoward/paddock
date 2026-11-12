//! FreeBSD: each instance is `pdfork`ed under its app's user, in a
//! session and process group of its own, held by `RLIMIT_AS` and `rctl`
//! rules on its process (`memoryuse`, `nthr`) that the helper adds before
//! the child goes on. A jail per instance and capability mode come with
//! ROADMAP FB2. The helper learns of exits from one kqueue of process
//! descriptors (`EVFILT_PROCDESC`), and ends an instance by signalling its
//! process group, as the node cannot under another user (S36, finding 1).

use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;

use bulkhead_priv_wire::Spawn;
use rustix::process::{Pid, Resource, Rlimit, Signal};

use crate::child::{self, Plan};
use crate::config::Config;
use crate::sys::{self, Exec, Forked, ProcQueue};

/// The kqueue tag for the node.
const NODE_TAG: u64 = u64::MAX;

/// What a child does on FreeBSD before its descriptors move.
pub struct ChildFences {
    /// The read end of the go-ahead pipe: the child waits for a byte, so
    /// its `rctl` rules are in place before it executes anything.
    go: Option<RawFd>,
    /// `RLIMIT_AS`, for an instance.
    address_space: Option<u64>,
}

/// The helper's FreeBSD state.
pub struct State {
    queue: ProcQueue,
}

/// The node's process.
pub struct Node {
    pub pid: u32,
    handle: OwnedFd,
}

/// An instance the helper started.
pub struct Instance {
    pub pid: u32,
    pd: OwnedFd,
}

impl Instance {
    /// No cgroup on FreeBSD.
    pub fn cgroup(&self) -> String {
        String::new()
    }
}

/// The kqueue; `rctl` is checked at the first spawn.
///
/// # Errors
///
/// The kernel refused.
pub fn init(_config: &Config) -> Result<State, String> {
    Ok(State {
        queue: ProcQueue::new().map_err(|e| format!("kqueue: {e}"))?,
    })
}

/// Start the node from `exec` under its user, with `fds` placed.
///
/// # Errors
///
/// The fork, or a step in the child.
pub fn start_node(
    config: &Config,
    st: &State,
    exec: Exec,
    fds: Vec<(i32, i32)>,
) -> Result<Node, String> {
    let (err_read, err_write) = child::error_pipe().map_err(|e| e.to_string())?;
    let plan = Plan {
        exec,
        fds,
        uid: config.node_uid,
        gid: config.node_gid,
        fences: ChildFences {
            go: None,
            address_space: None,
        },
        err: err_write.as_raw_fd(),
    };
    match sys::fork().map_err(|e| format!("pdfork: {e}"))? {
        Forked::Child => child::run(&plan),
        Forked::Parent { pid, handle } => {
            drop(err_write);
            child::outcome(err_read)?;
            st.queue
                .add(handle.as_fd(), NODE_TAG)
                .map_err(|e| format!("kqueue: {e}"))?;
            Ok(Node { pid, handle })
        }
    }
}

/// Start `s`'s instance under `uid`, with `fds` (already checked).
///
/// # Errors
///
/// The fork, its `rctl` rules (is `kern.racct.enable=1` set?), or a step
/// in the child.
pub fn spawn(
    _config: &Config,
    st: &State,
    s: &Spawn,
    uid: u32,
    _binds: &[(PathBuf, bool)],
    exec: Exec,
    fds: Vec<(i32, i32)>,
) -> Result<Instance, String> {
    let (go_read, go_write) = child::error_pipe().map_err(|e| e.to_string())?;
    let (err_read, err_write) = child::error_pipe().map_err(|e| e.to_string())?;
    let plan = Plan {
        exec,
        fds,
        uid,
        gid: uid,
        fences: ChildFences {
            go: Some(go_read.as_raw_fd()),
            address_space: Some(s.memory_max),
        },
        err: err_write.as_raw_fd(),
    };
    match sys::fork().map_err(|e| format!("pdfork: {e}"))? {
        Forked::Child => child::run(&plan),
        Forked::Parent { pid, handle } => {
            drop(err_write);
            drop(go_read);
            let rules = [
                format!("process:{pid}:memoryuse:deny={}", s.memory_max),
                format!("process:{pid}:nthr:deny={}", s.pids_max),
            ];
            for rule in &rules {
                let added = CString::new(rule.as_str())
                    .map_err(io::Error::other)
                    .and_then(|r| sys::rctl_add(&r));
                if let Err(e) = added {
                    let _ = sys::pdkill(handle.as_fd(), libc::SIGKILL);
                    remove_rules(pid);
                    return Err(format!("rctl {rule}: {e} (is kern.racct.enable=1 set?)"));
                }
            }
            let _ = rustix::io::write(&go_write, &[1]);
            drop(go_write);
            if let Err(e) = child::outcome(err_read) {
                remove_rules(pid);
                return Err(e);
            }
            st.queue
                .add(handle.as_fd(), s.id)
                .map_err(|e| format!("kqueue: {e}"))?;
            Ok(Instance { pid, pd: handle })
        }
    }
}

fn remove_rules(pid: u32) {
    if let Ok(filter) = CString::new(format!("process:{pid}")) {
        sys::rctl_remove(&filter);
    }
}

/// In the child, before its descriptors move.
///
/// # Errors
///
/// The helper went away before its go-ahead, or a limit failed.
pub fn child_fences(f: &ChildFences) -> io::Result<()> {
    if let Some(go) = f.go
        && !sys::read_byte(go)
    {
        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
    }
    if let Some(bytes) = f.address_space {
        rustix::process::setrlimit(
            Resource::As,
            Rlimit {
                current: Some(bytes),
                maximum: Some(bytes),
            },
        )?;
    }
    Ok(())
}

/// The descriptor whose readiness means an exit: the kqueue.
pub fn watched<'a>(
    st: &'a State,
    _node: Option<&'a Node>,
    _insts: &'a [&'a Instance],
) -> Vec<BorrowedFd<'a>> {
    vec![st.queue.fd()]
}

/// An ended process: the node (`None`) or an instance, and how it ended.
pub type Exit = (Option<u64>, Option<i32>, Option<i32>);

/// The exits the kqueue reports.
pub fn exits(st: &State, _node: Option<&Node>, _insts: &[&Instance], ready: &[bool]) -> Vec<Exit> {
    if !ready.first().copied().unwrap_or(false) {
        return Vec::new();
    }
    st.queue
        .exits()
        .into_iter()
        .map(|(tag, status)| {
            let (code, sig) = sys::decode_status(status);
            ((tag != NODE_TAG).then_some(tag), code, sig)
        })
        .collect()
}

/// Send `signal` to an instance's process group, which holds what it
/// forked unless that left it.
///
/// # Errors
///
/// The kernel refused.
pub fn signal(inst: &Instance, signal: i32) -> io::Result<()> {
    let sig =
        Signal::from_named_raw(signal).ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    let pid = Pid::from_raw(i32::try_from(inst.pid).map_err(io::Error::other)?)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))?;
    match rustix::process::kill_process_group(pid, sig) {
        Ok(()) => Ok(()),
        Err(_) => sys::pdkill(inst.pd.as_fd(), signal),
    }
}

/// Send `signal` to the node.
///
/// # Errors
///
/// The kernel refused.
pub fn signal_node(node: &Node, signal: i32) -> io::Result<()> {
    sys::pdkill(node.handle.as_fd(), signal)
}

/// No cgroup on FreeBSD: a memory limit there fails an allocation.
pub fn memory_limited(_inst: &Instance) -> bool {
    false
}

/// Remove what the helper made for an instance that has exited: its
/// rules, and its descriptor, which reaps it.
pub fn finish(_st: &State, inst: Instance) {
    remove_rules(inst.pid);
    drop(inst.pd);
}

/// The node's exit, waited for.
pub fn wait_node(node: &Node) -> Option<(Option<i32>, Option<i32>)> {
    let q = ProcQueue::new().ok()?;
    q.add(node.handle.as_fd(), NODE_TAG).ok()?;
    loop {
        let fd = q.fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let _ = rustix::event::poll(&mut fds, None);
        if let Some((_, status)) = q.exits().first() {
            return Some(sys::decode_status(*status));
        }
    }
}

/// The helper's own fence, once the node runs: `no_new_privs`. It stays
/// root otherwise: it makes users' processes and, with FB2, jails, which
/// capability mode would refuse.
///
/// # Errors
///
/// The kernel refused.
pub fn fence_self() -> Result<(), String> {
    rustix::process::set_no_new_privs(None).map_err(|e| format!("no_new_privs: {e}"))
}

/// Kill every instance's processes at once (the node has gone).
pub fn kill_all(insts: &[&Instance]) {
    for inst in insts {
        let _ = signal(inst, libc::SIGKILL);
    }
}
