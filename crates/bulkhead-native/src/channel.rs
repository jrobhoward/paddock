//! The channel between a node and a native app: two byte rings in shared
//! memory, one each way, carrying [`crate::protocol`]'s length-prefixed
//! frames as a pipe would (ROADMAP T1, spike S32).
//!
//! A side waiting for the other spins for a budget (node config
//! `[native] spin_us`, given to the app as [`SPIN_ENV`]), then sleeps: a
//! blocking round trip between processes costs two wake-ups of a sleeping
//! thread, 7 to 20 µs, which only spinning avoids. Each side sleeps on a
//! pipe the other writes a byte to: the app's door for frames from the
//! node, and the node's bell, whose end of file also tells the node the
//! app has exited. A pipe, not a futex: a pipe's writer wakes its reader
//! with the scheduler's sync hint, which favours the writer's warm core
//! (not measured apart from T1's other fixes). Only an app's thread
//! waiting for room in the ring to the node sleeps on a futex in the
//! mapping: rare, and one at a time.
//!
//! The app inherits:
//! - [`MEMFD_FD`]: a memfd of [`CHANNEL_BYTES`], the ring to the app at
//!   offset 0 and the ring to the node at [`RING_BYTES`];
//! - [`BELL_FD`]: the write end of the node's bell pipe, non-blocking;
//! - [`DOOR_FD`]: the read end of its own door pipe.
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
//!
//! Positions only grow; a byte at position `p` is at `HEADER + p % RING_CAP`.
//! A side that finds the other's word set after writing (or reading)
//! clears it and wakes the other: a futex wake on that word for the app,
//! a byte on the bell for the node. Neither side trusts the positions the
//! other writes: [`Ring::read`] and [`Ring::write`] refuse a ring holding
//! more than [`RING_CAP`] bytes ([`Corrupt`]), every copy stays inside the
//! data area, and the node decodes frames only from its own copy.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

use rustix::fs::{MemfdFlags, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::thread::futex;

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
/// The environment variable holding the spin budget in microseconds.
pub const SPIN_ENV: &str = "BULKHEAD_SPIN_US";
/// The spin budget without one configured: it must outlast a wake-up,
/// about 20 µs on an x86 machine with deep C-states (S32).
pub const DEFAULT_SPIN: Duration = Duration::from_micros(20);

const HEAD: usize = 0;
const TAIL: usize = 64;
const READER_WAITING: usize = 128;
const WRITER_WAITING: usize = 192;
const CLOSED: usize = 256;

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
        let fd = memfd_create("bulkhead-channel", MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, CHANNEL_BYTES as u64)?;
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
        let _ = futex::wake(word, futex::Flags::empty(), 1);
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

/// Put `memfd`, `bell` (its write end) and `door` (its read end) at
/// [`MEMFD_FD`], [`BELL_FD`] and [`DOOR_FD`], open across exec, in a
/// child between fork and exec: system calls only, no allocation.
///
/// # Errors
///
/// A failed system call.
pub fn install(
    memfd: BorrowedFd<'_>,
    bell: BorrowedFd<'_>,
    door: BorrowedFd<'_>,
) -> io::Result<()> {
    // Above every target first, so no `dup2` overwrites another's source;
    // the copies close at exec.
    let memfd = rustix::io::fcntl_dupfd_cloexec(memfd, DOOR_FD + 1)?;
    let bell = rustix::io::fcntl_dupfd_cloexec(bell, DOOR_FD + 1)?;
    let door = rustix::io::fcntl_dupfd_cloexec(door, DOOR_FD + 1)?;
    for (from, to) in [(&memfd, MEMFD_FD), (&bell, BELL_FD), (&door, DOOR_FD)] {
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
        Self(
            std::env::var(SPIN_ENV)
                .ok()
                .and_then(|v| v.parse().ok())
                .map_or(DEFAULT_SPIN, Duration::from_micros),
        )
    }

    /// Block until `ready()`: spin with `sched_yield` up to the budget,
    /// then set `word` and `sleep` until the other side clears it and
    /// wakes this thread (`sleep` may return early: the condition is
    /// checked again). `sched_yield` rather than a pause instruction: with
    /// more busy processes than cores a pause starves the other side
    /// (S32).
    pub fn block_until(self, word: &AtomicU32, ready: impl Fn() -> bool, sleep: impl Fn()) {
        if ready() {
            return;
        }
        if !self.0.is_zero() {
            let start = Instant::now();
            let mut turn = 0u32;
            loop {
                std::thread::yield_now();
                if ready() {
                    return;
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
                return;
            }
            sleep();
            if ready() {
                word.store(0, Ordering::SeqCst);
                return;
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
    spin: Spin,
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
        let (memfd, bell, door) = unsafe {
            (
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(MEMFD_FD),
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(BELL_FD),
                <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(DOOR_FD),
            )
        };
        let channel = Channel::open(memfd.as_fd())?;
        // Not for anything this process might start: the bell's end of
        // file tells the node this process has exited.
        for fd in [&bell, &door] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
        }
        Ok(Self {
            channel,
            bell,
            door,
            spin: Spin::from_env(),
        })
    }

    /// An app's end of `channel`, for tests: `bell` is the bell's write
    /// end, `door` the door's read end.
    #[must_use]
    pub fn new(channel: Channel, bell: OwnedFd, door: OwnedFd, spin: Spin) -> Self {
        Self {
            channel,
            bell,
            door,
            spin,
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
            self.spin.block_until(
                ring.reader_waiting(),
                || ring.is_readable() || ring.is_closed(),
                || {
                    // Blocks until the node writes a byte; a stale one
                    // only means one more look.
                    let _ = rustix::io::read(&self.door, &mut [0u8; 64]);
                },
            );
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
            self.spin.block_until(
                word,
                || ring.has_room(),
                || {
                    // Returns at once if the node cleared the word first.
                    let _ = futex::wait(word, futex::Flags::empty(), 1, None);
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

#[cfg(test)]
#[path = "channel_tests.rs"]
mod channel_tests;
