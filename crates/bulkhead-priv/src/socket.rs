//! Messages on the seqpacket socket to the node, with descriptors beside
//! them.

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use bulkhead_priv_wire::{MAX_FDS, MAX_MESSAGE};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};

/// What ends a record on a `SOCK_SEQPACKET` socket. FreeBSD follows
/// POSIX: a record ends only at a send with `MSG_EOR`, so two sends
/// without it read as one message there. Linux ends one at every send.
#[cfg(target_os = "freebsd")]
const RECORD: SendFlags = SendFlags::EOR;
#[cfg(not(target_os = "freebsd"))]
const RECORD: SendFlags = SendFlags::empty();

/// A connected pair of seqpacket sockets, close-on-exec.
///
/// # Errors
///
/// The kernel refused.
pub fn pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

/// Send one message, with `fds` beside it.
///
/// # Errors
///
/// The node's end is gone, or the message was cut short.
pub fn send(sock: BorrowedFd<'_>, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> std::io::Result<()> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(std::io::Error::other("no room for the descriptors"));
    }
    let sent = rustix::net::sendmsg(
        sock,
        &[IoSlice::new(bytes)],
        &mut control,
        SendFlags::NOSIGNAL | RECORD,
    )?;
    if sent != bytes.len() {
        return Err(std::io::Error::other("a message was cut short"));
    }
    Ok(())
}

/// What a receive found.
pub enum Received {
    /// A message and the descriptors beside it (close-on-exec).
    Message(Vec<u8>, Vec<OwnedFd>),
    /// A message cut short or with too many descriptors: refused whole.
    Malformed,
    /// The other end closed.
    Closed,
}

/// Receive one message, blocking.
///
/// # Errors
///
/// The kernel refused.
pub fn recv(sock: BorrowedFd<'_>) -> std::io::Result<Received> {
    let mut buf = vec![0u8; MAX_MESSAGE];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = loop {
        match rustix::net::recvmsg(
            sock,
            &mut [IoSliceMut::new(&mut buf)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        ) {
            Err(rustix::io::Errno::INTR) => {}
            other => break other?,
        }
    };
    let mut fds = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = message {
            fds.extend(received);
        }
    }
    if msg
        .flags
        .intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC)
        || fds.len() > MAX_FDS
    {
        return Ok(Received::Malformed);
    }
    if msg.bytes == 0 && fds.is_empty() {
        return Ok(Received::Closed);
    }
    buf.truncate(msg.bytes);
    Ok(Received::Message(buf, fds))
}

#[cfg(test)]
#[path = "socket_tests.rs"]
mod socket_tests;
