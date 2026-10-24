//! Node-level runtime configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;

use paddock_bundle::manifest::Durability;
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
/// `pending_call_limit`; a `paddock:groups` join or a `paddock:watch` watch
/// over its limit returns `limit-reached`. Registered names need no limit: an instance may
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
    /// Keyvalue watches held (`paddock:watch`).
    pub watches: u32,
}

impl Default for HostObjects {
    /// 10,000 of each, the default `mailbox_max`, and 1,000 groups and
    /// watches.
    fn default() -> Self {
        Self {
            timers: 10_000,
            monitors: 10_000,
            links: 10_000,
            pending_calls: 10_000,
            groups: 1_000,
            watches: 1_000,
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
            watches: pick("watches_max", requested.watches, self.watches)?,
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
    /// `watches_max`.
    pub watches: Option<u32>,
}

/// A shared keyvalue bucket: named in node configuration, opened by every
/// app whose manifest lists it and whose grant includes it. Its scope is
/// this node (a `cluster` scope is Phase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedBucket {
    /// `Memory` or `Disk`; `Replicated` buckets fail every operation until
    /// Phase 3.
    pub durability: Durability,
    /// Most bytes the bucket may hold, across every app writing it.
    pub quota: Option<u64>,
}

/// Where a secret's value comes from. Read on every access.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretSource {
    /// A file's whole contents.
    File(PathBuf),
    /// An environment variable of the node process.
    Env(String),
}

/// A SQL datasource the node binds (`[datasources.<name>]`, ROADMAP E5):
/// a Postgres database, pooled. Apps name it; they never see its address
/// or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlDatasource {
    /// Host name or address; a path (starting `/`) is a Unix socket
    /// directory.
    pub host: String,
    /// TCP port, or the Unix socket's number.
    pub port: u16,
    /// Database name.
    pub dbname: String,
    /// Database user.
    pub user: String,
    /// Where the password comes from (a node secret's source); `None` for
    /// none (trust or peer authentication).
    pub password: Option<SecretSource>,
    /// TLS to the server.
    pub tls: SqlTls,
    /// Most connections the node opens to it, for every app together.
    pub pool_size: usize,
    /// Time allowed to open a connection.
    pub connect_timeout: Duration,
    /// Time a call waits for a connection (the app's cap or the pool) before
    /// `limit-reached`.
    pub acquire_timeout: Duration,
    /// The server cancels a statement that runs longer (`timeout`).
    pub statement_timeout: Duration,
    /// Largest result a call returns, in bytes (`too-large` above it).
    pub max_result_bytes: u64,
}

impl SqlDatasource {
    /// A datasource at `host` with the defaults: port 5432, no password,
    /// no TLS, 10 connections, 5 s to connect and to acquire, statements
    /// cancelled after 30 s, results up to 16 MiB.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        dbname: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port: 5432,
            dbname: dbname.into(),
            user: user.into(),
            password: None,
            tls: SqlTls::Disable,
            pool_size: 10,
            connect_timeout: Duration::from_secs(5),
            acquire_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(30),
            max_result_bytes: 16 << 20,
        }
    }
}

/// TLS to a SQL datasource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlTls {
    /// Plain connections: a Unix socket or a trusted network.
    Disable,
    /// TLS, verifying the server against the CA certificates in this PEM
    /// file (no system roots: the operator names whom to trust).
    Require {
        /// PEM file of CA certificates.
        ca_file: PathBuf,
    },
}

/// Configuration for a [`Runtime`](crate::Runtime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// This node's id, carried in every pid.
    pub node: u32,
    /// Most instances alive at once. Sizes the pooling allocator, including
    /// one fiber stack per instance. With passivation
    /// ([`crate::InstanceSpec::passivate_after`]) it counts instances that
    /// hold a Store; a passivated one holds none.
    pub max_instances: u32,
    /// Most instances in the process table, passivated or not; `None` is
    /// `max_instances`. Above `max_instances` only with passivation: an
    /// instance starting or waking while every Store slot is taken waits
    /// for one (spike S11).
    pub max_processes: Option<u32>,
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
    /// Shared keyvalue buckets, by name.
    pub buckets: BTreeMap<String, SharedBucket>,
    /// Secrets, by name.
    pub secrets: BTreeMap<String, SecretSource>,
    /// SQL datasources, by name (ROADMAP E5).
    pub datasources: BTreeMap<String, SqlDatasource>,
    /// HTTP egress for every app.
    pub egress: Egress,
    /// Most log records per second per app, `wasi:logging` records and
    /// captured stdout and stderr lines together; an app's own
    /// `rate_limit_per_sec` can only lower it. 0 for no ceiling.
    pub log_records_per_sec: u32,
    /// Most calls from one other node handled at once; a call beyond it is
    /// answered `overloaded` at once. An honest peer is bounded by its
    /// instances' `pending_calls_max`; this bounds a faulty one. 0 for no
    /// limit.
    pub incoming_calls_per_peer: u32,
    /// Native-process apps (ROADMAP N2).
    pub native: NativeConfig,
}

/// How a node runs native-process apps.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeConfig {
    /// The `paddock-runner` executable; `None` for the one beside the
    /// node's own executable.
    pub runner: Option<PathBuf>,
    /// A cgroup v2 directory delegated to the node (systemd's
    /// `Delegate=yes`). With it, the node moves itself into its `node`
    /// child and runs each native instance in a cgroup of its own under
    /// `native`, with `memory.max` from the app's `memory_max`, no swap,
    /// and `pids.max`. Without it, `memory_max` is the process's
    /// `RLIMIT_DATA`.
    pub cgroup: Option<PathBuf>,
    /// Most processes and threads in one native instance (with a cgroup).
    pub pids_max: u32,
    /// How long an idle native instance goes between pings.
    pub ping_interval: Duration,
    /// How long a ping may go unanswered before the instance is killed
    /// (`timeout`).
    pub ping_timeout: Duration,
    /// Whether to run native apps on a kernel without Landlock (which the
    /// runner then cannot restrict to its files): off by default.
    pub landlock_optional: bool,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            runner: None,
            cgroup: None,
            pids_max: 64,
            ping_interval: Duration::from_secs(10),
            ping_timeout: Duration::from_secs(5),
            landlock_optional: false,
        }
    }
}

/// HTTP egress (`wasi:http/outgoing-handler`, ROADMAP E2): what every
/// app's outgoing requests share. Each app may reach only what its
/// manifest's `allow` lists, within [`Egress::allow`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Egress {
    /// The most any app may be allowed, in the manifest's `allow` syntax;
    /// `None` for no ceiling. A deploy whose manifest asks for more is
    /// refused (`services::egress::within`).
    pub allow: Option<Vec<String>>,
    /// Trust the operating system's root certificates.
    pub system_roots: bool,
    /// More trusted certificate authorities, PEM.
    pub ca_file: Option<PathBuf>,
    /// Allow host names that resolve to loopback or link-local addresses
    /// (`169.254.0.0/16`, `fe80::/10`: cloud metadata). An address written
    /// in an allow entry is always allowed.
    pub allow_loopback: bool,
    /// Time allowed to open a connection.
    pub connect_timeout: Duration,
    /// Most time from sending a request to its response headers; a guest's
    /// `first-byte-timeout` may only shorten it.
    pub first_byte_timeout: Duration,
    /// Most time between two chunks of a response body; a guest's
    /// `between-bytes-timeout` may only shorten it.
    pub between_bytes_timeout: Duration,
    /// How long an idle pooled connection is kept.
    pub pool_idle_timeout: Duration,
    /// Idle connections kept per host.
    pub pool_max_idle_per_host: usize,
    /// Requests in flight at once per app, when its manifest does not set
    /// `max_concurrent`.
    pub max_concurrent: u32,
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            allow: None,
            system_roots: true,
            ca_file: None,
            allow_loopback: false,
            connect_timeout: Duration::from_secs(10),
            first_byte_timeout: Duration::from_secs(30),
            between_bytes_timeout: Duration::from_secs(30),
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 32,
            max_concurrent: 64,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node: 0,
            max_instances: 1_000,
            max_processes: None,
            memory_profile: MemoryProfile::Standard,
            epoch_tick: Duration::from_millis(2),
            data_dir: None,
            heartbeat_interval: Duration::from_secs(1),
            node_timeout: Duration::from_secs(5),
            creation: 1,
            role: Role::Member,
            address: String::new(),
            host_objects: HostObjects::default(),
            buckets: BTreeMap::new(),
            secrets: BTreeMap::new(),
            datasources: BTreeMap::new(),
            egress: Egress::default(),
            log_records_per_sec: 1_000,
            incoming_calls_per_peer: 10_000,
            native: NativeConfig::default(),
        }
    }
}
