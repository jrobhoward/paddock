//! System calls both systems share: seqpacket pairs, messages with
//! descriptors, poll.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// A connected pair of `SOCK_SEQPACKET` Unix sockets, close-on-exec.
pub fn seqpacket_pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    cvt(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) })
        .expect("socketpair");
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Send one message, with `fds` as `SCM_RIGHTS`.
pub fn send(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
    assert!(!data.is_empty(), "an empty seqpacket message reads as EOF");
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let mut cbuf = [0u64; 32];
    if !fds.is_empty() {
        let bytes = std::mem::size_of_val(fds) as u32;
        let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
        assert!(space <= std::mem::size_of_val(&cbuf));
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, fds.len());
        }
    }
    let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Receive one message and its descriptors (close-on-exec). `Ok(None)` at
/// end of stream.
pub fn recv(sock: RawFd, buf: &mut [u8]) -> io::Result<Option<(usize, Vec<OwnedFd>)>> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = [0u64; 32];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        break n as usize;
    };
    let mut fds = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let p = libc::CMSG_DATA(c) as *const RawFd;
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(p.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if n == 0 && fds.is_empty() {
        return Ok(None);
    }
    Ok(Some((n, fds)))
}

/// Poll `fds` for input; returns each one's `revents`.
pub fn poll(fds: &[RawFd], timeout_ms: i32) -> io::Result<Vec<libc::c_short>> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    loop {
        let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as _, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(p.iter().map(|p| p.revents).collect());
    }
}

/// Move `fds` to 3, 4, ... in a child before exec, clearing close-on-exec on
/// them; the rest stay close-on-exec. Only system calls: safe after fork.
pub fn place_fds(fds: &[RawFd]) -> io::Result<()> {
    // Above every target first, so a source never sits on a target slot.
    let mut high = [0 as RawFd; 16];
    for (i, &fd) in fds.iter().enumerate() {
        high[i] = cvt(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 64) })?;
    }
    for (i, &fd) in high[..fds.len()].iter().enumerate() {
        cvt(unsafe { libc::dup2(fd, 3 + i as RawFd) })?;
    }
    Ok(())
}

/// The calling thread's errno as an `i32`, for reporting from a child.
pub fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A name for an errno value (`EACCES`), or the number.
pub fn errname(e: i32) -> String {
    let names: &[(i32, &str)] = &[
        (libc::EPERM, "EPERM"),
        (libc::ENOENT, "ENOENT"),
        (libc::ESRCH, "ESRCH"),
        (libc::EACCES, "EACCES"),
        (libc::EINVAL, "EINVAL"),
        (libc::ENOSYS, "ENOSYS"),
        (libc::ECONNREFUSED, "ECONNREFUSED"),
        (libc::ENETUNREACH, "ENETUNREACH"),
        (libc::EROFS, "EROFS"),
        (libc::EBADF, "EBADF"),
        (libc::EAFNOSUPPORT, "EAFNOSUPPORT"),
        (libc::EPROTONOSUPPORT, "EPROTONOSUPPORT"),
        (libc::EADDRNOTAVAIL, "EADDRNOTAVAIL"),
        (libc::EEXIST, "EEXIST"),
        #[cfg(target_os = "freebsd")]
        (libc::ENOTCAPABLE, "ENOTCAPABLE"),
        #[cfg(target_os = "freebsd")]
        (libc::ECAPMODE, "ECAPMODE"),
    ];
    names.iter().find(|(n, _)| *n == e && e != 0).map_or_else(|| format!("errno {e}"), |(_, s)| (*s).to_string())
}

/// The result of a system call as `ok` or its errno's name.
pub fn outcome(r: io::Result<()>) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(e) => e.raw_os_error().map_or_else(|| e.to_string(), errname),
    }
}

