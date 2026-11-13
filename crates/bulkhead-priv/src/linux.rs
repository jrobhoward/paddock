//! Linux: a cgroup per instance, entered by `clone3` (`CLONE_INTO_CGROUP`),
//! new mount, PID, network, IPC and UTS namespaces, and a minimal root
//! (spike S36). The node ends an instance by writing its `cgroup.kill`,
//! which the helper gives it; the helper learns of exits through pidfds.
//!
//! ```text
//! <cgroup>/helper              the helper
//! <cgroup>/node                the node
//! <cgroup>/apps/<app>/i<id>    each instance: memory.max, memory.swap.max 0, pids.max
//! ```
//!
//! The minimal root is a tmpfs, built in the instance's own mount
//! namespace, then `pivot_root`ed into and remounted read-only. It holds
//! read-only binds of `/usr` and the library directories (or their
//! symbolic links), `/etc/ld.so.cache` and `/etc/localtime`, the runner's
//! directory, and the spawn's binds at their own paths (a read-write bind
//! without `exec`); `/dev/null`, `zero`, `full`, `random` and `urandom`;
//! a private tmpfs `/tmp`; and `/proc`, which in the new PID namespace
//! shows the instance's own processes only.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use bulkhead_priv_wire::Spawn;
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use rustix::process::{WaitId, WaitIdOptions};

use crate::child::{self, Plan};
use crate::config::Config;
use crate::sys::{self, Exec, Forked};

/// The controllers the helper turns on for instances, those of them the
/// kernel has.
const CONTROLLERS: [&str; 3] = ["memory", "pids", "cpu"];

/// The host name an instance sees (its own UTS namespace).
const HOSTNAME: &[u8] = b"bulkhead-app";

/// What a child does on Linux before its descriptors move: for an
/// instance, its root; for the node, nothing.
pub struct ChildFences {
    root: Option<RootPlan>,
}

/// The helper's Linux state.
pub struct State {
    cgroup: PathBuf,
    roots: PathBuf,
    node_cgroup: OwnedFd,
}

/// The node's process.
pub struct Node {
    pub pid: u32,
    handle: OwnedFd,
}

/// An instance the helper started.
pub struct Instance {
    pub id: u64,
    pub pid: u32,
    pidfd: OwnedFd,
    cgroup: PathBuf,
    root: PathBuf,
}

impl Instance {
    /// Its cgroup, for the node.
    pub fn cgroup(&self) -> String {
        self.cgroup.display().to_string()
    }
}

/// Set up the cgroups: the helper into `helper`, controllers on below the
/// delegated directory, `node` and `apps` made.
///
/// # Errors
///
/// No `cgroup` in the configuration, or the directory is not a cgroup v2
/// directory the helper may write.
pub fn init(config: &Config) -> Result<State, String> {
    let cgroup = config
        .cgroup
        .clone()
        .ok_or("cgroup is required on Linux (a cgroup v2 directory delegated to the helper)")?;
    let e = |what: &str, err: io::Error| format!("cgroup {}: {what}: {err}", cgroup.display());
    let helper = cgroup.join("helper");
    mkdir(&helper).map_err(|err| e("helper", err))?;
    std::fs::write(helper.join("cgroup.procs"), std::process::id().to_string())
        .map_err(|err| e("moving the helper", err))?;
    let available = std::fs::read_to_string(cgroup.join("cgroup.controllers"))
        .map_err(|err| e("controllers", err))?;
    let on: Vec<String> = CONTROLLERS
        .iter()
        .filter(|c| available.split_whitespace().any(|a| a == **c))
        .map(|c| format!("+{c}"))
        .collect();
    for needed in ["+memory", "+pids"] {
        if !on.iter().any(|c| c == needed) {
            return Err(format!(
                "cgroup {}: the {} controller is not delegated",
                cgroup.display(),
                &needed[1..]
            ));
        }
    }
    let on = on.join(" ");
    std::fs::write(cgroup.join("cgroup.subtree_control"), &on).map_err(|err| {
        e(
            "enabling controllers (other processes may be in it; the helper must be alone there)",
            err,
        )
    })?;
    let node = cgroup.join("node");
    mkdir(&node).map_err(|err| e("node", err))?;
    let apps = cgroup.join("apps");
    mkdir(&apps).map_err(|err| e("apps", err))?;
    std::fs::write(apps.join("cgroup.subtree_control"), &on).map_err(|err| e("apps", err))?;
    let node_cgroup = open_dir(&node).map_err(|err| e("node", err))?;
    let roots = config.state.join("roots");
    mkdir(&roots).map_err(|err| format!("{}: {err}", roots.display()))?;
    Ok(State {
        cgroup,
        roots,
        node_cgroup,
    })
}

fn mkdir(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?)
}

/// Start the node from `exec` under its user, in its cgroup, with `fds`
/// placed.
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
        fences: ChildFences { root: None },
        err: err_write.as_raw_fd(),
    };
    match sys::clone3(0, Some(st.node_cgroup.as_fd())).map_err(|e| format!("clone3: {e}"))? {
        Forked::Child => child::run(&plan),
        Forked::Parent { pid, handle } => {
            drop(err_write);
            child::outcome(err_read)?;
            Ok(Node { pid, handle })
        }
    }
}

/// Start `s`'s instance under `uid`, with `fds` (already checked).
///
/// # Errors
///
/// A cgroup, the root's mount point, the clone, or a step in the child.
pub fn spawn(
    config: &Config,
    st: &State,
    s: &Spawn,
    uid: u32,
    binds: &[(PathBuf, bool)],
    exec: Exec,
    fds: Vec<(i32, i32)>,
) -> Result<Instance, String> {
    let cgroup = make_cgroup(config, st, s).map_err(|e| format!("cgroup: {e}"))?;
    let cleanup = |root: Option<&Path>| {
        remove_cgroup(&cgroup);
        if let Some(r) = root {
            let _ = std::fs::remove_dir(r);
        }
    };
    let root = st.roots.join(format!("i{}", s.id));
    if let Err(e) = mkdir(&root) {
        cleanup(None);
        return Err(format!("{}: {e}", root.display()));
    }
    let plan_root = match root_plan(config, &root, binds) {
        Ok(p) => p,
        Err(e) => {
            cleanup(Some(&root));
            return Err(format!("root: {e}"));
        }
    };
    let cg = match open_dir(&cgroup) {
        Ok(fd) => fd,
        Err(e) => {
            cleanup(Some(&root));
            return Err(format!("cgroup: {e}"));
        }
    };
    let (err_read, err_write) = child::error_pipe().map_err(|e| e.to_string())?;
    let plan = Plan {
        exec,
        fds,
        uid,
        gid: uid,
        fences: ChildFences {
            root: Some(plan_root),
        },
        err: err_write.as_raw_fd(),
    };
    let forked = match sys::clone3(sys::NAMESPACES, Some(cg.as_fd())) {
        Ok(f) => f,
        Err(e) => {
            cleanup(Some(&root));
            return Err(format!("clone3: {e}"));
        }
    };
    match forked {
        Forked::Child => child::run(&plan),
        Forked::Parent { pid, handle } => {
            drop(err_write);
            if let Err(e) = child::outcome(err_read) {
                let _ =
                    rustix::process::waitid(WaitId::PidFd(handle.as_fd()), WaitIdOptions::EXITED);
                cleanup(Some(&root));
                return Err(e);
            }
            Ok(Instance {
                id: s.id,
                pid,
                pidfd: handle,
                cgroup,
                root,
            })
        }
    }
}

/// `<cgroup>/apps/<app>/i<id>` with the spawn's limits, its `cgroup.kill`
/// given to the node.
fn make_cgroup(config: &Config, st: &State, s: &Spawn) -> io::Result<PathBuf> {
    let app = st.cgroup.join("apps").join(&s.app);
    if !app.exists() {
        mkdir(&app)?;
        let on = std::fs::read_to_string(st.cgroup.join("apps").join("cgroup.subtree_control"))?;
        let on: Vec<String> = on.split_whitespace().map(|c| format!("+{c}")).collect();
        std::fs::write(app.join("cgroup.subtree_control"), on.join(" "))?;
    }
    let leaf = app.join(format!("i{}", s.id));
    std::fs::create_dir(&leaf)?;
    let set = || -> io::Result<()> {
        std::fs::write(leaf.join("memory.max"), s.memory_max.to_string())?;
        if leaf.join("memory.swap.max").exists() {
            std::fs::write(leaf.join("memory.swap.max"), "0")?;
        }
        std::fs::write(leaf.join("pids.max"), s.pids_max.to_string())?;
        // The node may end the instance, and all it forked, by writing
        // this one file: no capability and no round trip (S36).
        std::os::unix::fs::chown(leaf.join("cgroup.kill"), Some(config.node_uid), None)
    };
    if let Err(e) = set() {
        remove_cgroup(&leaf);
        return Err(e);
    }
    Ok(leaf)
}

/// Remove an instance's cgroup, which its last process may still be
/// leaving for a moment.
fn remove_cgroup(dir: &Path) {
    for _ in 0..100 {
        match std::fs::remove_dir(dir) {
            Ok(()) => return,
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                eprintln!("bulkhead-priv: removing {}: {e}", dir.display());
                return;
            }
        }
    }
    eprintln!("bulkhead-priv: {} still busy; left in place", dir.display());
}

/// One step of building an instance's root, every path prepared.
enum RootOp {
    Dir(CString),
    File(CString),
    Symlink {
        target: CString,
        at: CString,
    },
    Bind {
        src: CString,
        dst: CString,
        flags: MountFlags,
    },
    Tmpfs {
        dst: CString,
        data: CString,
    },
    Proc(CString),
}

/// An instance's root, prepared before the clone.
struct RootPlan {
    root: CString,
    tmpfs: CString,
    ops: Vec<RootOp>,
}

fn c(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path"))
}

/// `path` inside `root`.
fn inside(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

/// The steps to build `root` for an instance with `binds` (canonical,
/// checked paths, and whether each is writable).
fn root_plan(config: &Config, root: &Path, binds: &[(PathBuf, bool)]) -> io::Result<RootPlan> {
    let ro = MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV;
    let mut ops = Vec::new();
    let mut made = BTreeSet::new();
    let mut dirs = |ops: &mut Vec<RootOp>, path: &Path| -> io::Result<()> {
        let mut at = PathBuf::from("/");
        for part in path.strip_prefix("/").unwrap_or(path).components() {
            at.push(part);
            if made.insert(at.clone()) {
                ops.push(RootOp::Dir(c(&inside(root, &at))?));
            }
        }
        Ok(())
    };
    dirs(&mut ops, Path::new("/usr"))?;
    ops.push(RootOp::Bind {
        src: c(Path::new("/usr"))?,
        dst: c(&inside(root, Path::new("/usr")))?,
        flags: ro,
    });
    for l in ["bin", "sbin", "lib", "lib64", "lib32", "libx32"] {
        let host = Path::new("/").join(l);
        match std::fs::symlink_metadata(&host) {
            Ok(m) if m.file_type().is_symlink() => ops.push(RootOp::Symlink {
                target: c(&std::fs::read_link(&host)?)?,
                at: c(&inside(root, &host))?,
            }),
            Ok(m) if m.is_dir() => {
                dirs(&mut ops, &host)?;
                ops.push(RootOp::Bind {
                    src: c(&host)?,
                    dst: c(&inside(root, &host))?,
                    flags: ro,
                });
            }
            _ => {}
        }
    }
    dirs(&mut ops, Path::new("/etc"))?;
    for f in ["/etc/ld.so.cache", "/etc/localtime"] {
        let host = Path::new(f);
        if host.is_file() {
            ops.push(RootOp::File(c(&inside(root, host))?));
            ops.push(RootOp::Bind {
                src: c(host)?,
                dst: c(&inside(root, host))?,
                flags: ro,
            });
        }
    }
    // The system's places first: a bind under `/tmp` then lands in the
    // private tmpfs instead of under it.
    dirs(&mut ops, Path::new("/dev"))?;
    for d in ["null", "zero", "full", "random", "urandom"] {
        let host = Path::new("/dev").join(d);
        if !host.exists() {
            continue;
        }
        ops.push(RootOp::File(c(&inside(root, &host))?));
        ops.push(RootOp::Bind {
            src: c(&host)?,
            dst: c(&inside(root, &host))?,
            flags: MountFlags::NOSUID,
        });
    }
    dirs(&mut ops, Path::new("/tmp"))?;
    ops.push(RootOp::Tmpfs {
        dst: c(&inside(root, Path::new("/tmp")))?,
        data: cstring(&format!("mode=1777,size={}", config.tmp_size))?,
    });
    dirs(&mut ops, Path::new("/proc"))?;
    ops.push(RootOp::Proc(c(&inside(root, Path::new("/proc")))?));
    let runner_dir = config
        .runner
        .canonicalize()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("the runner has no directory"))?;
    let shown = |p: &Path| crate::check::SYSTEM_DIRS.iter().any(|d| p.starts_with(d));
    let mut all: Vec<(PathBuf, bool)> = binds.to_vec();
    if !shown(&runner_dir) && !binds.iter().any(|(p, _)| runner_dir.starts_with(p)) {
        all.push((runner_dir, false));
    }
    for (path, writable) in &all {
        dirs(&mut ops, path)?;
        let flags = if *writable {
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC
        } else {
            ro
        };
        ops.push(RootOp::Bind {
            src: c(path)?,
            dst: c(&inside(root, path))?,
            flags,
        });
    }
    Ok(RootPlan {
        root: c(root)?,
        tmpfs: cstring("mode=0755,size=1m")?,
        ops,
    })
}

/// In the child, before its descriptors move.
///
/// # Errors
///
/// A mount, the host name, or the pivot.
pub fn child_fences(f: &ChildFences) -> io::Result<()> {
    let Some(plan) = &f.root else {
        return Ok(());
    };
    rustix::mount::mount_change(
        c"/",
        MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
    )?;
    rustix::system::sethostname(HOSTNAME)?;
    rustix::mount::mount(
        c"tmpfs",
        plan.root.as_c_str(),
        c"tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        plan.tmpfs.as_c_str(),
    )?;
    for op in &plan.ops {
        match op {
            RootOp::Dir(p) => {
                rustix::fs::mkdir(p.as_c_str(), rustix::fs::Mode::from_raw_mode(0o755))?
            }
            RootOp::File(p) => drop(rustix::fs::open(
                p.as_c_str(),
                rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o644),
            )?),
            RootOp::Symlink { target, at } => {
                rustix::fs::symlink(target.as_c_str(), at.as_c_str())?
            }
            RootOp::Bind { src, dst, flags } => {
                rustix::mount::mount_bind_recursive(src.as_c_str(), dst.as_c_str())?;
                rustix::mount::mount_remount(dst.as_c_str(), MountFlags::BIND | *flags, c"")?;
            }
            RootOp::Tmpfs { dst, data } => rustix::mount::mount(
                c"tmpfs",
                dst.as_c_str(),
                c"tmpfs",
                MountFlags::NOSUID | MountFlags::NODEV,
                data.as_c_str(),
            )?,
            RootOp::Proc(p) => rustix::mount::mount(
                c"proc",
                p.as_c_str(),
                c"proc",
                MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
                c"",
            )?,
        }
    }
    rustix::process::chdir(plan.root.as_c_str())?;
    rustix::process::pivot_root(c".", c".")?;
    rustix::mount::unmount(c".", UnmountFlags::DETACH)?;
    rustix::process::chdir(c"/")?;
    rustix::mount::mount_remount(
        c"/",
        MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
        c"",
    )?;
    Ok(())
}

/// The descriptors whose readiness means an exit: the node's pidfd, then
/// each instance's.
pub fn watched<'a>(
    _st: &'a State,
    node: Option<&'a Node>,
    insts: &'a [&'a Instance],
) -> Vec<BorrowedFd<'a>> {
    node.map(|n| n.handle.as_fd())
        .into_iter()
        .chain(insts.iter().map(|i| i.pidfd.as_fd()))
        .collect()
}

/// An ended process: the node (`None`) or an instance, and how it ended.
pub type Exit = (Option<u64>, Option<i32>, Option<i32>);

/// The exits among the watched descriptors that polled ready (`ready`,
/// in [`watched`]'s order).
pub fn exits(_st: &State, node: Option<&Node>, insts: &[&Instance], ready: &[bool]) -> Vec<Exit> {
    let mut out = Vec::new();
    let mut i = 0;
    if let Some(n) = node {
        if ready.first().copied().unwrap_or(false)
            && let Some((code, sig)) = reap(n.handle.as_fd())
        {
            out.push((None, code, sig));
        }
        i = 1;
    }
    for (inst, r) in insts.iter().zip(ready.iter().skip(i)) {
        if *r && let Some((code, sig)) = reap(inst.pidfd.as_fd()) {
            out.push((Some(inst.id), code, sig));
        }
    }
    out
}

/// Reap the process behind `pidfd` if it has exited: its code or signal.
fn reap(pidfd: BorrowedFd<'_>) -> Option<(Option<i32>, Option<i32>)> {
    let status = rustix::process::waitid(
        WaitId::PidFd(pidfd),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
    )
    .ok()??;
    Some((status.exit_status(), status.terminating_signal()))
}

/// Send `signal` to an instance: `SIGKILL` ends its whole cgroup.
///
/// # Errors
///
/// The kernel refused.
pub fn signal(inst: &Instance, signal: i32) -> io::Result<()> {
    if signal == libc::SIGKILL {
        return std::fs::write(inst.cgroup.join("cgroup.kill"), "1");
    }
    let sig = rustix::process::Signal::from_named_raw(signal)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    Ok(rustix::process::pidfd_send_signal(&inst.pidfd, sig)?)
}

/// Send `signal` to the node.
///
/// # Errors
///
/// The kernel refused.
pub fn signal_node(node: &Node, signal: i32) -> io::Result<()> {
    let sig = rustix::process::Signal::from_named_raw(signal)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    Ok(rustix::process::pidfd_send_signal(&node.handle, sig)?)
}

/// Whether the instance's cgroup killed a process for its memory limit
/// (`oom_kill` in `memory.events`): read before the cgroup goes.
pub fn memory_limited(inst: &Instance) -> bool {
    std::fs::read_to_string(inst.cgroup.join("memory.events")).is_ok_and(|events| {
        events.lines().any(|l| {
            l.strip_prefix("oom_kill ")
                .is_some_and(|n| n.trim().parse::<u64>().is_ok_and(|n| n > 0))
        })
    })
}

/// Remove what the helper made for an instance that has exited.
pub fn finish(_st: &State, inst: Instance) {
    drop(inst.pidfd);
    remove_cgroup(&inst.cgroup);
    let _ = std::fs::remove_dir(&inst.root);
}

/// The node's exit, waited for.
pub fn wait_node(node: &Node) -> Option<(Option<i32>, Option<i32>)> {
    let status = rustix::process::waitid(WaitId::PidFd(node.handle.as_fd()), WaitIdOptions::EXITED)
        .ok()??;
    Some((status.exit_status(), status.terminating_signal()))
}

/// The helper's own fence, once the node runs: `no_new_privs`, and only
/// the capabilities its requests need, in every set and the bounding set.
///
/// # Errors
///
/// The kernel refused.
pub fn fence_self() -> Result<(), String> {
    use rustix::thread::{CapabilitySet as C, CapabilitySets};

    rustix::thread::set_no_new_privs(true).map_err(|e| format!("no_new_privs: {e}"))?;
    // Changing ids, mounting and pivoting (`SYS_ADMIN`), the cgroup and
    // share files (`CHOWN`, `FOWNER`, `DAC_*`), ending instances, low
    // ports.
    let keep = C::CHOWN
        | C::DAC_OVERRIDE
        | C::DAC_READ_SEARCH
        | C::FOWNER
        | C::KILL
        | C::SETGID
        | C::SETUID
        | C::NET_BIND_SERVICE
        | C::SYS_ADMIN;
    for raw in 0..64u32 {
        let cap = C::from_bits_retain(1u64 << raw);
        if !keep.contains(cap) {
            // One the kernel does not know is not there to drop.
            let _ = rustix::thread::remove_capability_from_bounding_set(cap);
        }
    }
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: keep,
            permitted: keep,
            inheritable: C::empty(),
        },
    )
    .map_err(|e| format!("capabilities: {e}"))
}

/// Kill every instance's processes at once (the node has gone).
pub fn kill_all(insts: &[&Instance]) {
    for inst in insts {
        let _ = std::fs::write(inst.cgroup.join("cgroup.kill"), "1");
    }
}

/// `s` as a C string; the helper's own text, which holds no NUL.
fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a string"))
}

/// Nothing is left mounted on Linux: an instance's root is in its own
/// mount namespace, gone with it.
pub fn retry_pending(_st: &State) -> Option<std::time::Duration> {
    None
}

/// As [`retry_pending`].
pub fn unmount_all(_st: &State) {}
