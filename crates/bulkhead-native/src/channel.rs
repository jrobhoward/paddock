//! The channel between a node and a native app: two byte rings in shared
//! memory, one each way, carrying [`crate::protocol`]'s length-prefixed
//! frames as a pipe would (ROADMAP T1, spike S32).
//!
//! A side waiting for the other spins for a budget (node config
//! `[native] spin_us`, given to the app as [`SPIN_ENV`]), then sleeps: a
//! blocking round trip between processes costs two wake-ups of a sleeping
//! thread, 7 to 20 µs, which only spinning avoids. Each side sleeps on a
//! pipe the other writes a byte to: the app's door for frames from the
//! node, whose end of file tells the app the node has gone, and the
//! node's bell, whose end of file tells the node the app has exited.
//! The node writes `idle_hint` before it hands the app a callback: 1 when
//! nothing waits behind that callback and it came longer than the spin
//! after the frame before (ROADMAP T1b). Waiting for its next callback
//! after one so hinted, the app sleeps at once instead of spinning: with
//! many instances each called now and then, their spins took the cores
//! the work needed (the Pi's calls in flight halved), while an instance
//! called back to back still spins. Waits for an answer within a callback
//! always spin. A pipe, not a futex: a pipe's writer wakes its reader
//! with the scheduler's sync hint, which favours the writer's warm core
//! (not measured apart from T1's other fixes). Only an app's thread
//! waiting for room in the ring to the node sleeps on a word in the
//! mapping: rare, and one at a time. That wait is a futex on Linux and
//! `_umtx_op(UMTX_OP_WAIT_UINT)` on FreeBSD, both shared between
//! processes; the mapping is writable on both sides, which FreeBSD's
//! needs (paddock-b spike PB2: a wait on a read-only mapping fails with
//! `EFAULT`).
//!
//! The app inherits:
//! - [`MEMFD_FD`]: a memfd of [`CHANNEL_BYTES`] (on FreeBSD an anonymous
//!   shared memory object, which `memfd_create` makes there, sealed as on
//!   Linux), the ring to the app at
//!   offset 0 and the ring to the node at [`RING_BYTES`];
//! - [`BELL_FD`]: the write end of the node's bell pipe, non-blocking;
//! - [`DOOR_FD`]: the read end of its own door pipe;
//! - [`FDS_FD`]: its end of the descriptor socket (ROADMAP T3).
//!
//! Descriptors the node passes (a bucket's published snapshot, a socket
//! it opened) travel on the descriptor socket, a `SOCK_SEQPACKET` pair,
//! not on the rings: one message per descriptor, its id (8 bytes, little
//! endian) and the descriptor as `SCM_RIGHTS`. The node sends the
//! descriptor first, then a frame that names it by id
//! ([`crate::protocol::FdRef`]), so a frame and a descriptor cannot be
//! paired wrongly: the app takes the descriptor the frame names
//! ([`AppEnd::take_fd`]), keeping any that came first. The node's end
//! only sends: an app cannot pass descriptors to the node.
//!
//! Each ring is a [`HEADER`] of words, then [`RING_CAP`] bytes of data
//! that wrap:
//!
//! | Offset | Word | Written by |
//! | --- | --- | --- |
//! | 0 | `head` (u64): bytes read so far | the reader |
//! | 64 | `tail` (u64): bytes written so far | the writer |
//! | 128 | `reader_waiting` (u32): 1 while the reader sleeps, or is about to | the reader; cleared by the writer that wakes it |
//! | 192 | `writer_waiting` (u32): 1 while the writer waits for room | the writer; cleared by the reader that wakes it |
//! | 256 | `closed` (u32): 1 once the writer will write no more | the writer |
//! | 320 | `idle_hint` (u32), in the ring to the app: 1 when the node expects no callback soon | the node |
//! | 384 | `refused_call` (u32), in the ring to the node: 1 plus the number of the system call the app's filter refused, as the app ends | the app |
//!
//! Positions only grow; a byte at position `p` is at `HEADER + p % RING_CAP`.
//! A side that finds the other's word set after writing (or reading)
//! clears it and wakes the other: a wake on that word for the app,
//! a byte on the bell for the node. Neither side trusts the positions the
//! other writes: [`Ring::read`] and [`Ring::write`] refuse a ring holding
//! more than [`RING_CAP`] bytes ([`Corrupt`]), every copy stays inside the
//! data area, and the node decodes frames only from its own copy. The
//! memfd's size is sealed ([`Channel::create`]), so the app cannot shrink
//! the file under the node's mapping.

use std::collections::HashMap;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustix::fs::{MemfdFlags, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, Shutdown, SocketFlags, SocketType,
};

/// The words at the start of each ring, a page.
pub const HEADER: usize = 4096;
/// Each ring's data, a pipe's default buffer.
pub const RING_CAP: usize = 64 << 10;
/// One ring: its header and data.
pub const RING_BYTES: usize = HEADER + RING_CAP;
/// The memfd: the ring to the app, then the ring to the node.
pub const CHANNEL_BYTES: usize = 2 * RING_BYTES;
/// The descriptor the app finds the memfd at.
pub const MEMFD_FD: RawFd = 3;
/// The descriptor the app finds the bell's write end at.
pub const BELL_FD: RawFd = 4;
/// The descriptor the app finds its door's read end at.
pub const DOOR_FD: RawFd = 5;
/// The descriptor the app finds its end of the descriptor socket at.
pub const FDS_FD: RawFd = 6;
/// Most descriptors an app keeps received and not yet taken.
pub const MAX_UNTAKEN: usize = 64;
/// The environment variable holding the spin budget in microseconds.
pub const SPIN_ENV: &str = "BULKHEAD_SPIN_US";
/// The spin budget without one configured: it must outlast a wake-up,
/// about 20 µs on an x86 machine with deep C-states (S32).
pub const DEFAULT_SPIN: Duration = Duration::from_micros(20);
/// The environment variable holding a sender channel's spin budget in
/// microseconds (`[native] sender_spin_us`).
pub const SENDER_SPIN_ENV: &str = "BULKHEAD_SENDER_SPIN_US";
/// How long a thread waiting for an answer on a sender channel spins
/// without a budget configured: longer than an instance channel's, which
/// took a single client through a driver app about 15% off each request
/// on a Raspberry Pi 5 (ROADMAP DA1b).
pub const DEFAULT_SENDER_SPIN: Duration = Duration::from_micros(50);

const HEAD: usize = 0;
const TAIL: usize = 64;
const READER_WAITING: usize = 128;
const WRITER_WAITING: usize = 192;
const CLOSED: usize = 256;
const IDLE_HINT: usize = 320;
const REFUSED_CALL: usize = 384;

/// The other side broke the ring's rules: more queued than the ring
/// holds. The node ends such an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corrupt;

impl std::fmt::Display for Corrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the channel's ring is corrupt")
    }
}

impl std::error::Error for Corrupt {}

impl From<Corrupt> for io::Error {
    fn from(c: Corrupt) -> Self {
        Self::new(io::ErrorKind::InvalidData, c)
    }
}

/// Both rings, mapped.
#[derive(Debug)]
pub struct Channel {
    base: NonNull<u8>,
}

// SAFETY: the mapping is shared memory whose words are only touched
// through atomics, and whose data each side copies under the ring's
// positions; one reader and one writer per ring at a time is the users'
// rule (the node's instance task, the app's lock on its writer).
unsafe impl Send for Channel {}
// SAFETY: as for `Send`.
unsafe impl Sync for Channel {}

impl Channel {
    /// A new channel for an app, with the memfd to hand it.
    ///
    /// # Errors
    ///
    /// The memfd could not be made or mapped.
    pub fn create() -> io::Result<(Self, OwnedFd)> {
        let fd = memfd_create(
            "bulkhead-channel",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?;
        rustix::fs::ftruncate(&fd, CHANNEL_BYTES as u64)?;
        // The app holds this memfd too. Were it to shrink it, the node's
        // next read of a ring would fault (`SIGBUS`), so its size is
        // fixed here; both sides write, so writes are not sealed.
        rustix::fs::fcntl_add_seals(
            &fd,
            rustix::fs::SealFlags::SHRINK
                | rustix::fs::SealFlags::GROW
                | rustix::fs::SealFlags::SEAL,
        )?;
        Ok((Self::map(fd.as_fd())?, fd))
    }

    /// The channel the node handed this app at `fd`.
    ///
    /// # Errors
    ///
    /// `fd` is not a memfd of [`CHANNEL_BYTES`], or it could not be mapped.
    pub fn open(fd: BorrowedFd<'_>) -> io::Result<Self> {
        let size = rustix::fs::fstat(fd)?.st_size;
        if usize::try_from(size).ok() != Some(CHANNEL_BYTES) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the channel is {size} bytes, not {CHANNEL_BYTES}"),
            ));
        }
        Self::map(fd)
    }

    fn map(fd: BorrowedFd<'_>) -> io::Result<Self> {
        // SAFETY: a new shared mapping of a whole file, at an address the
        // kernel picks; it overlaps nothing and is unmapped only on drop.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                CHANNEL_BYTES,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                fd,
                0,
            )
        }?;
        let base = NonNull::new(p.cast::<u8>()).ok_or_else(|| io::Error::other("null mapping"))?;
        Ok(Self { base })
    }

    /// The ring from the node to the app.
    #[must_use]
    pub fn to_app(&self) -> Ring<'_> {
        Ring {
            base: self.base,
            _channel: std::marker::PhantomData,
        }
    }

    /// The ring from the app to the node.
    #[must_use]
    pub fn to_node(&self) -> Ring<'_> {
        Ring {
            // SAFETY: the second ring starts inside the mapping.
            base: unsafe { self.base.add(RING_BYTES) },
            _channel: std::marker::PhantomData,
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made; no `Ring` outlives the channel.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), CHANNEL_BYTES) };
    }
}

/// One direction of a [`Channel`].
#[derive(Debug, Clone, Copy)]
pub struct Ring<'a> {
    base: NonNull<u8>,
    _channel: std::marker::PhantomData<&'a Channel>,
}

// SAFETY: a view of a `Channel`, which is `Send` and `Sync` for the same
// reasons; it borrows the channel, so it cannot outlive the mapping.
unsafe impl Send for Ring<'_> {}
// SAFETY: as for `Send`.
unsafe impl Sync for Ring<'_> {}

impl Ring<'_> {
    fn word64(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: an aligned u64 inside the ring's header, which lives as
        // long as the channel this ring borrows.
        unsafe { self.base.add(offset).cast::<AtomicU64>().as_ref() }
    }

    fn word32(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: as for `word64`.
        unsafe { self.base.add(offset).cast::<AtomicU32>().as_ref() }
    }

    /// 1 while the reader sleeps, or is about to.
    #[must_use]
    pub fn reader_waiting(&self) -> &AtomicU32 {
        self.word32(READER_WAITING)
    }

    /// 1 when the node expects no callback soon after the one it handed
    /// over (the ring to the app only; module docs).
    #[must_use]
    pub fn idle_hint(&self) -> &AtomicU32 {
        self.word32(IDLE_HINT)
    }

    /// 1 plus the number of the system call the app's seccomp filter
    /// refused, written by the app's `SIGSYS` handler as it ends (the ring
    /// to the node only); 0 otherwise. Only a reason to show: an app can
    /// write anything here.
    #[must_use]
    pub fn refused_call(&self) -> &AtomicU32 {
        self.word32(REFUSED_CALL)
    }

    /// 1 while the writer waits for room.
    #[must_use]
    pub fn writer_waiting(&self) -> &AtomicU32 {
        self.word32(WRITER_WAITING)
    }

    /// Bytes queued, checked against the ring's size.
    fn queued(&self) -> Result<usize, Corrupt> {
        let head = self.word64(HEAD).load(Ordering::Acquire);
        let tail = self.word64(TAIL).load(Ordering::Acquire);
        usize::try_from(tail.wrapping_sub(head))
            .ok()
            .filter(|q| *q <= RING_CAP)
            .ok_or(Corrupt)
    }

    /// How many bytes there are to read.
    ///
    /// # Errors
    ///
    /// [`Corrupt`] if the positions are impossible.
    pub fn available(&self) -> Result<usize, Corrupt> {
        self.queued()
    }

    /// Whether there are bytes to read (a corrupt ring counts as readable,
    /// so the reader finds out).
    #[must_use]
    pub fn is_readable(&self) -> bool {
        self.queued() != Ok(0)
    }

    /// Whether there is room to write (a corrupt ring counts as room, so
    /// the writer finds out).
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.queued() != Ok(RING_CAP)
    }

    /// Whether the writer closed the ring.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.word32(CLOSED).load(Ordering::Acquire) == 1
    }

    /// Close the ring: the reader reads what is left, then the end.
    pub fn close(&self) {
        self.word32(CLOSED).store(1, Ordering::Release);
        fence(Ordering::SeqCst);
    }

    /// Write as much of `bytes` as fits; how much did.
    ///
    /// # Errors
    ///
    /// [`Corrupt`] if the reader's position is impossible.
    pub fn write(&self, bytes: &[u8]) -> Result<usize, Corrupt> {
        let tail = self.word64(TAIL).load(Ordering::Relaxed);
        let n = bytes.len().min(RING_CAP - self.queued()?);
        if n > 0 {
            self.copy(tail, bytes.as_ptr(), n, true);
            self.word64(TAIL).store(tail + n as u64, Ordering::Release);
            fence(Ordering::SeqCst);
        }
        Ok(n)
    }

    /// Read up to `out.len()` bytes; how many came.
    ///
    /// # Errors
    ///
    /// [`Corrupt`] if the writer's position is impossible.
    pub fn read(&self, out: &mut [u8]) -> Result<usize, Corrupt> {
        let head = self.word64(HEAD).load(Ordering::Relaxed);
        let n = out.len().min(self.queued()?);
        if n > 0 {
            self.copy(head, out.as_mut_ptr(), n, false);
            self.word64(HEAD).store(head + n as u64, Ordering::Release);
            fence(Ordering::SeqCst);
        }
        Ok(n)
    }

    /// Copy `n` bytes (at most [`RING_CAP`]) between `at` in the ring and
    /// `other`, wrapping at the end of the data area.
    fn copy(&self, at: u64, other: *const u8, n: usize, into_ring: bool) {
        // `% RING_CAP` keeps `start` inside the data area whatever `at` is.
        let start = usize::try_from(at % RING_CAP as u64).unwrap_or(0);
        let first = n.min(RING_CAP - start);
        // SAFETY: both ring ranges lie inside the data area (`start <
        // RING_CAP`, `first <= RING_CAP - start`, `n - first <= start`), and
        // `other` is valid for `n` bytes (the caller's slice); the ring and
        // a caller's slice never overlap.
        unsafe {
            let data = self.base.add(HEADER).as_ptr();
            let other = other.cast_mut();
            if into_ring {
                std::ptr::copy_nonoverlapping(other, data.add(start), first);
                std::ptr::copy_nonoverlapping(other.add(first), data, n - first);
            } else {
                std::ptr::copy_nonoverlapping(data.add(start), other, first);
                std::ptr::copy_nonoverlapping(data, other.add(first), n - first);
            }
        }
    }
}

/// Clear `word` and say whether it was set: the other side waits, and
/// must be woken.
#[must_use]
pub fn take_waiting(word: &AtomicU32) -> bool {
    word.load(Ordering::SeqCst) == 1 && word.swap(0, Ordering::SeqCst) == 1
}

/// Wake the app's thread waiting for frames, through its `door`, if
/// `word` (the ring to the app's `reader_waiting`) says it sleeps.
pub fn wake_reader(word: &AtomicU32, door: BorrowedFd<'_>) {
    if take_waiting(word) {
        // Full, the door already wakes the app.
        let _ = rustix::io::write(door, &[1]);
    }
}

/// Wake the app's thread waiting for room, if `word` (the ring to the
/// node's `writer_waiting`) says it sleeps.
pub fn wake_writer(word: &AtomicU32) {
    if take_waiting(word) {
        word_wake(word);
    }
}

/// The app's door: a pipe whose write end, non-blocking, the node keeps
/// and whose read end the app gets as [`DOOR_FD`].
///
/// # Errors
///
/// The pipe could not be made.
pub fn door() -> io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    rustix::fs::fcntl_setfl(&write, rustix::fs::OFlags::NONBLOCK)?;
    Ok((read, write))
}

/// The node's bell: a pipe whose read end the node keeps and whose write
/// end, non-blocking, the app gets as [`BELL_FD`].
///
/// # Errors
///
/// The pipe could not be made.
pub fn bell() -> io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    rustix::fs::fcntl_setfl(&write, rustix::fs::OFlags::NONBLOCK)?;
    Ok((read, write))
}

/// The descriptor socket: the node's end, which only sends and does not
/// block, and the app's, for [`FDS_FD`].
///
/// # Errors
///
/// The socket could not be made.
pub fn descriptors() -> io::Result<(OwnedFd, OwnedFd)> {
    let (node, app) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::net::shutdown(&node, Shutdown::Read)?;
    rustix::fs::fcntl_setfl(&node, rustix::fs::OFlags::NONBLOCK)?;
    Ok((node, app))
}

/// What ends a record on a `SOCK_SEQPACKET` socket. FreeBSD follows
/// POSIX: a record ends only at a send with `MSG_EOR`, so two sends
/// without it read as one message there. Linux ends one at every send.
#[cfg(target_os = "freebsd")]
const RECORD: SendFlags = SendFlags::EOR;
#[cfg(not(target_os = "freebsd"))]
const RECORD: SendFlags = SendFlags::empty();

/// Pass `fd` as `id` on the node's end of a descriptor socket.
///
/// # Errors
///
/// The socket refused it: full (an app that takes no descriptors), or the
/// app gone.
pub fn send_fd(socket: BorrowedFd<'_>, id: u64, fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds = [fd];
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::other("no room for the descriptor"));
    }
    let bytes = id.to_le_bytes();
    let sent = rustix::net::sendmsg(
        socket,
        &[IoSlice::new(&bytes)],
        &mut control,
        SendFlags::DONTWAIT | SendFlags::NOSIGNAL | RECORD,
    )?;
    if sent != bytes.len() {
        return Err(io::Error::other("the descriptor's message was cut short"));
    }
    Ok(())
}

/// The next descriptor on the app's end of a descriptor socket, without
/// waiting: its id and the descriptor (close-on-exec), or `None`.
///
/// # Errors
///
/// The message is not one id and one descriptor.
pub fn recv_fd(socket: BorrowedFd<'_>) -> io::Result<Option<(u64, OwnedFd)>> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut bytes = [0u8; 8];
    let msg = match rustix::net::recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut bytes)],
        &mut control,
        RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
    ) {
        Ok(msg) => msg,
        Err(rustix::io::Errno::AGAIN) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut fds: Vec<OwnedFd> = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = message {
            fds.extend(received);
        }
    }
    let cut = msg
        .flags
        .intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC);
    match (msg.bytes, fds.pop()) {
        (8, Some(fd)) if fds.is_empty() && !cut => Ok(Some((u64::from_le_bytes(bytes), fd))),
        (0, None) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the node closed the descriptor socket",
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a descriptor message is not one id and one descriptor",
        )),
    }
}

/// A sealed memfd holding `bytes`: nothing can write, shrink or grow it,
/// so an app may map it read-only and trust it stays as it was (a
/// published bucket's snapshot).
///
/// # Errors
///
/// The memfd could not be made, written or sealed.
pub fn sealed(name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    let fd = memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)?;
    let fd = OwnedFd::from(file);
    rustix::fs::fcntl_add_seals(
        &fd,
        rustix::fs::SealFlags::WRITE
            | rustix::fs::SealFlags::SHRINK
            | rustix::fs::SealFlags::GROW
            | rustix::fs::SealFlags::SEAL,
    )?;
    Ok(fd)
}

/// A sealed memfd, mapped read-only.
#[derive(Debug)]
pub struct Mapped {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: read-only memory no one can change (the memfd is sealed against
// writes, shrinking and growing), unmapped only on drop.
unsafe impl Send for Mapped {}
// SAFETY: as for `Send`.
unsafe impl Sync for Mapped {}

impl Mapped {
    /// Map `fd`, which must be sealed against writes, shrinking and
    /// growing.
    ///
    /// # Errors
    ///
    /// It is not sealed, is empty, or could not be mapped.
    pub fn open(fd: &OwnedFd) -> io::Result<Self> {
        let need = rustix::fs::SealFlags::WRITE
            | rustix::fs::SealFlags::SHRINK
            | rustix::fs::SealFlags::GROW;
        if !rustix::fs::fcntl_get_seals(fd)?.contains(need) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the memfd is not sealed",
            ));
        }
        let len = usize::try_from(rustix::fs::fstat(fd)?.st_size)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "an empty memfd"))?;
        // SAFETY: a new read-only shared mapping of a whole sealed file, at
        // an address the kernel picks; unmapped only on drop.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::SHARED,
                fd,
                0,
            )
        }?;
        let base = NonNull::new(p.cast::<u8>()).ok_or_else(|| io::Error::other("null mapping"))?;
        Ok(Self { base, len })
    }

    /// The mapped bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `len` readable bytes from `base` for as long as `self`
        // lives, which nothing writes: the memfd is sealed against it.
        unsafe { std::slice::from_raw_parts(self.base.as_ptr(), self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: the mapping `open` made; `bytes` borrows `self`, so no
        // slice outlives it.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
    }
}

/// Put `memfd`, `bell` (its write end), `door` (its read end) and `fds`
/// (the app's end) at [`MEMFD_FD`], [`BELL_FD`], [`DOOR_FD`] and
/// [`FDS_FD`], open across exec, in a child between fork and exec: system
/// calls only, no allocation.
///
/// # Errors
///
/// A failed system call.
pub fn install(
    memfd: BorrowedFd<'_>,
    bell: BorrowedFd<'_>,
    door: BorrowedFd<'_>,
    fds: BorrowedFd<'_>,
) -> io::Result<()> {
    // Above every target first, so no `dup2` overwrites another's source;
    // the copies close at exec.
    let memfd = rustix::io::fcntl_dupfd_cloexec(memfd, FDS_FD + 1)?;
    let bell = rustix::io::fcntl_dupfd_cloexec(bell, FDS_FD + 1)?;
    let door = rustix::io::fcntl_dupfd_cloexec(door, FDS_FD + 1)?;
    let fds = rustix::io::fcntl_dupfd_cloexec(fds, FDS_FD + 1)?;
    for (from, to) in [
        (&memfd, MEMFD_FD),
        (&bell, BELL_FD),
        (&door, DOOR_FD),
        (&fds, FDS_FD),
    ] {
        // SAFETY: `to` is a descriptor number this child owns from here
        // on; whatever it held is replaced, as `dup2` does, and the
        // `OwnedFd` is forgotten, so nothing closes it before exec.
        let mut target = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(to) };
        let done = rustix::io::dup2(from, &mut target);
        std::mem::forget(target);
        done?;
    }
    Ok(())
}

/// How long a waiting side spins before it sleeps.
#[derive(Debug, Clone, Copy)]
pub struct Spin(pub Duration);

impl Spin {
    /// The budget [`SPIN_ENV`] gives, or [`DEFAULT_SPIN`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::env_or(SPIN_ENV, DEFAULT_SPIN)
    }

    /// A sender channel's budget: what [`SENDER_SPIN_ENV`] gives, or
    /// [`DEFAULT_SENDER_SPIN`].
    #[must_use]
    pub fn sender_from_env() -> Self {
        Self::env_or(SENDER_SPIN_ENV, DEFAULT_SENDER_SPIN)
    }

    fn env_or(var: &str, default: Duration) -> Self {
        Self(
            std::env::var(var)
                .ok()
                .and_then(|v| v.parse().ok())
                .map_or(default, Duration::from_micros),
        )
    }

    /// Block until `ready()`, and say so: spin with `sched_yield` up to
    /// the budget, then set `word` and `sleep` until the other side clears
    /// it and wakes this thread (`sleep` may return early: the condition is
    /// checked again). `false` if `sleep` says the other side has gone.
    /// `sched_yield` rather than a pause instruction: with more busy
    /// processes than cores a pause starves the other side (S32).
    #[must_use]
    pub fn block_until(
        self,
        word: &AtomicU32,
        ready: impl Fn() -> bool,
        sleep: impl Fn() -> bool,
    ) -> bool {
        if ready() {
            return true;
        }
        if !self.0.is_zero() {
            let start = Instant::now();
            let mut turn = 0u32;
            loop {
                std::thread::yield_now();
                if ready() {
                    return true;
                }
                turn = turn.wrapping_add(1);
                if turn.is_multiple_of(16) && start.elapsed() >= self.0 {
                    break;
                }
            }
        }
        loop {
            word.store(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if ready() {
                word.store(0, Ordering::SeqCst);
                return true;
            }
            let there = sleep();
            if ready() {
                word.store(0, Ordering::SeqCst);
                return true;
            }
            if !there {
                return false;
            }
        }
    }
}

/// The app's end of a channel: frames from the node through [`Read`],
/// frames to it through [`Write`], each blocking as a pipe would.
#[derive(Debug)]
pub struct AppEnd {
    channel: Channel,
    bell: OwnedFd,
    door: OwnedFd,
    /// The app's end of the descriptor socket; a sender channel has none.
    fds: Option<OwnedFd>,
    /// Descriptors received before the one asked for, by id.
    untaken: Mutex<HashMap<u64, OwnedFd>>,
    spin: Spin,
    /// Whether the app waits for its next callback, not an answer.
    between: std::sync::atomic::AtomicBool,
}

impl AppEnd {
    /// The channel the node handed this process ([`MEMFD_FD`],
    /// [`BELL_FD`], [`SPIN_ENV`]).
    ///
    /// # Errors
    ///
    /// The descriptors are missing or not a channel.
    pub fn inherited() -> io::Result<Self> {
        // SAFETY: the node put the memfd, the bell and the door at these
        // numbers before exec, and nothing else in this process uses them.
        let (memfd, bell, door, fds) = unsafe {
            (
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(MEMFD_FD),
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(BELL_FD),
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(DOOR_FD),
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(FDS_FD),
            )
        };
        let channel = Channel::open(memfd.as_fd())?;
        // Not for anything this process might start: the bell's end of
        // file tells the node this process has exited.
        for fd in [&bell, &door, &fds] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
        }
        Ok(Self::new(channel, bell, door, fds, Spin::from_env()))
    }

    /// Record that the seccomp filter refused system call `number`, for
    /// the node to read once this process has ended. Only an atomic
    /// store, so a signal handler may call it.
    pub fn refused_call(&self, number: u32) {
        self.channel
            .to_node()
            .refused_call()
            .store(number.saturating_add(1), Ordering::SeqCst);
    }

    /// An app's end of `channel`, for tests: `bell` is the bell's write
    /// end, `door` the door's read end, `fds` the app's end of the
    /// descriptor socket.
    #[must_use]
    pub fn new(channel: Channel, bell: OwnedFd, door: OwnedFd, fds: OwnedFd, spin: Spin) -> Self {
        Self {
            channel,
            bell,
            door,
            fds: Some(fds),
            untaken: Mutex::new(HashMap::new()),
            spin,
            between: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The app's end of a sender channel (ROADMAP AP1) the node passed:
    /// its memfd, its bell's write end and its door's read end. It has no
    /// descriptor socket: descriptors come on the app's own channel's.
    ///
    /// # Errors
    ///
    /// The memfd is not a channel, or a descriptor flag could not be set.
    pub fn sender(memfd: OwnedFd, bell: OwnedFd, door: OwnedFd, spin: Spin) -> io::Result<Self> {
        let channel = Channel::open(memfd.as_fd())?;
        rustix::fs::fcntl_setfl(&bell, rustix::fs::OFlags::NONBLOCK)?;
        Ok(Self {
            channel,
            bell,
            door,
            fds: None,
            untaken: Mutex::new(HashMap::new()),
            spin,
            between: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The descriptor the node passed as `id`, which a frame named: the
    /// node sends it before the frame, so it is there by now. Those that
    /// came first are kept, up to [`MAX_UNTAKEN`], for when their frames
    /// are read.
    ///
    /// # Errors
    ///
    /// The node passed no descriptor `id`, more than [`MAX_UNTAKEN`] came
    /// first, or the socket broke.
    pub fn take_fd(&self, id: u64) -> io::Result<OwnedFd> {
        let mut untaken = self.untaken.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(fd) = untaken.remove(&id) {
            return Ok(fd);
        }
        loop {
            let Some(fds) = &self.fds else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "a sender channel passes no descriptors",
                ));
            };
            match recv_fd(fds.as_fd())? {
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("the node passed no descriptor {id}"),
                    ));
                }
                Some((got, fd)) if got == id => return Ok(fd),
                Some((got, fd)) => {
                    if untaken.len() >= MAX_UNTAKEN {
                        return Err(io::Error::other(format!(
                            "more than {MAX_UNTAKEN} descriptors passed and not taken"
                        )));
                    }
                    untaken.insert(got, fd);
                }
            }
        }
    }

    /// Claim more bytes in the ring to the node than it holds, and wake
    /// the node: for tests of its checks only.
    #[doc(hidden)]
    pub fn break_for_tests(&self) {
        let ring = self.channel.to_node();
        ring.word64(TAIL)
            .fetch_add(2 * RING_CAP as u64, Ordering::SeqCst);
        if take_waiting(ring.reader_waiting()) {
            self.ring_bell();
        }
    }

    fn ring_bell(&self) {
        // A full bell pipe already wakes the node; the node gone, nothing
        // to wake.
        let _ = rustix::io::write(&self.bell, &[1]);
    }

    /// Say whether the app now waits for its next callback rather than an
    /// answer: then, after a callback the node hinted idle, a wait sleeps
    /// at once.
    pub fn set_between(&self, on: bool) {
        self.between.store(on, Ordering::Relaxed);
    }

    /// Read what the node sent into `out`, blocking until something is
    /// there; 0 once the node closed the ring and everything is read.
    ///
    /// # Errors
    ///
    /// The node broke the ring.
    pub fn read_from_node(&self, out: &mut [u8]) -> io::Result<usize> {
        let ring = self.channel.to_app();
        loop {
            let n = ring.read(out)?;
            if n > 0 || out.is_empty() {
                if take_waiting(ring.writer_waiting()) {
                    self.ring_bell();
                }
                return Ok(n);
            }
            if ring.is_closed() && !ring.is_readable() {
                return Ok(0);
            }
            let idle = self.between.load(Ordering::Relaxed)
                && ring.idle_hint().load(Ordering::SeqCst) == 1;
            let spin = if idle {
                Spin(Duration::ZERO)
            } else {
                self.spin
            };
            let there = spin.block_until(
                ring.reader_waiting(),
                || ring.is_readable() || ring.is_closed(),
                || {
                    // Blocks until the node writes a byte (a stale one only
                    // means one more look); the end of file means the
                    // node closed the door: it has gone.
                    !matches!(rustix::io::read(&self.door, &mut [0u8; 64]), Ok(0))
                },
            );
            if !there {
                return Ok(0);
            }
        }
    }

    /// Write some of `bytes` to the node, blocking until there is room.
    ///
    /// # Errors
    ///
    /// The node broke the ring.
    pub fn write_to_node(&self, bytes: &[u8]) -> io::Result<usize> {
        let ring = self.channel.to_node();
        loop {
            let n = ring.write(bytes)?;
            if n > 0 || bytes.is_empty() {
                if take_waiting(ring.reader_waiting()) {
                    self.ring_bell();
                }
                return Ok(n);
            }
            let word = ring.writer_waiting();
            let _ = self.spin.block_until(
                word,
                || ring.has_room(),
                || {
                    // Returns at once if the node cleared the word first.
                    word_wait(word, 1);
                    true
                },
            );
        }
    }
}

/// [`AppEnd::read_from_node`] as [`Read`].
#[derive(Debug)]
pub struct FromNode<'a>(pub &'a AppEnd);

impl Read for FromNode<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_from_node(buf)
    }
}

/// [`AppEnd::write_to_node`] as [`Write`].
#[derive(Debug)]
pub struct ToNode<'a>(pub &'a AppEnd);

impl Write for ToNode<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_to_node(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Sleep while `word` holds `expected`, shared between processes; it may
/// return early, so callers check their condition again.
#[cfg(target_os = "linux")]
fn word_wait(word: &AtomicU32, expected: u32) {
    let _ =
        rustix::thread::futex::wait(word, rustix::thread::futex::Flags::empty(), expected, None);
}

/// Wake one thread of any process sleeping in [`word_wait`] on `word`.
#[cfg(target_os = "linux")]
fn word_wake(word: &AtomicU32) {
    let _ = rustix::thread::futex::wake(word, rustix::thread::futex::Flags::empty(), 1);
}

/// Sleep while `word` holds `expected`, shared between processes (not
/// the `_PRIVATE` operation); it may return early, so callers check their
/// condition again.
#[cfg(target_os = "freebsd")]
fn word_wait(word: &AtomicU32, expected: u32) {
    // SAFETY: `word` is a live, aligned u32 in a writable mapping for the
    // whole call (it is borrowed); the kernel only reads it, and the two
    // null pointers mean no timeout.
    unsafe {
        libc::_umtx_op(
            word.as_ptr().cast(),
            libc::UMTX_OP_WAIT_UINT,
            libc::c_ulong::from(expected),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

/// Wake one thread of any process sleeping in [`word_wait`] on `word`.
#[cfg(target_os = "freebsd")]
fn word_wake(word: &AtomicU32) {
    // SAFETY: as for `word_wait`; a wake reads nothing at the address,
    // which only names the queue.
    unsafe {
        libc::_umtx_op(
            word.as_ptr().cast(),
            libc::UMTX_OP_WAKE,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

#[cfg(test)]
#[path = "channel_tests.rs"]
mod channel_tests;
