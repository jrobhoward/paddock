//! Node-level runtime configuration.

use std::path::PathBuf;
use std::time::Duration;

/// How much address space each linear memory reserves (spike S2).
///
/// Virtual address space, not RAM, limits how many instances a node can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MemoryProfile {
    /// wasmtime's default reservations (about 4 GiB plus guards per memory),
    /// so bounds checks are elided. Suits up to a few thousand instances.
    #[default]
    Standard,
    /// 128 MiB reservations with 64 KiB guards, for dense nodes (measured past
    /// 20,000 instances). Caps `limits.memory_max` at 128 MiB. Not measured
    /// with a memory-heavy guest, which pays for explicit bounds checks.
    Dense,
}

impl MemoryProfile {
    /// The largest `limits.memory_max` an instance may have.
    #[must_use]
    pub const fn memory_max(self) -> u64 {
        match self {
            Self::Standard => 4 << 30,
            Self::Dense => 128 << 20,
        }
    }
}

/// Configuration for a [`Runtime`](crate::Runtime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// This node's id, carried in every pid.
    pub node: u32,
    /// Most instances alive at once. Sizes the pooling allocator, including
    /// one fiber stack per instance.
    pub max_instances: u32,
    /// Address-space reservation per linear memory.
    pub memory_profile: MemoryProfile,
    /// Epoch tick: the granularity of deadlines and of preemption between
    /// instances.
    pub epoch_tick: Duration,
    /// Where node state lives (`disk` keyvalue buckets). `None` disables
    /// disk buckets.
    pub data_dir: Option<PathBuf>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node: 0,
            max_instances: 1_000,
            memory_profile: MemoryProfile::Standard,
            epoch_tick: Duration::from_millis(10),
            data_dir: None,
        }
    }
}
