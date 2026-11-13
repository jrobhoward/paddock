//! FreeBSD: each instance runs in a jail of its own (ROADMAP FB2), under
//! its app's user, in a session and process group of its own.
//!
//! Before the fork the helper builds the instance's root under
//! `state/jails/i<id>`: a small tmpfs owned by root, on which it mounts
//! read-only nullfs views of `/lib`, `/usr/lib` and `/libexec` (the
//! run-time linker), the runner's directory and the instance's read-only
//! directories, read-write ones (no `exec`, no set-user-id) each at its
//! own path, a devfs with the helper's ruleset, and a tmpfs `/tmp`. Only
//! this instance's directories are there, so no other app's name shows
//! (S36's two fixes). It creates the jail persistent, with no IPv4 or
//! IPv6, adds `rctl` rules on it by its name (`vmemoryuse`, `memoryuse`,
//! `maxproc`, `nthr`), and
//! lets the child go on: it `jail_attach`es, sets `RLIMIT_AS`, drops to
//! the app's user and executes the runner, which enters capability mode
//! before the app's code runs. Once the runner executes, the jail is made
//! non-persistent, so it goes with its last process. `SIGKILL` removes
//! the jail, which ends everything the app forked (PS7 on FreeBSD).
//!
//! The helper learns of exits from one kqueue of process descriptors
//! (`EVFILT_PROCDESC`). A dying jail holds its root for a moment after its
//! last process (PB5), so a busy unmount is retried from the helper's
//! loop, and forced after [`UNMOUNT_PATIENCE`].

use std::cell::RefCell;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bulkhead_priv_wire::Spawn;
use rustix::process::{Pid, Resource, Rlimit, Signal};

use crate::child::{self, Plan};
use crate::config::Config;
use crate::sys::{self, Exec, Forked, Params, ProcQueue};

/// The kqueue tag for the node.
const NODE_TAG: u64 = u64::MAX;

/// An instance's host name.
const HOSTNAME: &[u8] = b"bulkhead-app";

/// The system's directories in every jail, read-only: the libraries and
/// the run-time linker, where they exist.
const SYSTEM: [&str; 3] = ["/lib", "/usr/lib", "/libexec"];

/// The devices a jail's `/dev` shows.
const DEVICES: [&str; 5] = ["null", "zero", "random", "urandom", "pvclock"];

/// How long a busy root's unmount is retried before it is forced.
pub const UNMOUNT_PATIENCE: Duration = Duration::from_secs(5);

/// How often a busy root's unmount is retried.
const UNMOUNT_RETRY: Duration = Duration::from_millis(50);

/// What a child does on FreeBSD before its descriptors move.
pub struct ChildFences {
    /// The read end of the go-ahead pipe: the child waits for a byte, so
    /// its `rctl` rules are in place before it executes anything.
    go: Option<RawFd>,
    /// `RLIMIT_AS`, for an instance.
    address_space: Option<u64>,
    /// The jail an instance attaches to.
    jail: Option<i32>,
}

/// The helper's FreeBSD state.
pub struct State {
    queue: ProcQueue,
    /// Where instances' roots are made: `state/jails`.
    jails: PathBuf,
    /// Roots whose unmount found them busy, and since when.
    pending: RefCell<Vec<(Root, Instant)>>,
}

/// An instance's root and what is mounted there, in mount order.
struct Root {
    dir: PathBuf,
    mounts: Vec<CString>,
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
    jid: i32,
    name: String,
    root: Root,
}

impl Instance {
    /// No cgroup on FreeBSD.
    pub fn cgroup(&self) -> String {
        String::new()
    }
}

/// The kqueue, `state/jails` (emptied of what a previous run left
/// mounted) and the devfs ruleset; `rctl` is checked at the first spawn.
///
/// # Errors
///
/// The kernel refused, or `/sbin/devfs` failed.
pub fn init(config: &Config) -> Result<State, String> {
    use std::os::unix::fs::DirBuilderExt;

    let jails = config.state.join("jails");
    match std::fs::DirBuilder::new().mode(0o700).create(&jails) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            return Err(format!("{}: {e}", jails.display()));
        }
        _ => {}
    }
    unmount_leftovers(&jails)?;
    devfs_ruleset(config.devfs_ruleset)?;
    Ok(State {
        queue: ProcQueue::new().map_err(|e| format!("kqueue: {e}"))?,
        jails,
        pending: RefCell::new(Vec::new()),
    })
}

/// Unmount, deepest first and forced, what is mounted under `jails` (a
/// helper that was killed left it), and remove the roots.
fn unmount_leftovers(jails: &Path) -> Result<(), String> {
    let mut left: Vec<PathBuf> = sys::mount_points()
        .map_err(|e| format!("getfsstat: {e}"))?
        .into_iter()
        .filter(|m| m.starts_with(jails) && m != jails)
        .collect();
    left.sort_by_key(|m| std::cmp::Reverse(m.components().count()));
    for m in &left {
        let c = cpath(m).map_err(|e| e.to_string())?;
        sys::unmount(&c, true).map_err(|e| format!("unmounting {}: {e}", m.display()))?;
    }
    if let Ok(entries) = std::fs::read_dir(jails) {
        for e in entries.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    Ok(())
}

/// Define devfs ruleset `n`: everything hidden but [`DEVICES`].
fn devfs_ruleset(n: u16) -> Result<(), String> {
    let n = n.to_string();
    let run = |args: &[&str]| -> Result<(), String> {
        let out = std::process::Command::new("/sbin/devfs")
            .args(["rule", "-s", &n])
            .args(args)
            .env_clear()
            .output()
            .map_err(|e| format!("/sbin/devfs: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "/sbin/devfs rule -s {n} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    run(&["delset"])?;
    run(&["add", "hide"])?;
    for d in DEVICES {
        run(&["add", "path", d, "unhide"])?;
    }
    Ok(())
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path"))
}

/// `path` inside `root`.
fn inside(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix("/").unwrap_or(path))
}

impl Root {
    /// Mount `fstype` at `at` (inside the root, made first) with `params`
    /// and `flags`.
    fn mount(
        &mut self,
        at: &Path,
        fstype: &str,
        params: Params,
        flags: libc::c_int,
    ) -> io::Result<()> {
        let path = inside(&self.dir, at);
        std::fs::create_dir_all(&path)?;
        let c = cpath(&path)?;
        params
            .text("fstype", fstype.as_bytes())?
            .text("fspath", c.as_bytes())?
            .nmount(flags)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", at.display())))?;
        self.mounts.push(c);
        Ok(())
    }

    /// A read-only (or writable, without `exec`) nullfs view of `host` at
    /// its own path.
    fn view(&mut self, host: &Path, writable: bool) -> io::Result<()> {
        let flags = if writable {
            libc::MNT_NOSUID | libc::MNT_NOEXEC
        } else {
            libc::MNT_RDONLY | libc::MNT_NOSUID
        };
        let target = cpath(host)?;
        self.mount(
            host,
            "nullfs",
            Params::new().text("target", target.as_bytes())?,
            flags,
        )
    }

    /// Unmount everything, last mounted first; `false` if one is busy
    /// (what was unmounted stays unmounted). The directory is removed.
    fn unmount(&mut self, force: bool) -> bool {
        while let Some(m) = self.mounts.last() {
            match sys::unmount(m, force) {
                Ok(()) => {}
                // Gone already (a forced unmount of a parent took it).
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
                Err(_) => return false,
            }
            self.mounts.pop();
        }
        let _ = std::fs::remove_dir(&self.dir);
        true
    }
}

/// Build instance `id`'s root under `jails` with `binds` (canonical,
/// checked paths, and whether each is writable).
fn build_root(
    config: &Config,
    jails: &Path,
    id: u64,
    binds: &[(PathBuf, bool)],
) -> io::Result<Root> {
    let dir = jails.join(format!("i{id}"));
    std::fs::create_dir(&dir)?;
    let mut root = Root {
        dir: dir.clone(),
        mounts: Vec::new(),
    };
    let built = (|| {
        root.mount(
            Path::new("/"),
            "tmpfs",
            Params::new()
                .text("mode", b"0755")?
                .text("size", b"1048576")?,
            libc::MNT_NOSUID | libc::MNT_NOEXEC,
        )?;
        for d in SYSTEM {
            if Path::new(d).is_dir() {
                root.view(Path::new(d), false)?;
            }
        }
        let ruleset = config.devfs_ruleset.to_string();
        root.mount(
            Path::new("/dev"),
            "devfs",
            Params::new().text("ruleset", ruleset.as_bytes())?,
            libc::MNT_NOEXEC | libc::MNT_NOSUID,
        )?;
        root.mount(
            Path::new("/tmp"),
            "tmpfs",
            Params::new()
                .text("mode", b"1777")?
                .text("size", config.tmp_size.to_string().as_bytes())?,
            libc::MNT_NOSUID | libc::MNT_NOEXEC,
        )?;
        let runner_dir = config
            .runner
            .canonicalize()?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("the runner has no directory"))?;
        let shown = |p: &Path| SYSTEM.iter().any(|d| p.starts_with(d));
        if !shown(&runner_dir) && !binds.iter().any(|(p, _)| runner_dir.starts_with(p)) {
            root.view(&runner_dir, false)?;
        }
        for (path, writable) in binds {
            root.view(path, *writable)?;
        }
        Ok::<(), io::Error>(())
    })();
    match built {
        Ok(()) => Ok(root),
        Err(e) => {
            root.unmount(true);
            Err(e)
        }
    }
}

/// Instance `id`'s jail's name, `bulkhead-<helper's pid>-i<id>`: unique
/// among helpers on the machine, and the subject of its `rctl` rules,
/// which name a jail by name.
fn jail_name(id: u64) -> String {
    format!("bulkhead-{}-i{id}", std::process::id())
}

/// Create instance `id`'s jail on `root`: persistent until its process
/// runs, with no network addresses.
fn create_jail(id: u64, root: &Root) -> io::Result<i32> {
    let path = cpath(&root.dir)?;
    let name = jail_name(id);
    Params::new()
        .text("path", path.as_bytes())?
        .text("name", name.as_bytes())?
        .text("host.hostname", HOSTNAME)?
        .int("ip4", libc::JAIL_SYS_DISABLE)?
        .int("ip6", libc::JAIL_SYS_DISABLE)?
        .flag("persist")?
        .jail_set(libc::JAIL_CREATE)
}

/// Let jail `jid` go with its last process.
fn release_jail(jid: i32) -> io::Result<()> {
    Params::new()
        .int("jid", jid)?
        .flag("nopersist")?
        .jail_set(libc::JAIL_UPDATE)
        .map(drop)
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
            jail: None,
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

/// Start `s`'s instance under `uid` in a jail of its own, with `binds`
/// and `fds` (already checked).
///
/// # Errors
///
/// Its root, its jail, its `rctl` rules (is `kern.racct.enable=1`
/// set?), the fork, or a step in the child.
pub fn spawn(
    config: &Config,
    st: &State,
    s: &Spawn,
    uid: u32,
    binds: &[(PathBuf, bool)],
    exec: Exec,
    fds: Vec<(i32, i32)>,
) -> Result<Instance, String> {
    let mut root =
        build_root(config, &st.jails, s.id, binds).map_err(|e| format!("jail root: {e}"))?;
    let jid = match create_jail(s.id, &root) {
        Ok(jid) => jid,
        Err(e) => {
            root.unmount(true);
            return Err(format!("jail: {e}"));
        }
    };
    let name = jail_name(s.id);
    let undo = |root: &mut Root| {
        let _ = sys::jail_remove(jid);
        remove_rules(&name);
        if !root.unmount(false) {
            st.pending.borrow_mut().push((
                Root {
                    dir: root.dir.clone(),
                    mounts: std::mem::take(&mut root.mounts),
                },
                Instant::now(),
            ));
        }
    };
    // `vmemoryuse`, the jail's address space, is refused at its limit;
    // `memoryuse` (resident) is only paged out towards it.
    let rules = [
        format!("jail:{name}:vmemoryuse:deny={}", s.memory_max),
        format!("jail:{name}:memoryuse:deny={}", s.memory_max),
        format!("jail:{name}:maxproc:deny={}", s.pids_max),
        format!("jail:{name}:nthr:deny={}", s.pids_max),
    ];
    for rule in &rules {
        let added = CString::new(rule.as_str())
            .map_err(io::Error::other)
            .and_then(|r| sys::rctl_add(&r));
        if let Err(e) = added {
            undo(&mut root);
            return Err(format!("rctl {rule}: {e} (is kern.racct.enable=1 set?)"));
        }
    }
    let pipes = child::error_pipe().and_then(|go| Ok((go, child::error_pipe()?)));
    let ((go_read, go_write), (err_read, err_write)) = match pipes {
        Ok(p) => p,
        Err(e) => {
            undo(&mut root);
            return Err(e.to_string());
        }
    };
    let plan = Plan {
        exec,
        fds,
        uid,
        gid: uid,
        fences: ChildFences {
            go: Some(go_read.as_raw_fd()),
            address_space: Some(s.memory_max),
            jail: Some(jid),
        },
        err: err_write.as_raw_fd(),
    };
    let forked = match sys::fork() {
        Ok(f) => f,
        Err(e) => {
            undo(&mut root);
            return Err(format!("pdfork: {e}"));
        }
    };
    match forked {
        Forked::Child => child::run(&plan),
        Forked::Parent { pid, handle } => {
            drop(err_write);
            drop(go_read);
            let _ = rustix::io::write(&go_write, &[1]);
            drop(go_write);
            let ran = child::outcome(err_read)
                .and_then(|()| release_jail(jid).map_err(|e| format!("jail: {e}")))
                .and_then(|()| {
                    st.queue
                        .add(handle.as_fd(), s.id)
                        .map_err(|e| format!("kqueue: {e}"))
                });
            if let Err(e) = ran {
                undo(&mut root);
                return Err(e);
            }
            Ok(Instance {
                pid,
                pd: handle,
                jid,
                name,
                root,
            })
        }
    }
}

fn remove_rules(name: &str) {
    if let Ok(filter) = CString::new(format!("jail:{name}")) {
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
    if let Some(jid) = f.jail {
        sys::jail_attach(jid)?;
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
/// forked unless that left it; `SIGKILL` removes its jail, which ends
/// every process in it.
///
/// # Errors
///
/// The kernel refused.
pub fn signal(inst: &Instance, signal: i32) -> io::Result<()> {
    if signal == libc::SIGKILL && sys::jail_remove(inst.jid).is_ok() {
        return Ok(());
    }
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

/// Remove what the helper made for an instance that has exited: its jail
/// (whatever it forked goes with it), its rules, its descriptor, which
/// reaps it, and its root, retried later while the dying jail holds it.
pub fn finish(st: &State, inst: Instance) {
    let Instance {
        pd,
        jid,
        name,
        mut root,
        ..
    } = inst;
    let _ = sys::jail_remove(jid);
    remove_rules(&name);
    drop(pd);
    if !root.unmount(false) {
        st.pending.borrow_mut().push((root, Instant::now()));
    }
}

/// Retry busy roots' unmounts, forced after [`UNMOUNT_PATIENCE`]; how
/// long until the next retry, if any is left.
pub fn retry_pending(st: &State) -> Option<Duration> {
    let mut pending = st.pending.borrow_mut();
    pending.retain_mut(|(root, since)| {
        let force = since.elapsed() > UNMOUNT_PATIENCE;
        if force {
            eprintln!(
                "bulkhead-priv: forcing the unmount of {}",
                root.dir.display()
            );
        }
        !root.unmount(force)
    });
    (!pending.is_empty()).then_some(UNMOUNT_RETRY)
}

/// Unmount every root left, forced (the helper is exiting).
pub fn unmount_all(st: &State) {
    for (mut root, _) in st.pending.borrow_mut().drain(..) {
        if !root.unmount(true) {
            eprintln!("bulkhead-priv: {} stays mounted", root.dir.display());
        }
    }
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
/// root otherwise: it makes users' processes, mounts and jails, which
/// capability mode would refuse.
///
/// # Errors
///
/// The kernel refused.
pub fn fence_self() -> Result<(), String> {
    rustix::process::set_no_new_privs(None).map_err(|e| format!("no_new_privs: {e}"))
}

/// Kill every instance's processes at once (the node has gone): each
/// jail removed.
pub fn kill_all(insts: &[&Instance]) {
    for inst in insts {
        let _ = signal(inst, libc::SIGKILL);
    }
}
