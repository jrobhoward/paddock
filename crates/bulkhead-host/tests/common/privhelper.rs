//! The real privileged helper, `bulkhead-priv`, for tests that run as
//! root (`BULKHEAD_PRIV_TESTS=1`): built on first use, started with
//! `--inherited-socket` so the test process is its node, and configured
//! with a temporary state directory, a cgroup of its own (Linux) and the
//! test's directories as its only binds.
//!
//! The test runs as root, but the helper shares only what the node's user
//! owns, so the node's user here is an ordinary one: the owner of the
//! workspace's `target/`, which must therefore be built by one first.
//! Programs are copied into a small directory the helper may share (a
//! bind is shared whole, and `target/` holds thousands of files).

#![allow(dead_code)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;

/// Whether the helper's tests run: as root, with `BULKHEAD_PRIV_TESTS=1`.
pub fn enabled() -> bool {
    std::env::var_os("BULKHEAD_PRIV_TESTS").is_some_and(|v| v == "1")
        && rustix::process::geteuid().is_root()
}

/// Say why a helper test did nothing.
pub fn skip(name: &str) {
    eprintln!("{name}: skipped (needs root and BULKHEAD_PRIV_TESTS=1)");
}

/// `bulkhead-priv`, built once.
pub fn binary() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = super::workspace_root();
        let target = root.join("target/fixtures/priv");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = Command::new(cargo)
            .current_dir(&root)
            .args(["build", "-p", "bulkhead-priv", "--target-dir"])
            .arg(&target)
            .status()
            .unwrap()
            .success();
        assert!(ok, "building bulkhead-priv failed");
        target.join("debug/bulkhead-priv")
    })
}

/// A running helper serving this process.
pub struct Started {
    /// The node's connection to it.
    pub helper: bulkhead_host::PrivHelper,
    /// The runner the helper starts instances as (a copy).
    pub runner: PathBuf,
    /// Where tests put programs and volumes: under the helper's binds,
    /// owned by [`Started::node_uid`].
    pub dir: tempfile::TempDir,
    /// The node's user, as the helper knows it.
    pub node_uid: u32,
    /// The first user apps get.
    pub first_uid: u32,
    child: Child,
    cgroup: Option<PathBuf>,
}

impl Drop for Started {
    fn drop(&mut self) {
        // `SIGTERM` first: the helper ends its instances and removes what
        // it made for them (on FreeBSD, their jails' mounts).
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(cg) = &self.cgroup {
            remove_tree(cg);
        }
    }
}

/// Remove a cgroup tree, leaves first.
fn remove_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_tree(&e.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Copy `file` into `dir`, owned by `uid`, keeping its mode.
pub fn copy_owned(file: &Path, dir: &Path, uid: u32) -> PathBuf {
    let to = dir.join(file.file_name().unwrap());
    std::fs::copy(file, &to).unwrap();
    std::os::unix::fs::chown(&to, Some(uid), None).unwrap();
    to
}

/// Make `path` a directory owned by `uid`, mode `mode`.
pub fn dir_owned(path: &Path, uid: u32, mode: u32) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    std::os::unix::fs::chown(path, Some(uid), None).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.to_path_buf()
}

/// Start the helper, on the current tokio runtime; `runner` is the
/// `bulkhead-runner` to copy for it.
pub fn start(runner: &Path) -> Started {
    let node_uid = std::fs::metadata(super::workspace_root().join("target"))
        .unwrap()
        .uid();
    assert_ne!(
        node_uid, 0,
        "build target/ as an ordinary user first: the helper refuses root as the node"
    );
    let dir = tempfile::Builder::new()
        .prefix("bh-priv-")
        .tempdir()
        .unwrap();
    // Traversable for apps on a system without a minimal root (FreeBSD).
    dir_owned(dir.path(), node_uid, 0o711);
    let bin = dir_owned(&dir.path().join("bin"), node_uid, 0o755);
    let runner = copy_owned(runner, &bin, node_uid);
    let state = dir.path().join("state");
    let first_uid = 61000;
    let cgroup = cfg!(target_os = "linux").then(|| {
        // One per helper: tests in one process run helpers at once.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cg = PathBuf::from(format!(
            "/sys/fs/cgroup/bulkhead-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&cg).unwrap();
        cg
    });
    let mut text = format!(
        "node = /bin/false\nnode_args = \nnode_uid = {node_uid}\nnode_gid = {node_uid}\n\
         runner = {}\nuids = {first_uid}-{}\nstate = {}\nbinds = {}\n\
         memory_max = 1GiB\npids_max = 256\nports = 81\n",
        runner.display(),
        first_uid + 99,
        state.display(),
        dir.path().display(),
    );
    if let Some(cg) = &cgroup {
        text.push_str(&format!("cgroup = {}\n", cg.display()));
    }
    let conf = dir.path().join("priv.conf");
    std::fs::write(&conf, text).unwrap();
    std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (mine, theirs) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let mut cmd = Command::new(binary());
    cmd.arg("--inherited-socket").arg(&conf);
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let raw = theirs.as_raw_fd();
        // SAFETY: between fork and exec the closure only makes one system
        // call, `dup2`, which clears close-on-exec on the copy.
        unsafe {
            cmd.pre_exec(move || {
                if libc_dup2(raw, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = cmd.spawn().unwrap();
    drop(theirs);
    let helper = bulkhead_host::PrivHelper::new(mine).unwrap();
    Started {
        helper,
        runner,
        dir,
        node_uid,
        first_uid,
        child,
        cgroup,
    }
}

unsafe extern "C" {
    #[link_name = "dup2"]
    fn c_dup2(old: i32, new: i32) -> i32;
}

fn libc_dup2(old: i32, new: i32) -> i32 {
    // SAFETY: `dup2` takes no pointers.
    unsafe { c_dup2(old, new) }
}
