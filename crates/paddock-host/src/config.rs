//! Node-level runtime configuration.

use std::path::PathBuf;
use std::time::Duration;

use crate::transport::membership::Role;

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

/// Most host objects one instance may hold. These live in host memory,
/// outside `memory_max` (a pending timer holds its envelope), so each is
/// charged to the instance that creates it. Over a limit, the instance
/// crashes with `timer_limit`, `monitor_limit`, `link_limit` or
/// `pending_call_limit`; a `paddock:groups` join over its limit returns
/// `limit-reached`. Registered names need no limit: an instance may
/// hold only the names its manifest lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostObjects {
    /// Pending `send-after` timers.
    pub timers: u32,
    /// Monitors held on other instances.
    pub monitors: u32,
    /// Links, counted when the instance links again. Links others make to
    /// it are charged to them and never refused.
    pub links: u32,
    /// Calls answered `noreply` and not yet replied to.
    pub pending_calls: u32,
    /// Process groups joined (`paddock:groups`).
    pub groups: u32,
}

impl Default for HostObjects {
    /// 10,000 of each, the default `mailbox_max`, and 1,000 groups.
    fn default() -> Self {
        Self {
            timers: 10_000,
            monitors: 10_000,
            links: 10_000,
            pending_calls: 10_000,
            groups: 1_000,
        }
    }
}

impl HostObjects {
    /// What an instance asking for `requested` gets on a node whose limits
    /// are `self`: an unset field takes the node's value.
    ///
    /// # Errors
    ///
    /// A message naming the first field that asks for more than the node
    /// allows.
    pub fn grant(&self, requested: &HostObjectRequest) -> Result<Self, String> {
        let pick = |name: &str, asked: Option<u32>, node: u32| match asked {
            None => Ok(node),
            Some(n) if n <= node => Ok(n),
            Some(n) => Err(format!("{name} of {n} exceeds the node limit of {node}")),
        };
        Ok(Self {
            timers: pick("timers_max", requested.timers, self.timers)?,
            monitors: pick("monitors_max", requested.monitors, self.monitors)?,
            links: pick("links_max", requested.links, self.links)?,
            pending_calls: pick(
                "pending_calls_max",
                requested.pending_calls,
                self.pending_calls,
            )?,
            groups: pick("groups_max", requested.groups, self.groups)?,
        })
    }
}

/// The host-object limits an instance asks for (manifest `limits`); an
/// unset field takes the node's value from [`HostObjects`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostObjectRequest {
    /// `timers_max`.
    pub timers: Option<u32>,
    /// `monitors_max`.
    pub monitors: Option<u32>,
    /// `links_max`.
    pub links: Option<u32>,
    /// `pending_calls_max`.
    pub pending_calls: Option<u32>,
    /// `groups_max`.
    pub groups: Option<u32>,
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
    /// instances. A busy instance can keep its tokio worker for up to one
    /// tick, so the tick also bounds how long other tasks on that worker
    /// wait (`docs/BASELINE.md`, 2026-10-07).
    pub epoch_tick: Duration,
    /// Where node state lives (`disk` keyvalue buckets). `None` disables
    /// disk buckets.
    pub data_dir: Option<PathBuf>,
    /// How often this node tells each peer it is alive
    /// ([`crate::transport::liveness`]). Zero sends no heartbeats.
    pub heartbeat_interval: Duration,
    /// How long a peer may stay silent on a path before that path counts as
    /// silent; a silent cluster link ends the session with the peer.
    pub node_timeout: Duration,
    /// This start of the node, raised on every start and kept on disk by the
    /// node ([`crate::transport::membership`]). From 1 to
    /// [`crate::pid::MAX_CREATION`]; it is the high bits of every instance
    /// id.
    pub creation: u32,
    /// What this node does in the cluster.
    pub role: Role,
    /// Where other nodes reach this one; its meaning is the transport's.
    pub address: String,
    /// Host-object limits for instances whose manifest sets none, and the
    /// most any manifest may ask for.
    pub host_objects: HostObjects,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node: 0,
            max_instances: 1_000,
            memory_profile: MemoryProfile::Standard,
            epoch_tick: Duration::from_millis(2),
            data_dir: None,
            heartbeat_interval: Duration::from_secs(1),
            node_timeout: Duration::from_secs(5),
            creation: 1,
            role: Role::Member,
            address: String::new(),
            host_objects: HostObjects::default(),
        }
    }
}
