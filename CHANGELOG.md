# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## Unreleased

### Added

- `wit/runtime.wit`: the `paddock:runtime@0.1.0` app contract (`types`,
  `process`, and the `app` world).
- Tests that enforce the contract's rules: synchronous functions only, no
  `stream`/`future`, no `wasi:io` types, and an `access-denied` error on every
  capability function.
- `paddock-fixture-echo`, the smallest app implementing the `app` world.
- CI: tests on Linux, macOS and Windows, guest component checks, MSRV 1.96.0,
  cargo-deny, fmt and docs, and a weekly run against fresh dependencies.
- `paddock-manifest`: the full app manifest model (every section, including
  fields for later phases), with defaults, path-aware parse errors, validation
  that reports every broken rule, and a list of fields Phase 1 does not enforce.
- `paddock-bundle`: opens `.rbp` 1.2 bundles, verifies signatures against
  operator-trusted keys only, checks checksums and the component's world, and
  reads the `appserver` section.
- Deploy-time import inspection: a component importing an interface its
  manifest does not declare, or that no host provides, is refused.
- Operator policy (TOML) and the effective grant: imports ∩ manifest ∩ policy,
  with ungranted capabilities marked for deny stubs.
- `paddock-host`: the instance runtime. A pooling-allocator engine sized from
  node config (standard or dense memory profile), an epoch ticker, pids with
  incarnations, a process table, and instances that run `init`, handle calls
  and casts one at a time under a per-callback deadline, and run `terminate`
  on a graceful stop. Traps, memory and table limits, deadlines and kills end
  an instance with an exit reason; a full mailbox fails casts and calls with
  `overloaded`.
- `paddock-fixture-chaos`: a guest that traps, spins, allocates or fails on
  request, for exit-path tests.
- Messaging: guest `cast`, `call` (with timeout, `noproc` for dead or stale
  pids, the target's exit reason if it dies mid-call) and `reply` for deferred
  answers; `send-after` and `cancel-timer` timers delivered to `handle-info`;
  a node-local registry (`register` limited to the manifest's
  `registered_names`, `whereis`, names released on exit). `Runtime` gains
  `whereis`, `call` and `cast`.
- `message_max_bytes` is enforced; an oversized body fails with the new
  `too-large` call error. Casts dropped for a full mailbox are counted
  (`InstanceHandle::dropped_messages`).
- Links, monitors and exit signals: `link`, `unlink`, `trap-exits`,
  `monitor`, `demonitor` and `exit` follow OTP's propagation rules. Exit and
  down notifications never drop.
- Host-native supervisors (`Runtime::start_supervisor`): `one_for_one`,
  `one_for_all`, `rest_for_one` and `simple_one_for_one`; permanent,
  transient and temporary children; restart intensity with escalation;
  backoff with jitter; nested supervisors.
- Apps (`Runtime::start_apps`): dependency order (with version checks and
  cycle detection), a root supervisor per app built from its manifest,
  status (`running`, `failed` with the escalation reason, `stopped`), stop in
  reverse order, and instance pools with round-robin and JSON/CBOR hash
  routing. `whereis` on an app's name returns a pool member.
- Host services: `wasi:logging` (level cap, per-app rate limit, tagged
  records), stdout/stderr captured as tagged logs, `wasi:config`,
  `wasi:keyvalue` (memory and redb disk buckets, per-app namespaces, byte
  quotas, `increment`, batch calls) and `wasi:filesystem` preopens (`/priv`
  read-only, volumes read-write). Ungranted capabilities are denied per
  instance; interfaces no host implements get typed deny stubs. Vendored WIT
  for the draft interfaces lives in `wit/vendor`.
- `RuntimeConfig::data_dir` enables disk buckets. `InstanceSpec::new`,
  `with_granted` and `resolve_preopens`.
- Hot upgrade: `InstanceHandle::upgrade` swaps an instance's component
  between callbacks through `save-snapshot`/`load-snapshot`, keeping its pid,
  links, monitors, names, timers and waiting callers; any failure after the
  snapshot keeps the old version. `Apps::upgrade` rolls an app out in
  `snapshot` or `restart` mode, in batches, stopping and reverting on a failed
  instance or too many restarts; `Apps::rollback` returns to the previous
  version. `validate_upgrade` refuses other apps, the same version, a world
  change, versions outside `from_versions`, and unapproved new capabilities.
- Supervisors can replace a child's spec, with or without restarting it
  (`replace_child`, `replace_template`), and count their restarts.
- `paddock-node`: the node daemon. TOML node config (trusted keys, policy,
  volumes, per-app config overrides), a deploy store with bundles, `priv/`
  files and desired state, restore on restart, an audit log, and the admin API
  (JSON over HTTP on a 0600 Unix socket): deploy, list, status, stop, start,
  remove, kill, upgrade, rollback, logs, ps and call.
- `paddock` CLI over the admin API, with text and `--json` output.
- `paddock-api`: the admin API's request and response types and a client.
- Bundles' `priv/` files are verified against the signed manifest's checksums
  and extracted for `/priv`; an unlisted or altered file refuses the deploy.
- `AppDeployment` carries its `InstanceSpec`; `Apps::add` and `Apps::remove`
  change a running set; instances report app, version and queue depth;
  `Runtime::instances`; `Pid` parses from text.
- `wit/runtime.wit`: `info` gains a reserved `host-event(tuple<string,
  envelope>)` case, so later host events reach `@0.1.0` guests.
- `paddock-sdk`: write apps in Rust with the `App` trait, `Ctx` (every
  `process` function), typed envelopes (CBOR, JSON, text), logging macros, and
  `paddock_sdk::app!`. `#[derive(Snapshot)]` (`paddock-sdk-macros`) gives
  versioned snapshot state with a `migrate` hook.
- Examples written with the SDK: `counter` (v1, v2, broken v3), `ping-pong`
  (typed CBOR calls, registry, monitor) and `chaos`, each with a
  `paddock.toml`.
- `paddock bundle` builds a signed `.rbp` from a component and `paddock.toml`
  (refusing undeclared imports, adding `--priv` files), and `paddock keygen`
  creates minisign keys.
- `paddock state <pid>` (`GET /v1/instances/{pid}/state`): an instance's
  state through `save-snapshot`, shown as JSON for SDK snapshots, JSON or text
  bodies, or hex (`InstanceHandle::snapshot` in `paddock-host`).
- `paddock log-level <app> <level|reset>` (`POST /v1/apps/{app}/log-level`):
  change an app's log level at run time, audited (`Runtime::set_log_level`).
- `paddock cast <app> <body>` (`POST /v1/apps/{app}/cast`), and a `DROPPED`
  column in `paddock ps` (`InstanceInfo::dropped`): casts lost to a full
  mailbox.
- A node holds an exclusive lock on `<data_dir>/node.lock`; a second node on
  the same data directory is refused.
- `crates/paddock-node/tests/exit_criteria.rs`: every Phase 1 exit criterion
  as a test against a running node.
- `docs/BASELINE.md` and `paddock-host`'s `baseline` example: call latency,
  cast throughput, instantiate time and idle memory per instance.
- `fuzz/`: cargo-fuzz targets for bundle opening, manifest parsing, small
  parsers, hash-routing keys and sequences of process operations on live
  instances, run nightly in CI.
- `docs/GETTING_STARTED.md`, `docs/WRITING_APPS.md` and `docs/MANIFEST.md`.
- `docs/ROADMAP.md`, the single plan for remaining work, replacing
  `docs/PHASE1_PLAN.md`.
- `paddock rollback --restart` and `paddock upgrade --restart` (API:
  `restart=true`): move instances by restart whatever the manifest's upgrade
  mode, discarding in-memory state. A refused snapshot rollout suggests the
  flag, and the audit log records `restart_requested`.
- Baseline harnesses `epoch_cost` (epoch-interruption overhead on guest
  compute) and `pool_load` (concurrent clients against a pool), with results
  in `docs/BASELINE.md`. The chaos fixture's `busy:<n>` command gives a call
  a fixed amount of guest work.

### Changed

- `paddock-bundle`: `OperatorPolicy::granted` returns `Granted`
  (capabilities and extension packages), `effective_grant` takes it, and
  `EffectiveGrant` reports `granted_extensions` and `denied_extensions`.
  `ImportClass` and `ImportLink` gained extension cases and are no longer
  `Copy`.
- `paddock-host` host services follow one pattern (`paddock_host::services`):
  each has its own WIT world, spec, shared and per-instance state, and
  linking. `wasi:logging`, `wasi:config` and `wasi:keyvalue` use it.
  `InstanceSpec` fields moved under `InstanceSpec::services`: `log_level` and
  `log_rate_limit` to `logging.level` and `logging.rate_limit`, `config` to
  `config.values`, `buckets` to `keyvalue.buckets`.
- A guest log record below the app's level costs about 3 ns (was about
  440 ns): the host announces the effective level as
  `host-event("log-level", ...)` after `init`, after a snapshot upgrade and
  on `paddock log-level`, and `paddock-sdk`'s macros check it before
  formatting. Adds `paddock_sdk::trace!` and `paddock_sdk::log::enabled`.
  Host services can announce host events (`Service::announce`).
- `paddock-sdk` features `config` and `keyvalue`: `paddock_sdk::config`
  (`get`, `get_all`) and `paddock_sdk::kv::Bucket` (store, atomics, batch,
  paging `keys`, CBOR-typed `get_value` and `set_value`), with the
  `paddock-fixture-sdk` test fixture.
- Connection handlers: a component exporting `paddock:connection/handler`
  serves TCP connections on listeners the manifest declares
  (`[[appserver.listeners]]`) and the node binds (`[listeners.<name>]`),
  with an instance per connection reading and writing `wasi:io` streams.
  Idle timeout, connection cap, crashes close only their connection,
  upgrades and rollbacks apply to new connections. `Runtime::listen`,
  `Runtime::load_connection`, a `connection__open` probe, listeners in
  `paddock status`; `paddock bundle` detects the world from the component.
  SDK feature `connection`; example `examples/line-server`.
- Delivery between nodes: `paddock_host::transport` (`Frame`, the
  `Transport` trait, `Runtime::set_transport`, `deliver`, `node_down`).
  Calls, casts, exit signals, links and monitors reach pids on other nodes;
  a lost node ends its links and monitors with `noconnection` and fails
  calls waiting on it. Without a transport, pids on other nodes get
  `noconnection`.
- `transport::memory::Mesh`: runtimes in one process as nodes, with
  per-pair delay and jitter, random drops from a seed, partition, isolate
  and heal, for tests.
- Deterministic distribution tests (`paddock-host`'s `tests/simulation`):
  nodes on a paused tokio clock, so races between links, monitors, exits,
  partitions and heals replay the same way every run. Targeted race tests,
  and seeded random scenarios checked against the distribution rules
  (`PADDOCK_SIM_SEEDS`, `PADDOCK_SIM_FIRST_SEED`, `PADDOCK_SIM_SEED` to
  replay one); 500 new seeds run nightly.
- The epoch ticker parks between ticks, so dropping a runtime stops it at
  once whatever the tick.
- Node liveness (`paddock_host::transport::liveness`): heartbeats between
  peers (`RuntimeConfig::heartbeat_interval`, `node_timeout`), sessions
  numbered on every frame so both sides of a lost node end together and
  frames from an ended session are discarded, extra heartbeat paths
  (`HeartbeatPath`, for a serial line), `Runtime::node_events`,
  `peer_status`, `add_peer` and `heard`, and the `node__down` probe. The
  mesh can stall a pair silently (both ways or one way), cut an extra path,
  and registers peers on join.
- Cluster membership (`paddock_host::transport::membership`): joining
  through seeds (`Runtime::join_cluster`), member lists passed on so every
  pair connects, `leave_cluster`, `members`, roles (`member`, `witness`),
  and `Joined`/`Left` events. Node creation numbers
  (`RuntimeConfig::creation`, raised on every node start in
  `<data_dir>/creation`) on every packet, so a restarted node's peers end
  its old sessions and discard frames from or for its earlier start.
- Node config `[cluster]`: `role`, `heartbeat_interval_ms`,
  `node_timeout_ms`, and `address` and `seeds` (not used until the QUIC
  transport). A witness refuses deploys, and refuses to start on a data
  directory that holds apps.
- `spikes/s16`: a TLA+ model of failover with a lease, adapted from
  HAppliance's specifications.
- Breaking (`paddock_host::transport`): `Transport::send` and
  `Runtime::deliver` take a `Packet` (session number and frame) instead of
  a `Frame`.
- The dev profile builds Cranelift optimized: tests that compile guest
  components compile each in about 0.2 s instead of over 2 s.
- USDT probes on the message path (provider `paddock`): calls, casts,
  callbacks with guest time, exits, supervisor restarts, rollouts and
  mailbox drops, from anyprobe (a git dependency on its GitHub `main`).
  `paddock-host`'s `probes` feature; on in the `paddock` binary.
  `docs/OBSERVABILITY.md` lists them with bpftrace one-liners.
- `paddock ps` and `GET /v1/instances` show memory, restart count and the
  last exit reason (`InstanceHandle::memory_bytes`, `restarts`,
  `last_exit`).
- `paddock call`/`cast` take `--pid` or `--name` to address one instance,
  and `--cbor` to send a JSON body as CBOR; routes
  `/v1/instances/{pid}/call|cast` and `/v1/names/{name}/call|cast`,
  `api::Target`, `Client::call_to`/`cast_to`, `Node::call_to`/`cast_to`.
- `admin_latency` example: admin-API call latency, in `docs/BASELINE.md`.
- `examples/tracing`: a script that runs a node with traffic worth tracing,
  and bpftrace scripts for its probes.
- Extensions: host services from outside `paddock-host`. The
  `paddock_host::services::Extension` trait serves one WIT package;
  `Runtime::with_extensions`, `Node::open_with` and `paddock::node::run`
  take extensions. Manifests declare them under
  `capabilities.extensions."<package>"` and policy grants them by package
  name. `paddock_host::wasmtime` re-exports wasmtime for their bindings.
  Example: `examples/greeter`.
- `examples/chaos` has a `log:<n>` command and declares `wasi:logging`; the
  chaos fixture's `infos` includes host events with their payload.
- `disk` keyvalue reads run on the calling worker instead of tokio's
  blocking pool: a small read costs about 2.4 µs (was 22–30 µs). Writes,
  which fsync, stay on the blocking pool.
- The default epoch tick is 2 ms (was 10 ms), in `RuntimeConfig` and the
  node's `epoch_tick_ms`. A busy instance holds its worker for at most a
  tick, so p99 latency under oversubscription falls from about 10.7 ms to
  3.4 ms at the same throughput (`docs/BASELINE.md`).
- `pool_load` takes the epoch tick as an optional second argument.
- Manifest: `placement.mode` (`any`, `singleton`, `all-nodes`,
  `hot-standby`) replaces `placement.singleton`, which is now rejected as an
  unknown field. A `singleton` whose `instances` asks for more than one is
  invalid. Still not enforced (Phase 3).
- Crates consolidated from nine to five publishable ones. `paddock-manifest`
  is now the `manifest` module of `paddock-bundle`. `paddock-node`,
  `paddock-api` and `paddock-cli` are now the `paddock` crate: the node runs
  as `paddock node --config node.toml` (was `paddock-node`), and the admin
  API is `paddock::api`. Building `paddock` without its default `node`
  feature gives the CLI and bundle tooling without wasmtime.
- `rustbridge-bundle` is a git dependency on rustbridge's `experiment/wasm`
  branch instead of a sibling checkout, so CI can build the workspace.
