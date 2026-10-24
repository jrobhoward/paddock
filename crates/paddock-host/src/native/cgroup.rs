//! cgroup v2 limits for native instances, under a directory delegated to
//! the node (node config `[native] cgroup`).
//!
//! ```text
//! <root>/node/            the node's own processes
//! <root>/native/<pid>/    one per native instance: memory.max, pids.max
//! ```
//!
//! cgroup v2 allows no processes in a cgroup whose controllers are enabled
//! for its children, so the node moves itself into the leaf `node` before
//! enabling `memory` and `pids` below `<root>` (spike S19).

use std::io;
use std::path::{Path, PathBuf};

/// The controllers enabled for native instances.
const CONTROLLERS: &str = "+memory +pids";

/// Move this process into `<root>/node` and make `<root>/native`, with
/// `memory` and `pids` enabled for its children.
///
/// # Errors
///
/// The directory is not a delegated cgroup v2 directory the node may
/// write.
pub(crate) fn prepare(root: &Path) -> io::Result<()> {
    let node = root.join("node");
    std::fs::create_dir_all(&node)?;
    std::fs::write(node.join("cgroup.procs"), std::process::id().to_string())?;
    std::fs::write(root.join("cgroup.subtree_control"), CONTROLLERS).map_err(|e| {
        if e.raw_os_error() == Some(16) {
            // EBUSY: processes other than the node's are still in `root`.
            io::Error::other(format!(
                "{e}: other processes are in this cgroup; the node must be the only one there"
            ))
        } else {
            e
        }
    })?;
    let native = root.join("native");
    std::fs::create_dir_all(&native)?;
    std::fs::write(native.join("cgroup.subtree_control"), CONTROLLERS)?;
    Ok(())
}

/// A cgroup for one native instance, named `name`, with its limits.
///
/// # Errors
///
/// The cgroup cannot be made or limited.
// Only the Linux `Port::spawn` makes one; native apps run on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn create(
    root: &Path,
    name: &str,
    memory_max: u64,
    pids_max: u32,
) -> io::Result<PathBuf> {
    let dir = root.join("native").join(name);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("memory.max"), memory_max.to_string())?;
    // Absent without swap accounting; then there is no swap to limit.
    let _ = std::fs::write(dir.join("memory.swap.max"), "0");
    std::fs::write(dir.join("pids.max"), pids_max.to_string())?;
    Ok(dir)
}

/// Whether the kernel's OOM killer has killed a process in `dir`: the
/// instance went over `memory.max`.
pub(crate) fn oom_killed(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("memory.events"))
        .ok()
        .and_then(|events| {
            events
                .lines()
                .find_map(|l| l.strip_prefix("oom_kill "))
                .and_then(|n| n.trim().parse::<u64>().ok())
        })
        .is_some_and(|n| n > 0)
}

/// Remove an instance's cgroup once its processes have gone. The kernel
/// empties a cgroup shortly after its last process is reaped, so a busy
/// one is tried again a few times.
pub(crate) async fn remove(dir: &Path) {
    for _ in 0..20 {
        match std::fs::remove_dir(dir) {
            Ok(()) => return,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    tracing::warn!(dir = %dir.display(), "native instance's cgroup not removed");
}
