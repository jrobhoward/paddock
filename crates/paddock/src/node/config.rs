//! Node configuration (TOML).
//!
//! ```toml
//! node = 1
//! data_dir = "/var/lib/paddock"
//! # admin_socket = "/var/lib/paddock/admin.sock"   (the default)
//! trusted_keys = ["RWQ..."]
//! policy = "/etc/paddock/policy.toml"
//! max_instances = 1000           # instances holding a Store at once
//! max_processes = 20000          # all instances, passivated ones too
//! max_core_instances_per_component = 8   # 14 for componentize-py apps
//! memory_profile = "standard"   # or "dense"
//! keep_resident = "16MiB"       # per free instance slot; profile's default
//! release_idle_after_s = 300    # give those pages back after this long with no
//!                               # instance start; 0: never
//!
//! [volumes]
//! orders-data = "/srv/paddock/volumes/orders-data"
//!
//! # An extension's own configuration, by package.
//! [extensions."paddock-modbus:modbus".devices.pump-1]
//! address = "10.0.0.5:502"
//!
//! # Secrets apps read by name (policy grants each): a file or a variable.
//! [secrets.db-password]
//! file = "/run/secrets/db-password"
//!
//! # Shared keyvalue buckets, opened by apps whose manifest lists them
//! # and whose policy entry grants them.
//! [buckets.points]
//! durability = "disk"           # or "memory"
//! quota = "256MiB"
//! scope = "node"                # or "cluster": replicated on an HA pair
//!                               # (durability "disk", [failover] with
//!                               # arbiter "witness"; both nodes the same)
//!
//! # Node-level config overrides, per app, on top of the manifest defaults.
//! # HTTP ingress: every app's `wasi:http/incoming-handler` routes, on
//! # each address.
//! [http.main]
//! address = "0.0.0.0:8080"
//! max_connections = 1024        # more are closed on accept
//! header_timeout_ms = 10000     # slow clients are dropped
//! body_timeout_ms = 30000       # then 408
//! max_response = "16MiB"        # larger app replies become 502
//! tls = [{ cert = "/etc/paddock/shop.pem", key = "/etc/paddock/shop.key", hosts = ["shop.example"] }]
//! tls_reload_s = 60             # check the files for changes; also SIGHUP
//! # With [failover], what the standby does with a request for a singleton
//! # that runs on the primary: "refuse" (503, for load balancers),
//! # "redirect" (307 to partner_url) or "forward" (over the cluster to the
//! # primary's app). Default: "redirect" with partner_url, else "refuse".
//! standby = "forward"
//! partner_url = "https://node2.example:8443"  # the partner's listener
//!
//! # The admin API for other machines: TLS 1.3, pinned keys both ways.
//! [admin]
//! address = "10.0.0.1:7443"
//! operators = "/etc/paddock/operators.toml"  # [operators] alice = ["ed25519:..."]
//! # key = "/etc/paddock/node.key"           # default: [cluster] key
//!
//! # Prometheus scrape endpoint (GET /metrics). Unauthenticated.
//! [metrics]
//! address = "127.0.0.1:9100"
//!
//! [config.orders]
//! "db.host" = "pg.internal"
//!
//! [cluster]
//! role = "member"               # or "witness": runs no apps
//! heartbeat_interval_ms = 1000
//! node_timeout_ms = 5000
//! address = "10.0.0.1:7400"     # UDP, QUIC between nodes
//! seeds = ["10.0.0.2:7400"]
//! key = "/etc/paddock/node.key" # paddock cluster keygen
//! peers = "/etc/paddock/peers.toml"
//! send_buffer_bytes = 67108864  # messages queued per peer; more: overloaded
//!
//! # One node of an HA pair: `singleton` apps run only on the primary;
//! # `hot-standby` apps run on both and are told the role.
//! [failover]
//! partner = 2
//! arbiter = "witness"           # or "paths", "operator"
//! witness = 3                   # the witness's node id, with "witness"
//! lease_ms = 3000
//! margin_ms = 500               # less than half of lease_ms
//! # A second heartbeat path to the partner over a null-modem cable (Unix
//! # only; the node's user needs the device's group, `dialout` on most
//! # Linux systems). Both nodes set the same baud.
//! serial = { device = "/dev/ttyUSB0", baud = 115200 }
//!
//! # On the witness (cluster role "witness") instead:
//! # [witness]
//! # pair = [1, 2]
//! # first = 1                   # primary at epoch 1
//! # lease_ms = 3000             # as on the pair
//! # margin_ms = 500
//!
//! # Host objects per instance: the value for apps that set none, and the
//! # most a manifest may ask for.
//! [limits]
//! timers_max = 10000
//! monitors_max = 10000
//! links_max = 10000
//! pending_calls_max = 10000
//! groups_max = 1000
//! watches_max = 1000
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use paddock_bundle::manifest::{ByteSize, Durability};
use serde::Deserialize;

use super::error::NodeError;

/// Address-space profile, as in the host's `MemoryProfile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryProfile {
    /// Large reservations; up to a few thousand instances.
    #[default]
    Standard,
    /// 128 MiB reservations; dense nodes.
    Dense,
}

/// The node's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Node id, carried in every pid.
    #[serde(default)]
    pub node: u32,
    /// Where the deploy store, keyvalue data and audit log live.
    pub data_dir: PathBuf,
    /// The admin API socket. Defaults to `<data_dir>/admin.sock`.
    #[serde(default)]
    pub admin_socket: Option<PathBuf>,
    /// Minisign public keys whose signatures the node accepts.
    #[serde(default)]
    pub trusted_keys: Vec<String>,
    /// Operator policy file (TOML). Without one, nothing beyond the
    /// implicit interfaces is granted.
    #[serde(default)]
    pub policy: Option<PathBuf>,
    /// Most instances holding a Store at once: the pooling allocator's
    /// slots, each reserving address space.
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
    /// Most instances in all, passivated ones included (apps with
    /// `instances.passivate_after_ms`); at least `max_instances`. Unset:
    /// `max_instances`.
    #[serde(default)]
    pub max_processes: Option<u32>,
    /// Most core module instances one component may contain (the pooling
    /// allocator's limit); at least 1. Python components built with
    /// componentize-py need 14.
    #[serde(default = "default_max_core_instances_per_component")]
    pub max_core_instances_per_component: u32,
    /// Address-space profile.
    #[serde(default)]
    pub memory_profile: MemoryProfile,
    /// Memory a free instance slot keeps resident for the next instance;
    /// unset: 16 MiB with the standard profile, none with dense.
    #[serde(default)]
    pub keep_resident: Option<ByteSize>,
    /// Seconds with no Store taken (a start, wake, upgrade, connection or
    /// HTTP handler request) after which free slots' kept pages go back to
    /// the kernel; 0 never.
    #[serde(default = "default_release_idle_after_s")]
    pub release_idle_after_s: u64,
    /// Epoch tick, in milliseconds.
    #[serde(default = "default_tick")]
    pub epoch_tick_ms: u64,
    /// How the node writes its log to stderr.
    #[serde(default)]
    pub log_format: LogFormat,
    /// Host directories backing volume names in manifests.
    #[serde(default)]
    pub volumes: BTreeMap<String, PathBuf>,
    /// Shared keyvalue buckets, by name.
    #[serde(default)]
    pub buckets: BTreeMap<String, BucketConfig>,
    /// Secrets, by name: where each value comes from.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretBinding>,
    /// SQL datasources apps may use, by name (`paddock:sql`).
    #[serde(default)]
    pub datasources: BTreeMap<String, DatasourceBinding>,
    /// Each extension's node configuration, by package
    /// (`[extensions."paddock-modbus:modbus"]`), handed to the extension as
    /// it starts. A table for an extension the node does not have refuses
    /// the start.
    #[serde(default)]
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Config overrides per app.
    #[serde(default)]
    pub config: BTreeMap<String, BTreeMap<String, String>>,
    /// Addresses for the listeners apps declare, by listener name.
    #[serde(default)]
    pub listeners: BTreeMap<String, ListenerBinding>,
    /// HTTP ingress addresses, by a name of the operator's choosing. Every
    /// address serves every app's routes.
    #[serde(default)]
    pub http: BTreeMap<String, HttpBinding>,
    /// Where Prometheus scrapes the node's metrics.
    #[serde(default)]
    pub metrics: Option<MetricsBinding>,
    /// HTTP requests from apps (`wasi:http/outgoing-handler`): the most any
    /// app may reach, trusted roots, timeouts and the connection pool.
    #[serde(default)]
    pub egress: EgressBinding,
    /// TCP and UDP from apps (`wasi:sockets`): the most any app may reach,
    /// bind and listen on. Unset: no ceiling.
    #[serde(default)]
    pub sockets: Option<SocketsCeiling>,
    /// Native-process apps: the runner, the cgroup and the pings.
    #[serde(default)]
    pub native: NativeBinding,
    /// The admin API over TLS, for operators on other machines.
    #[serde(default)]
    pub admin: Option<AdminBinding>,
    /// This node's part in a cluster.
    #[serde(default)]
    pub cluster: ClusterConfig,
    /// This node is one of an HA pair.
    #[serde(default)]
    pub failover: Option<FailoverBinding>,
    /// This node is the witness for an HA pair.
    #[serde(default)]
    pub witness: Option<WitnessBinding>,
    /// Host-object limits per instance.
    #[serde(default)]
    pub limits: NodeLimits,
}

/// How a node writes its log to stderr: `text` for people (colour only on
/// a terminal) or `json`, one object per line, for collectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

/// The `[limits]` section: host objects per instance, as the manifest's
/// `limits` fields of the same names. Each is the value for apps that set
/// none and the most a manifest may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeLimits {
    /// Pending `send-after` timers.
    #[serde(default = "default_host_objects")]
    pub timers_max: u32,
    /// Monitors held on other instances.
    #[serde(default = "default_host_objects")]
    pub monitors_max: u32,
    /// Links.
    #[serde(default = "default_host_objects")]
    pub links_max: u32,
    /// Calls answered `noreply` and not yet replied to.
    #[serde(default = "default_host_objects")]
    pub pending_calls_max: u32,
    /// Process groups joined.
    #[serde(default = "default_groups")]
    pub groups_max: u32,
    /// Keyvalue watches held.
    #[serde(default = "default_groups")]
    pub watches_max: u32,
    /// Log records per second per app, `wasi:logging` records and captured
    /// stdout and stderr lines together. An app's `rate_limit_per_sec` can
    /// only lower it; 0 for no ceiling.
    #[serde(default = "default_log_records")]
    pub log_records_per_sec: u32,
}

impl Default for NodeLimits {
    fn default() -> Self {
        Self {
            timers_max: default_host_objects(),
            monitors_max: default_host_objects(),
            links_max: default_host_objects(),
            pending_calls_max: default_host_objects(),
            groups_max: default_groups(),
            watches_max: default_groups(),
            log_records_per_sec: default_log_records(),
        }
    }
}

fn default_log_records() -> u32 {
    1_000
}

fn default_groups() -> u32 {
    1_000
}

fn default_host_objects() -> u32 {
    10_000
}

/// A `[buckets.<name>]` entry: a shared keyvalue bucket.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketConfig {
    /// `memory` or `disk`.
    #[serde(default)]
    pub durability: Durability,
    /// Most bytes the bucket holds, across every app writing it.
    #[serde(default)]
    pub quota: Option<ByteSize>,
    /// Where the bucket's data lives: this node, or both nodes of the HA
    /// pair, replicated.
    #[serde(default)]
    pub scope: BucketScope,
}

/// A `[secrets.<name>]` entry: exactly one of `file` and `env`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretBinding {
    /// A file whose whole contents are the value (mode 0600, owned by the
    /// node's user, is the operator's to set).
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// An environment variable of the node process.
    #[serde(default)]
    pub env: Option<String>,
}

/// A SQL datasource: `[datasources.<name>]`, a Postgres database the node
/// pools connections to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasourceBinding {
    /// Host name or address; a path (starting `/`) is a Unix socket
    /// directory.
    pub host: String,
    /// TCP port, or the Unix socket's number.
    #[serde(default = "default_pg_port")]
    pub port: u16,
    /// Database name.
    pub dbname: String,
    /// Database user.
    pub user: String,
    /// The node secret (`[secrets.<name>]`) holding the password; unset
    /// for none.
    #[serde(default)]
    pub password_secret: Option<String>,
    /// `"require"` (with `ca_file`) or `"disable"`.
    #[serde(default)]
    pub tls: DatasourceTls,
    /// PEM file of the CA certificates the server must chain to.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Most connections the node opens, for every app together.
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    /// Time allowed to open a connection.
    #[serde(default = "default_sql_wait_ms")]
    pub connect_timeout_ms: u64,
    /// Time a call waits for a connection before `limit-reached`.
    #[serde(default = "default_sql_wait_ms")]
    pub acquire_timeout_ms: u64,
    /// The server cancels a statement that runs longer.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u64,
    /// Largest result a call returns.
    #[serde(default = "default_max_result")]
    pub max_result_bytes: ByteSize,
}

/// TLS to a datasource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasourceTls {
    /// Plain: a Unix socket or a trusted network.
    #[default]
    Disable,
    /// TLS, verified against `ca_file`.
    Require,
}

fn default_pg_port() -> u16 {
    5432
}

fn default_pool_size() -> usize {
    10
}

fn default_sql_wait_ms() -> u64 {
    5_000
}

fn default_statement_timeout_ms() -> u64 {
    30_000
}

fn default_max_result() -> ByteSize {
    ByteSize::mib(16)
}

/// Where a shared bucket's data lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketScope {
    /// One copy on this node.
    #[default]
    Node,
    /// Replicated across the cluster. Refused until Phase 3.
    Cluster,
}

/// What a node does in the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterRole {
    /// Runs apps.
    #[default]
    Member,
    /// Runs no apps: a third node for failover decisions.
    Witness,
}

/// The `[cluster]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// What this node does.
    #[serde(default)]
    pub role: ClusterRole,
    /// How often this node tells each peer it is alive, in milliseconds.
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_interval_ms: u64,
    /// Silence after which a peer's session ends, in milliseconds. More
    /// than `heartbeat_interval_ms`.
    #[serde(default = "default_node_timeout_ms")]
    pub node_timeout_ms: u64,
    /// The UDP address (`ip:port`) this node listens on for other nodes
    /// and, without `advertise`, tells them to reach it at. Unset runs no
    /// cluster transport.
    #[serde(default)]
    pub address: Option<String>,
    /// The address (`ip:port`) other nodes reach this node at, when it is
    /// not `address`: behind NAT, port forwarding or a proxy. Needs
    /// `address`.
    #[serde(default)]
    pub advertise: Option<String>,
    /// Addresses (`ip:port`) of nodes to join through; each is redialed
    /// whenever its connection is lost.
    #[serde(default)]
    pub seeds: Vec<String>,
    /// This node's private key (`paddock cluster keygen`). Required with
    /// `address`. Refused if its group or others may access it.
    #[serde(default)]
    pub key: Option<PathBuf>,
    /// The peers file: every node's public key by node id. Required with
    /// `address`.
    #[serde(default)]
    pub peers: Option<PathBuf>,
    /// How often to check the key and peers files for changes and reload
    /// them, in seconds; 0 never (SIGHUP and `paddock reload-tls` still
    /// do).
    #[serde(default = "default_identity_reload_s")]
    pub identity_reload_s: u64,
    /// Most calls from one other node handled at once; beyond it a call
    /// is answered `overloaded`. 0 for no limit.
    #[serde(default = "default_incoming_calls_max")]
    pub incoming_calls_max: u32,
    /// Bytes of messages (casts, calls, replies) queued for one other
    /// node before more are refused: a cast is dropped and counted, a call
    /// fails with `overloaded`, as at a full mailbox. Link and monitor
    /// notices are queued past it.
    #[serde(default = "default_send_buffer_bytes")]
    pub send_buffer_bytes: u64,
}

fn default_send_buffer_bytes() -> u64 {
    paddock_host::transport::quic::SEND_BUFFER_BYTES
}

fn default_incoming_calls_max() -> u32 {
    10_000
}

fn default_identity_reload_s() -> u64 {
    60
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            role: ClusterRole::default(),
            heartbeat_interval_ms: default_heartbeat_ms(),
            node_timeout_ms: default_node_timeout_ms(),
            address: None,
            advertise: None,
            seeds: Vec::new(),
            key: None,
            peers: None,
            identity_reload_s: default_identity_reload_s(),
            incoming_calls_max: default_incoming_calls_max(),
            send_buffer_bytes: default_send_buffer_bytes(),
        }
    }
}

fn default_release_idle_after_s() -> u64 {
    300
}

fn default_heartbeat_ms() -> u64 {
    1_000
}

fn default_node_timeout_ms() -> u64 {
    5_000
}

/// Who promotes the standby, in `[failover]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArbiterKind {
    /// The witness node grants promotions; leases are used.
    Witness,
    /// The standby promotes itself when its partner is silent on every path.
    Paths,
    /// The standby promotes itself when an operator confirms
    /// (`paddock failover confirm`).
    Operator,
}

/// The `[failover]` section: this node and `partner` are an HA pair.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailoverBinding {
    /// The other node of the pair.
    pub partner: u32,
    /// Who promotes the standby.
    pub arbiter: ArbiterKind,
    /// The witness's node id, with `arbiter = "witness"`.
    #[serde(default)]
    pub witness: Option<u32>,
    /// How long a renewal keeps the primary's lease, in milliseconds.
    #[serde(default = "default_lease_ms")]
    pub lease_ms: u64,
    /// How long before its lease ends a primary stops its singletons, in
    /// milliseconds. Less than half of `lease_ms`.
    #[serde(default = "default_margin_ms")]
    pub margin_ms: u64,
    /// A serial line to the partner, a second heartbeat path.
    #[serde(default)]
    pub serial: Option<SerialBinding>,
    /// Run replication for apps' `replicated` buckets. Implied by a
    /// `cluster` shared bucket; needs `arbiter = "witness"`. Set alike on
    /// both nodes. Off, a pair fails over without replicating.
    #[serde(default)]
    pub replication: bool,
    /// With replication: the most the replicated log keeps, in bytes,
    /// and its oldest entry's age in seconds. A partner back from an outage
    /// within both catches up from the log, past them from a snapshot.
    #[serde(default = "default_replication_log_max")]
    pub replication_log_max: ByteSize,
    /// See `replication_log_max`.
    #[serde(default = "default_replication_log_age_s")]
    pub replication_log_age_s: u64,
}

fn default_replication_log_max() -> ByteSize {
    ByteSize::mib(64)
}

fn default_replication_log_age_s() -> u64 {
    3600
}

/// `[failover] serial`: the device of a serial line to the partner.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialBinding {
    /// The device (`/dev/ttyUSB0`).
    pub device: PathBuf,
    /// Bits per second; the partner's must match.
    #[serde(default = "default_baud")]
    pub baud: u32,
}

fn default_baud() -> u32 {
    115_200
}

/// The `[witness]` section: this node arbitrates an HA pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WitnessBinding {
    /// The pair's node ids.
    pub pair: [u32; 2],
    /// The node primary at epoch 1.
    pub first: u32,
    /// As the pair's `lease_ms`.
    #[serde(default = "default_lease_ms")]
    pub lease_ms: u64,
    /// As the pair's `margin_ms`.
    #[serde(default = "default_margin_ms")]
    pub margin_ms: u64,
}

fn default_lease_ms() -> u64 {
    3_000
}

fn default_margin_ms() -> u64 {
    500
}

/// Where a listener an app declares listens on this node.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerBinding {
    /// The address to bind, `ip:port`.
    pub address: std::net::SocketAddr,
    /// Tasks accepting connections at once.
    #[serde(default = "default_acceptors")]
    pub acceptors: usize,
}

/// `[admin]`: the admin API over TCP with TLS 1.3 and pinned keys, for
/// operators on other machines. The node presents `key`'s
/// public key; an operator connects only with a key the operators file
/// lists, and has every right the admin socket gives.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminBinding {
    /// The TCP address to listen on, `ip:port`.
    pub address: std::net::SocketAddr,
    /// The operators file: each operator's name and public keys
    /// (`[operators]`, `name = ["ed25519:..."]`). Read again on SIGHUP and
    /// `paddock reload-tls`.
    pub operators: PathBuf,
    /// The node's private key; `[cluster] key` if unset.
    #[serde(default)]
    pub key: Option<PathBuf>,
}

/// `[egress]`: HTTP requests from apps. Each app reaches
/// only what its manifest's `allow` lists, inside `allow` here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressBinding {
    /// The most any app may be allowed, in the manifest's syntax
    /// (`https://api.example.com`, `*.internal`, `http://10.0.0.5:8080`).
    /// Unset: no ceiling. A deploy asking for more is refused.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    /// Trust the operating system's root certificates.
    #[serde(default = "yes")]
    pub system_roots: bool,
    /// More trusted certificate authorities (PEM).
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Let host names that resolve to loopback or link-local addresses
    /// through (refused by default; an address in an allow entry is
    /// always reached).
    #[serde(default)]
    pub allow_loopback: bool,
    /// Time to open a connection.
    #[serde(default = "default_egress_connect_ms")]
    pub connect_timeout_ms: u64,
    /// Most time to a response's headers.
    #[serde(default = "default_egress_wait_ms")]
    pub first_byte_timeout_ms: u64,
    /// Most time between chunks of a response body.
    #[serde(default = "default_egress_wait_ms")]
    pub between_bytes_timeout_ms: u64,
    /// How long an idle pooled connection is kept.
    #[serde(default = "default_egress_idle_ms")]
    pub pool_idle_timeout_ms: u64,
    /// Idle connections kept per host.
    #[serde(default = "default_egress_idle_per_host")]
    pub pool_max_idle_per_host: usize,
    /// Requests in flight per app when its manifest sets no
    /// `max_concurrent`.
    #[serde(default = "default_egress_concurrent")]
    pub max_concurrent: u32,
}

impl Default for EgressBinding {
    fn default() -> Self {
        Self {
            allow: None,
            system_roots: true,
            ca_file: None,
            allow_loopback: false,
            connect_timeout_ms: default_egress_connect_ms(),
            first_byte_timeout_ms: default_egress_wait_ms(),
            between_bytes_timeout_ms: default_egress_wait_ms(),
            pool_idle_timeout_ms: default_egress_idle_ms(),
            pool_max_idle_per_host: default_egress_idle_per_host(),
            max_concurrent: default_egress_concurrent(),
        }
    }
}

impl EgressBinding {
    /// The runtime's egress settings.
    #[must_use]
    pub fn runtime(&self) -> paddock_host::Egress {
        let mut e = paddock_host::Egress::default();
        e.allow.clone_from(&self.allow);
        e.system_roots = self.system_roots;
        e.ca_file.clone_from(&self.ca_file);
        e.allow_loopback = self.allow_loopback;
        e.connect_timeout = std::time::Duration::from_millis(self.connect_timeout_ms);
        e.first_byte_timeout = std::time::Duration::from_millis(self.first_byte_timeout_ms);
        e.between_bytes_timeout = std::time::Duration::from_millis(self.between_bytes_timeout_ms);
        e.pool_idle_timeout = std::time::Duration::from_millis(self.pool_idle_timeout_ms);
        e.pool_max_idle_per_host = self.pool_max_idle_per_host;
        e.max_concurrent = self.max_concurrent;
        e
    }
}

/// `[sockets]`: TCP and UDP from apps. Each app's
/// `wasi:sockets` rules must fall inside the rules here, in the manifest's
/// syntax (`{ cidr = "10.0.0.0/8", ports = "1883" }`); a deploy asking for
/// more is refused.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketsCeiling {
    /// Addresses apps may bind.
    #[serde(default)]
    pub bind: Vec<paddock_bundle::manifest::SocketRule>,
    /// Addresses apps may connect and send to.
    #[serde(default)]
    pub connect: Vec<paddock_bundle::manifest::SocketRule>,
    /// Whether apps may listen.
    #[serde(default)]
    pub listen: bool,
}

impl SocketsCeiling {
    /// Check an app's `wasi:sockets` scope against this ceiling.
    ///
    /// # Errors
    ///
    /// What is outside it, for the operator.
    pub fn admits(&self, scope: &paddock_bundle::manifest::SocketsScope) -> Result<(), String> {
        let ceiling = paddock_bundle::manifest::SocketsScope {
            bind: self.bind.clone(),
            connect: self.connect.clone(),
            listen: self.listen,
            ..paddock_bundle::manifest::SocketsScope::default()
        };
        paddock_host::services::sockets::within(scope, &ceiling)
    }
}

/// `[native]`: how the node runs native-process apps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeBinding {
    /// The `paddock-runner` executable; unset for the one beside
    /// `paddock`.
    pub runner: Option<PathBuf>,
    /// A cgroup v2 directory delegated to the node (systemd
    /// `Delegate=yes`), with the node its only process: native instances
    /// then run in cgroups of their own under it, with `memory.max` and
    /// `pids.max`. Unset: `memory_max` is each process's `RLIMIT_DATA`.
    pub cgroup: Option<PathBuf>,
    /// Most processes and threads in one native instance, with `cgroup`.
    pub pids_max: u32,
    /// How long an idle native instance goes between pings.
    pub ping_interval_ms: u64,
    /// How long a ping may go unanswered before the instance is killed.
    pub ping_timeout_ms: u64,
    /// Run native apps on a kernel without Landlock.
    pub landlock_optional: bool,
}

impl Default for NativeBinding {
    fn default() -> Self {
        let host = paddock_host::NativeConfig::default();
        Self {
            runner: None,
            cgroup: None,
            pids_max: host.pids_max,
            ping_interval_ms: 10_000,
            ping_timeout_ms: 5_000,
            landlock_optional: false,
        }
    }
}

impl NativeBinding {
    /// The runtime's settings.
    #[must_use]
    pub fn runtime(&self) -> paddock_host::NativeConfig {
        let mut native = paddock_host::NativeConfig::default();
        native.runner.clone_from(&self.runner);
        native.cgroup.clone_from(&self.cgroup);
        native.pids_max = self.pids_max;
        native.ping_interval = std::time::Duration::from_millis(self.ping_interval_ms);
        native.ping_timeout = std::time::Duration::from_millis(self.ping_timeout_ms);
        native.landlock_optional = self.landlock_optional;
        native
    }
}

fn yes() -> bool {
    true
}

fn default_egress_connect_ms() -> u64 {
    10_000
}

fn default_egress_wait_ms() -> u64 {
    30_000
}

fn default_egress_idle_ms() -> u64 {
    90_000
}

fn default_egress_idle_per_host() -> usize {
    32
}

fn default_egress_concurrent() -> u32 {
    64
}

/// `[metrics]`: an address serving `GET /metrics` in the Prometheus text
/// format. Unauthenticated: bind it to a management network or loopback.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsBinding {
    /// The address to bind, `ip:port`.
    pub address: std::net::SocketAddr,
}

/// An HTTP ingress address and its limits: `[http.<name>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpBinding {
    /// The address to bind, `ip:port`.
    pub address: std::net::SocketAddr,
    /// Tasks accepting connections at once.
    #[serde(default = "default_acceptors")]
    pub acceptors: usize,
    /// Most connections open at once; more are closed on accept.
    #[serde(default = "default_http_connections")]
    pub max_connections: usize,
    /// Time a client has to send a request's headers.
    #[serde(default = "default_header_timeout_ms")]
    pub header_timeout_ms: u64,
    /// Time a client has to send a request's body.
    #[serde(default = "default_body_timeout_ms")]
    pub body_timeout_ms: u64,
    /// Largest reply body an app may send.
    #[serde(default = "default_max_response")]
    pub max_response: ByteSize,
    /// Serve HTTPS with these certificates (the first is the default);
    /// empty serves plain HTTP.
    #[serde(default)]
    pub tls: Vec<TlsBinding>,
    /// How often to check the certificate files for changes and reload
    /// them, in seconds; 0 never (reload with SIGHUP or `paddock
    /// reload-tls`).
    #[serde(default = "default_tls_reload_s")]
    pub tls_reload_s: u64,
    /// With `[failover]`: the partner's address for this listener, as
    /// clients reach it (`scheme://host[:port][/path]`), where `standby =
    /// "redirect"` sends a request for a singleton that runs on the
    /// partner (307).
    #[serde(default)]
    pub partner_url: Option<String>,
    /// With `[failover]`: what this listener does with a request for a
    /// singleton that runs on the partner. Unset: `redirect` with
    /// `partner_url`, `refuse` without.
    #[serde(default)]
    pub standby: Option<StandbyMode>,
}

impl HttpBinding {
    /// What this listener does with a singleton's request on the standby.
    #[must_use]
    pub fn standby_mode(&self) -> StandbyMode {
        self.standby.unwrap_or(if self.partner_url.is_some() {
            StandbyMode::Redirect
        } else {
            StandbyMode::Refuse
        })
    }
}

/// What a listener on the standby of an HA pair does with a request for a
/// singleton that runs on the primary: `[http.<name>] standby`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandbyMode {
    /// `503 app not running`, which a load balancer's health check reads
    /// as down.
    Refuse,
    /// `307 Temporary Redirect` to the same path under `partner_url`.
    Redirect,
    /// Over the cluster to the app on the primary, whose reply it answers
    /// with.
    Forward,
}

/// One certificate of an HTTPS ingress.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsBinding {
    /// PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// PEM private key.
    pub key: PathBuf,
    /// Host names it serves, for SNI (`shop.example`, `*.example.com`).
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Whether `url` is `http(s)://host[:port][/path]`, with no query,
/// fragment or user, and nothing a header value cannot carry.
fn partner_url_ok(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    !host.is_empty()
        && !host.contains('@')
        && rest.bytes().all(|b| b.is_ascii_graphic())
        && !rest.contains(['?', '#'])
}

fn default_tls_reload_s() -> u64 {
    60
}

fn default_http_connections() -> usize {
    1_024
}

fn default_header_timeout_ms() -> u64 {
    10_000
}

fn default_body_timeout_ms() -> u64 {
    30_000
}

fn default_max_response() -> ByteSize {
    ByteSize::mib(16)
}

fn default_acceptors() -> usize {
    4
}

fn default_max_instances() -> u32 {
    1_000
}

fn default_max_core_instances_per_component() -> u32 {
    8
}

fn default_tick() -> u64 {
    2
}

impl NodeConfig {
    /// The names of the `cluster` shared buckets.
    pub(crate) fn cluster_buckets(&self) -> std::collections::BTreeSet<String> {
        self.buckets
            .iter()
            .filter(|(_, b)| b.scope == BucketScope::Cluster)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether this node runs replication: `[failover] replication`, or a
    /// `cluster` shared bucket.
    pub(crate) fn replicates(&self) -> bool {
        self.failover.as_ref().is_some_and(|f| f.replication)
            || self
                .buckets
                .values()
                .any(|b| b.scope == BucketScope::Cluster)
    }

    fn check_native(&self) -> Result<(), NodeError> {
        let n = &self.native;
        for (name, value) in [
            ("pids_max", u64::from(n.pids_max)),
            ("ping_interval_ms", n.ping_interval_ms),
            ("ping_timeout_ms", n.ping_timeout_ms),
        ] {
            if value == 0 {
                return Err(NodeError::Invalid(format!(
                    "node config: native.{name} must be more than 0"
                )));
            }
        }
        Ok(())
    }

    fn check_sockets(&self) -> Result<(), NodeError> {
        let Some(s) = &self.sockets else {
            return Ok(());
        };
        for (list, rules) in [("bind", &s.bind), ("connect", &s.connect)] {
            for (i, rule) in rules.iter().enumerate() {
                if paddock_bundle::manifest::parse_cidr(&rule.cidr).is_none() {
                    return Err(NodeError::Invalid(format!(
                        "node config: sockets.{list}[{i}].cidr '{}' is not a CIDR block such as 10.0.0.0/8",
                        rule.cidr
                    )));
                }
            }
        }
        Ok(())
    }

    fn check_egress(&self) -> Result<(), NodeError> {
        let e = &self.egress;
        if let Some(allow) = &e.allow {
            paddock_host::services::egress::within(allow, None)
                .map_err(|m| NodeError::Invalid(format!("node config: egress.{m}")))?;
        }
        let times = [
            ("connect_timeout_ms", e.connect_timeout_ms),
            ("first_byte_timeout_ms", e.first_byte_timeout_ms),
            ("between_bytes_timeout_ms", e.between_bytes_timeout_ms),
        ];
        for (name, ms) in times {
            if ms == 0 {
                return Err(NodeError::Invalid(format!(
                    "node config: egress.{name} must be more than 0"
                )));
            }
        }
        if e.max_concurrent == 0 {
            return Err(NodeError::Invalid(
                "node config: egress.max_concurrent must be more than 0".into(),
            ));
        }
        Ok(())
    }

    /// Parse a configuration from TOML.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] with the location of the problem.
    pub fn from_toml_str(text: &str) -> Result<Self, NodeError> {
        let config: Self =
            toml::from_str(text).map_err(|e| NodeError::Invalid(format!("node config: {e}")))?;
        let cluster = &config.cluster;
        if cluster.heartbeat_interval_ms == 0
            || cluster.node_timeout_ms <= cluster.heartbeat_interval_ms
        {
            return Err(NodeError::Invalid(format!(
                "node config: cluster.node_timeout_ms ({}) must be more than \
                 cluster.heartbeat_interval_ms ({}), which must be more than 0",
                cluster.node_timeout_ms, cluster.heartbeat_interval_ms
            )));
        }
        if cluster.address.is_some() && (cluster.key.is_none() || cluster.peers.is_none()) {
            return Err(NodeError::Invalid(
                "node config: cluster.address needs cluster.key and cluster.peers".into(),
            ));
        }
        if cluster.advertise.is_some() && cluster.address.is_none() {
            return Err(NodeError::Invalid(
                "node config: cluster.advertise needs cluster.address".into(),
            ));
        }
        for addr in cluster
            .address
            .iter()
            .chain(&cluster.advertise)
            .chain(&cluster.seeds)
        {
            if addr.parse::<std::net::SocketAddr>().is_err() {
                return Err(NodeError::Invalid(format!(
                    "node config: cluster address '{addr}' is not ip:port"
                )));
            }
        }
        config.check_failover()?;
        config.check_egress()?;
        config.check_sockets()?;
        config.check_native()?;
        for (name, binding) in &config.http {
            let invalid =
                |m: String| Err(NodeError::Invalid(format!("node config: http.{name}.{m}")));
            if config.failover.is_none() {
                if binding.partner_url.is_some() {
                    return invalid("partner_url needs [failover]".into());
                }
                if binding.standby.is_some() {
                    return invalid("standby needs [failover]".into());
                }
            }
            if binding.standby == Some(StandbyMode::Redirect) && binding.partner_url.is_none() {
                return invalid("standby \"redirect\" needs partner_url".into());
            }
            let Some(url) = &binding.partner_url else {
                continue;
            };
            if !partner_url_ok(url) {
                return Err(NodeError::Invalid(format!(
                    "node config: http.{name}.partner_url '{url}' is not \
                     http(s)://host[:port][/path], without a query"
                )));
            }
        }
        let witness_pair = config
            .failover
            .as_ref()
            .is_some_and(|f| f.arbiter == ArbiterKind::Witness);
        if let Some(f) = &config.failover
            && f.replication
            && f.arbiter != ArbiterKind::Witness
        {
            return Err(NodeError::Invalid(
                "node config: failover.replication needs arbiter \"witness\"".into(),
            ));
        }
        for (name, bucket) in &config.buckets {
            let problem = if !paddock_bundle::manifest::is_valid_name(name) {
                Some(
                    "not a valid name (lowercase letters, digits, '-' and '_', \
                     starting with a letter or digit)",
                )
            } else if bucket.durability == Durability::Replicated {
                Some("durability \"replicated\" is for app buckets; use scope \"cluster\"")
            } else if bucket.scope == BucketScope::Cluster && bucket.durability != Durability::Disk
            {
                Some("scope \"cluster\" needs durability \"disk\"")
            } else if bucket.scope == BucketScope::Cluster && !witness_pair {
                Some("scope \"cluster\" needs [failover] with arbiter \"witness\"")
            } else if bucket.quota.is_some_and(|q| q.bytes() == 0) {
                Some("quota must be more than 0")
            } else {
                None
            };
            if let Some(problem) = problem {
                return Err(NodeError::Invalid(format!(
                    "node config: buckets.{name}: {problem}"
                )));
            }
        }
        for (name, secret) in &config.secrets {
            if secret.file.is_some() == secret.env.is_some() {
                return Err(NodeError::Invalid(format!(
                    "node config: secrets.{name} needs exactly one of file and env"
                )));
            }
        }
        let limits = &config.limits;
        config.check_datasources()?;
        if config
            .max_processes
            .is_some_and(|p| p < config.max_instances)
        {
            return Err(NodeError::Invalid(
                "node config: max_processes must be at least max_instances".into(),
            ));
        }
        if config.max_core_instances_per_component == 0 {
            return Err(NodeError::Invalid(
                "node config: max_core_instances_per_component must be at least 1".into(),
            ));
        }
        for (name, value) in [
            ("timers_max", limits.timers_max),
            ("monitors_max", limits.monitors_max),
            ("links_max", limits.links_max),
            ("pending_calls_max", limits.pending_calls_max),
            ("groups_max", limits.groups_max),
            ("watches_max", limits.watches_max),
        ] {
            if value == 0 {
                return Err(NodeError::Invalid(format!(
                    "node config: limits.{name} must be more than 0"
                )));
            }
        }
        Ok(config)
    }

    /// Each `[datasources.<name>]` against `[secrets]` and itself.
    fn check_datasources(&self) -> Result<(), NodeError> {
        for (name, ds) in &self.datasources {
            let invalid = |m: &str| {
                Err(NodeError::Invalid(format!(
                    "node config: datasources.{name}: {m}"
                )))
            };
            if let Some(secret) = &ds.password_secret
                && !self.secrets.contains_key(secret)
            {
                return invalid(&format!("password_secret '{secret}' is not in [secrets]"));
            }
            match (ds.tls, &ds.ca_file) {
                (DatasourceTls::Require, None) => {
                    return invalid("tls \"require\" needs ca_file");
                }
                (DatasourceTls::Disable, Some(_)) => {
                    return invalid("ca_file is only for tls \"require\"");
                }
                _ => {}
            }
            if ds.pool_size == 0
                || ds.connect_timeout_ms == 0
                || ds.acquire_timeout_ms == 0
                || ds.statement_timeout_ms == 0
                || ds.max_result_bytes.bytes() == 0
            {
                return invalid("pool_size, the timeouts and max_result_bytes must be more than 0");
            }
        }
        Ok(())
    }

    /// `[failover]` and `[witness]` against each other and `[cluster]`.
    fn check_failover(&self) -> Result<(), NodeError> {
        let invalid = |m: &str| Err(NodeError::Invalid(format!("node config: {m}")));
        let witness_role = self.cluster.role == ClusterRole::Witness;
        let timing_ok = |lease: u64, margin: u64| margin > 0 && margin.saturating_mul(2) < lease;
        if let Some(f) = &self.failover {
            if self.cluster.address.is_none() {
                return invalid("[failover] needs cluster.address");
            }
            if witness_role {
                return invalid("a witness (cluster.role) cannot have [failover]");
            }
            if f.partner == self.node {
                return invalid("failover.partner is this node");
            }
            match (f.arbiter, f.witness) {
                (ArbiterKind::Witness, None) => {
                    return invalid("failover.arbiter \"witness\" needs failover.witness");
                }
                (ArbiterKind::Witness, Some(w)) if w == self.node || w == f.partner => {
                    return invalid("failover.witness must be a third node");
                }
                (ArbiterKind::Paths | ArbiterKind::Operator, Some(_)) => {
                    return invalid("failover.witness is only for arbiter \"witness\"");
                }
                _ => {}
            }
            if let Some(serial) = &f.serial {
                if cfg!(not(unix)) {
                    return invalid("failover.serial needs a Unix node");
                }
                if serial.baud == 0 {
                    return invalid("failover.serial.baud must be more than 0");
                }
            }
            if !timing_ok(f.lease_ms, f.margin_ms) {
                return invalid(
                    "failover.margin_ms must be more than 0 and less than half of lease_ms",
                );
            }
        }
        if let Some(w) = &self.witness {
            if !witness_role || self.cluster.address.is_none() {
                return invalid("[witness] needs cluster.role \"witness\" and cluster.address");
            }
            if w.pair[0] == w.pair[1] || w.pair.contains(&self.node) {
                return invalid("witness.pair must be two other nodes");
            }
            if !w.pair.contains(&w.first) {
                return invalid("witness.first must be in witness.pair");
            }
            if !timing_ok(w.lease_ms, w.margin_ms) {
                return invalid(
                    "witness.margin_ms must be more than 0 and less than half of lease_ms",
                );
            }
        }
        Ok(())
    }

    /// Read and parse a configuration file.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] if the file cannot be read or parsed.
    pub fn from_file(path: &Path) -> Result<Self, NodeError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| NodeError::Invalid(format!("{}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// The admin socket path.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.admin_socket
            .clone()
            .unwrap_or_else(|| self.data_dir.join("admin.sock"))
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
