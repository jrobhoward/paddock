# Paddock: Roadmap

2026-10-10 · Companion to [DESIGN.md](DESIGN.md)

The single plan for remaining work: what is left, in what order, and which part of the code each piece touches. Finished work is not tracked here: [CHANGELOG.md](../CHANGELOG.md) records what landed and DESIGN.md why. The plan as it stood before 2026-10-10, with the detail of every finished item, is in git history (`git log -- docs/ROADMAP.md`).

## How to use this plan

- **Stages** run in order. Items inside a stage can run in parallel unless one names another. An item found while working on another goes into the stage where it fits, not into the current change.
- **Finishing an item** removes it from this file, adds a CHANGELOG entry, and updates DESIGN.md if a decision changed. A partly done item keeps only what is left.
- **Checks** are one command, `scripts/check.sh` (`--quick` while working); CLAUDE.md's Definition of Done lists what it runs.
- **Interface docs** change with the code: CHANGELOG.md, MANIFEST.md (manifest and node config), OBSERVABILITY.md (probe and metric names) and OPERATIONS.md (paths, variables, signals). Narrative docs (README, GETTING_STARTED, WRITING_APPS, DESIGN prose, ARCHITECTURE, the tutorial) may fall behind: a change that leaves one wrong adds a line to "Doc debt" below, and W1 clears the list once, near the end.
- **Reviews (stage 2)** produce items for stage 3 and decisions for DESIGN.md. A finding is fixed during the review only if the fix is a line or two.
- **Failover changes** follow the verification rule in V1.
- **Sizes** are relative: S = days, M = about a week, L = more than a week.

## Where things stand

Phase 1 (single node) was finished on 2026-10-06; its nine exit criteria run in CI (`crates/paddock/tests/exit_criteria.rs`).

| Phase | Exit | Left |
| --- | --- | --- |
| 2a: platform on one node | An app serves HTTP, reads a secret, logs and exports metrics; an operator traces its messages on a live node; Modbus feeds a hash-routed pool that keeps a point table in a shared bucket other apps watch; baselines on the 16-core machine and a Raspberry Pi 5 (and a multi-socket machine when one is available) | Throughput and baseline numbers on an idle machine and the Pi (B7, G2) |
| 2b: cluster | A primary and standby fail over on node loss; with the network cut nobody promotes; with network and serial cut at most one side runs a singleton; a three-node cluster passes partition tests | D10 (an hour of the failover soak passing), C4, D3 to D9, D11 |
| 3: application server | One real workload runs a full release cycle with zero downtime | See "Phase 3" |

The failover pair and witness run over QUIC between `paddock node` processes (`examples/failover/demo.sh`). The failover protocol is model-checked exhaustively in TLA+ up to two faults ([docs/tla/VERIFICATION.md](tla/VERIFICATION.md)), apart from one rerun (V2).

## Order

```
1 Tooling ─► 2 Review ─► 3 Rework ─► V2 ─► 4 Cluster ─► 5 Platform features ─► 6 Platforms and benchmarks ─► 7 Docs
   (S)         (M)        (M–L)     (20 h)     (L)              (L)                       (M)                  (M)
```

- **Tools first.** The checks, the soak runner and the benchmark runner are each one command, so every later stage runs them instead of re-deriving them.
- **Review before more cluster code.** D3 to D8 add more code than any track so far. Reviewing structure, tests and security first, then reworking, puts that code into its final shape once instead of moving it later.
- **V2 after the rework.** The 20-hour model run is repeated once, after stage 3 has made its changes to failover, not before and after.
- **Benchmarks after the rework and the cluster**, so the numbers are not out of date as soon as they are taken. Benchmarks run on an idle machine only.
- **Docs once, at the end.** The architecture map (R1) and threat model (R3) are drafted during the review because the review needs them; the other docs are reconciled in W1.

## Stage 1: Tooling (S)

Done on 2026-10-10: `scripts/check.sh`, `scripts/soak.sh`, and a build and test run on the Raspberry Pi 5 (CHANGELOG). What the Pi run found is Q9 (Q4 is fixed).

## Stage 2: Review (M)

Done on 2026-10-10. Each review wrote its findings as stage 3 items and its decisions into DESIGN.md: R1, the draft [ARCHITECTURE.md](ARCHITECTURE.md); R2, the test audit (Q5); R3, the draft [SECURITY.md](SECURITY.md) (Q6); R4, failover conformance (Q8, Q10 to Q14 are fixed); R5, structure (DESIGN.md, "Operating a node"; Q1 to Q3 and Q7 are done). W1 finishes both drafts.

## Stage 3: Rework (M–L)

From stage 2 and earlier work:

- [ ] **Q5 Test gaps: what is left** (the rest landed 2026-10-11, CHANGELOG). `NotYet` to a node that is down: it arises only from the millisecond rounding of `confirmed_ago_ms`, and a direct test needs the node state machine without a runtime. A pool template the rollout cannot put back (now logged and reported in the upgrade's error) and a singleton kill that fails: both need a supervisor that exits mid-operation, which no test can yet arrange.
- [ ] **Q6 Security fixes** from R3 (`docs/SECURITY.md`). S1 to S9 are fixed and the fuzz targets added. Left: a connection cap on the Modbus server (tokio-modbus's server gives no hook for one).
- [ ] **Q15 Singletons on a node that does not run them.** A singleton deployed to a standby is only held: nothing checks it can start (a missing dependency, say) until a promotion tries, logs `cannot start singleton`, and leaves it stopped while the node is primary. A start that fails at promotion is not tried again until the next one. Check what `Apps::add` checks at hold time, and retry a failed start (with backoff) while the node may run singletons. Found writing Q5's gate tests.
- [ ] **Q9 Debug build size.** Debug test binaries are about 530 MB each (full debug info; wasmtime dominates). On the Raspberry Pi 5 (7.7 GiB, no swap) `cargo test --workspace` at the default four jobs had `ld` killed for memory at 1.7 GB resident (2026-10-10); two jobs, or swap, avoid it. Measure `debug = "line-tables-only"` for the dev and test profiles (binary size, link memory, build and test time, backtraces still usable) and adopt it if it holds up.

### Verification of failover

- **V1 The rule for failover changes.** A change to a transition in `failover/` updates the matching action in `FailoverL2.tla` (CLAUDE.md). Before it lands, rerun the configurations it can affect: `L2_Crashes_Fixed1` (about 20 minutes) for anything touching restarts, epochs or the witness, and `L2_Links_Fixed_holds1` (about 2.5 hours) for heartbeats, leases or links. Random simulation of the two-fault configurations runs for at least 20 minutes. Record the results in VERIFICATION.md.
- [ ] **V2 Two-fault exhaustive rerun**, once stage 3 is done. `L2_Crashes_Fixed` with every switch as in the code now, which it was not run with (about 20 hours and 250 GB of disk), and `L2_Links_Fixed_loss2` (about 10 hours) if stage 3 changed heartbeats or leases. Add a configuration for the deployment without a serial line (lost from the start, two more faults) (VERIFICATION.md, "Conformance with the code"). Each uses a fingerprint seed (`-fp`) other than the first run's, which also covers the chance that a fingerprint collision hid a state (about 1 in the first runs). Also rerun before Phase 2b is declared done if failover changed after V2, and before a release.

## Stage 4: Cluster (L)

DESIGN.md "Distribution", on the transport, membership and failover that exist. No item needs consensus; Raft (or HAppliance-style replication) comes with Phase 3's replicated buckets.

- [ ] **D10 Failover: what is left.** An hour of `scripts/soak.sh --only failover` passes. The criterion with the serial line up (network cut: nobody promotes) needs a serial path between processes, so it moves to F5 with the serial heartbeat on real hardware.
- [ ] **D10b Planned switchover.** `paddock failover switchover`: the primary stands down and the standby takes over without waiting for a node timeout, for maintenance and to swap roles between hot-standby instances on purpose. A new transition, so V1 applies; the model gains the action first.
- [ ] **C4 Spike S12.** A TLA+ model of remote links, monitors and `noconnection` with pid incarnations and C5's sessions (numbers on frames, `hears_you`, ending on either side), checked with a safe and an adversarial network. Time-box 3 days. If a rule does not hold, narrow Phase 2's guarantees to what the model shows. Before D3.
- [ ] **D3 Remote pids, calls, casts, links and monitors between processes**, on the QUIC transport and the versioned wire format (D1). Instance ids hold the node's creation (settled 2026-10-08). From D1: QUIC over loopback in the simulation harness, and a throughput figure between nodes (B7). Three-node partition runs for Phase 2b's exit join `scripts/soak.sh` (`paddock-soak`).
- [ ] **D7 Health exports** (manifest `health`).
- [ ] **D8 Backpressure between nodes**: what a sender sees when the link to a remote node is congested (DESIGN.md defines only full mailboxes).
- [ ] **D5 Messaging ACLs** (`messaging_acl`): declared routes by app name (`may_call`, `accept_from`) checked against the host-stamped sender, local and remote. Capability tokens are parked.
- [ ] **D6 Admin API over TLS TCP**, for operating a node from another machine. Reuses D1's pinned keys or E1b's certificates, as R3 decides.
- [ ] **D4 Process groups across nodes**: H2's groups gossiped like OTP `pg`; `publish` sends one copy per remote node, which fans out locally. Demo: tutorial chapter 3 (the board app as `hot-standby`, the primary sending each write to the standby before answering, the count surviving `kill -9` of the primary).
- [ ] **D11 Reaching the primary without root.** In order:
  1. Any node answers. The standby answers a singleton's HTTP route with a redirect to the primary's ingress address, which `paddock failover` knows. This needs no D3, and clients that follow redirects need no load balancer. Today the standby answers 503, which suits a load balancer's health check (tutorial chapter 2).
  2. With D3: the standby's ingress forwards the request to the primary's instance, so either address works.
  3. A way to follow leadership from outside the node (`paddock failover --watch`, streaming), so an external virtual-IP agent (`ip addr` and `arping` with `CAP_NET_ADMIN` and `CAP_NET_RAW` only, or a cloud provider's API) can move an address. Only for sites that need one address; `paddock node` itself never gets network privileges. keepalived following Paddock's role is documented as a fallback.
- [ ] **D9 Spike S11: passivation** of idle instances through the snapshot pair (snapshot, drop the Store, rebuild on the next message), for address-space headroom. Time-box 2 days. Shares the mechanism of S17.

Exit (Phase 2b): the soak's three-node partition and node-loss runs pass for an hour, and D10's exit criteria hold under it.

## Stage 5: Platform features (L)

- [ ] **E5 SQL datasources (Postgres)** (`paddock:sql`), brought forward from Phase 3. The host owns a connection pool per named datasource (`tokio-postgres`, permissive license), configured per node; the manifest requests a datasource and a connection cap; credentials come from E3's secrets. Built in by default (Q1) but idle without a datasource in node config, so a Pi without Postgres is unaffected. Tests and a tutorial chapter run against a scratch Postgres started as an ordinary user (`initdb` and `pg_ctl` in a temporary directory, or a rootless container), and are skipped when none is available. An app can also ship its own driver over `wasi:sockets` later (Phase 3 sockets).
- [ ] **P1 Cluster shell**, after D6: a `paddock` session to any node that lists members, follows logs and events, and runs admin commands; logs and events stream over D6 as chunked HTTP responses.
- [ ] **F5 Serial extension.** Named serial ports declared by apps and bound to device paths in node config; the host owns the tty and offers a byte stream or framed messages; its own crate (A5). Then D10's serial heartbeat path on real hardware, the soak's failover runs with the serial line up and cut (network cut: nobody promotes), and Modbus RTU.
- [ ] **E2 HTTP egress** (`wasi:http/outgoing-handler`) through a filtering proxy with an allow-list and pooled connections (S15: wasmtime's default sender opens a connection per request).
- [ ] **F4 Batching.** Guidance and an SDK helper for sending many updates in one message (a cast costs about 1.3 µs whatever it carries); points per second end to end with `pool_load`.
- [ ] **H5 Ordered range scans** (ETS `ordered_set`), if shared-bucket users need them.
- [ ] **E4b OTLP push** beside the Prometheus endpoint, when a site needs push. Review the opentelemetry crates with cargo-deny first.
- [ ] **E6 A `wasi:http` handler world**: an instance per request, for unmodified components and streaming bodies, selected by a route (S15: about 15% of native throughput, so not the default).
- [ ] **Smaller open items.**
  - HTTP ingress: `paddock status` listing routes, HTTP/2, client certificates (mTLS), OCSP stapling.
  - Listeners: host framing into messages for a pool (handlers forward to apps themselves today).
  - Connection handlers: a drain timeout for connections open across an upgrade, `log-level` announcements to handlers.
  - Modbus: deadbands, scaling and data types beyond raw registers.
  - Secrets: Vault or another store (an extension or a source kind), caching with a TTL.
  - Groups: probes for join and publish.
  - B6: a node watchdog (the epoch ticker pats it; a wedged node is restarted by its service manager).

## Stage 6: Platforms and benchmarks (M)

On an idle machine only.

- [ ] **B7 Benchmark suite.** `scripts/bench.sh`, which writes a dated section for BASELINE.md: `baseline`, `pool_load`, `log_cost`, the S14 and S15 sweeps, `watch_cost`, `modbus_load`, ingress throughput, throughput between nodes, a publish to many group members, revision writes on `disk` buckets, and the cost of the per-callback metric counters. CPU time and peak memory are sampled for each from `/proc` (no root). Replaces the numbers flagged provisional (taken on a busy machine, 2026-10-09).
- [ ] **G2 Raspberry Pi 5 baseline**: B7 on the Pi, and instances per GB with the dense profile.
- [ ] **G3 Linux arm64 in CI.**
- [ ] **G5 Dedicated workers and CPU budgets** for latency-critical instances: priority classes mapped to separate worker sets, a CPU budget per period per app (`CallbackClock`), and a rule for priority inversion. Measure first (S15 saw 10–47 µs over native at one connection that client-side timing could not explain).
- [ ] **G1 Large machine** (tabled 2026-10-07: none with more than 16 cores or several sockets is available). B7 at 32 to 80 workers; watch the process table, registry and pool round robin for contention. Until then no design may assume one core or one socket.

## Stage 7: Docs (M)

- [ ] **W1 Docs against the code.** Check every doc against what was built, rewrite where needed, and clear "Doc debt". Finish `docs/ARCHITECTURE.md` (R1) and `docs/SECURITY.md` (R3). Add a platform capabilities overview (each service, its manifest section and node config, or a section of MANIFEST.md), a guest development guide (WRITING_APPS.md refreshed: SDK features, testing an app, bundles), and `docs/GLOSSARY.md` (frame, packet, session, creation, guest, instance, app, pool, standing, epoch, lease, witness, and so on). Tutorial chapters: a hot upgrade (counter v1 to v2 with state, a broken v3 reverted, `rollback --restart`; today this is covered only by tests) and Postgres (E5). `docs/REVIEW_HAPPLIANCE_OTP.md` leaves the tree (git keeps it): its adopted items are in DESIGN.md and this plan. HAppliance is a private project, so W1 also removes the description of its design and code from the rest of the tree (DESIGN.md, this plan, CHANGELOG.md, `docs/tla`, `spikes/s10` and `spikes/s16`): what Paddock adopted is stated as Paddock's own design. Done when `git grep -i happliance` matches only a one-line prior-art entry in DESIGN.md.

## Doc debt

Docs known to be out of date, cleared by W1:

- README, GETTING_STARTED and WRITING_APPS were not reviewed as Tracks D, E, F and H landed.
- DESIGN.md places SQL datasources in Phase 3; E5 is now in stage 5.
- DESIGN.md's roadmap section repeats parts of this plan.
- DESIGN.md, `docs/REVIEW_HAPPLIANCE_OTP.md`, `docs/tla` and `spikes/s16` describe HAppliance, a private project, in detail (see W1).
- DESIGN.md, WRITING_APPS.md and BASELINE.md cite finished roadmap items by id (A5, A6, E1, E1b, F2, F3, F6, H1 to H4, 0.5). Those ids are no longer in this file and the CHANGELOG does not use them; W1 replaces each with the feature's name.

## Phase 3

From DESIGN.md's roadmap, not yet broken into items:

- placement across nodes;
- opt-in global names (Raft);
- replicated buckets (`scope = "cluster"` on shared buckets). Raft, or HAppliance-style synchronous replication in an HA pair with a witness, is chosen then. Writes go to the leader, each node keeps a full copy for reads and delivers watches from it, linearizable reads are on request, and writes in a minority partition fail as `unavailable`. Later: keys owned by a pid, and a `mirrored` asynchronous durability. DESIGN.md, "Shared buckets and watches";
- sockets for guests;
- native-process plugins (see S19);
- blue-green upgrades;
- more guest SDKs;
- fuel metering.

The rustbridge work for Phase 3 is the guest adapter (`Plugin` → `paddock:runtime/app`), the `Server` trait and the native-process runner.

## Investigations

Questions for later, each a spike with a time-box when scheduled.

- **S17 Moving a running instance to another node.** The snapshot pair that upgrades use (`snapshot`, `load_snapshot`), carried over the cluster network and loaded on the other node. To settle: the new pid (pids name their node), what happens to links, monitors and registered names, and how messages sent during the move are handled. After D3; shares its mechanism with D9.
- **S18 Checking hot-standby instances follow the primary.** Compare the primary's and the standby's sequences of host calls (from probes), allowing for timestamps, to find a standby that has diverged. After D4.
- **S19 Ports or nodes for external processes.** An Erlang-style port (the host spawns a process and talks over stdio, owned and supervised like an instance) against a native process joining as a cluster node through a client library. Relates to Phase 3's native-process plugins and rustbridge's native runner.
- **ZFS.** A node's data directory on a dataset: snapshots for backups and for seeding a standby's buckets.
- **Jails and containers.** Running native plugins (S19) in FreeBSD jails as HAppliance does, or the node itself in a rootless container.
- **S9 Debugging guests** (parked). wasmtime 49 offers a native debugger (`Config::debug_info` with `debug-builtins`: GDB and LLDB on JIT code, pausing the whole node, for development nodes) and an experimental guest debug API (`Config::guest_debug`: breakpoints and stepping per Store, which could park one instance behind a Debug Adapter Protocol endpoint while the others run; needs the guest's DWARF, an Engine of its own, and time parked counted as host time). The spike would check whether `guest_debug` works in wasmtime 49, measure the slowdown, and try one DAP session from VS Code. Time-box 2 days; if the API is not ready, document the native route.

## Parked

- **DNP3 and ICCP extensions.** The mature DNP3 implementations in Rust found so far are source-available with commercial terms; no permissively licensed ICCP stack in Rust is known.
- **Redis or etcd bridge** as an extension (A5), for sites that already run one. Not the platform's shared buckets.
- **Capability tokens for messaging** (seL4-style endpoint handles), after D5's declared routes.
- **Zero-copy large messages** (host-owned blobs passed by handle), only if F4's measurements show copies matter.
- **Handing a connection to another instance** (`gen_tcp:controlling_process`), rare with an instance per connection.
- **Persistent timers** (cron, the EJB timer service) that survive restarts.
- **Node alarms** (OTP `os_mon`): memory, disk and storage errors as host events and in the admin API.
- **Key expiry** in keyvalue: a TTL per bucket or per `set`.
- **rustbridge release.** Switch `rustbridge-bundle` from the `experiment/wasm` branch to a crates.io release with bundle format 1.2; required before the repository goes public or a crate is published.
- **`gen_statem` support in the SDK**: state timeouts and `postpone` on the existing callbacks and timers.
- **WASI 0.3 world** (`paddock:runtime@0.2.0`, async), when wasmtime marks component-model async stable and Rust has `wasm32-wasip3`; rerun S15's connection sweep first (DESIGN.md, "WASI 0.2 to 0.3 migration").

## Code areas

An area is a set of modules with a narrow interface to the rest: modules in crates with `pub(crate)` and a small `pub` surface, rather than a crate each. R5 (2026-10-10) kept these areas and the crates as they are.

| Area | Where | Interface to the rest |
| --- | --- | --- |
| Contract | `wit/`, WIT rules tests | The `paddock:runtime` world and the separate packages (`connection`, `groups`, `watch`, `secrets`, `metrics`); versioned, never changed in place |
| Manifest and bundle | `paddock-bundle` | `AppSpec`, `VerifiedBundle`, the effective grant |
| Process core | `paddock-host`: `pid`, `table`, `registry`, `links`, `message`, `instance`, `runtime`, `state`, `process`, `delivery` | Spawn, call, cast, link, monitor, exit; `delivery` routes every pid |
| Host services | `paddock-host::services`, one module each; `metrics` | The `Service` trait; extensions through `Extension` |
| Orchestration | `paddock-host`: `apps`, `pool`, `supervisor`, `upgrade`, `lifecycle`, `spec` | `Apps`, `AppHandle`, `UpgradeOptions` |
| Cluster | `paddock-host`: `transport` (`liveness`, `membership`, `memory`, `quic`, `wire`), `failover` | The `Transport` trait, node events, `FailoverHandle` |
| I/O and protocols | `paddock-host`: `listener`, `http`; `paddock-modbus` | Listeners, connection handlers, ingress routes; extensions in their own crates |
| Observability | `paddock-host::probe` | Probe functions called from the other areas |
| Node and admin | `paddock` (`node`, `api`, `cli`) | Admin API routes under `/v1`, the CLI, node config |
| Guest SDK | `paddock-sdk`, `paddock-sdk-macros` | `App`, `Ctx`, envelopes, snapshots; services by cargo feature |
| Fixtures and examples | `crates/paddock-fixture-*`, `examples/` | Guest components and demos |
| Soak | `paddock-soak`, `scripts/soak.sh` | `paddock node` processes through their config, admin API and logs; reads the gate's `singleton starting` and `singleton killed` records |

Crates meant for publishing (all `publish = false` while the project is private): `paddock-sdk`, `paddock-sdk-macros`, `paddock-bundle`, `paddock-host`, `paddock` and `paddock-modbus`. App developers depend on the SDK only; extension developers on `paddock-host` only. Fewer crates are preferred.

## Settled decisions

| Decision | Choice |
| --- | --- |
| Name | `paddock`: crates `paddock-*`, WIT package `paddock:runtime`, CLI `paddock` |
| License | MIT OR Apache-2.0, no copyleft in the dependency graph |
| WASI target | WASI 0.2, sync guest ABI, wasmtime 49.x; 0.3 later as `paddock:runtime@0.2.0` |
| Async runtime | tokio |
| Persistent store | redb |
| Default envelope encoding | CBOR, with JSON and text allowed |
| Admin API transport | JSON over HTTP on a Unix socket; TLS TCP in D6 |
| Node identity | Pinned Ed25519 keys and peers files, not a CA (2026-10-09) |
| Failover | Epochs, heartbeat paths, a witness and a lease; no Raft (2026-10-08) |
| HTTP requests | Calls to a pooled instance, node-level ingress (2026-10-09) |
| Metrics export | Prometheus text, metrics per app (2026-10-09) |
| Build features | Modbus off by default; HTTP, TLS, probes, SQL on (2026-10-10) |
| Node layout | Everything a node writes under `data_dir`, admin socket and compiled components included (2026-10-10) |
| Logging | stderr only; journald or the container runtime rotates (2026-10-10) |
| Privileges | Unprivileged user; `CAP_NET_BIND_SERVICE` only, for ports below 1024 (2026-10-10) |

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
| S9 | Debugging guests | Parked (Investigations) |
| S10 | New `info` cases and old guests | Done; `host-event` reserved ([spikes/s10](../spikes/s10/README.md)) |
| S11 | Passivation | Planned, D9 |
| S12 | TLA+ model of remote links and monitors | Planned, C4 |
| S13 | Host-owned TCP listeners and guest streams | Guest streams at native throughput with an instance per connection ([spikes/s13](../spikes/s13/README.md)) |
| S14 | Storage in the guest versus host services | About 140 ns per host import; keyvalue reads 1.2 µs over native ([spikes/s14](../spikes/s14/README.md)) |
| S15 | HTTP in the guest versus the host | Host HTTP plus an instance call keeps about 75% of native; an instance per request about 15% ([spikes/s15](../spikes/s15/README.md)) |
| S16 | TLA+ model of failover | Done; now layer 1 of [docs/tla](tla/VERIFICATION.md) ([spikes/s16](../spikes/s16/README.md)) |
| S17 | Moving a running instance to another node | Investigation |
| S18 | Hot-standby instances following the primary | Investigation |
| S19 | Ports or nodes for external processes | Investigation |
