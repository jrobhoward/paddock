//! Linux: `/proc` and `TCP_INFO`.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::Duration;

use crate::native::Passed;

/// Linux passes the descriptor itself: the app's seccomp filter and
/// Landlock bound what it does with it.
///
/// # Errors
///
/// None on Linux.
#[allow(clippy::unnecessary_wraps)] // FreeBSD's can fail.
pub(crate) fn for_app(_fd: BorrowedFd<'_>, _what: Passed) -> io::Result<Option<OwnedFd>> {
    Ok(None)
}

/// Bytes resident in `pid` and its direct children: the app, or for an
/// instance bound to a lease the runner's watchdog and the app under it.
/// 0 once it has gone.
pub(crate) fn process_bytes(pid: u32) -> u64 {
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
fn resident_pages(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|statm| parse_resident(&statm))
        .unwrap_or(0)
}

/// The resident pages in a `/proc/<pid>/statm` line: its second field.
pub(super) fn parse_resident(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

/// How long a connection has carried no data either way. Linux's
/// `TCP_INFO` has both times, so this keeps nothing between reads.
#[derive(Debug, Default)]
pub(crate) struct TcpQuiet;

impl TcpQuiet {
    pub(crate) fn new() -> Self {
        Self
    }

    /// Since the connection last sent or received data.
    pub(crate) fn quiet(&mut self, stream: &std::net::TcpStream) -> std::io::Result<Duration> {
        let info = tcp_info(stream)?;
        let ms = info.tcpi_last_data_recv.min(info.tcpi_last_data_sent);
        Ok(Duration::from_millis(u64::from(ms)))
    }
}

fn tcp_info(stream: &std::net::TcpStream) -> std::io::Result<libc::tcp_info> {
    use std::mem::{MaybeUninit, size_of};
    use std::os::fd::AsRawFd;

    let mut info = MaybeUninit::<libc::tcp_info>::zeroed();
    let mut len =
        libc::socklen_t::try_from(size_of::<libc::tcp_info>()).map_err(std::io::Error::other)?;
    // SAFETY: the descriptor is open for the call (`stream` is borrowed),
    // and the kernel writes at most `len` bytes into `info`, which is that
    // large.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            info.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: zeroed, then partly written by the kernel; every field is an
    // integer, for which any bytes are a value.
    Ok(unsafe { info.assume_init() })
}
