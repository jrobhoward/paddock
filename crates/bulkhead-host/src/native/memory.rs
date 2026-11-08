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
    /// The process the node started, and its direct children: the app, or
    /// for an instance bound to a lease the runner's watchdog and the app
    /// under it.
    #[cfg(target_os = "linux")]
    Process(u32),
}

impl MemorySource {
    /// Bytes held now: 0 once the process or cgroup has gone.
    #[cfg(target_os = "linux")]
    pub(crate) fn read(&self) -> u64 {
        match self {
            Self::Cgroup(dir) => cgroup_bytes(dir),
            Self::Process(pid) => process_bytes(*pid),
        }
    }

    #[cfg(not(target_os = "linux"))]
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

#[cfg(target_os = "linux")]
fn process_bytes(pid: u32) -> u64 {
    let page = u64::try_from(rustix::param::page_size()).unwrap_or(4096);
    let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"));
    let pages: u64 = std::iter::once(pid)
        .chain(
            children
                .iter()
                .flat_map(|text| text.split_whitespace())
                .filter_map(|child| child.parse().ok()),
        )
        .map(resident_pages)
        .sum();
    pages.saturating_mul(page)
}

/// `pid`'s resident pages, or 0 if it has gone.
#[cfg(target_os = "linux")]
fn resident_pages(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|statm| parse_resident(&statm))
        .unwrap_or(0)
}

/// The resident pages in a `/proc/<pid>/statm` line: its second field.
#[cfg(target_os = "linux")]
fn parse_resident(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;
