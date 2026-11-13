//! FreeBSD: `sysctl(kern.proc.pgrp)`, `TCP_INFO` and Capsicum rights.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use bulkhead_native::freebsd::{limit_ioctls, limit_rights, rights};

use crate::native::Passed;

/// A duplicate of `fd` limited to `what`'s rights, to pass to an app in
/// capability mode: the node's own descriptor keeps its rights.
///
/// # Errors
///
/// The duplicate or the limit failed.
pub(crate) fn for_app(fd: BorrowedFd<'_>, what: Passed) -> io::Result<Option<OwnedFd>> {
    let copy = rustix::io::fcntl_dupfd_cloexec(fd, 0)?;
    let list = match what {
        Passed::Snapshot => rights::SNAPSHOT,
        Passed::Stream => rights::STREAM,
        Passed::Memfd => rights::MEMFD,
        Passed::Door => rights::DOOR,
        Passed::Bell => rights::BELL,
    };
    limit_rights(copy.as_raw_fd(), list)?;
    if what == Passed::Stream {
        limit_ioctls(copy.as_raw_fd(), rights::STREAM_IOCTLS)?;
    }
    Ok(Some(copy))
}

/// Bytes resident in the process group `pid` leads: the node starts each
/// instance's runner as a group of its own, so this is the runner (or the
/// app it became), a lease's watchdog and the app under it, and anything
/// the app forked. 0 once it has gone.
pub(crate) fn process_bytes(pid: u32) -> u64 {
    let page = u64::try_from(rustix::param::page_size()).unwrap_or(4096);
    group(pid)
        .iter()
        .map(|p| u64::try_from(p.ki_rssize).unwrap_or(0))
        .sum::<u64>()
        .saturating_mul(page)
}

/// The processes in group `pgid`, or none.
fn group(pgid: u32) -> Vec<libc::kinfo_proc> {
    let Ok(pgid) = libc::c_int::try_from(pgid) else {
        return Vec::new();
    };
    let mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PGRP, pgid];
    let one = std::mem::size_of::<libc::kinfo_proc>();
    // The group may grow between asking its size and reading it: room for
    // a few more, and one retry.
    for _ in 0..2 {
        let mut len = 0usize;
        // SAFETY: a size query: null output, `len` a valid pointer.
        let r = unsafe {
            libc::sysctl(
                mib.as_ptr(),
                4,
                std::ptr::null_mut(),
                &raw mut len,
                std::ptr::null(),
                0,
            )
        };
        if r != 0 || len == 0 {
            return Vec::new();
        }
        let room = len / one + 4;
        let mut procs: Vec<libc::kinfo_proc> = Vec::with_capacity(room);
        let mut len = room * one;
        // SAFETY: the kernel writes at most `len` bytes, the capacity of
        // `procs`, and says how many in `len`.
        let r = unsafe {
            libc::sysctl(
                mib.as_ptr(),
                4,
                procs.as_mut_ptr().cast(),
                &raw mut len,
                std::ptr::null(),
                0,
            )
        };
        if r != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM) {
                continue;
            }
            return Vec::new();
        }
        // SAFETY: the kernel wrote `len` bytes of whole `kinfo_proc`s, each
        // of plain integers and arrays, into the vector's buffer.
        unsafe { procs.set_len((len / one).min(room)) };
        // A kernel whose structure differs from `libc`'s would give
        // misplaced fields; read nothing rather than wrong numbers.
        procs.retain(|p| usize::try_from(p.ki_structsize).ok() == Some(one));
        return procs;
    }
    Vec::new()
}

/// How long a connection has carried no data either way. FreeBSD's
/// `TCP_INFO` gives the time since data was last received but not sent
/// (`__tcpi_last_data_sent` is unimplemented), so sending is seen as a
/// change of `tcpi_snd_nxt` between reads, timed from the read that saw
/// it: never sooner than the truth.
#[derive(Debug)]
pub(crate) struct TcpQuiet {
    /// `tcpi_snd_nxt` at the last read, and when it was first seen.
    sent: Option<(u32, Instant)>,
}

impl TcpQuiet {
    pub(crate) fn new() -> Self {
        Self { sent: None }
    }

    /// Since the connection last sent or received data, as far as reads
    /// so far tell: the first read counts the connection as just sending.
    pub(crate) fn quiet(&mut self, stream: &std::net::TcpStream) -> std::io::Result<Duration> {
        let info = tcp_info(stream)?;
        let now = Instant::now();
        let since_sent = match self.sent {
            Some((nxt, at)) if nxt == info.tcpi_snd_nxt => now - at,
            _ => {
                self.sent = Some((info.tcpi_snd_nxt, now));
                Duration::ZERO
            }
        };
        let since_recv = Duration::from_millis(u64::from(info.tcpi_last_data_recv));
        Ok(since_recv.min(since_sent))
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
