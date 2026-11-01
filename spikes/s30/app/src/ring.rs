//! A single-producer, single-consumer byte ring in shared memory, two per
//! app process (host to app, app to host), with an eventfd to wake a
//! consumer that sleeps. Shared by `s30-app` and `s30-bench` (by path).
//!
//! Layout of one ring: `head` (consumer's position) at 0, `tail`
//! (producer's) at 64, `waiting` (the consumer sleeps) at 128, data from
//! 192. Frames are a 4-byte length and the bytes, wrapping at the end.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

pub const CAP: usize = 1 << 20;
pub const RING: usize = 192 + CAP;

pub struct Ring {
    base: *mut u8,
}

// SAFETY: the ring is used by one producer and one consumer, each on one
// thread, through atomics for the shared positions.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

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
    fn waiting(&self) -> &AtomicU32 {
        // SAFETY: as for `head`.
        unsafe { &*(self.base.add(128) as *const AtomicU32) }
    }

    fn copy_in(&self, at: u64, bytes: &[u8]) {
        let start = (at as usize) % CAP;
        let first = bytes.len().min(CAP - start);
        // SAFETY: both ranges are inside the data area.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(192 + start), first);
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(first),
                self.base.add(192),
                bytes.len() - first,
            );
        }
    }

    fn copy_out(&self, at: u64, out: &mut [u8]) {
        let start = (at as usize) % CAP;
        let first = out.len().min(CAP - start);
        // SAFETY: as for `copy_in`.
        unsafe {
            std::ptr::copy_nonoverlapping(self.base.add(192 + start), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(
                self.base.add(192),
                out.as_mut_ptr().add(first),
                out.len() - first,
            );
        }
    }

    /// Append a frame; true if the consumer was asleep and must be woken.
    pub fn push(&self, frame: &[u8]) -> bool {
        let need = 4 + frame.len() as u64;
        let tail = self.tail().load(Ordering::Relaxed);
        while tail + need - self.head().load(Ordering::Acquire) > CAP as u64 {
            std::hint::spin_loop();
        }
        self.copy_in(tail, &(frame.len() as u32).to_le_bytes());
        self.copy_in(tail + 4, frame);
        self.tail().store(tail + need, Ordering::Release);
        fence(Ordering::SeqCst);
        self.waiting().swap(0, Ordering::SeqCst) == 1
    }

    /// The next frame, if any.
    pub fn pop(&self) -> Option<Vec<u8>> {
        let head = self.head().load(Ordering::Relaxed);
        if head == self.tail().load(Ordering::Acquire) {
            return None;
        }
        let mut len = [0u8; 4];
        self.copy_out(head, &mut len);
        let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
        self.copy_out(head + 4, &mut frame);
        self.head()
            .store(head + 4 + frame.len() as u64, Ordering::Release);
        Some(frame)
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

/// Write 1 to an eventfd.
pub fn wake(fd: i32) {
    let one = 1u64;
    // SAFETY: an 8-byte write from a valid buffer.
    unsafe { libc::write(fd, (&one as *const u64).cast(), 8) };
}

/// Read an eventfd's counter (blocking or not, as the fd is).
pub fn drain(fd: i32) -> bool {
    let mut n = 0u64;
    // SAFETY: an 8-byte read into a valid buffer.
    unsafe { libc::read(fd, (&mut n as *mut u64).cast(), 8) == 8 }
}
