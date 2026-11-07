//! Waiting in the CPU until another core writes a word's cache line:
//! `MONITORX`/`MWAITX` on AMD x86-64, a load-exclusive and `WFE` on arm64.
//!
//! [`until_changed`] returns when the word may have changed, when its
//! timeout ran out, or for any other reason the CPU woke (an interrupt; on
//! arm64 any event, including Linux's event stream, every 100 µs at most).
//! The caller always loads the word again.

use std::sync::atomic::AtomicU32;
#[cfg(target_arch = "x86_64")]
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Whether this CPU can wait on a cache line from user space.
#[must_use]
pub fn supported() -> bool {
    *SUPPORTED.get_or_init(detect)
}

static SUPPORTED: OnceLock<bool> = OnceLock::new();

#[cfg(target_arch = "x86_64")]
fn detect() -> bool {
    use std::arch::x86_64::__cpuid;
    // CPUID 8000_0001h, ECX bit 29: MONITORX/MWAITX (AMD), allowed at
    // every privilege level.
    #[allow(unused_unsafe)]
    // SAFETY: CPUID is present on every x86-64 CPU and has no side effects.
    let (max, ext) = unsafe { (__cpuid(0x8000_0000).eax, __cpuid(0x8000_0001).ecx) };
    max >= 0x8000_0001 && (ext >> 29) & 1 == 1
}

#[cfg(target_arch = "aarch64")]
fn detect() -> bool {
    // WFE at EL0 is allowed by Linux (SCTLR_EL1.nTWE is set); the event
    // stream (`evtstrm` in /proc/cpuinfo's features) bounds a wait no
    // write ends. Without the stream a wait could last until an interrupt.
    std::fs::read_to_string("/proc/cpuinfo").is_ok_and(|s| {
        s.lines()
            .find(|l| l.starts_with("Features"))
            .is_some_and(|l| l.split_whitespace().any(|f| f == "evtstrm"))
    })
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect() -> bool {
    false
}

/// TSC ticks per microsecond (x86-64), measured once.
#[cfg(target_arch = "x86_64")]
fn ticks_per_us() -> u64 {
    static TICKS: OnceLock<u64> = OnceLock::new();
    *TICKS.get_or_init(|| {
        use std::arch::x86_64::_rdtsc;
        let start = Instant::now();
        // SAFETY: RDTSC has no side effects (constant_tsc on both machines).
        let t0 = unsafe { _rdtsc() };
        while start.elapsed() < Duration::from_millis(20) {}
        // SAFETY: as above.
        let t1 = unsafe { _rdtsc() };
        let us = start.elapsed().as_nanos() as u64 / 1000;
        ((t1 - t0) / us.max(1)).max(1)
    })
}

/// Wait until `word` may no longer hold `old`, for at most about
/// `timeout` (x86-64; on arm64 until the next event, whatever `timeout`).
#[inline]
pub fn until_changed(word: &AtomicU32, old: u32, timeout: Duration) {
    imp(word, old, timeout);
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn imp(word: &AtomicU32, old: u32, timeout: Duration) {
    let ticks = u32::try_from(timeout.as_nanos() as u64 * ticks_per_us() / 1000)
        .unwrap_or(u32::MAX)
        .max(1);
    let p = word.as_ptr();
    // SAFETY: MONITORX (0f 01 fa) arms the monitor on the line holding
    // `p`, a valid address for as long as `word` is borrowed; ECX and EDX
    // (extensions, hints) are zero. It reads no memory and writes none.
    unsafe {
        std::arch::asm!(
            ".byte 0x0f, 0x01, 0xfa",
            in("rax") p,
            in("ecx") 0u32,
            in("edx") 0u32,
            options(nostack, preserves_flags),
        );
    }
    // A write between the caller's load and the monitor being armed would
    // be missed: look once more.
    if word.load(Ordering::Acquire) != old {
        return;
    }
    // SAFETY: MWAITX (0f 01 fb) waits until the monitored line is written,
    // an interrupt, or the timer: EAX 0xf0 (no C-state, as Linux's
    // `delay_mwaitx`), ECX bit 1 (EBX holds the timer, in TSC ticks).
    // EBX is reserved by the compiler, so it is swapped in and back.
    unsafe {
        std::arch::asm!(
            "xchg {t:r}, rbx",
            ".byte 0x0f, 0x01, 0xfb",
            "xchg {t:r}, rbx",
            t = inout(reg) u64::from(ticks) => _,
            in("eax") 0xf0u32,
            in("ecx") 2u32,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn imp(word: &AtomicU32, old: u32, _timeout: Duration) {
    let v: u32;
    // SAFETY: a load-acquire exclusive of a valid, aligned u32 (`word` is
    // borrowed): it reads the word and arms this core's exclusive monitor
    // on its line, so another core's write to the line sends an event.
    unsafe {
        std::arch::asm!(
            "ldaxr {v:w}, [{p}]",
            v = out(reg) v,
            p = in(reg) word.as_ptr(),
            options(nostack, preserves_flags),
        );
    }
    if v != old {
        // SAFETY: CLREX only clears this core's exclusive monitor.
        unsafe { std::arch::asm!("clrex", options(nomem, nostack, preserves_flags)) };
        return;
    }
    // SAFETY: WFE waits for an event: the monitor cleared by a write to
    // the line, the event stream, an interrupt. It touches no memory.
    unsafe { std::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) };
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn imp(_word: &AtomicU32, _old: u32, _timeout: Duration) {
    std::hint::spin_loop();
}

/// How long one [`until_changed`] took when nothing wrote the word: the
/// timer's or the event stream's granularity, for the README.
#[must_use]
pub fn idle_wait(timeout: Duration) -> Duration {
    let word = AtomicU32::new(0);
    let start = Instant::now();
    until_changed(&word, 0, timeout);
    start.elapsed()
}
