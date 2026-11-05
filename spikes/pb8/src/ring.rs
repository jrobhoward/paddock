//! A single-producer, single-consumer frame ring in shared memory, copied
//! from spike S32 (S30's ring, with a futex word for a consumer that sleeps
//! and a reader that checks what the other side wrote). PB8 puts two in
//! each app's memfd for the hub, and two in each pair's memfd for a direct
//! channel.
//!
//! Layout of one ring: `head` (the consumer's position) at 0, `tail` (the
//! producer's) at 64, `waiting` (the consumer sleeps; also its futex word)
//! at 128, data from 192. A frame is a 4-byte length and its bytes,
//! wrapping at the end.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

pub const CAP: usize = 1 << 16;
const DATA: usize = 192;
pub const RING: usize = DATA + CAP;

pub struct Ring {
    base: *mut u8,
}

// SAFETY: one producer and one consumer, each on one thread at a time;
// the shared positions are atomics.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

/// The other side broke the ring's rules: a length or position no honest
/// producer writes. The node ends such an app.
#[derive(Debug)]
pub struct Corrupt;

impl Ring {
    /// # Safety
    /// `base` points to `RING` bytes of shared memory that outlive the ring.
    pub unsafe fn new(base: *mut u8) -> Self {
        Self { base }
    }

    fn head(&self) -> &AtomicU64 {
        // SAFETY: aligned, inside the mapping.
        unsafe { &*(self.base as *const AtomicU64) }
    }
    fn tail(&self) -> &AtomicU64 {
        // SAFETY: as for `head`.
        unsafe { &*(self.base.add(64) as *const AtomicU64) }
    }
    pub fn waiting(&self) -> &AtomicU32 {
        // SAFETY: as for `head`.
        unsafe { &*(self.base.add(128) as *const AtomicU32) }
    }

    fn copy_in(&self, at: u64, bytes: &[u8]) {
        let start = (at as usize) % CAP;
        let first = bytes.len().min(CAP - start);
        // SAFETY: both ranges are inside the data area.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(DATA + start), first);
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(first),
                self.base.add(DATA),
                bytes.len() - first,
            );
        }
    }

    fn copy_out(&self, at: u64, out: &mut [u8]) {
        let start = (at as usize) % CAP;
        let first = out.len().min(CAP - start);
        // SAFETY: as for `copy_in`; `out` is at most CAP bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(self.base.add(DATA + start), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(
                self.base.add(DATA),
                out.as_mut_ptr().add(first),
                out.len() - first,
            );
        }
    }

    /// Append a frame; true if the consumer was asleep and must be woken.
    pub fn push(&self, frame: &[u8]) -> bool {
        let need = 4 + frame.len() as u64;
        assert!(need as usize <= CAP);
        let tail = self.tail().load(Ordering::Relaxed);
        while tail + need - self.head().load(Ordering::Acquire) > CAP as u64 {
            std::hint::spin_loop();
        }
        self.copy_in(tail, &(frame.len() as u32).to_le_bytes());
        self.copy_in(tail + 4, frame);
        self.tail().store(tail + need, Ordering::Release);
        fence(Ordering::SeqCst);
        self.waiting().load(Ordering::Relaxed) == 1 && self.waiting().swap(0, Ordering::SeqCst) == 1
    }

    /// The next frame into `out`, if any. The producer is untrusted: the
    /// positions and the length are checked before anything is copied, and
    /// the frame is copied out before it is read.
    pub fn pop(&self, out: &mut Vec<u8>) -> Result<bool, Corrupt> {
        let head = self.head().load(Ordering::Relaxed);
        let tail = self.tail().load(Ordering::Acquire);
        if head == tail {
            return Ok(false);
        }
        let queued = tail.wrapping_sub(head);
        if queued < 4 || queued > CAP as u64 {
            return Err(Corrupt);
        }
        let mut len = [0u8; 4];
        self.copy_out(head, &mut len);
        let len = u32::from_le_bytes(len) as u64;
        if 4 + len > queued {
            return Err(Corrupt);
        }
        out.resize(len as usize, 0);
        self.copy_out(head + 4, out);
        self.head().store(head + 4 + len, Ordering::Release);
        Ok(true)
    }

    /// Say the consumer is about to sleep; false if a frame arrived
    /// meanwhile, so it must not.
    pub fn sleep(&self) -> bool {
        self.waiting().store(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.head().load(Ordering::Relaxed) != self.tail().load(Ordering::SeqCst) {
            self.waiting().store(0, Ordering::SeqCst);
            return false;
        }
        true
    }
}

/// Sleep while `word` is 1 (shared, not process-private: the word is in
/// a mapping two processes share).
pub fn futex_wait(word: &AtomicU32) {
    // SAFETY: a valid aligned u32 in a live mapping; no timeout.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const AtomicU32,
            libc::FUTEX_WAIT,
            1u32,
            std::ptr::null::<libc::timespec>(),
        );
    }
}

/// Wake one sleeper on `word`.
pub fn futex_wake(word: &AtomicU32) {
    // SAFETY: as for `futex_wait`.
    unsafe {
        libc::syscall(libc::SYS_futex, word as *const AtomicU32, libc::FUTEX_WAKE, 1u32);
    }
}

/// Write 1 to an eventfd.
pub fn ring_bell(fd: i32) {
    let one = 1u64;
    // SAFETY: an 8-byte write from a valid buffer.
    unsafe { libc::write(fd, (&one as *const u64).cast(), 8) };
}

/// Read a non-blocking eventfd's counter.
pub fn drain(fd: i32) {
    let mut n = 0u64;
    // SAFETY: an 8-byte read into a valid buffer.
    unsafe { libc::read(fd, (&mut n as *mut u64).cast(), 8) };
}
