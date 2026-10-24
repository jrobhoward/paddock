# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `docs/GLOSSARY.md`: the terms the docs and code use, with the two
  meanings of "epoch" (the wasmtime epoch tick and the failover epoch) kept
  apart.
- Benchmarks of what running in Wasm costs, each a `paddock-host` example
  and a `scripts/bench.sh` entry: `timer_cost` (`send-after` and guest
  sleep lateness against a native tokio sleep, idle and loaded; clock reads),
  `restart_cost` (a crash or kill to the restarted instance's first reply;
  upgrades by state size), `heap_cost` (heap growth, memory held live and
  after exit, slot reuse, passivated state by size) and `compile_cost`
  (compile time by component size, and cached loads); and
  `scripts/beam_baseline.escript`, the same shapes on Erlang/OTP for
  reference (`bench.sh beam`).
- `call_split` (`paddock-host` example, `bench.sh call_split`): where an
  empty call's time goes, as wall time, on-CPU time, context switches and
  time inside the callback, against a bare tokio request and reply.
- The chaos fixture's `timed`, `lateness`, `clock` and `ballast` commands,
  for those benchmarks; its snapshot carries the ballast after a marker,
  and reads snapshots without one as before.

### Changed

- A call no longer arms a timer when its reply arrives before the caller
  next runs: `InstanceHandle::call` yields once first, then waits on a
  timer at the same deadline. About 8% off an empty call (ROADMAP B12).
- The minimum Rust version is 1.99.0 (was 1.96.0), so dependencies resolve
  to current releases; the lock files of the workspace, fuzz and spikes are
  updated within semver.

### Fixed

- The workspace builds on macOS and Windows again: the native-app code,
  `paddock-runner` and the native test fixture, the serial pty, the
  native program's file mode and the admin client compiled Unix- or
  Linux-only APIs there. Native apps stay Linux only; elsewhere a deploy's
  instances fail to start with a message saying so.

## [0.3.0] - 2026-10-23

The first tagged version: a research release, not feature complete, with
unstable APIs and formats.

### Changed

- Every workspace crate takes its version, 0.3.0, from the workspace.
- `README.md` states that Paddock is a research project without support.
- rustbridge comes from its `experiment/wasm` branch on GitHub again, with
  zip 9; the fuzz workspace's lock file moves from zip 8.6 to 9.0.5.
- The workspace requires tokio 1.54 (the serial tty layer needs
  `AsyncFd::register`, from 1.53.3).
- CI and fuzz workflows run with a read-only token.
- `docs/SECURITY.md` says how to report a vulnerability.

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
- Host-object quotas per instance: manifest `limits.timers_max`,
  `monitors_max`, `links_max` and `pending_calls_max`, with node defaults
  under `[limits]` (10,000 each) that are also the most a manifest may ask
  for. Over a limit, the instance crashes with `timer_limit`,
  `monitor_limit`, `link_limit` or `pending_call_limit`. Timers whose message
  was dropped by a full mailbox no longer stay counted as pending.
- Process groups on one node (`paddock:groups@0.1.0`, `wit/groups`): `join`,
  `leave`, `members` and `publish`, which casts one copy to each member. The
  capability's manifest scope lists the groups an app may join and publish
  to. Instances leave their groups on exit and keep them across upgrades.
  `limits.groups_max` and node `[limits] groups_max` (default 1,000). SDK
  feature `groups` (`paddock_sdk::groups`).
- Shared keyvalue buckets: node config `[buckets.<name>]` (durability,
  quota, `scope = "node"`), manifest `wasi:keyvalue` `shared.<name> = "ro"`
  or `"rw"`, and policy `buckets = [...]`. Apps granted a shared bucket see
  one copy; read-only grants refuse writes. Deploys report shared buckets as
  `bucket:<name>` and refuse ones the node does not configure.
- Keyvalue revisions: each change raises a bucket's revision and stamps the
  keys written, in memory and on disk (groundwork for watches, H4).
- `paddock:watch@0.1.0` (`wit/watch`), under the `wasi:keyvalue`
  capability: reads with revisions, compare-and-swap and compare-and-delete
  on a revision, and watches on a key or prefix that deliver conflated
  `kv-changed` host events, resumable from a revision (`too-old` when the
  bucket cannot replay). `limits.watches_max` and node `[limits]
  watches_max` (default 1,000). SDK feature `watch` (`paddock_sdk::watch`).
- `watch_cost` baseline harness (`paddock-host` example).
- Extensions can run node-level work: `Extension::start` with an
  `ExtensionNode` (node config from `[extensions."<package>"]`, casts to
  apps by pool routing, a stop signal); `Runtime::start_extensions`.
- Modbus TCP extension (`crates/paddock-modbus`, `paddock-modbus:modbus`),
  in `paddock node` by default (`modbus` feature): polls devices from node
  config and casts changed points per device and scan to an app, reports
  devices offline, lets apps read and write listed devices, and serves
  registers to Modbus masters. `modbus_load` throughput harness.
- `examples/scada`: a hash-routed `points` pool keeping Modbus points in a
  shared bucket, and an `hmi` connection handler answering over a listener.
- HTTP ingress (`paddock_host::http`, node config `[http.<name>]`): apps'
  `wasi:http/incoming-handler` routes (host, path prefix, `hash_header`,
  `max_body`, `timeout_ms`) receive each request as a call, granted by
  policy without an import; failures map to 404, 413, 502, 503 and 504.
  SDK feature `http` (`paddock_sdk::http::{Request, Response}`);
  `examples/http-counter`. Each `[http.<name>]` address caps connections
  (`max_connections`) and the time to send headers (`header_timeout_ms`)
  and a body (`body_timeout_ms`, 408), and refuses app replies over
  `max_response` (502).
- Secrets (`paddock:secrets@0.1.0`, `wit/secrets`): names listed in the
  manifest, granted by name in policy (`secrets = [...]`), bound in node
  config to a file or environment variable (`[secrets.<name>]`), read on
  every call and never logged. SDK feature `secrets`.
- Metrics (`paddock:metrics@0.1.0`, `wit/metrics`): app-defined counters,
  gauges and histograms per app (`<prefix>_<name>`, `app` label,
  `max_series`), host counters per app (callbacks, guest time, exits,
  drops, instances, memory, mailbox depth) and HTTP ingress counters, in
  the Prometheus text format: `paddock metrics`, `GET /v1/metrics`, and
  `GET /metrics` on node config's `[metrics] address`. SDK feature
  `metrics`.
- HTTPS for the ingress: `tls = [{ cert, key, hosts }]` per `[http.<name>]`
  (rustls on ring), certificates chosen by SNI, reloaded when their files
  change (`tls_reload_s`), on SIGHUP, or with `paddock reload-tls`; a broken
  file keeps the old certificates. Requests carry `scheme`.
- QUIC between nodes (`paddock_host::transport::quic`, quinn on rustls with
  ring): pinned Ed25519 keys instead of a certificate authority (TLS 1.3 raw
  public keys, a peers file per node), a versioned CBOR wire format
  (`transport::wire`), control and data streams per pair, redialing, and a
  graceful leave. Node config `[cluster]` gains `key`, `peers` and
  `identity_reload_s`; `seeds` are addresses. `paddock cluster keygen`,
  `paddock cluster pubkey`, `paddock members` (`GET /v1/cluster`); calls and
  casts by pid reach other nodes.
- `examples/board`: a counter on a shared disk bucket over HTTP, with a
  crash endpoint and the answering pid in every reply, plus a watcher that
  logs each change (one component, two manifests).
- `examples/tutorial`: chapter scripts with pauses and printed commands.
  Chapter 1 crashes the board app and restarts its node (the count
  survives); chapter 2 kills the primary of an HA pair (the app moves, its
  disk bucket does not; a load balancer health check reaches the primary).
  Chapters 3 (hot standby, after D4) and 4 (replicated buckets) are
  outlined.
- `examples/failover`: a demo script that fails a singleton over between
  three `paddock node` processes, killing the primary with SIGKILL.
- Failover on the node: `[failover]` (partner, arbiter `witness`, `paths` or
  `operator`, lease and margin) makes a node one of an HA pair, and
  `[witness]` (on a node with cluster role `witness`) arbitrates one.
  `placement.mode = "singleton"` apps run only on the primary while it may
  run them, start from `init` on promotion and are killed at once when it
  may not; `hot-standby` apps run on both nodes and receive
  `host-event("failover-role", "primary" | "standby" | "down")`. The epoch
  and the witness's state are synced to disk before they are acted on.
  `paddock failover` (`GET /v1/failover`) and `paddock failover confirm`
  (`POST /v1/failover/confirm`). Deploy warnings now name only the placement
  fields still unenforced (`all-nodes`, labels, `spread`).

- `scripts/check-secrets.sh`: scans tracked files, and with `--push` the
  lines added by unpushed commits, for private keys, access tokens,
  passwords, credentials in URLs, email addresses other than the
  repository's authors, the local user and host names, private links, and
  key-like, large or binary files. Run in CI and before every push; usable
  as a pre-push hook.
- `scripts/check.sh`: every check of the Definition of Done as one command,
  with a one-line result per check (`--quick` for fmt, clippy, conventions
  and the tests; or named checks). Also checks what clippy does not: test
  names, test file layout and headers, and a CHANGELOG entry for changes to
  `crates/`, `examples/` or `wit/`. Guest crates are found from the
  workspace instead of a list. CI runs its checks through the script.
- `scripts/soak.sh --hours N` and `paddock-soak`: hours of fault injection,
  not run in CI. Each round runs the simulation's random scenarios and
  failover schedules from new seeds; a witness and a pair as `paddock node`
  processes behind user-space UDP proxies that cut, hold or drop each
  direction, with `SIGKILL` and `SIGSTOP` of nodes, failing on any moment
  when both nodes ran the singleton or a heal after which other than one
  runs it; and the chaos example on a node, which must stay responsive with
  bounded memory. A failed run keeps its seed and logs.
- The workspace builds and its tests pass on aarch64 Linux (Raspberry Pi 5,
  Ubuntu 26.04, 2026-10-10: 791 passed, as on x86_64), built on the Pi. Run
  by hand; CI does not cover arm64 yet.
- `scripts/check.sh`'s `test` check runs every test binary
  (`--no-fail-fast`), so one failure no longer hides the rest.
- Node config `cluster.advertise`: the address other nodes reach this node
  at, when it is not `cluster.address` (NAT, port forwarding, a proxy).
- A node logs `singleton starting` before a singleton's `init` runs, also
  when it starts at deploy, and `cannot start singleton` when that fails.
- Modbus server `allow`: the masters' addresses or networks it accepts,
  loopback only by default. A master not listed is refused.
- `paddock_host::{parse_cidr, cidr_contains}` for extensions' node
  configuration.
- Node config `log_format = "json"`: one JSON object per line on stderr,
  for log collectors. Text output has colour only on a terminal.
- Node config `[limits] log_records_per_sec` (default 1000): every app's
  log budget, `wasi:logging` records and captured stdout and stderr lines
  together. An app's `rate_limit_per_sec` can only lower it.
- The failover simulation's random schedules start from
  `PADDOCK_SIM_FAILOVER_FIRST_SEED` (default 0).
- `InstanceSpec::bound_to_lease`, set for `singleton` apps.
- Node config `[cluster] incoming_calls_max` (default 10000): the most
  calls from one peer handled at once; beyond it a call is answered
  `overloaded` (`RuntimeConfig::incoming_calls_per_peer`).
- `paddock_bundle::check_key_permissions`, and the `UnsafePath` bundle
  error.
- Fuzz targets `wire_packet`, `failover_status`, `http_reply`,
  `state_view` and `modbus_request`, run nightly with the others.
- Compiled components are kept under `data_dir/compiled/<app>/<version>/`,
  so a restart reads them instead of compiling every app again; removed with
  their version, and pruned at start. `Runtime::load_cached` and
  `Runtime::load_connection_cached` in `paddock-host`.
- `paddock` cargo features `http` (the HTTP ingress), `tls` (HTTPS) and
  `probes`, on by default; `paddock-host` features `http` and `tls`. A node
  built without one refuses node config or apps that need it.
- `scripts/check.sh features`: the node with no optional feature, and with
  HTTP but no TLS, builds clean.
- `docs/OPERATIONS.md` (paths, environment variables, signals, backups) and
  `examples/systemd/paddock.service`.
- `Mesh::lose_next` (`paddock_host::transport::memory`): lose the next
  frame between two nodes that a predicate matches, for tests.
- Tests for: the singleton gate (deploy to a primary; upgrade, rollback and
  removal of a held singleton; a refused start; an epoch that cannot be
  kept), failover after a lost grant (refused, step down, resume) and a
  standby standing down when its partner says `Down`, the Modbus guest
  interface, links cascading along a chain, watches ended when their
  instance exits, and secret values absent from every log record and error.
- `Apps::check_add`: whether `Apps::add` would accept an app, without
  starting it.
- Modbus server `max_connections` (default 32): masters connected at once;
  more are closed on accept, with one warning each time the server fills.
- Tests that drive failover transitions one at a time against a fake
  environment (`failover::node`'s `Env` trait), among them `NotYet` to a
  node that is down, and one where a pool's supervisor exits mid-rollout.

- `paddock --json ps` lists each instance's links and monitors (`links`,
  `monitoring`, `monitored_by`), as its node records them;
  `Runtime::links` returns them.
- `scripts/soak.sh` runs `paddock-soak cluster`: three member nodes behind
  UDP proxies, their instances linking, monitoring and killing each other
  at random under link cuts, holds and loss, `SIGKILL` and `SIGSTOP`; after
  each heal, every link and monitor must be listed on both of its nodes and
  name a live instance (`--cluster-minutes`, `--only cluster`).

- Health checks (manifest `health`): the host calls each instance with a
  `paddock/health` envelope every `interval_ms` and kills it as
  `crashed: unhealthy` after `failure_threshold` failures in a row (a reply
  other than `ok`, an error, or no reply within the interval); its
  supervisor restarts it. `paddock-sdk` answers from `App::health`,
  healthy by default. The app world is unchanged.

- SQL datasources (ROADMAP E5): `paddock:sql` (`wit/sql/sql.wit`) lets an
  app run Postgres statements on datasources the node binds
  (`[datasources.<name>]`: address, password from a node secret, TLS with
  a CA file, pool size, timeouts, a result size cap). `query`, `execute`
  and a `transaction` resource (dropping it rolls back, as does the
  instance exiting); parameters bound and sent as text for the server to
  convert; result columns as booleans, integers, floats, text and bytes,
  with dates, times, `numeric`, `json` and `uuid` as text. The manifest
  names each datasource with a connection cap per app
  (`capabilities."paddock:sql".datasources`), and policy grants each
  (`datasources`). Pooled connections are reset before reuse. Cargo
  feature `sql`, on by default; `paddock_sdk::sql` behind the SDK's `sql`
  feature. `examples/tally` keeps a counter in Postgres, each change in one
  transaction, and tutorial chapter 4 (`04-database.sh`) runs it as a
  singleton on an HA pair: the count survives `kill -9` of the primary.
  The tests start a scratch Postgres as the current user and skip without
  one.
- Cluster shell (ROADMAP P1): `paddock shell` runs `paddock` commands at a
  prompt (rustyline: history in `~/.config/paddock/history`, completion of
  commands and contacts), against the admin socket or `--remote`;
  `connect <name|id|local>` moves the session to another node listed in a
  contacts file (`--contacts`, `PADDOCK_CONTACTS`, default
  `~/.config/paddock/contacts.toml`: each node's admin address and pinned
  key, and the operator's key), and `members` then names each node's
  contact. Ctrl-C stops the command running, not the session.
  `deny.toml` allows BSL-1.0 for rustyline's Windows-only clipboard crates.
- `paddock events` (`GET /v1/events`): the node's events as they happen,
  one JSON object per line over a chunked response (on the socket and the
  remote admin API): audit records as they are written, instance exits
  and supervisor restarts, cluster members joined, left, reachable or not,
  and failover leadership changes. `paddock::api::Event` and
  `Client::events`; `paddock_host::Runtime::instance_events` gives the
  host's exits and restarts (`InstanceEvent`).
- `paddock logs --follow` (`GET /v1/apps/{app}/logs?follow=true`): the last
  records, then each new one as it is logged (`Client::follow_logs`). A
  stream sends an empty line every 15 seconds, and a client gives up after
  45 seconds without one; a follower that falls behind is told how many
  records or events it missed.
- HTTP egress (ROADMAP E2): `wasi:http/outgoing-handler` is linked and
  enforced. A request goes out only to a scheme, host and port the
  manifest's `allow` lists (origins, bare hosts, `*.suffix` patterns or
  addresses), with its `methods`, at most `max_concurrent` per app at once,
  through one connection pool for the node (hyper-util, rustls with the
  system's roots and `[egress] ca_file`). A name resolving only to loopback
  or link-local addresses is refused unless `[egress] allow_loopback`.
  Node config `[egress]`: `allow` (a ceiling every manifest must fit inside,
  checked at deploy), timeouts, pool size and the default
  `max_concurrent`. Refusals are wasi:http error codes, never traps.
  Metric `paddock_egress_requests_total{app,outcome}`. Cargo feature
  `egress` in `paddock-host` and `paddock`, on by default.
- `paddock_sdk::http_client` (SDK feature `http-client`): blocking `get`,
  `post` and `send` over `wasi:http`, with `HttpError` naming refusals
  (`Denied`, `Limited`, `Prohibited`), timeouts and TLS failures.
  `paddock-fixture-sdk` uses it (`fetch <method> <url>`).
- `paddock_sdk::batch` (ROADMAP F4): a `Batcher` collects items per
  destination and returns one envelope when a batch is full (`max_items`,
  `max_bytes`) or its `max_delay` timer fires (`on_timer`); `items` reads a
  batch back, `envelope` encodes one from a slice. Wire format: a CBOR
  array, schema `paddock/batch`. WRITING_APPS.md says when to batch.
  `paddock-fixture-sdk` gains `produce`, `produce-timed` and `points`;
  `batch_cost` example and `scripts/bench.sh batch` (batches of 100 move 3
  to 4.5 times the points of one cast each).
- `egress_cost` example and `scripts/bench.sh egress`: a guest's requests
  through the pool against a native client, pooled and not.
- `zip` 9 (RUSTSEC-2026-0367, -0368, -0369, all in `ZipArchive::extract`,
  which neither Paddock nor `rustbridge-bundle` calls), through
  `rustbridge-bundle` and `paddock-bundle`'s test helpers.
- Hash routing (`instances.routing = "hash:<field>"`) reads the routing
  field of a CBOR body without decoding the rest (ROADMAP B8 follow-up);
  the same key as before for every body, checked against the full decode
  by property tests. `modbus_load` stores about 25% more updates with
  about 12% less CPU.
- CI runs the test job on Linux arm64 too (`ubuntu-24.04-arm`, ROADMAP G3).
- Faster calls into an app's pool (ROADMAP B8): picking an instance no
  longer copies the supervisor's child list on every message, which every
  host-to-app call and every HTTP request paid.
- HTTP ingress (ROADMAP B8): one app and handler lookup per request, the
  request envelope encoded straight from the request (the same bytes,
  without copying headers or a CBOR value tree), and no body timer for a
  request without a body. On the 16-thread machine: 78k to 85k requests/s
  at 32 connections with the SDK fixture, one-connection p50 86 to 79 µs.
- `quic_floor` example (bare quinn ping-pong, the floor under a call
  between nodes) and `ingress_throughput`'s `s15` and `chaos` modes, in
  `scripts/bench.sh`'s `cluster` and `ingress` runs.
- `paddock failover confirm`'s help and OPERATIONS.md ("Confirming a
  promotion") state when to confirm: only with the old primary down and
  kept down until the pair hears each other, since an old primary that
  restarts and reaches the witness resumes beside the confirmed node
  (found by V3's two-fault simulations, VERIFICATION.md finding 6;
  accepted).
- Failover serial heartbeat path (ROADMAP F5): `[failover] serial = { device,
  baud }` in node config gives an HA pair a second heartbeat path over a
  null-modem cable, so a network cut with the serial line up is a partition
  (nobody promotes). Beats are COBS-framed with a CRC-16 and both node ids;
  the node reopens a device that fails, every second. `paddock::serial`: a
  raw tty layer of Paddock's own on `rustix` termios (Unix only; the common
  serial crates are MPL-2.0), pty pairs, and a relay that can cut a line.
  `paddock failover` and `FailoverInfo.paths` show each heartbeat path to
  the partner, heard or silent. `paddock-soak failover --serial pty|DEV_A,DEV_B`
  (`scripts/soak.sh --serial`) adds the line to the failover soak and cuts it
  among the other faults. Tests run on ptys; with
  `PADDOCK_SERIAL_PAIR=DEV_A,DEV_B` they also run on a real cable.
- HTTP handlers (ROADMAP E6): an unmodified `wasi:http/proxy` component
  (exporting `wasi:http/incoming-handler` at any 0.2 version) deploys as an
  app whose routes start a new instance per request, with streaming request
  and response bodies, beside the requests-as-calls ingress. The world is
  read from the component's exports (`paddock_bundle::HTTP_HANDLER_WORLD`);
  the manifest must declare routes, and may not declare listeners, groups
  or a placement other than `any`. The route's `timeout_ms` runs until the
  response headers and again as an idle timer while either body moves;
  `max_body` bounds the request body as it is read; a trap or timeout
  answers 500 or 504 before the headers and cuts the stream after them;
  each request holds a Store slot (none free: 503 with `Retry-After`).
  Upgrades and rollbacks apply to new requests (mode `new-requests`).
  Imports follow the usual declare-and-grant rules; `paddock:runtime/process`
  is always available. `Runtime::load_handler`,
  `HttpIngress::set_handler`/`remove_handler`/`handler_requests`, metric
  `paddock_http_handler_failures_total`. Needs the `http` and `egress`
  features. `paddock-fixture-http` is the test guest;
  `scripts/bench.sh ingress` now also measures a handler.
- Passivation for apps (ROADMAP E7): manifest `instances.passivate_after_ms`
  turns S11's passivation on for an app, and node config `max_processes`
  lets a node hold more instances than `max_instances` Store slots. A deploy
  is refused when the instances that never passivate would need more slots
  than `max_instances`; a start, wake or upgrade that finds no slot within
  `init_timeout_ms` fails (`crashed: no_slot`; an upgrade reverts), and an
  upgrade's second Store now takes a slot. Metrics
  `paddock_instances_passive`, `paddock_passivations_total`,
  `paddock_wake_seconds`, `paddock_slot_timeouts_total`. Each instance
  costs less memory: operations queue boxed, and instantiation futures are
  boxed (a passivated instance 19 KiB instead of 25, an idle live one 62
  instead of 69). The SDK fixture keeps its casts and notices across
  snapshots.
- Planned switchover (ROADMAP D10b): `paddock failover switchover` (and
  `POST /v1/failover/switchover`) on the primary stops its singletons
  gracefully, with `terminate`, and hands over: it steps down and beats
  that it yields, and the standby asks the witness at once rather than
  after a node timeout, so the singletons move within about a lease. The
  old primary rejoins as standby. Refused, with nothing stopped, unless the
  node is a primary with a lease, a witness arbitrates and the partner is
  heard as a standby. `FailoverHandle::switchover` and `check_switchover`,
  `SwitchoverRefused`, `Apps::take_out`; heartbeat statuses gain a `yields`
  flag (the promoting byte becomes flags). The gate logs `singleton
  stopped`, which `paddock-soak check` reads. The TLA+ model gains the
  action (`Switchover`, `MaxSwitchovers`).
- The TLA+ failover model's leases were zero (found 2026-10-15): with
  `Lease = 3`, `Margin = 1` and `Delay = 1` every lease came from a frame two
  ticks old, so since 2026-10-11 no run checked two leased runners. Layer 2
  configurations now use `Lease = 4`, and `L2_Vacuity` must find a leased
  runner (`docs/tla/VERIFICATION.md`, bug 5).
- `paddock failover --watch` (ROADMAP D11, step 3): a line now and one
  per leadership change (role, epoch, whether singletons may run), JSON
  lines with `--json`, `role=unreachable` while the node cannot be reached;
  through a long poll, `GET /v1/failover?after=<generation>&wait_ms=<ms>`
  (`FailoverInfo::generation`, `Client::failover_after`). An example agent
  that moves a virtual IP with it, `examples/systemd/paddock-vip.sh` and
  `paddock-vip.service`; keepalived following Paddock's role is documented
  as the alternative (OPERATIONS.md).
- A standby can forward instead of redirecting (ROADMAP D11, step 2):
  `[http.<name>] standby = "forward"` sends a request for a singleton that
  runs on the partner over the cluster to the app there, whose pool routes
  it, and answers with its reply; `"redirect"` and `"refuse"` name the
  existing behaviours (unset: redirect with `partner_url`, else refuse).
  New frame `AppCall`, wire format version 5 (version 4 is no longer
  spoken); `Runtime::call_app_on`, `http::Standby` (replacing
  `IngressListen::partner_url`); metric `paddock_http_forwarded_total`.
- `scripts/bench.sh` (ROADMAP B7): runs every benchmark one at a time,
  each under GNU time for CPU time and peak memory, and writes a dated
  section for `docs/BASELINE.md` to `target/bench/`; refuses to start on a
  busy machine unless `--busy`, which marks the numbers provisional. New
  benchmarks: `cluster_throughput` (calls, casts and payloads between two
  nodes over QUIC), `ingress_throughput` (the HTTP ingress), `publish_cost`
  (a publish to 1 to 1,000 group members), and `watch_cost ... disk`
  (revision writes on a `disk` bucket).
- Passivation, off by default (spike S11, `spikes/s11`):
  `InstanceSpec::passivate_after` passivates an idle instance (its
  `save-snapshot` kept, its Store dropped, lifecycle state `Passive`), and
  the next message, signal or operation wakes it with `load-snapshot` of its
  own version, keeping its pid, mailbox, links, names and timers.
  `RuntimeConfig::max_processes` lets more instances exist than Store
  slots; an instance waiting for a slot has an idle one passivate early.
  Connection handlers now hold a Store slot too. `Runtime::store_count`;
  example `passivation_cost`. Not yet in manifests or node config.
- The standby redirects to the primary (ROADMAP D11, step 1): with
  `partner_url` on an `[http.<name>]` listener, a request for a singleton
  that runs on the partner is answered `307 Temporary Redirect` to the
  same path and query there, while the partner is heard; otherwise 503 as
  before. Clients that follow redirects can use either node's address.
  Metric `paddock_http_redirected_total`; `IngressListen::partner_url`,
  `HttpIngress::set_elsewhere`. Tutorial chapter 2 shows it.
- Tutorial chapter 3 (`examples/tutorial/03-hot-standby.sh`): the board
  example as a hot standby (`examples/board/hot-standby.toml`), its
  primary putting each write on the standby's board through a process
  group before answering, so the count survives `kill -9` of the primary.
- Process groups across the cluster, as OTP's `pg`: `members` lists
  members on every node, and `publish` reaches them, one copy per node
  with members, which fans it out. Each node sends the others a snapshot
  of its members per session and each join and leave; a node cut off
  stops being listed until it is heard again. `publish` now skips members
  the messaging ACLs keep apart, on one node too.
- The remote admin API (`[admin]` in node config): the admin socket's
  routes over TCP with TLS 1.3 and pinned keys both ways, for operators on
  other machines. Operators' public keys go in an operators file (reread on
  SIGHUP and `paddock reload-tls`); the CLI reaches a node with `--remote`,
  `--node-key` and `--key` (or `PADDOCK_REMOTE`, `PADDOCK_NODE_KEY`,
  `PADDOCK_KEY`). Each change is audited with the operator's name.
  `Client::remote`; `admin_server_tls`, `admin_client_tls`,
  `parse_operators` in `paddock_host::transport::quic`.
- Messaging ACLs (manifest `messaging_acl`, by app name): a call, cast,
  `process.exit` or link between apps passes only when the sender's
  `may_call` and the target's `accept_from` allow it (empty: any); a
  refused call fails with `access-denied`, a refused cast or exit is
  dropped, a refused link exits at once with `noproc`. Checked on the
  target's node, also across nodes; instances of the same app, the host
  and monitors are not checked. A node drops a frame whose sender pid is
  on another node.
- Backpressure between nodes: `[cluster] send_buffer_bytes` (64 MiB by
  default) bounds the messages queued for each peer. Past it, a cast is
  dropped and counted and a call fails with `overloaded`, as at a full
  mailbox; link and monitor notices are still queued, so congestion no
  longer ends a session and drops every link and monitor with that node.
  Metrics per peer: `paddock_cluster_send_queue_bytes`,
  `paddock_cluster_send_refused_total`. `Mesh::set_send_budget` gives the
  in-memory transport the same budget for tests.
- The SCADA example as Phase 3's reference app (`examples/scada`, see its
  README): `hmi` serves the point table over HTTP with long polls
  (`GET /scada/points?after=<revision>&wait_ms=<n>`), and a new `control`
  singleton raises alarms from the table (rules in config, and one per
  device offline), takes acknowledgements and device writes, and keeps an
  event log, in two versions with a snapshot migration each way.
  `examples/scada/drill.sh` runs a release cycle (upgrades, a switchover, a
  rollback, the primary lost mid-upgrade) on a pair and a witness and
  reports failed requests and point update gaps per step.
  `paddock-modbus`'s `fake_device` example serves fake devices to it.
- `spikes/s20`: a TLA+ model of replication within the HA pair (ROADMAP
  K1), with the commit, read and catch-up rules K2 implements.
- `spikes/s19`: native-process plugins as ports (ROADMAP N2): a runner
  that loads a rustbridge plugin and serves it over stdin and stdout, a
  plugin that misbehaves on request, and the node's side measuring calls,
  failures, Landlock and cgroup limits.
- The witness keeps a record that the primary's partner is out of date,
  for replicated buckets (ROADMAP K2, R13 in `docs/tla/VERIFICATION.md`):
  set and cleared by the primary at its epoch (`FailoverMsg::Record`),
  kept in `witness-state` (` outdated`), and while it stands the witness
  does not promote the partner and a planned switchover is refused
  (`SwitchoverRefused::PartnerOutdated`). `Leadership::partner_outdated`
  shows it on the primary.
- Replicated shared buckets (ROADMAP K2): `scope = "cluster"` in
  `[buckets.<name>]` (with `durability = "disk"`, on a pair with
  `arbiter = "witness"`) keeps the bucket on both nodes. Writes from
  either node commit on the primary once the standby has them on disk, or
  alone once the witness has recorded the standby out of date, and
  return then; reads and watches use the node's own copy; a standby that
  falls behind catches up from the log or a snapshot. Without a primary
  holding its lease, writes fail (`paddock:watch` `unavailable`).
- The replicated log is bounded by bytes and age (`[failover]`
  `replication_log_max`, default 64 MiB, and `replication_log_age_s`,
  default an hour; `FailoverConfig::replication_log_bytes` and
  `replication_log_age`): within both a partner catches up from the log,
  past them the oldest committed entries are dropped and it catches up
  from a snapshot. `paddock failover` shows the log's size
  (`ReplicationInfo::log_bytes`).
- `replication_cost` (`scripts/bench.sh replication`): writes to a
  replicated bucket on a pair and a witness over QUIC, against a disk
  bucket (BASELINE.md, 2026-10-18).
- `examples/ledger`: text entries in a shared bucket by `paddock call`
  (`put`, `get`, `latest`, `keys`), the example for `cluster` buckets.
- `paddock-soak replication` (and `scripts/soak.sh`'s `replication` kind,
  `--replication-minutes`): the failover soak's witness and pair under
  the same faults, with a `cluster` bucket the ledger example writes
  through both nodes; after each heal it fails unless both copies list
  the same keys, every acknowledged write is among them, and no write the
  client was told was refused appears.
- `paddock:watch` `get-linearizable` (`paddock_sdk::watch::get_linearizable`):
  a read served by the HA pair's primary on a replicated bucket, the same
  as `get` elsewhere.
- Replicated app buckets (ROADMAP K3): `durability = "replicated"` for an
  app's own `wasi:keyvalue` buckets, kept whole on both nodes of an HA
  pair in the log `cluster` shared buckets use, with the same quotas,
  watches and `get-linearizable`. Any placement. Node config
  `[failover] replication = true` (witness arbiter; implied by a `cluster`
  bucket) runs replication; a deploy with a replicated bucket is refused on
  a node without it. A singleton opening a replicated bucket (its own or a
  `cluster` one) starts on a new primary only once that node's copy holds
  every committed write (`ReplicationStatus::ready`,
  `Runtime::replication_updates`), logging `singleton waiting` meanwhile.
- A planned switchover hands over in a few heartbeats instead of about a
  lease (ROADMAP L1): the old primary releases its lease at the witness
  once it has stepped down (`FailoverMsg::Release`), and the witness
  grants the standby at once (rule R14 in `docs/tla/VERIFICATION.md`).
  Wire protocol version 7, the only one spoken: the witness and both nodes
  are upgraded together.
- A node speaks the previous wire protocol version as well as its own
  (ROADMAP U1; DESIGN.md, "Mixed versions during a release"): version 6
  and 7 today, so a pair, or a node and its witness, can run builds one
  release apart. A frame newer than the version a connection agreed on is
  refused before it is written (`SendError::Unsupported`,
  `transport::wire::since`) instead of closing the connection at the
  older node, and `Transport::version` says which version each peer
  agreed. A witness on version 6 gets no lease release, so a switchover
  then waits the lease out, logged as `the witness's version has no lease
  release`.
- The pair's version for what it keeps (ROADMAP U1): a record in the
  replicated `@release` bucket, at 6 until raised. `paddock release`
  (`GET /v1/release`) shows the versions the node speaks, each connected
  member's and the pair's; `paddock release finalize`
  (`POST /v1/release/finalize`, `Runtime::finalize_formats`) raises the
  pair's version to the build's once the partner and the witness are
  connected at it, audited as `release_finalized`. A node refuses to start
  a build older than the pair's version (`Runtime::check_formats`). A
  standby that applies a pair's version newer than its build, from the
  log or a snapshot (rolled back while cut off from the raise), stops
  replicating (`Runtime::formats_outrun`), and `paddock node` shuts down
  with an error.
- `paddock release upgrade <app> <bundle> --pair <file>` and `paddock
  release rollback <app> --pair <file>` (ROADMAP U1): an app's release
  across an HA pair, driven over each node's admin socket or remote admin
  API as a pair file lists them. The standby first, then the primary in
  place, with a gate after each step; a refused step or failed gate rolls
  back what the run changed (`--no-abort` leaves it), and a run stopped by
  a node out of reach continues when run again.
  `VerifiedBundle::unverified_identity` reads a bundle's name and version
  without verifying it.
- `paddock-soak scada` (`scripts/soak.sh`, kind `scada`; ROADMAP X2): the
  failover soak's witness and pair under the same faults, running the
  SCADA example against two fake Modbus devices with HTTP probes on both
  nodes, and between fault episodes a release of `control` across the
  pair (every other upgrade with the primary lost between its steps) and a
  planned switchover. A planned step fails the run if it fails a point
  read, leaves a point gap of 1 s or more, or fails a request to
  `control` outside a switchover's hand-over (1 s); a heal fails it if
  other than one node runs `control`, the acknowledged alarm is lost, or
  the release does not converge. `scripts/soak.sh` now builds `paddock`
  with `modbus`.
- `paddock release upgrade --standby-only` and `release rollback
  --standby-only` change the standby and pause (outcome `paused`) before
  the primary's step, for a run without the flag to finish. The SCADA
  drill (`examples/scada/drill.sh`) runs its upgrades and rollback through
  `paddock release`, and loses the primary between a release's two steps.
- Blue-green upgrades (ROADMAP U2): an app whose new manifest says
  `upgrade.mode = "blue-green"` runs the new version beside the old under
  a root supervisor of its own, and the app's pool sends the new version a
  share of the messages to the app by name (calls, casts, HTTP routes,
  forwarded requests); hash routing keeps each key on one side. `paddock
  upgrade --steps 10,50,100` moves the share step by step, watched with
  `--observe-ms` and `--max-restarts`, and stops the old version at 100
  (after `--drain-ms` for its mailboxes); a failed step, or a new version
  that gives up, stops the new one. `paddock traffic <app> <percent>`,
  `--promote` and `--drop` (`POST /v1/apps/{app}/traffic`) change an
  upgrade left below 100, `paddock rollback` drops it, `paddock status`
  shows it, and a node that restarts starts both versions at the same
  share. `Apps::start_new`, `set_traffic`, `promote`, `drop_new` and
  `AppHandle::new_side`; `AppHandle::supervisor` now returns an owned
  handle. Refused for singletons, apps with `registered_names` or other
  apps' children, and versions whose HTTP routes differ.
- Singletons move with their state in a planned switchover (ROADMAP M1):
  on a pair with `[failover] replication = true`, a singleton in
  `snapshot` upgrade mode writes its snapshot to the replicated log
  instead of running `terminate`, and the new primary's instance loads it
  in place of `init` (under the upgrade's `from_versions` rule). Every
  start of such a singleton takes the record first, so no state older than
  the last stop is loaded. The moved instance gets a new pid; the old one
  exits `normal`. `Apps::move_out`, `Apps::add_moved`,
  `Runtime::put_handover`, `Runtime::take_handover`,
  `Runtime::moves_state`, `paddock_host::Handover`, `HostError::Handover`.
- Modbus devices polled by an HA pair's primary only: `primary_only = true`
  in a device's node config skips polls while the node does not run
  singletons (the standby, or out of lease), closes the standby's
  connection, and sends every point on the first poll after a promotion.
  `ExtensionNode::runs_singletons` for extensions.
- The SCADA example (`examples/scada`) on the pair's replicated state: its
  point table is a `cluster` bucket polled by the primary alone, and
  `control`'s alarm and event buckets are `replicated`, so an
  acknowledged alarm survives a switchover. `control` now needs a node with
  `[failover] replication = true`. `tests/scada.rs` runs it on a pair and
  a witness too.
- `paddock failover` shows the replicated log (commit and last index, how
  far the standby trails, whether the primary commits alone) and whether
  the partner is recorded out of date (`FailoverInfo::replication`,
  `partner_outdated`); `Runtime::replication_status`. Metrics
  `paddock_replication_commit_index`, `paddock_replication_last_index`,
  `paddock_replication_partner_lag_entries`, `paddock_replication_alone`.
- Guest sockets (ROADMAP N1): `wasi:sockets` is enforced. The manifest's
  `connect` and `bind` rules and `listen` decide every address a socket
  binds, listens on, accepts from, connects or sends to; loopback,
  link-local and unspecified destinations need a rule inside their range;
  name lookups need `connect` rules. `max_sockets` (default 16) caps the
  sockets an app holds open across its instances (`new-socket-limit`).
  Node config `[sockets]` is a ceiling checked at deploy. Metrics
  `paddock_sockets_open` and `paddock_sockets_refused_total`.
- `paddock:io@0.1.0` (`wit/io`), granted to every app: `ready.notify`
  sends the host event `io-ready` with a tag once a pollable is ready, so
  an instance waits on its sockets between callbacks, as Erlang's
  `{active, once}`, and handles messages meanwhile; `ready.cancel`.
- `paddock_sdk::net` (cargo feature `net`): `TcpStream` (`connect`,
  `connect_host`, non-blocking `read`, `write_all`, `notify_readable`),
  `TcpListener` (`bind`, `accept`, `notify_acceptable`), `notify`,
  `cancel` and `ready_tag`. `paddock-fixture-net`, its test guest.
- Native-process apps (ROADMAP N2): `kind = "native-process"` with
  `native.program = "library"` (a rustbridge plugin: calls and casts as
  JSON, by schema id) or `"executable"` (a program on `paddock-native`'s
  app API, with the process functions). The bundle's release variant for
  the node's platform runs as one process per instance through
  `paddock-runner`, with a pid, mailbox, links, monitors and supervision;
  a callback's deadline and pings kill a stuck one, and its exit, a panic
  or an OOM kill become exit reasons. `memory_max` is a delegated cgroup's
  `memory.max` or `RLIMIT_DATA`. A native app may declare only
  `wasi:logging`. Linux only.
- `crates/paddock-native`: the frame protocol between a node and a native
  app, the app API (feature `app`), and `paddock-runner` (feature
  `runner`, default), which restricts itself with Landlock and a seccomp
  filter (no files but its libraries and the app's, no execution, no
  sockets, signals only to itself, non-dumpable) before loading or
  executing the program. `paddock-fixture-native` and
  `paddock-fixture-plugin` are its test apps.
- `paddock bundle` packs a native-process app: its program (a rustbridge
  `.so` or an executable) as the release variant for the machine's
  platform. A native app finds its bundle's `priv/` files in the
  directory `PADDOCK_PRIV` names.
- Node config `[native]`: `runner`, `cgroup` (a delegated cgroup v2
  directory), `pids_max`, `ping_interval_ms`, `ping_timeout_ms` and
  `landlock_optional`. `examples/systemd/paddock.service` installs
  `paddock-runner` and says how to delegate a cgroup.

### Fixed

- A standby back from an outage stalled the primary's replicated writes
  for about the resend timeout (500 ms): each append in flight behind the
  gap was refused at the same place, the second refusal held the primary's
  sending until the timer, and the acknowledgment of the resent entries
  did not release it. An acknowledgment that moves the partner's match
  now does (found by the SCADA drill: the point table stopped for about
  570 ms on both nodes when a standby rejoined).

- A planned switchover's hand-over grew with the replicated log: from
  about 0.4 s to 0.8 s over an hour of SCADA point writes, past 1 s for the
  point table. A new primary counted its send window (4,096 entries) from
  the partner's last acknowledgment, none yet in its epoch, so once the log
  was longer than the window it sent nothing until the resend timeout
  (500 ms), and then resent its whole log from the first entry, a window
  per round trip. It now resends, and counts the window, from its own no-op
  or from where the standby's last refusal put it (found by `paddock-soak
  scada`).

- `paddock release upgrade` rolled back the node it had just upgraded when
  that node's role changed during the step's gate (the standby promoted
  because the primary was lost), or when the node changed did not answer;
  both now stop the run, and the next run continues from the new roles.

- tokio 1.54.1 and ctutils 0.4.5, for RUSTSEC-2026-0386 (`AsyncFd`'s safe
  constructors accepted any descriptor; the serial heartbeat path's tty
  now registers its owned descriptor through `AsyncFd::register`) and
  RUSTSEC-2026-0385; clipboard-win 5.4.3, as 5.4.1 was yanked.

- wasmtime 49.0.4 and rustls-webpki 0.103.16, for RUSTSEC-2026-0375 to
  0379 and 0382 (among them wasi-http content lengths set by guests, a
  Cranelift `br_table` miscompile on aarch64, and certificate checks
  accepting distrusted intermediates).
- A node that missed the members list a seed sent when it joined (lost
  with a connection replaced or cut as it was sent) never learned of the
  members it lacked, and when only the other node of such a pair dials
  (the lower id), the two never connected; an HA pair then stayed apart
  after a heal (found by the replication soak). The same followed when a
  node's join was dropped because its seed was still marked unreachable
  after an outage. Every ten heartbeats each node now sends every member
  it knows to each member it reaches, and a join to each peer it watches
  that is not yet a member.
- The tests `start_apps____dependencies____providers_start_first_and_stop_last`
  and `shutdown____running_children____stopped_gracefully_in_reverse_order`
  failed now and then under load: two waiter tasks recorded the stop order
  as they were scheduled, not as the instances exited. Each now checks,
  when the last instance exits, that the other one already has.
- `whereis` of an app's name could return a member that had just exited:
  the pool still listed it until its supervisor noticed the exit, which
  comes after the exit's notices. A watcher that looked the name up again
  on its `down` got the dead pid, an immediate `noproc` down, and looked it
  up again, in a loop until the restart (the ping-pong example counted up
  to 50 downs for one kill under load). Only live members are returned.
- wasmtime 49.0.3 and rustls 0.23.46, for the advisories published against
  49.0.2 and 0.23.45 (RUSTSEC-2026-0346 to 0349 and 0352 to 0364), among
  them a filesystem sandbox escape in `wasmtime-wasi` when renaming
  symlinks with trailing slashes (RUSTSEC-2026-0352) and several host
  panics a guest could cause.
- Links and monitors between nodes could end up on one node only, so an
  instance was never told that its partner on the other node had exited
  (spike S12's model of links and sessions found four ways):
  - A link crossing an unlink from the other side survived on one node.
    Unlinking a remote pid is now acknowledged, as in OTP 23, and a link
    request crossing it is ignored.
  - An exit or down notice, an unlink or demonitor, or a `noproc` answer
    that could not be sent (the peer counted unreachable for a moment, or
    a full send queue) was dropped while the session went on. Such a
    notice now ends the session with that node, so both sides drop what
    it was about with `noconnection`.
  - A QUIC connection replaced by a redial lost the frames queued on it
    without ending the session. Its close now ends the session too.
  - A restarted node could link to a peer it had not yet heard from, and
    the peer could drop that link without the restarted node learning of
    it. Links and monitors with a node now fail with `noconnection` until
    a frame from its current start has been heard.
- The test `link____partner_exits_normal____non_trapping_instance_keeps_running`
  failed about one run in thirty: the instance asked to exit could stop
  before replying. It now accepts either.
- A singleton deployed to, or upgraded on, a node that does not run it was
  not checked until a promotion tried to start it: a missing dependency
  failed then, and the node stayed primary without it. It is now checked
  as a start would be and refused at once. A start that fails at
  promotion (a dependency stopped meanwhile) is tried again, waiting 1 s
  then twice as long each time up to 30 s, until it starts or the
  leadership changes.
- A rollout that reverted but could not put the pool's template back, and
  an upgrade whose HTTP routes could not be put back, said nothing: later
  instances would run the version rolled back from, or the app lose its
  routes. Both are now logged and added to the upgrade's error.
- Failover: a grant that arrived after the standby had asked again
  started its lease from the later request, so the new primary's lease
  could run past the end the witness counted. Requests are now numbered,
  the witness echoes the number in `Granted`, and the lease runs from when
  that request was sent. The witness's answers no longer need to arrive
  within a lease. The failover messages `Request` and `Granted` gain a
  `seq` field: the pair and the witness must run the same version.
- Failover witness: a grant is now written to disk before the witness
  applies it. Before, the epoch and promoted node changed in memory first,
  so a failed write left memory ahead of disk and a `Fenced` answer could
  carry an epoch a restart would lose. While a grant is being written the
  witness answers nothing from the pair.
- Failover: a standby whose request's answer was lost, and which then
  heard its primary again, kept asking and never again confirmed the
  primary's lease. A request past its retry time now counts as answered.
- Cluster transport: a dial whose peer completed the QUIC handshake but
  never sent its hello waited forever, since keep-alives kept the
  connection open, and the redial loop waiting on it never tried again.
  The soak met it when a handshake held in the network arrived after the
  process that answered it was replaced at the same address: the node
  never reconnected, and no node ran the singleton. The hello exchange,
  dialing and accepting, now gives up after the idle timeout.
- Failover: a primary paused past its lease (`SIGSTOP`, a stalled host)
  ran its singleton's callbacks for a moment after it continued, until
  the gate killed it, while the standby already ran it. Singleton
  instances now check the lease against the monotonic clock before each
  callback and at each epoch tick: out of lease a message waits, and a
  callback running at the lease's end ends with `crashed: lease_ended`.
  The node logs `failover lease ended` when it notices a lease end late,
  and `failover lease extended` (debug, target `paddock::lease`) as the
  lease moves; the soak's check reads both to end a run at the lease.
- A deploy, upgrade or rollback that lists a registered name another
  deployed app lists is refused; before, the first instance to register a
  shared name got the other app's messages.
- A node of an HA pair takes failover answers only from its configured
  witness; before, any cluster member's `Granted` promoted it.
- Guest text written to the node's log is escaped: a newline or other
  control character in a log message or context, a stdout or stderr line,
  or a crash reason no longer starts what looks like a new host record.
- A guest's stdout and stderr had no rate limit; they now count against
  the app's log budget.
- The node's text log no longer carries colour codes into journald or a
  file.
- `paddock_sdk::connection!` and the connection fixture export the handler
  only in Wasm builds, so `cargo build --workspace --all-targets` links on
  the host: the export's symbol has a `:`, which native linkers refuse.
  `scripts/check.sh` gains a `build` check, run in CI on every OS.
- Failover: an operator's `paddock failover confirm` given while nothing
  was wrong stayed in force, and on a later cut of network and serial line
  promoted the standby while the primary still ran, so both ran the
  singleton. A confirmation now lapses when the standby hears its partner
  again, steps down or rejoins.
- `paddock`'s `board` and `failover` tests declare the `node` feature they
  need, so the crate's tests build again without default features.
- A node that left the cluster cleanly and restarted could not rejoin: the
  closing connection re-created the departed peer with a raised session
  number, so the new start's join was dropped as stale.
- A node that restarted after a crash, or started after a peer had stopped
  waiting for it, never rejoined: the peer still marked it unreachable and
  refused every reply to its join. The first frame from a new start of a
  node now resets what was known of it.
- A graceful leave could be lost: a peer closed the connection when the
  first of its two streams finished, discarding a `Leave` still unread on
  the other, and kept the departed node as a member. A peer now closes once
  both streams have finished, and the leaving node waits for that close.
- `scripts/bench.sh` runs on arm64 Linux: it exited before its first
  benchmark when `/proc/cpuinfo` had no `model name` line, before its
  fallback to the board's name could run.

### Changed

- Node config `[buckets.<name>]` names follow the manifest's rule
  (lowercase letters, digits, `-` and `_`); any other name, which no app
  could open, refuses the start.
- Applying committed entries of the replicated log is no longer synced
  to disk (the entries are synced in both nodes' logs already): a
  replicated write takes about 2.3 ms at p50 on loopback instead of 3.6 ms
  (BASELINE.md, 2026-10-18).
- Cluster wire format version 6: replication between the pair
  (`Replication`) and the witness's record (`Record`, `Recorded`, and
  `Granted`'s `outdated`). Nodes speak version 6 only.
- Breaking (`paddock_host::failover`): `WitnessPersist` takes a
  `WitnessState` (epoch, promoted node, record), and
  `WitnessConfig::resume` is an `Option<WitnessState>`.
- Cluster wire format version 4: group frames (`GroupSync`, `GroupJoin`,
  `GroupLeave`, `GroupPublish`). Nodes speak version 4 only.
- Cluster wire format version 3: casts, calls, exit signals and links
  carry the sender's app and `may_call` (`acl::Stamp`). Nodes speak
  version 3 only, so a cluster upgrades all its nodes together.
- `Transport::send` returns `Result<(), SendError>` (`Unreachable`,
  `Busy`, `Full`) instead of a `bool`. A call to a node whose link is
  congested fails with `overloaded`, not `noconnection`. The QUIC data
  stream's queue is bounded by bytes of messages, not 4,096 frames.
- Cluster wire format version 2 (unlink acknowledgement: `Unlink` carries
  an id, and `UnlinkAck` answers it). Nodes speak version 2 only, so a
  cluster upgrades all its nodes together.
- Linking or monitoring a pid on a node fails at once with `noconnection`
  until a frame from that node's current start has been heard; right
  after a node starts, that is until its first heartbeat.
  `PeerStatus::contacted` says whether it has.
- `Apps::kill` returns whether the app was there to kill, instead of an
  error for one that was not: the gate no longer has a failure path that
  only a race could reach.
- Debug and test builds carry line tables only, not full debug info: test
  binaries about a third the size, and less memory to link them (the
  Raspberry Pi 5 ran out at the default four jobs).
- The Modbus extension is no longer in a default build: build `paddock`
  with `--features modbus`.
- `paddock list`, `paddock status`, `paddock logs` and `paddock log-level`
  (and their admin API routes) no longer wait while a deploy, upgrade or
  rollback verifies, compiles or rolls out a bundle. Changes still run one
  at a time.
- Failover with a witness: a standby asks for an operator's
  confirmation (`awaiting_confirmation`, `paddock failover confirm`) only
  while its partner is silent on every path and the witness cannot be
  reached either; while the witness can be reached it decides alone.
  `paddock failover confirm` is ignored, and says so, when the node is not
  waiting for one. A confirmation once taken is final: it promotes the
  standby when the partner's lease has run out even if the witness is
  reached again meanwhile, and lapses only if the partner is heard first.

- The Modbus server accepts masters from loopback only unless
  `server.allow` lists their networks.
- The node's cluster key and the signer key `paddock bundle` reads are
  refused if their group or others may access them, as ssh does:
  `chmod 600` them. Keys written by `paddock keygen` and `paddock cluster
  keygen` already are.
- A manifest `quota` on a `wasi:filesystem` preopen is refused; it was
  accepted with a warning and never enforced. Give a writable volume a
  filesystem quota instead (MANIFEST.md).
- A bundle with a `priv/` path that is not plain relative names (`..`,
  `.`, a root, an empty part, `:` or a backslash) is refused by Paddock
  itself, on open and again on extraction, not only by rustbridge-bundle.
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
- Failover of singletons between an HA pair (`paddock_host::failover`):
  `Runtime::start_failover` and `serve_witness`, three arbiters (a
  witness with leases, heartbeat paths, an operator), `FailoverHandle`
  with the node's leadership (`may_run`) and an operator's `confirm`.
  Follows spike S16's rules; not yet wired to the node's apps.
- `docs/tla/`: TLA+ verification of failover, in layers. Layer 1
  is spike S16's model; layer 2 models the code with time, in rounds.
  `VERIFICATION.md` lists configs, results, bugs found and the
  requirements each assumption puts on the code. Layer 2 models kept
  epochs (`PersistEpoch`) and a witness that restarts without its renewal
  record (`ForgetRenewals`), as the node now runs.
- Failover fixes found by layer 2: the primary renews through its partner
  by an echo of its own stamp, not by a beat's arrival; a standby's
  confirmation time is when its beats stop confirming, recorded before a
  request leaves. The node is now a state enum with one type per standing
  (`failover/node.rs`), its transitions named after the model's actions.
- Heartbeats carry a payload on every path (`HeartbeatPath::beat` and
  `Runtime::heard` take it), read whatever the session it arrives in.
- Pids print as `node.serial.incarnation@creation` (`ps`, the API, logs),
  and `--pid` takes that form or the whole id. Instance ids hold the node's
  creation in their high 24 bits and a serial number in their low 40, so a
  pid kept from before a node restarted never names a later instance.
- `spikes/s16`: a TLA+ model of failover with a lease: roles, epochs,
  the LAN and serial paths, and a witness.
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
- Host time is measured by the Store's call hook (wasmtime's `call-hook`
  feature): time inside any host import, `wasi:io` waits included, no
  longer counts against `callback_deadline_ms`. A guest that sleeps or
  waits on a stream or an HTTP response past its deadline is no longer
  killed with `timeout`. `ServiceCtx::add_host_time` is removed;
  extensions need not time their host functions.
- The manifest's `wasi:http/outgoing-handler` is no longer reported as
  not enforced.
- `examples/scada`'s `hmi` is an HTTP app instead of a line protocol on a
  listener, and `points` no longer registers `points-api`, answers
  queries or writes to devices (`control` does).

[Unreleased]: https://github.com/jrobhoward/paddock/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/jrobhoward/paddock/releases/tag/v0.3.0
