//! How much memory a native instance holds, read from the OS when asked
//! (`ps` and a metrics scrape), so messages pay nothing for it.

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

/// Where a native instance's memory can be read.
#[derive(Debug, Clone)]
pub(crate) enum MemorySource {
    /// Its cgroup: the kernel's whole count for the instance.
    #[cfg(target_os = "linux")]
    Cgroup(PathBuf),
    /// The process the node started, and those under it
    /// ([`super::os::process_bytes`]).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Process(u32),
}

impl MemorySource {
    /// Bytes held now: 0 once the process or cgroup has gone.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(crate) fn read(&self) -> u64 {
        match self {
            #[cfg(target_os = "linux")]
            Self::Cgroup(dir) => cgroup_bytes(dir),
            Self::Process(pid) => super::os::process_bytes(*pid),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    pub(crate) fn read(&self) -> u64 {
        match *self {}
    }
}

#[cfg(target_os = "linux")]
fn cgroup_bytes(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join("memory.current"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;
