//! Giving free Store slots' memory back to the kernel once instance starts
//! have been quiet.
//!
//! A freed slot keeps up to `keep_resident` of the pages its last instance
//! wrote, zeroed, so the next instance on it takes no page faults for them
//! (`docs/BASELINE.md`, 2026-10-24). After a burst those pages stay
//! resident while nothing uses them. Once no Store has been taken for
//! [`RuntimeConfig::release_idle_after`](crate::RuntimeConfig::release_idle_after),
//! wasmtime's `Engine::release_idle_pool_memory` decommits them.
//!
//! wasmtime 50 decommits several slots with one `process_madvise` call on
//! the calling process's `PIDFD_SELF`, which Linux has only from 6.13; an
//! older kernel answers EBADF, which wasmtime does not take for "not
//! supported", so the release frees nothing unless one slot is warm (the
//! slots are left as they were). The task then tries once per quiet period,
//! not at every check.
//!
//! While wasmtime releases, the warm slots it works on cannot be allocated,
//! so a start at that moment could find the pool full. The release holds
//! every free permit of [`RuntimeInner::slots`] for its duration instead:
//! a start waits for it (a few milliseconds) rather than failing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crate::runtime::RuntimeInner;

/// How often the task looks at the pool.
const CHECK_EVERY: Duration = Duration::from_secs(10);

/// When Stores were last taken, and what releasing has given back.
#[derive(Debug)]
pub(crate) struct PoolMemory {
    base: Instant,
    /// Milliseconds after `base` when a Store slot was last taken.
    last_taken_ms: AtomicU64,
    /// Milliseconds after `base` of the task's last release; it tries once
    /// per quiet period, so a release that frees nothing is not repeated
    /// every check.
    tried_ms: AtomicU64,
    /// Bytes given back to the kernel since the runtime started.
    released: AtomicU64,
    started: AtomicBool,
}

impl Default for PoolMemory {
    fn default() -> Self {
        Self {
            base: Instant::now(),
            last_taken_ms: AtomicU64::new(0),
            tried_ms: AtomicU64::new(0),
            released: AtomicU64::new(0),
            started: AtomicBool::new(false),
        }
    }
}

impl PoolMemory {
    /// A Store slot was taken: a start, a wake, an upgrade or an HTTP
    /// handler's request.
    pub(crate) fn taken(&self) {
        self.last_taken_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    fn now_ms(&self) -> u64 {
        // At least 1, so a slot taken at once is later than no try at all.
        u64::try_from(self.base.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    /// Whether the task tried since the last Store was taken.
    fn tried_this_quiet(&self) -> bool {
        self.tried_ms.load(Ordering::Relaxed) >= self.last_taken_ms.load(Ordering::Relaxed)
    }

    fn quiet_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_taken_ms.load(Ordering::Relaxed));
        self.base.elapsed().saturating_sub(last)
    }

    /// Bytes released since the runtime started.
    pub(crate) fn released(&self) -> u64 {
        self.released.load(Ordering::Relaxed)
    }
}

/// Start the release task, once per runtime; nothing with
/// `release_idle_after` unset. Called from a tokio context.
pub(crate) fn start_once(rt: &Arc<RuntimeInner>) {
    let Some(after) = rt.config().release_idle_after else {
        return;
    };
    if rt.pool_memory.started.swap(true, Ordering::Relaxed) {
        return;
    }
    tokio::spawn(run(Arc::downgrade(rt), after));
}

async fn run(rt: Weak<RuntimeInner>, after: Duration) {
    let mut ticker = tokio::time::interval(CHECK_EVERY.min(after));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(rt) = rt.upgrade() else {
            return;
        };
        let pool = &rt.pool_memory;
        if pool.quiet_for() < after || pool.tried_this_quiet() || resident(&rt) == 0 {
            continue;
        }
        pool.tried_ms.store(pool.now_ms(), Ordering::Relaxed);
        let bytes = release(&rt).await;
        if bytes > 0 {
            tracing::info!(
                released_bytes = bytes,
                quiet_s = after.as_secs(),
                "released free Store slots' memory"
            );
        }
    }
}

/// Bytes free slots keep resident now.
fn resident(rt: &RuntimeInner) -> usize {
    rt.engine
        .pooling_allocator_metrics()
        .map_or(0, |m| m.unused_memory_bytes_resident())
}

/// Give free slots' resident memory back, holding every free slot permit
/// meanwhile so no Store starts during it. Returns the bytes released, 0
/// if the free permits changed under it (it tries again next time).
pub(crate) async fn release(rt: &Arc<RuntimeInner>) -> u64 {
    let free = u32::try_from(rt.slots.available_permits()).unwrap_or(u32::MAX);
    let Ok(permits) = rt.slots.clone().try_acquire_many_owned(free) else {
        return 0;
    };
    let engine = rt.engine.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let bytes = engine.release_idle_pool_memory();
        drop(permits);
        bytes
    })
    .await
    .unwrap_or(0);
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    rt.pool_memory.released.fetch_add(bytes, Ordering::Relaxed);
    bytes
}

/// The pool's node-wide metrics, in the Prometheus text format.
pub(crate) fn render(rt: &RuntimeInner, out: &mut String) {
    use std::fmt::Write as _;
    let Some(m) = rt.engine.pooling_allocator_metrics() else {
        return;
    };
    let gauges = [
        (
            "paddock_pool_live_memories",
            "Linear memories in use: one per instance holding a Store.",
            m.memories() as u64,
        ),
        (
            "paddock_pool_warm_slots",
            "Free Store slots keeping pages resident for the next instance.",
            u64::from(m.unused_warm_memories()),
        ),
        (
            "paddock_pool_resident_bytes",
            "Bytes free Store slots keep resident (keep_resident).",
            m.unused_memory_bytes_resident() as u64,
        ),
    ];
    for (name, help, value) in gauges {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = writeln!(out, "{name} {value}");
    }
    let name = "paddock_pool_released_bytes_total";
    let _ = writeln!(
        out,
        "# HELP {name} Bytes of free Store slots' memory given back to the kernel (release_idle_after)."
    );
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {}", rt.pool_memory.released());
}

#[cfg(test)]
#[path = "pool_memory_tests.rs"]
mod pool_memory_tests;
