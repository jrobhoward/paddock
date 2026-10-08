# Paddock: Roadmap

2026-10-07 · Companion to [DESIGN.md](DESIGN.md)

The single plan for remaining work. It replaces `docs/PHASE1_PLAN.md`, which tracked Phase 1 to completion (see git history up to commit `a1f84c8`). DESIGN.md holds the design and its rationale; this file holds what is left to build, in what order, and which part of the code each piece of work touches.

## How to use this plan

- Work is grouped in tracks. Each track names the code areas it owns ("Code areas" below). A change that needs an area outside its track is a signal to stop and record it here, not to widen the change.
- Tracks have numbered items. Tick an item when it lands with passing tests and docs (CLAUDE.md, "Definition of Done").
- Ideas that are not scheduled go under "Parked", with enough detail to pick them up later.
- Sizes are relative: S = days, M = about a week, L = more than a week.

## Status

Phase 1 (single node) is complete as of 2026-10-06. All nine exit criteria pass in CI on Linux, through a running node's admin API (`crates/paddock/tests/exit_criteria.rs`) and at the runtime level (`crates/paddock-host/tests/`):

1. Kill and restart: new incarnation, `exit` to links, `down` to monitors, `noproc` for stale pids.
2. Crash loop: restart intensity exceeded, escalation, app marked `failed`.
3. Runaway code: killed by the epoch deadline with `timeout`; the node stays responsive.
4. Memory cap: `memory_limit`, other apps unaffected.
5. Overload: dropped casts counted, calls fail fast with `overloaded`.
6. Upgrade: counter v1 → v2 with state, pid, links and name kept; a broken v3 reverts.
7. Capabilities: undeclared imports refused at deploy; ungranted imports denied at call time.
8. Storage and files: `disk` buckets survive crashes and restarts; `/priv` read-only, `/data` writable, no path escapes.
9. Signatures: tampered bundles refused.

Baseline numbers are in [BASELINE.md](BASELINE.md).

Phase 2 is split in two, so that single-node platform work does not wait on clustering:

| Phase | Tracks | Exit |
| --- | --- | --- |
| 2a: platform on one node | 0, A, B, E, F, G | An app serves HTTP, reads a secret, logs and exports metrics on one node, and an operator can trace its messages on a live node; a protocol extension (Modbus) feeds device data to a hash-routed pool; baselines exist for a large multi-socket machine and a Raspberry Pi 5 |
| 2b: cluster | C, D | A primary and standby node fail over on node loss, then a three-node cluster passes partition tests (DESIGN.md roadmap) |
| 3: application server | see "Phase 3" | One real workload runs a full release cycle with zero downtime |

## Order

```
0 ─► A ─┬─► B ──────────────────────┐
        ├─► F (F1 spike first) ─► E1 ┼─► Phase 2a done
        ├─► E (E2…) ────────────────┤
        ├─► G ──────────────────────┤
        │                           │
        └─► C ─► D (failover first) ┴─► Phase 2b done
```

Track 0 comes first because the crate consolidation (0.1) moves files that every later track touches. Track A comes next because B, E and F add host code that should follow A's service pattern. B, C, E, F and G can then run in parallel: they touch different areas. E1 (HTTP ingress) builds on F's listeners. D needs C, and starts with failover (DESIGN.md, "Scaling and where code runs").

## Code areas

An area is a unit of focus: a set of modules with a narrow interface to the rest. Areas are modules inside crates, with `pub(crate)` and a small `pub` surface, rather than one crate each.

| Area | Where (after 0.1) | Interface to the rest | Owned by |
| --- | --- | --- | --- |
| Contract | `wit/`, WIT rules tests | The `paddock:runtime` world; versioned, never changed in place | Any track, with the WIT rules |
| Manifest and bundle | `paddock-bundle` (`manifest`, `bundle`, `policy`, `grant`) | `AppSpec`, `VerifiedBundle`, the effective grant | 0, E (new manifest sections) |
| Process core | `paddock-host`: `pid`, `table`, `registry`, `links`, `message`, `instance`, `runtime`, `state` (process functions) | Spawn, call, cast, link, monitor, exit; after C1, a delivery interface | C, D |
| Host services | `paddock-host`: `services` (A1), one module per service (`services::config`, `services::keyvalue`, `services::logging`), plus `deny` and `output` | The `Service` trait and `ServiceCtx` (A1); each service owns its state, linking and capability check | A, E |
| Orchestration | `paddock-host`: `apps`, `pool`, `supervisor`, `upgrade`, `lifecycle`, `spec` | `Apps`, `AppHandle`, `UpgradeOptions` | 0 (rollback), D (placement later) |
| Observability | `paddock-host`: `probe` (new in B1) | Probe functions called from the other host areas | B |
| I/O and protocols | `paddock-host`: listeners and streams (new in F2, F3); protocol and serial services as extensions (A5) in their own crates | Host-owned listeners, stream resources, connection-to-instance policies | F, E1 |
| Node and admin | `paddock` (`node`, `api`, `cli`) | Admin API routes under `/v1`, the CLI | 0, B, D |
| Guest SDK | `paddock-sdk`, `paddock-sdk-macros` | `App`, `Ctx`, envelopes, snapshots, logging; services by cargo feature | A, E |
| Fixtures and examples | `crates/paddock-fixture-*`, `examples/` | Guest components for tests | Any track |

## Crates

Two audiences consume Paddock as a library, and each should depend on only what it uses:

- **App developers** depend on `paddock-sdk` (and its macros). Nothing from the host. They install the `paddock` binary for bundle tooling, and can build it without the node.
- **Edge-service developers** (a new host service such as a datasource or a message broker bridge) depend on `paddock-host` and implement its service trait (A1). They do not need the node, the CLI or the SDK.

Apart from that, fewer crates are preferred: modules and visibility keep areas apart, and fewer crates means less to version and publish. The target set is five published crates, down from nine:

| Crate | Contents | Depends on | Audience |
| --- | --- | --- | --- |
| `paddock-sdk` | Guest SDK | `paddock-sdk-macros` | App developers |
| `paddock-sdk-macros` | `#[derive(Snapshot)]` (a proc-macro must be its own crate) | none | App developers |
| `paddock-bundle` | Manifest model (`manifest`), `.rbp` loading and verification, operator policy, effective grant | rustbridge-bundle, wasmparser; no wasmtime | Host, binary |
| `paddock-host` | Runtime, built-in services, the service trait | `paddock-bundle`, wasmtime | Edge-service developers, binary |
| `paddock` | The `paddock` binary: node daemon (`paddock node`), admin API types and client, CLI; also a library to run a node with extra services (A5) | all of the above | Operators |

The `paddock` crate has a default `node` feature. Without it, the binary has the CLI and bundle tooling only and does not build wasmtime. Fixtures and examples stay unpublished (`publish = false`).

## Track 0: Housekeeping (S)

Small items left from Phase 1, done before the structural work.

- [x] **0.1 Crate consolidation.** Merge `paddock-manifest` into `paddock-bundle` (as a `manifest` module). Merge `paddock-node`, `paddock-api` and `paddock-cli` into `paddock`: one binary with a `node` subcommand in place of `paddock-node`, the API types and client as a module, and a default `node` feature. The WIT rules tests stay in `paddock`. No behavior change: every test passes unchanged, apart from paths and the node's command line. Update README, GETTING_STARTED, CLAUDE.md and CI.
- [x] **0.2 `paddock rollback --restart`.** Found in M9. A rollback is an upgrade to the previous version in that version's mode. In `snapshot` mode it runs the older release's `load-snapshot` on the newer release's snapshot. If that fails (the counter example's v1 cannot read v2's format 2), the rollback is refused and the newer version keeps running, so an operator cannot return to a known-good release. Fix: `--restart` (API: `restart=true`) stops the current version gracefully and starts the previous one from `init`, discarding in-memory state. It is audited as a restart rollback and refused without the flag, so losing state is always explicit. Also `paddock upgrade --restart` for the same choice on upgrades, and a refusal message that suggests the flag. Reuses `restart` mode's rollout path.
- [x] **0.3 `placement.mode`** in place of `placement.singleton: bool` (review action A2), still unenforced, with its warning.
- [x] **0.4 Missing baselines.** The cost of epoch interruption against a store without it; many concurrent clients against a pool. Added as a dated section of BASELINE.md. (Admin-API latency is B5; log-record cost is A3.)
- [x] **0.5 Fairness under load.** Done: the default epoch tick is 2 ms (was 10 ms). With 16 workers, a pool of 16 and 256 clients, p99 fell from 10.7 ms to 3.4 ms at the same throughput; a single overloaded instance gives up some throughput (21,900–27,400 calls/s against 24,800–26,400) ([BASELINE.md](BASELINE.md), 2026-10-07). A time slice between messages was tried and did not help. Found in 0.4 ([BASELINE.md](BASELINE.md), 2026-10-07). An instance with a full mailbox runs callbacks back to back and keeps its tokio worker until the cooperative budget (128 mailbox receives) runs out or the 10 ms epoch tick yields it. With 256 clients on a pool of 16, p50 is 51 µs but p99 is 10.7 ms. A `yield_now` after every callback halves p99 but costs over half the throughput, so the fix needs a policy, not a yield per message. Candidates: yield after a slice of guest time (about 1 ms, measured with the existing `CallbackClock`), or shorter epoch ticks while other tasks wait. Done when the 16-worker, 256-client p99 drops well below one tick with throughput within 10% of today's, rerun with `pool_load` and `baseline`. Process core area.

## Track A: Host service architecture (M)

Today every host service is wired through the same files: one host world (`crates/paddock-host/wit/host.wit`), one `bindgen!` (`bindings.rs`), one store-data type holding every service's fields and host-trait impls (`state.rs`), shared state on `RuntimeInner` (`kv`, `log_limits`, `log_levels`), one `base_linker`, and per-service fields on `InstanceSpec`. A new service touches all of them. Track A gives each service its own module with a fixed shape, so a later service is added without editing the process core or orchestration.

- [x] **A1 The service pattern.** Done: `paddock_host::services`, with `wasi:logging` moved onto it as the worked example (DESIGN.md, "Host service pattern"). The `Service` trait is crate-private until A5 decides what services outside the crate need. Original scope: a module defining what a service is:
  - its capability (`Capability`), checked in one place;
  - its node-wide state (pools, connections, limits), built once per runtime from node configuration;
  - its per-instance state, built from the instance's spec. Built-in services may keep a concrete field on the store data; services from other crates get a typed slot;
  - its linking: a `bindgen!` per service for its own interfaces, linked with the per-interface `add_to_linker` and a getter that projects onto its own state, instead of one `PaddockApp::add_to_linker`;
  - its manifest and node-config sections, and its denial behavior.

  Linking runs once per engine, so the hot path stays a direct call on the service's own state; no dynamic dispatch per host call. Write the pattern up in DESIGN.md ("Host services") with the rules every service follows: named resources declared in the manifest and bound in node config (the JBoss datasource model), `access-denied` rather than a trap when not granted, time spent in host I/O counted as host time and not against the callback deadline (`CallbackClock::add_host_time`), a quota per app, and a probe point per operation (B1).
- [x] **A2 Move the built-in services onto it.** Done: `services::logging` (A1), `services::config` and `services::keyvalue`, each with its own WIT world; the app world now holds only the callbacks and `process`. `state.rs` keeps the process core and the WASI context (stdout capture, filesystem preopens), which wasmtime's own WASI host serves. Original scope: `wasi:logging`, `wasi:config` and `wasi:keyvalue`, one module each; the `process` interface stays in the process core. Done when the existing tests pass unchanged and `state.rs` holds only process-core state.
- [x] **A3 Guest-side log filtering.** Done: a filtered `debug!` costs 3 ns instead of about 440 ns ([BASELINE.md](BASELINE.md)). Services gained an `announce` hook for host events; logging sends `log-level`, and the instance re-sends after a snapshot upgrade and on `set_log_level`. Original scope: Today `paddock_sdk::debug!` runs `format!` in the guest and copies the string to the host, which then drops it below the app's level; `wasi:logging` has no "enabled" query. The host sends the effective level as `host-event("log-level", …)` after `init` and whenever `paddock log-level` changes it; the SDK handles that event itself, before `App::handle_info`, keeps the level in a static, and its macros check it before formatting. The host stays the authoritative filter, so a guest that ignores the event is still capped, and records logged during `init` are filtered by the host only. Add a `trace!` macro. Measure the cost of a filtered record before and after, in BASELINE.md.
- [x] **A4 SDK wrappers for `wasi:config` and `wasi:keyvalue`**, done: `paddock_sdk::config` and `paddock_sdk::kv` (features `config`, `keyvalue`), tested through `paddock-fixture-sdk` (`crates/paddock-host/tests/sdk_services.rs`). The SDK's world imports both always; checked with `wasm-tools` that an app not calling them has no such imports. Original scope: each behind a cargo feature of `paddock-sdk`, with a fixture that uses both through the SDK. Today an app generates its own bindings, and combining them with the SDK is untested. A component still imports only what its code calls.
- [x] **A5 Out-of-tree services.** Done: `paddock_host::services::Extension`, `Runtime::with_extensions`, `Node::open_with` and `paddock::node::run`; manifests declare `capabilities.extensions."<package>"` and policy grants the package by name. `examples/greeter` (service, app, node) runs end to end. Original scope: A node can be built as a library with extra services: `paddock::node::Builder` (or similar) takes services implementing the A1 trait. One example service in `examples/` built against `paddock-host` alone. This settles DESIGN.md's open question on shipping the server as a library.

- [x] **A6 Keyvalue reads without the blocking-pool hop.** Done 2026-10-07: read-only operations run on the worker; S14 rerun: 2.38 µs (16 B) and 3.80 µs (1 KiB) per read, against 1.15 and 2.44 µs native. From S14: a `disk` bucket read costs 22–30 µs against 1–2.5 µs for native redb, almost all of it the hop to tokio's blocking pool on every operation; run inline, reads cost 1.2–1.5 µs above native. Run read-only operations on the worker (redb reads usually hit its page cache; a miss blocks the worker for that read), or hand them to a dedicated storage thread more cheaply. Writes stay off the worker (they fsync). Rerun S14 and `pool_load`. Services area.

Exit: a new service is one module (or crate) plus its manifest section, and adding it changes no file in the process core or orchestration areas.

## Track B: Observability (M)

What an operator can see on a live node. The node already has `paddock call` and `paddock cast` (to an app, through its pool), `state <pid>`, `logs`, `ps` with a `DROPPED` count, runtime `log-level`, and the admin API over curl (`curl --unix-socket … http://paddock/v1/…`).

- [x] **B1 Probe points.** A `probe` module in `paddock-host` with one function per event; host code calls these, never a tracing backend directly, so the same call sites can later feed metrics and tracing. Events: `call__start` / `call__done` and `cast` (sender pid, target pid, body size, content type, outcome), `callback__start` / `callback__done` (pid, callback kind, guest execution time), `instance__exit` (pid, reason), `supervisor__restart`, `upgrade__start` / `upgrade__done`, `mailbox__drop`.
- [x] **B2 anyprobe backend** behind a `probes` cargo feature. anyprobe (MIT OR Apache-2.0) emits each platform's native probe format: SystemTap SDT on Linux (bpftrace, perf), DTrace USDT on macOS and FreeBSD, ETW on Windows. A tracer attaches to a running node with no restart; with none attached, a probe costs one enabled check and its arguments are not computed. Probes see what the host sees (pids, sizes, content types, timings), not guest internals or bodies. anyprobe is pre-release, so it can start as a git or path dependency like rustbridge. Document bpftrace one-liners for message tracing in a new `docs/OBSERVABILITY.md`; this covers most of the "message trace" in DESIGN.md's introspection row.
- [x] **B3 Per-instance introspection** in `ps` and `GET /v1/instances`: mailbox depth, memory in use, restart count and last exit reason.
- [x] **B4 Address one instance.** `paddock call` and `paddock cast` with `--pid` or `--name` (registered name) as well as an app; today only an app's pool routing is reachable. Also `--cbor`, which encodes a JSON body argument as CBOR.
- [x] **B5 Measure** admin-API call latency (HTTP on the Unix socket) against the in-process figures in BASELINE.md.
- [ ] **B6 Node watchdog** (deferred 2026-10-07; not scheduled) (review action A4): the epoch ticker thread pats a watchdog, Linux first, so a wedged node is restarted by its service manager.

Exit: on a live node, an operator traces calls between two apps with bpftrace, finds a stuck instance in `ps`, and calls it by pid. B1–B5 done 2026-10-07 ([OBSERVABILITY.md](OBSERVABILITY.md)): probes from anyprobe's GitHub `main`, on by default in the `paddock` binary; `ps` shows memory, restarts and last exit; call and cast by `--pid` or `--name`, and `--cbor`; admin-API latency in BASELINE.md. Attaching bpftrace to a live node is documented but not yet run (it needs root).

## Track C: Delivery seam and test harness (M)

Prepares the process core for remote pids without any networking. Today call, cast, link, monitor, `exit` and the registry resolve pids directly through `ProcessTable`, `Registry` and `LinkTable`.

- [x] **C1 Delivery interface.** Every pid resolution in the process core (call, cast, reply, link and unlink, monitor and demonitor, exit signals, `exit`/`down` notifications, whereis) goes through one interface that answers "local, or to node N". The local implementation is today's behavior; existing tests pass unchanged.
- [x] **C2 In-process transport.** Two or more runtimes in one process connected by an in-memory transport with fault injection: drop, delay, reorder between pairs, partition, heal. Remote links and monitors, and `noconnection` on partition, follow DESIGN.md "Distribution". Done as `paddock_host::transport` (`Frame`, the `Transport` trait, `Runtime::deliver` and `node_down`) and `transport::memory::Mesh` (per-pair delay and jitter in order, random drop from a seed, partition, isolate, heal); `tests/distribution.rs`. Frames between one pair never reorder (the transport's promise); pairs interleave. Open: node-down from a dropped runtime is reported only by `partition`/`isolate`, not detected.
- [ ] **C3 Deterministic tests.** Link, monitor and exit races, and partition and heal, on C2 with tokio's paused clock (or turmoil), repeatable from a seed.
- [ ] **C4 Spike S12.** A TLA+ model of remote links, monitors and `noconnection` with pid incarnations, model-checked with a safe and an adversarial network; invariants listed. Time-box 3 days. If a rule does not hold, narrow the Phase 2 guarantees to what the model shows. Runs before or beside C2.

Exit: two-runtime tests cover remote call, cast, link, monitor and `noconnection` under injected partitions, with no network.

## Track D: Cluster (L)

DESIGN.md "Distribution", built on C's transport interface. Detailed milestones are written when C is done; the list below is the scope.

Order (reprioritized 2026-10-07, DESIGN.md "Scaling and where code runs"): nodes are added for availability first, load second. Do D1, D2, D10, D7, then D3, D8, D5, D6, then D4 and D9.

- [ ] D1 Node identity and certificates (cluster CA, short-lived certs), QUIC transport with mutual TLS (quinn, rustls) implementing C's transport.
- [ ] D2 Membership: static seed list, control stream per node pair, node-down by QUIC idle timeout.
- [ ] D3 Remote pids, calls, casts, links and monitors over the mesh; wire format with protocol version negotiation.
- [ ] D4 Process groups (gossiped, like OTP `pg`).
- [ ] D5 Messaging ACLs (`messaging_acl`), enforced for local and remote senders. Settles DESIGN.md's open question on ACL identity.
- [ ] D6 Admin API over TLS TCP for remote operation.
- [ ] D7 Health exports (manifest `health`).
- [ ] D8 Backpressure between nodes: what a sender sees when the link to a remote node is congested (review finding; DESIGN.md defines only full mailboxes).
- [ ] D10 Failover: enforce `placement.mode` `singleton` and `hot-standby` across two or more nodes. On node-down the standby node starts (or promotes) the app; `hot-standby` instances learn their role through a `host-event`. Apps recover state from their sources (a SCADA app re-polls its field devices) or from `disk` buckets restored on the standby, before replicated storage (Phase 3) exists. Exit: a primary and standby pair, with the primary killed, serves again within a stated time.
- [ ] D9 Spike S11: passivation of idle instances through the snapshot pair (snapshot, drop the Store, rebuild on the next message), for address-space headroom. Time-box 2 days.

Exit: a primary and standby pair fails over on node loss (D10), and a three-node cluster passes partition and node-loss tests.

## Track E: Edge services (M each)

Guest-facing services, each one module on A's pattern, each with its manifest section enforced (removed from the unenforced list), its denial behavior, an SDK wrapper behind a cargo feature, and a MANIFEST.md entry. Independent of each other and of C and D.

- [ ] **E1 HTTP ingress** (`wasi:http/incoming-handler`), on F2's listeners. S15: terminate HTTP in the host and deliver each request to a pooled instance (about 75% of native throughput), not an instance per request (about 15%). The host terminates TLS and routes by host and path. Needs a design note first: how a request maps to an instance in a pool, how it relates to the app world's callbacks and the callback deadline, and whether a request is handled as a call. Spike S13 (host-owned TCP listeners kept across restart and upgrade) informs it.
- [ ] **E2 HTTP egress** (`wasi:http/outgoing-handler`; S15: wasmtime's default sender opens a connection per request, so the host sender needs connection pooling) through a filtering proxy with an allow-list; wasmtime's `wasi:http` host per S7.
- [ ] **E3 Secrets** (`paddock:secrets`), resolved from files or environment first, Vault later.
- [ ] **E4 Metrics** (`paddock:metrics`) for app-defined counters and histograms, and an export (OpenTelemetry or Prometheus) that also carries B1's host events.
- [ ] **E5 SQL datasources** (`paddock:sql`, Phase 3 in DESIGN.md). The host owns a connection pool per named datasource, configured per node, and the manifest requests a datasource and a connection cap. Because datasources live in node configuration, an app restarted on another node connects to the same database. The host-side pool uses a permissive-licensed driver such as `tokio-postgres`. A second route needs no new interface: an app ships its own driver and connects over `wasi:sockets` with a connect rule (Phase 3 sockets), at the cost of per-instance connections and app-managed credentials.

## Track F: I/O and protocols (L)

Getting data into and out of apps at high rates: host-owned sockets, protocol stacks and devices, with apps doing the decisions. From S14, S15 and DESIGN.md "Scaling and where code runs": terminate I/O and protocols in the host, where they use every core; give guests streams or framed messages.

- [x] **F1 Spike S13: host-owned listeners and guest streams.** Done 2026-10-07 ([spikes/s13](../spikes/s13/README.md)): with an instance per connection reading and writing `wasi:io` streams, guests matched native hyper from 4 to 64 connections (158.6k against 157.9k requests/s at 32); a new connection costs about 40–65 µs more (instantiation); a trapping guest loses only its connection; swapping the component under load lost no requests. Not done: attributing S15's per-connection cost (needs `perf` with `kernel.perf_event_paranoid` ≤ 2; moot for an instance per connection).
- [x] **F2 Listener service.** Done 2026-10-07: `paddock-host`'s `listener` module (`Runtime::listen`), manifest `[[appserver.listeners]]`, node config `[listeners.<name>]`; deploy, upgrade, rollback, stop, start, restore and remove for connection-handler apps; `status` lists listeners. Host framing into messages for a pool is not built yet (handlers forward to apps themselves, as `examples/line-server` does). Original scope: Listeners declared in the manifest (name, protocol) and bound to addresses in node config; several accept tasks per listener (`SO_REUSEPORT`), the Ranch pattern; listeners survive instance restarts and upgrades. Connection-to-instance policies: an instance per connection (instances start in about 77 µs), or host framing into messages to a hash-routed pool.
- [x] **F3 Streams for guests** (S13: cheap enough). Done 2026-10-07: the `paddock:connection` package and world, an instance per connection with `wasi:io` streams, idle timeout instead of a callback deadline, crashes closing only their connection, upgrades for new connections only; SDK feature `connection` (`Connection`, `Stream` as `std::io::Read`/`Write`). Not done: a drain timeout for connections open across an upgrade, and `log-level` announcements to handlers. Design input from S13: a connection handler runs for the connection's life, so it needs its own world (with `wasi:io` streams, versioned for WASI 0.3's `stream<u8>`), timeouts per request or idle instead of the per-callback deadline, supervision per connection (a crash closes the connection, no restart), and a drain rule for upgrades (open connections keep their version). an accepted connection as a stream resource an instance reads and writes, for low-rate protocols parsed in the sandbox. Few connections per instance (S15: cost grows with connections per instance under WASI 0.2), so the default policy is an instance per connection, with a cap per instance.
- [ ] **F4 Batching.** Guidance and an SDK helper for sending many updates in one message (one cast costs about 1.3 µs whatever it carries); measure points per second end to end with `pool_load`.
- [ ] **F5 Serial extension.** Named serial ports declared by apps and bound to device paths in node config (the volumes and datasources rule); the host owns the tty and offers a byte stream or framed messages. An extension (A5), in its own crate.
- [ ] **F6 Modbus TCP extension**, on `tokio-modbus` (MIT OR Apache-2.0): polling and serving registers, delivering point updates to a hash-routed pool by device id. The first protocol extension and the Phase 2a exit test. DNP3 and ICCP are parked until their stacks' licenses are reviewed.

Exit: a node polls Modbus devices through F6, routes updates by device to a pool, and an app answers through F2's listener; throughput recorded in BASELINE.md.

## Track G: Scale and platforms (M)

Measured so far only on one 16-core machine. Scale up (80+ cores, likely two sockets) and down (a 4-core, 8 GB Raspberry Pi 5) need numbers before designs depend on them.

- [ ] **G1 Large machine** (tabled 2026-10-07: no machine with more than 16 cores or several sockets is available; until then, no design may assume one core or one socket, see DESIGN.md). `pool_load`, `baseline` and S15 at 32 to 80 workers on a multi-socket machine. Watch for contention in shared structures (process table, registry, pool round robin) and cross-socket work stealing; options if it shows: a runtime per socket with pinned workers, or several nodes per machine.
- [ ] **G2 Raspberry Pi 5 baseline**: `baseline`, `pool_load`, `log_cost`, instances per GB with the dense profile.
- [ ] **G3 Linux arm64 in CI.** macOS on Apple silicon covers aarch64 code generation, not Linux paths such as probes.
- [ ] **G4 Cache compiled components** in the deploy store (wasmtime ahead-of-time compilation), so restarts and small machines do not recompile every bundle.
- [ ] **G5 Dedicated workers** for latency-critical instances (control loops), so bulk traffic cannot delay them. Measure first, with timing inside the server: S15 saw host HTTP plus an instance call add 10–47 µs over native at one connection, which client-side timing was too noisy to explain.

Exit: G1 and G2 numbers in BASELINE.md, arm64 in CI, and a node restart that does not recompile.

## Phase 3

From DESIGN.md's roadmap, not yet broken into tracks: placement (failover of singleton and hot-standby apps moved to D10), opt-in global names (Raft), replicated storage, sockets (after F1), native-process plugins, blue-green upgrades, more guest SDKs, fuel metering. rustbridge work for Phase 3: the guest adapter (`Plugin` → `paddock:runtime/app`), the `Server` trait, and the native-process runner.

## Parked

- **DNP3 and ICCP extensions** (from F6). The mature DNP3 implementations in Rust found so far are source-available with commercial terms; no permissively licensed ICCP (TASE.2 over MMS) stack in Rust is known. Review licenses before choosing a stack or wrapping a C library.
- **rustbridge release** (deferred from Track 0 on 2026-10-07). Switch `rustbridge-bundle` from the `experiment/wasm` git branch to a crates.io release that includes bundle format 1.2. Not needed while Paddock is a private research project; required before the repo goes public or any Paddock crate is published.
- **Spike S9: debugging guests.** Two routes exist in wasmtime 49:
  - **Native debugger (works today).** `Config::debug_info(true)` plus wasmtime's `debug-builtins` feature registers JIT code with GDB and LLDB, so breakpoints and stepping work in guest Rust source built with debug info. Best-effort: some locals are unrecoverable. Attaching pauses the whole node, so it suits a development node only.
  - **Guest-level debug API (experimental).** `Config::guest_debug(true)` instruments compiled code so `Store::debug_exit_frames`, `FrameHandle` locals and stack, `edit_breakpoints` and `single_step` reach the Wasm VM state. `Store::set_debug_handler` takes an async `DebugHandler`, so a breakpoint can park one instance's fiber while the others keep running, which fits one task per instance: an admin-API or Debug Adapter Protocol endpoint could debug one instance on a live node. Mapping file and line to Wasm PCs needs the guest's DWARF (`gimli`); time parked at a breakpoint must count as host time; `guest_debug` is Engine-wide and slows code, so debuggable apps would run on a separate Engine.

  S9 would establish whether `guest_debug` breakpoints and stepping work in wasmtime 49 (its doc comment still calls them unfinished), measure the slowdown, and try one DAP session from VS Code against a parked instance. Time-box 2 days. If the guest-level API is not ready, document the native-debugger route for development nodes.
- **`gen_statem` support in the SDK**: state timeouts and `postpone` on top of the existing callbacks and timers (review). A deferred queue in the SDK may settle DESIGN.md's open question on selective receive.
- **WASI 0.3 world** (`paddock:runtime@0.2.0`, async), when wasmtime and Rust support it (DESIGN.md, "WASI 0.2 to 0.3 migration"). Trigger: when wasmtime marks component-model async stable and Rust has a `wasm32-wasip3` target, rerun the S15 connection sweep on WASI 0.3 before fixing F3's stream API; 0.3 replaces poll lists, which S15 found make each connection an instance holds slow down every request.

## Settled decisions

| Decision | Choice |
| --- | --- |
| Name | `paddock`: crates `paddock-*`, WIT package `paddock:runtime`, CLI `paddock` |
| License | MIT OR Apache-2.0 |
| WASI target | WASI 0.2, sync guest ABI, wasmtime 49.x; 0.3 later as `paddock:runtime@0.2.0` ([spikes/S3-wasi-target.md](../spikes/S3-wasi-target.md)) |
| Async runtime | tokio |
| Persistent store | redb |
| Default envelope encoding | CBOR, with JSON and text allowed |
| Admin API transport | JSON over HTTP on a Unix socket; TLS TCP in D6 |
| Crate layout | Five published crates (see "Crates"); areas are modules |

## Spikes

| # | Question | Outcome |
| --- | --- | --- |
| S1 | Callback model with async host imports | Passed ([spikes/s1](../spikes/s1/README.md)) |
| S2 | Cost of an instance | About 35 µs and 30–40 KiB; address space is the limit ([spikes/s2](../spikes/s2/README.md)) |
| S3 | WASI 0.2 or 0.3 | 0.2 ([spikes/S3-wasi-target.md](../spikes/S3-wasi-target.md)) |
| S4 | Import inspection and deny stubs | Passed, with Paddock's own typed stubs ([spikes/s4](../spikes/s4/README.md)) |
| S5 | Snapshot upgrade | Passed ([spikes/s5](../spikes/s5/README.md)) |
| S6 | rustbridge as a library | Reusable; seven upstream changes, all done ([spikes/s6](../spikes/s6/README.md)) |
| S7 | Off-the-shelf WASI hosts | Paddock writes its own keyvalue, logging and config hosts ([spikes/S7-wasi-host-crates.md](../spikes/S7-wasi-host-crates.md)) |
| S8 | Guest versus native speed | 1.0x to 2.4x for compute, about 2 µs per call ([spikes/s8](../spikes/s8/README.md)) |
| S9 | Debugging guests | Parked |
| S10 | New `info` cases and old guests | Done; `host-event` reserved ([spikes/s10](../spikes/s10/README.md)) |
| S11 | Passivation | Planned, D9 |
| S12 | TLA+ model of remote links and monitors | Planned, C4 |
| S13 | Host-owned TCP listeners and guest streams | Done: guest streams at native throughput with an instance per connection; crashes and upgrades leave the listener untouched ([spikes/s13](../spikes/s13/README.md)) |
| S14 | Storage inside the guest versus host services; cost of a host call | Done: about 140 ns per host import; small keyvalue reads 1.2 µs over native once the blocking-pool hop goes; redb runs in a guest at 1.0–1.5x native in memory ([spikes/s14](../spikes/s14/README.md)) |
| S15 | HTTP in the guest versus the host | Done: host-terminated HTTP plus an instance call keeps about 75% of native throughput; `wasi:http` with an instance per request about 15%; a guest-owned socket is capped at one core; `wasi:http` egress opens a connection per request ([spikes/s15](../spikes/s15/README.md)) |
