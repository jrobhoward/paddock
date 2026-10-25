# Paddock: Roadmap

2026-10-16 · Companion to [DESIGN.md](DESIGN.md)

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
| 2a: platform on one node | An app serves HTTP, reads a secret, logs and exports metrics; an operator traces its messages on a live node; Modbus feeds a hash-routed pool that keeps a point table in a shared bucket other apps watch; baselines on the 16-core machine and a Raspberry Pi 5 (and a multi-socket machine when one is available) | Met on 2026-10-18 (G2, the Pi's baselines) |
| 2b: cluster | A primary and standby fail over on node loss; with the network cut nobody promotes; with network and serial cut at most one side runs a singleton; a three-node cluster passes partition tests | Met: the rest on 2026-10-15, the serial line criterion on 2026-10-16 (F5) |
| 3: application server | One real workload runs a full release cycle with zero downtime | Met on 2026-10-22 (X2); the docs pass left (see "Phase 3") |

The failover pair and witness run over QUIC between `paddock node` processes (`examples/failover/demo.sh`). The failover protocol is model-checked exhaustively in TLA+ up to two faults, as the code now runs, and for a deployment without a serial line ([docs/tla/VERIFICATION.md](tla/VERIFICATION.md), V2 on 2026-10-13).

## Next

Stages 1 to 4 are done (stage 4 on 2026-10-15), and Phase 2b's exit with them (its serial line criterion on 2026-10-16). Stage 5's and stage 6's planned items are done (G2, the last of Phase 2a's exit, on 2026-10-18). Of what is left:

1. Phase 3 (planned 2026-10-16, below): X1's first step, K1, K2 and K3 are done (2026-10-16 to 2026-10-18); X1's second step (the point table on a cluster bucket polled by the primary only, `control`'s buckets replicated) and M1 (singletons moved with their state in a planned switchover) on 2026-10-20. L1 (a planned switchover released at the witness, rule R14: about 200 ms instead of a lease) on 2026-10-20 too. U1 (releases across the pair: wire and kept-format versions, `paddock release`) and U2 (blue-green upgrades of pool apps: the new version beside the old, traffic by weight) on 2026-10-21, and X1's drill through `paddock release` the same day, which finishes X1. X2, the exit, on 2026-10-22: an hour of `paddock-soak scada` under faults passed. N1, guest sockets, on 2026-10-23. S19, N2's spike, the same day: a port; N2 itself the same day. Next: the docs pass for Phase 3.
2. W1 (Phase 2's docs), now unblocked; H5 and E4b only for a user who needs them.

## Order

```mermaid
flowchart LR
    s1["1 Tooling (S)<br/>done"] --> s2["2 Review (M)<br/>done"] --> s3["3 Rework (M–L)<br/>done"] --> v2["V2 (20 h)<br/>done"] --> s4["4 Cluster (L)<br/>done"]
    s4 --> s5["5 Platform features (L)<br/>next"] --> s6["6 Platforms and benchmarks (M)"] --> s7["7 Docs (M)"]
```

- **Tools first.** The checks, the soak runner and the benchmark runner are each one command, so every later stage runs them instead of re-deriving them.
- **Review before more cluster code.** D3 to D8 add more code than any track so far. Reviewing structure, tests and security first, then reworking, puts that code into its final shape once instead of moving it later.
- **V2 after the rework.** The 20-hour model run is repeated once, after stage 3 has made its changes to failover, not before and after.
- **Benchmarks after the rework and the cluster**, so the numbers are not out of date as soon as they are taken. Benchmarks run on an idle machine only.
- **Docs once, at the end.** The architecture map (R1) and threat model (R3) are drafted during the review because the review needs them; the other docs are reconciled in W1.

## Stage 1: Tooling (S)

Done on 2026-10-10: `scripts/check.sh`, `scripts/soak.sh`, and a build and test run on the Raspberry Pi 5 (CHANGELOG). What the Pi run found, Q4 and Q9, is fixed.

## Stage 2: Review (M)

Done on 2026-10-10. Each review wrote its findings as stage 3 items and its decisions into DESIGN.md: R1, the draft [ARCHITECTURE.md](ARCHITECTURE.md); R2, the test audit (Q5, done); R3, the draft [SECURITY.md](SECURITY.md) (Q6, done); R4, failover conformance (Q8, Q10 to Q14 are fixed); R5, structure (DESIGN.md, "Operating a node"; Q1 to Q3 and Q7 are done). W1 finishes both drafts.

## Stage 3: Rework (M–L)

Done on 2026-10-13: every item from stage 2 (CHANGELOG), and V2, the two-fault exhaustive runs (VERIFICATION.md).

### Verification of failover

- **V1 The rule for failover changes.** A change to a transition in `failover/` updates the matching action in `FailoverL2.tla` (CLAUDE.md). Before it lands, rerun the configurations it can affect: `L2_Crashes_Fixed1` (about 20 minutes) for anything touching restarts, epochs or the witness, and `L2_Links_Fixed_holds1` (about 2.5 hours) for heartbeats, leases or links. Random simulation of the two-fault configurations runs for at least 20 minutes. The vacuity checks, `L2_Vacuity` and `L2_Switchover_Reach`, must still fail (seconds each): a model in which nothing runs on a lease passes every safety property (VERIFICATION.md, bug 5). Record the results in VERIFICATION.md.
- **V3 Two faults, by simulation** (done 2026-10-16, VERIFICATION.md): at `Lease = 4`, `L2_Links_Fixed` passed; `L2_NoSerial` and `L2_Crashes_Fixed` found finding 6 (an old primary that restarts after an operator confirmed it gone resumes at the witness; accepted, the confirm rule documented) and passed without operator confirmations.
- **V2 The two-fault runs.** `L2_NoSerial` (about 1 hour), `L2_Links_Fixed_loss2` (about 12 hours) and `L2_Crashes_Fixed` (about 18 hours, 250 GB of disk), exhaustive, on an idle machine, each with a fingerprint polynomial (`-fp`) not used before. Rerun before Phase 2b is declared done if `failover/` changed after 2026-10-13, and before a release.

## Stage 4: Cluster (L)

DESIGN.md "Distribution", on the transport, membership and failover that exist. No item needs consensus.

Done on 2026-10-15: D10b, a planned switchover (`paddock failover switchover`): the primary stops its singletons gracefully and yields, and the standby asks the witness at once; the witness is unchanged (decided 2026-10-15). Checking it found that the TLA+ model's leases had been zero since 2026-10-11 (VERIFICATION.md, bug 5); with `Lease = 4`, any single failure plus a switchover passes exhaustively, with and without a serial line, and two faults pass random simulation (the guarantee decided 2026-10-15). D11, reaching the primary without root: the standby redirects (`partner_url`) or forwards (`standby = "forward"`, an app call over the cluster; both decided 2026-10-15) a singleton's requests, and `paddock failover --watch` (a long poll, decided 2026-10-15) lets an agent move a virtual IP (OPERATIONS.md; keepalived documented as the alternative). D3, remote pids, calls, casts, links and monitors: an hour of `scripts/soak.sh --only cluster` on an idle machine (six runs of 10 minutes, seeds 531272780 to 531272785, all passed), and the throughput between nodes (B7: about 9,200 sequential calls per second over QUIC on loopback, 62,000 with 16 callers; BASELINE.md). C4, spike S12's runs of the fixed model ([spikes/s12](../spikes/s12/README.md)): full send queues and the liveness properties pass exhaustively; the configs with closes and timeouts, and with crashes, show no violation up to about 840 and 890 million distinct states, stopped after 15 hours each at the time box. D9, spike S11, passivation ([spikes/s11](../spikes/s11/README.md); the prototype is in `paddock-host`, off by default, decided 2026-10-15; E7 exposes it).

Done on 2026-10-14: D4, process groups across nodes as OTP's `pg` (every group spans the cluster; `publish` checks the messaging ACLs per member; decided 2026-10-14), with tutorial chapter 3 as its demo. D6, the remote admin API over TLS with pinned keys, as the cluster (decided 2026-10-14; SECURITY.md lists its gaps: no read-only operators, no connection limit). D5, messaging ACLs (receiving node checks both lists from a stamp on each frame; calls, casts, exits and links; app names only; decided 2026-10-14; MANIFEST.md `messaging_acl`, SECURITY.md). D8, backpressure between nodes: a congested link follows the full-mailbox rule, notices exempt (decided 2026-10-14; DESIGN.md, "Backpressure between nodes"). D7, health checks as calls with a reserved schema (`paddock/health`, answered by the SDK's `App::health`; decided 2026-10-14 over a new export, so the app world stays frozen; MANIFEST.md `health`). C6, the four rules from S12 that keep both sides of a link or monitor in step (CHANGELOG; DESIGN.md, "Agreement on links and monitors"). D10, an hour of `scripts/soak.sh --only failover` (six runs of 10 minutes, seeds 394302291 to 394302296, all passed; TLC ran on 3 of the 16 cores meanwhile, at low priority). The criterion with the serial line up (network cut: nobody promotes) needs a serial path between processes, so it moved to F5 with the serial heartbeat on real hardware.

Exit (Phase 2b): the soak's three-node partition and node-loss runs pass for an hour, and D10's exit criteria hold under it. Met on 2026-10-15 (D3's soak hour; D10's failover hour on 2026-10-14), except D10's criterion with the serial line up, which needs a serial path between nodes and moved to F5. The serial line criterion was met on 2026-10-16 by F5: an hour of the failover soak with the pair's serial line on a null-modem cable between two USB adapters on one machine, cut among the other faults (6 of 6 runs passed, seeds 979441506 to 979441511: no moment with both nodes running the singleton, and exactly one runner after every heal). That a network cut with the line up promotes nobody is checked by the model (VERIFICATION.md) and on the in-memory mesh (`tests/simulation/failover.rs`), not between node processes: the soak checks safety only, and `crates/paddock/tests/failover.rs` cuts the line alone.

## Stage 5: Platform features (L)

Done on 2026-10-15: E5, SQL datasources (`paddock:sql`, Postgres): `query`, `execute` and a `transaction` resource (decided 2026-10-15), a `deadpool-postgres` pool per `[datasources.<name>]` with TLS on rustls (decided 2026-10-15), credentials from node secrets, a connection cap per app, `examples/tally` and tutorial chapter 4; the tests pass against Postgres 16 and skip without it. An app can also ship its own driver over `wasi:sockets` later (Phase 3 sockets). E7, passivation for apps (`instances.passivate_after_ms`, node `max_processes`; a deploy-time budget for instances that never passivate and a bounded wait for a Store slot, and an upgrade's second Store takes one, decided 2026-10-15; MANIFEST.md). P1, the cluster shell (`paddock shell`, decided 2026-10-15: a prompt with history and completion through rustyline; `connect` to nodes in an operator's contacts file, each reached directly over D6 rather than relayed by another node; `paddock events` streams audit records, instance exits and restarts, member changes and failover changes; `paddock logs --follow`). E2, HTTP egress: `wasi:http/outgoing-handler` through the host's filter and one connection pool; the manifest's `allow` inside node config's `[egress] allow` ceiling, the system's roots plus a CA file, names resolving to loopback or link-local refused unless `allow_loopback` (each decided 2026-10-15); host time now measured by the Store's call hook for every host import, so a guest waiting on a response is not killed at its deadline (decided 2026-10-15; no cost measurable in `baseline`, BASELINE.md). The pool doubles throughput with 16 instances requesting at once; one request at a time costs 140 to 164 µs on 4 workers, mostly in task hand-offs (BASELINE.md). F4, batching: `paddock_sdk::batch` (a `Batcher` per destination, flushed by count, bytes or a `max_delay` timer, decided 2026-10-16) and guidance in WRITING_APPS.md; measured with a new `batch_cost` example rather than `pool_load`, whose chaos guest has no SDK: batches of 100 move 3 to 4.5 times the points of one cast each, and past 100 the guests' work on the items dominates (BASELINE.md, 2026-10-16). E6, HTTP handlers: an unmodified `wasi:http/proxy` component (selected by its export, decided 2026-10-16) gets a new instance per request on its routes, with streaming bodies; no instance reuse; the route's timeout runs until the headers and then as an idle timer while a body moves; modelled on connection handlers (no supervision, a Store slot per request, upgrades for new requests); the usual import rules with `paddock:runtime/process` available; `placement.mode = "any"` only, so the D11 standby rules do not apply (each decided 2026-10-16). F5, the failover pair's serial heartbeat path: `[failover] serial` in node config, CRC-checked COBS frames, Paddock's own tty layer on `rustix` (the serial crates are MPL-2.0), tested on ptys and a USB null-modem pair, and the failover soak's `--serial` relay that cuts the line; serial ports for apps and Modbus RTU moved to Parked (each decided 2026-10-16).

- [ ] **H5 Ordered range scans** (ETS `ordered_set`), if shared-bucket users need them.
- [ ] **E4b OTLP push** beside the Prometheus endpoint, when a site needs push. Review the opentelemetry crates with cargo-deny first.
- [ ] **Smaller open items.**
  - One hot shared `memory` bucket does not scale with writers: `modbus_load` stores fewer updates with a pool of 8 than of 4 on the same bucket. Node-wide, every memory bucket shares one lock (`storage::Memory`); a lock per bucket would help several apps, not one hot bucket. WRITING_APPS gives the guidance (2026-10-16); sharding a bucket only if a site hits the limit anyway. B8, 2026-10-16.
  - Cluster calls: writing frames from the sender's task when the stream is free would save a hand-off each way (B8 measured about 25 to 50 µs of hand-offs over the QUIC floor). Not done (2026-10-16): it touches the ordering the failover and remote-link rules rely on, for about 10 to 25 µs on an idle runtime and less under load; only for a site with a latency target between nodes.
  - HTTP handlers: reusing an instance across requests (opt-in, once wasmtime's reusing `ProxyHandler` runs without `component-model-async`, or with WASI 0.3); probes for handler requests; a guest that drops its response body without `finish` and returns normally ends the body cleanly, because wasmtime 49 does not tell that from `finish` (a trap or timeout does cut it).
  - HTTP ingress: `paddock status` listing routes, HTTP/2, client certificates (mTLS), OCSP stapling.
  - Listeners: host framing into messages for a pool (handlers forward to apps themselves today).
  - Connection handlers: a drain timeout for connections open across an upgrade, `log-level` announcements to handlers.
  - Modbus: deadbands, scaling and data types beyond raw registers.
  - Secrets: Vault or another store (an extension or a source kind), caching with a TTL.
  - Groups: probes for join and publish.
  - HTTP egress: HTTP/2; an outbound proxy (`CONNECT`); client certificates; a deny list of private ranges per CIDR for internet-facing nodes; a cap on response bodies. wasmtime 49 gives a guest no error code for a failure inside a response body (`http-error-code` answers none), so `paddock_sdk::http_client` recognises a timeout by its text; recheck after a wasmtime update. Fewer task hand-offs per request (BASELINE.md, 2026-10-15 E2).
  - Shell: completing app names and pids (a request per Tab, or a cache refreshed by `ls` and `ps`); a follow across every app's logs, or across nodes.
  - B6: a node watchdog (the epoch ticker pats it; a wedged node is restarted by its service manager).
  - Blue-green (from U2, 2026-10-21): HTTP handlers and connection handlers (weighted new connections); apps with `registered_names` (move the names at promotion); a version whose HTTP routes change; a weight shared by the two nodes of a pair (each node moves its own today, so `paddock release upgrade` of a blue-green app promotes on each node in turn).
  - Tests: `board____cluster_bucket_primary_killed____count_survives_on_the_standby` failed once under `scripts/check.sh`'s load (2026-10-21): node 2's admin socket was not there after the helper's 10 s wait. It passes alone; seen once.
  - Releases (from U1, 2026-10-21): a version for a node's own stores without replication, once one of their formats changes (nothing to finalize there yet); stopping a node whose older build cannot decode the replication frame that carries a raise of the pair's version (it stays behind and recorded out of date instead).

## Stage 6: Platforms and benchmarks (M)

On an idle machine only.

Done on 2026-10-15: B7, `scripts/bench.sh`, the benchmark suite; its first run is BASELINE.md's 2026-10-15 section, which replaces the numbers flagged provisional on 2026-10-09 and records Modbus throughput. Done on 2026-10-16: B8, what B7 left unexplained (BASELINE.md, 2026-10-16): the call between nodes is mostly QUIC and tokio wakes (`quic_floor`); the ingress gap was per-request waste, removed (78k to 85k, 88k to 99k without guest CBOR, at 32 connections); `modbus_load`'s drop comes from the pool size on one shared bucket; the metric counters cost nothing measurable; boxing the callback futures saves nothing. Hash routing then stopped decoding whole bodies (it walks the CBOR map for the key): `modbus_load` (64 devices, pool of 4, 8 workers) stores about 25% more updates with about 12% less CPU. G3, Linux arm64 in CI (`ubuntu-24.04-arm` in the test matrix), the same day. Done on 2026-10-18: G2, the Raspberry Pi 5 baseline (BASELINE.md, 2026-10-18): the suite on the Pi, and about 9,400 idle live instances per GiB on the dense profile (40,000 held), 54,000 passivated. Left open from it: an idle live instance takes about 110 KiB on the Pi against 58 to 63 KiB on the 16-core machine, not explained, and epoch checks cost up to 1.5 times in tight loops there. Done on 2026-10-24: B9, benchmarks of what running in Wasm costs (BASELINE.md, 2026-10-24): `timer_cost`, `restart_cost`, `heap_cost`, `compile_cost` and an Erlang/OTP reference (`scripts/beam_baseline.escript`), each in `scripts/bench.sh`. A call costs 1.4 times a gen_server call on one thread; an instance 17 times a process to start and 22 times its memory; timers are handled about 0.7 ms later than BEAM's, from tokio's timer rather than Wasm; growing a heap costs what native allocation does.
- [x] **B10 Leads from B9** (done 2026-10-24; BASELINE.md, "compiling on every core, slots kept resident, timer lateness").
  - [x] wasmtime's `parallel-compilation`: release components compile 2.6 to 3 times faster on the Pi and 7 to 9 times on the 16-core machine, for about 25% more CPU in all.
  - [x] A wake or upgrade is page faults: the state is touched twice in fresh memory, and a freed slot gives its pages back to the kernel. Slots now keep up to 16 MiB of written pages resident (`keep_resident`, standard profile; none for dense; decided 2026-10-24): a 1 MiB wake is about half (Pi) to a third (16-core) of what it was, and restarts and instantiation on a reused slot are 7 to 29% faster.
  - [x] Timer lateness is tokio's 1 ms tick, rounded up twice; MANIFEST.md says what to expect (decided 2026-10-24: not a timer of Paddock's own, parked).
- [ ] **B11 Benchmarks not built yet.** A real app as a guest against the same code native (epoch checks on compiled Rust beyond S8's kernels); calls, casts and replicated writes between two machines on a LAN (the Pi and the 16-core machine); resident memory over hours. `scripts/soak.sh` samples each node process's resident memory every 10 s since 2026-10-24 (`memory.csv`, and the largest growth after a process's first minute on each summary line); each run starts its own nodes and the failover scenarios kill theirs every minute or two, so creep over hours needs one long chaos run. A 10-minute chaos run on the 16-core machine (2026-10-24): 65.1 to 66.3 MiB after the first minute. Two hours of chaos on the Pi (2026-10-25, BASELINE.md): the node's floor rose from 65.0 to 66.4 MiB in the first 30 minutes and stayed within 8 KiB of that for the last 90, so resident memory over hours is measured for one node. The LAN run has begun: `quic_floor` runs its server and client on two machines (`serve`, `connect`), and between the Pi and the 16-core machine on gigabit Ethernet (2026-10-25) a 64-byte QUIC round trip takes 220 to 240 µs at p50 against 140 µs for bare UDP and 162 µs for `ping`; `cluster_throughput` does the same (`keys`, `serve`, `connect`): a call between the two machines takes 271 to 284 µs at p50, 30 to 61 µs over that floor, about what Paddock adds on loopback; 16 or 64 callers reach 37,000 to 46,000 calls/s, 67 to 93% of loopback's; 64 KiB bodies fill the gigabit link. Next, `replication_cost` with the pair on the two machines. The real app remains. The B9 benchmarks ran on the Pi on 2026-10-24 (BASELINE.md); G2's two open questions remain (110 KiB per idle instance, epoch checks at 1.5 times), and that run added a third: aarch64 cache files several times larger for small components.
- [x] **B12 Host overhead per message** (done 2026-10-24: on the Pi an empty call went from 3.55 to 3.67 µs to 3.08 to 3.17 µs on 1 worker and from 4.33 to 4.63 µs to 3.73 to 3.89 µs on 4, and `pool_load 4` handles 15 to 23% more empty calls per second, comparing runs from different sessions; BASELINE.md). On one worker an empty call (2.7 µs, a gen_server call 2.1 µs) is all on-CPU, with no context switches, and only about a sixth of it is guest code; tokio (about 32%), wasmtime's side of the call (24%), `paddock_host` (12%) and allocation and copies (9%) take the rest (`call_split` and `perf`, 2026-10-24, provisional: the machine had disk load). Each measured with `call_split` and `baseline` before and after:
  - [x] No tokio timer per call (done 2026-10-24): `InstanceHandle::send_call` yields once and arms the timer only if the reply has not arrived, at the same deadline. About 8% off an empty call on 1 and 4 workers, the same as no timer at all, and 5 to 12% more empty calls/s under `pool_load` (Pi 5, BASELINE.md). p99.9 at 256 clients, higher in two of three rounds, was not the change: it moves between two levels from run to run of the same build (item 2's runs).
  - [x] A cheaper `killable()` (done 2026-10-24): a kill sets a flag on the instance and wakes its task through an `AtomicWaker`; `killable()` registers the waker, checks the flag and polls the callback, where it built a `watch::wait_for` future per callback. About 8% off an empty call on 1 worker and 2 to 3% on 4, the same as no kill check at all; 2 to 6% more empty calls/s under `pool_load` (Pi 5, BASELINE.md). A kill still interrupts a guest waiting in a host call (new test, which fails without the wake); `crates/paddock/tests/failover.rs`, the simulation tests and a 30-minute `scripts/soak.sh --only failover` (SIGKILL and SIGSTOP) passed.
  - [x] The call hook off the hot path (done 2026-10-24): the callback clock times the task's polls of a callback (`PollTime::poll`, from `killable`) instead of every host call through a Store call hook. Waiting no longer counts, in host calls as before and now also for a worker after an epoch yield; host work that does not wait now counts as the guest's (MANIFEST.md, DESIGN.md, OBSERVABILITY.md). About 1.5% off an empty call on 1 worker and 2 to 3% on 4, where removing the hook with no timing at all takes 4.5 to 6% (Pi 5, BASELINE.md): the two clock reads per poll cost the rest. A cheaper clock (the CPU's counter, as the `quanta` crate reads it) might recover it; not tried.
  - [x] Fewer allocations and copies per message (done 2026-10-24): the caller of the call being handled waits in a slot, and the pending-reply map is used only after `noreply`, so most calls hash nothing. 3 to 8% more empty calls/s under `pool_load`; no measurable change for calls to one instance (Pi 5, BASELINE.md). The `Envelope` conversions were moves already: an empty call makes 6 heap allocations (`call_split ... allocs`), 2 of them the caller's envelope, 1 the reply channel and 2 the reply lifted out of guest memory, which the canonical ABI needs; fewer would take an `Envelope` that does not own its content type (a public API change). Not taken further.
- [ ] **G5 Dedicated workers and CPU budgets** for latency-critical instances: priority classes mapped to separate worker sets, a CPU budget per period per app (`CallbackClock`), and a rule for priority inversion. Measure first (S15 saw 10–47 µs over native at one connection that client-side timing could not explain).
- [ ] **G1 Large machine** (tabled 2026-10-07: none with more than 16 cores or several sockets is available). `scripts/bench.sh` at 32 to 80 workers; watch the process table, registry and pool round robin for contention. Until then no design may assume one core or one socket.
- [ ] **Major-version dependency upgrades** (listed 2026-10-24, after `rust-version` went to 1.99.0 and every lock file was updated within semver). `cargo update --verbose --dry-run` lists what is behind. Each is its own change; raise `rust-version` to the current stable rustc with it if a dependency needs one.
  - [ ] **Y1 wasmtime 49 to 50**, with `wasmtime-wasi`, `wasmtime-wasi-http`, and `wasmparser`, `wit-parser` and `wit-bindgen` on the lines wasmtime 50 uses, as one group. Rerun `baseline` and `pool_load` (CLAUDE.md, the epoch tick constraint) and the S1 and S4 spikes on the constraints they back. Recheck the wasmtime-49 notes under stage 5's smaller items: `http-error-code` for a failure inside a response body (the SDK matches a timeout by its text), a response body dropped without `finish`, and instance reuse in `ProxyHandler`.
  - [ ] **Y2 redb 3 to 4.** Every node's `kv/` file (disk buckets and the replicated log) is redb. Find out first whether 4 changes the file format; if it does, a node upgraded in place needs a migration (or a documented export and import) and OPERATIONS.md says so, and a pair running two builds during a release must still replicate (U1's versions).
  - [ ] **Y3 minisign 0.9 to 0.10**, together with rustbridge-bundle on rustbridge's `experiment/wasm` branch, which uses the same major so bundles verify with one copy: change and push rustbridge first, then update the lock here.
  - [ ] **Y4 The smaller ones**: rstest 0.28, rustyline 19 (`paddock shell`), toml 1.x (manifest, node config and policy parsing; check the error messages tests match), wasip2 2.0 (`paddock_sdk::http_client` and the HTTP fixtures).

## Stage 7: Docs (M)

- [ ] **W1 Docs against the code.** Check every doc against what was built, rewrite where needed, and clear "Doc debt". Finish `docs/ARCHITECTURE.md` (R1) and `docs/SECURITY.md` (R3). Add a platform capabilities overview (each service, its manifest section and node config, or a section of MANIFEST.md), and a guest development guide (WRITING_APPS.md refreshed: SDK features, testing an app, bundles). Check [GLOSSARY.md](GLOSSARY.md) (drafted 2026-10-24) against the code with the rest. Tutorial chapter: a hot upgrade (counter v1 to v2 with state, a broken v3 reverted, `rollback --restart`; today this is covered only by tests). `docs/REVIEW_HAPPLIANCE_OTP.md` leaves the tree (git keeps it): its adopted items are in DESIGN.md and this plan. HAppliance is a private project, so W1 also removes the description of its design and code from the rest of the tree (DESIGN.md, this plan, CHANGELOG.md, `docs/tla`, `spikes/s10` and `spikes/s16`): what Paddock adopted is stated as Paddock's own design. Done when `git grep -i happliance` matches only a one-line prior-art entry in DESIGN.md.

## Doc debt

Docs known to be out of date, cleared by W1:

- README and GETTING_STARTED were not reviewed as Tracks D, E, F and H landed.
- DESIGN.md places SQL datasources in Phase 3; E5 is now in stage 5.
- GETTING_STARTED does not mention `paddock shell`, `paddock events` or `logs --follow`.
- DESIGN.md's roadmap section repeats parts of this plan.

## Phase 3

Planned on 2026-10-16. Exit: the SCADA reference app (X1) runs a full release cycle on an HA pair and a witness with zero downtime: deploy, an upgrade with state migration on both nodes, a planned switchover, a rollback, and a node lost in the middle of the upgrade, while the HMI keeps answering and point updates keep flowing (X2).

Done on 2026-10-18: K1 and K2, replicated shared buckets (`scope = "cluster"`, `paddock_host::replication`): spike S20's model (R1 to R7, with `PromoteAlone = FALSE` and `EagerSync`: one failure at a time exhaustively, 4.3 billion states, no violation), the witness's record of a partner out of date (R13), writes forwarded from the standby, local reads and watches, `get-linearizable`, the log bounded by bytes and age, `paddock failover` and metrics. A replicated write takes about 2.3 ms at p50 on loopback, and 64 writers reach about 4,000 writes/s (BASELINE.md, 2026-10-18). An hour of `scripts/soak.sh --only replication` with the serial line on the USB null-modem pair, on an idle machine, passed (six runs of 10 minutes, seeds 994837367 to 994837372: after every heal both copies agreed and held every acknowledged write). The soak found two ways for an HA pair to stay apart after a heal (a lost members list, and a join dropped while its seed was marked unreachable); membership is now gossiped (CHANGELOG, Fixed). Also done on 2026-10-18: K3, apps' own `replicated` buckets, in K2's log as `<app>/<bucket>`, on nodes with `[failover] replication = true` (opt-in: a lone primary running replication gets its standby recorded out of date, which blocks that standby's promotion until it catches up, so pairs that do not replicate keep failover as it was); a singleton that opens a replicated bucket starts on a new primary only once that node's copy is current (R5's condition; `docs/tla/VERIFICATION.md`). A bucket that changes durability starts empty.

Done on 2026-10-21: X1, the SCADA reference app (`examples/scada`): `points`, `hmi` and the `control` singleton in two versions on the pair's replicated state, and `drill.sh`, a release cycle through `paddock release` (with `--standby-only`, to lose the primary between a release's steps) and a planned switchover; its runs are in its README.

Done on 2026-10-23: N1, guest sockets. `wasi:sockets` is enforced from the manifest's `connect` and `bind` rules and `listen`, inside node config's `[sockets]`, with the address guard (loopback, link-local and unspecified destinations need a rule inside their range) and `max_sockets` per app across its instances. An instance waits on its sockets between callbacks through `paddock:io` (readiness as the host event `io-ready`, as Erlang's `{active, once}`); `paddock_sdk::net` wraps both for TCP.

Done on 2026-10-23: N2, native-process apps, after spike S19 chose a port over a node. `kind = "native-process"` runs the bundle's platform variant as one process per instance through `paddock-runner` (`crates/paddock-native`), which restricts itself (non-dumpable, Landlock, a seccomp filter) and then loads a rustbridge plugin (callbacks and logging only) or executes a program on `paddock-native`'s app API (with the process functions). Supervised like an instance; a callback deadline and pings kill a stuck one; `memory_max` as a delegated cgroup's `memory.max` or `RLIMIT_DATA`; `wasi:logging` only, the network and other services refused at deploy. Linux only; tested on Linux 6.8 x86-64 and on the Pi (Linux 7.0, aarch64), cgroup limits included.

Done on 2026-10-22: X2, Phase 3's exit. `paddock-soak scada` runs X1's apps on the failover soak's pair and witness, with link cuts, holds and drops, kills and stops between releases of `control` across the pair, planned switchovers and a primary lost in the middle of a release. On planned steps it fails no point read, leaves no point gap of 1 s, and fails `control`'s requests only within a switchover's hand-over (1 s). After every heal one node runs `control`, the acknowledged alarm is still acknowledged, and the release converges. Under faults it records failures and gaps without judging them. An hour on an idle machine (seed 2026) passed: 45 switchovers with point gaps of 813 ms at most, 34 releases with no failed request, 58 heals, no overlap (examples/scada/README.md). The first hour failed on switchovers: the hand-over grew with the replicated log and passed 1 s, because a new primary counted its send window from a match it did not yet have (CHANGELOG, Fixed; S20's `Retransmit` widened to match, Design1 and Eager1 rerun). The stall X1's third run found when a standby rejoins is fixed too (2026-10-21). X1's history of point values was dropped: the exit did not need it.

Decided 2026-10-16: the pair and its witness are the target topology, and general clusters of many nodes come later; replicated buckets replicate synchronously within the pair, using the failover epochs and leases, not Raft; blue-green upgrades, guest sockets, native-process plugins and moving instances are in scope; general placement, global names, more guest SDKs and fuel metering are parked.

Order: X1 first, so the app drives the rest; K3 next, on K2's machinery; N1 alongside K, since it shares no code; M1 after K3; L1 after M1; U1 after L1; U2 after U1; N2's spike whenever there is room, its implementation after; X2 last.

- [ ] **W2 Phase 3's docs.** A docs pass for Phase 3, as W1 for Phase 2.

Parked from Phase 3 (2026-10-16): placement across many nodes (labels, spread, `all-nodes`); opt-in global names (Raft); more guest SDKs; fuel metering.

The rustbridge work left from Phase 3 is the guest adapter (`Plugin` → `paddock:runtime/app`) and the `Server` trait; the native-process runner is Paddock's own (`paddock-runner`, N2), loading plugins through `rustbridge-consumer`.

## Investigations

Questions for later, each a spike with a time-box when scheduled.

- **S18 Checking hot-standby instances follow the primary.** Compare the primary's and the standby's sequences of host calls (from probes), allowing for timestamps, to find a standby that has diverged. After D4.
- **ZFS.** A node's data directory on a dataset: snapshots for backups and for seeding a standby's buckets.
- **Jails and containers.** Running native plugins (S19) in FreeBSD jails, or the node itself in a rootless container.
- **S9 Debugging guests** (parked). wasmtime 49 offers a native debugger (`Config::debug_info` with `debug-builtins`: GDB and LLDB on JIT code, pausing the whole node, for development nodes) and an experimental guest debug API (`Config::guest_debug`: breakpoints and stepping per Store, which could park one instance behind a Debug Adapter Protocol endpoint while the others run; needs the guest's DWARF, an Engine of its own, and time parked counted as host time). The spike would check whether `guest_debug` works in wasmtime 49, measure the slowdown, and try one DAP session from VS Code. Time-box 2 days; if the API is not ready, document the native route.

## Parked

- **DNP3 and ICCP extensions.** The mature DNP3 implementations in Rust found so far are source-available with commercial terms; no permissively licensed ICCP stack in Rust is known.
- **Redis or etcd bridge** as an extension (A5), for sites that already run one. Not the platform's shared buckets.
- **Capability tokens for messaging** (seL4-style endpoint handles), after D5's declared routes.
- **Zero-copy large messages** (host-owned blobs passed by handle), only if measurements show copies matter. F4 (2026-10-16) did not: with batches of 100 to 1,000 small points, the messages were under 1% of the cost; large single payloads were not measured.
- **Handing a connection to another instance** (`gen_tcp:controlling_process`), rare with an instance per connection.
- **Persistent timers** (cron, the EJB timer service) that survive restarts.
- **A finer timer** (from B10, 2026-10-24): an OS thread with a deadline heap for `send-after` and guest sleeps, waking tasks across threads. A thread's sleep is about 55 µs late against tokio's 1 to 2 ms (BASELINE.md, 2026-10-24); BEAM's `send_after` is 0.4 ms. Only if an app needs sub-millisecond timers.
- **Node alarms** (OTP `os_mon`): memory, disk and storage errors as host events and in the admin API.
- **Key expiry** in keyvalue: a TTL per bucket or per `set`.
- **rustbridge release.** Switch `rustbridge-bundle` from the `experiment/wasm` branch to a crates.io release with bundle format 1.2; required before the repository goes public or a crate is published.
- **`gen_statem` support in the SDK**: state timeouts and `postpone` on the existing callbacks and timers.
- **WASI 0.3 world** (`paddock:runtime@0.2.0`, async), when wasmtime marks component-model async stable and Rust has `wasm32-wasip3`; rerun S15's connection sweep first (DESIGN.md, "WASI 0.2 to 0.3 migration").
- **Serial ports for apps** (narrowed out of F5 on 2026-10-16, no user yet): a port bound like a listener and served by a connection handler as one byte stream, or frames (by delimiter, idle gap or length) cast to an app with a `write` import, as an extension. Would reuse F5's tty layer, moved into a crate of its own.
- **Placement across many nodes** (from Phase 3, 2026-10-16): node labels required or preferred, `spread`, `all-nodes`; the pair needs none of them.
- **Opt-in global names** (from Phase 3, 2026-10-16): a cluster-wide registry on Raft; cluster-wide groups (D4) cover lookups across nodes meanwhile.
- **More guest SDKs** (from Phase 3, 2026-10-16): other Component Model languages; any can target the WIT today without an SDK.
- **Fuel metering** (from Phase 3, 2026-10-16): deterministic CPU accounting in place of the epoch deadline.
- **Modbus RTU** (narrowed out of F5 on 2026-10-16; likely never): RTU master in `paddock-modbus`, one task per bus polling each unit in turn, over F5's tty layer; an RTU server would need tokio-modbus's MPL-licensed serial feature or an implementation of Paddock's own. Last on this plan.

## Code areas

An area is a set of modules with a narrow interface to the rest: modules in crates with `pub(crate)` and a small `pub` surface, rather than a crate each. R5 (2026-10-10) kept these areas and the crates as they are.

| Area | Where | Interface to the rest |
| --- | --- | --- |
| Contract | `wit/`, WIT rules tests | The `paddock:runtime` world and the separate packages (`connection`, `groups`, `watch`, `secrets`, `metrics`); versioned, never changed in place |
| Manifest and bundle | `paddock-bundle` | `AppSpec`, `VerifiedBundle`, the effective grant |
| Process core | `paddock-host`: `pid`, `table`, `registry`, `links`, `message`, `instance`, `runtime`, `state`, `process`, `delivery` | Spawn, call, cast, link, monitor, exit; `delivery` routes every pid |
| Host services | `paddock-host::services`, one module each; `metrics` | The `Service` trait; extensions through `Extension` |
| Orchestration | `paddock-host`: `apps`, `pool`, `supervisor`, `upgrade`, `lifecycle`, `spec` | `Apps`, `AppHandle`, `UpgradeOptions` |
| Cluster | `paddock-host`: `transport` (`liveness`, `membership`, `memory`, `quic`, `wire`), `failover`, `replication` | The `Transport` trait, node events, `FailoverHandle`, `ReplicationStatus`; replicated buckets reach the keyvalue service through `KvStore` |
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
| Admin API transport | JSON over HTTP on a Unix socket, and over TCP with TLS and pinned keys (`[admin]`, 2026-10-14) |
| Node identity | Pinned Ed25519 keys and peers files, not a CA (2026-10-09) |
| Failover | Epochs, heartbeat paths, a witness and a lease; no Raft (2026-10-08) |
| HTTP requests | Calls to a pooled instance, node-level ingress (2026-10-09) |
| Metrics export | Prometheus text, metrics per app (2026-10-09) |
| Build features | Modbus off by default; HTTP, TLS, probes, SQL on (2026-10-10) |
| Node layout | Everything a node writes under `data_dir`, admin socket and compiled components included (2026-10-10) |
| Logging | stderr only; journald or the container runtime rotates (2026-10-10) |
| Privileges | Unprivileged user; `CAP_NET_BIND_SERVICE` only, for ports below 1024 (2026-10-10) |
| Moving a singleton | Its snapshot through the replicated log, taken by every start; a new pid; links, monitors, names and the mailbox not carried; the old instance exits `normal` (2026-10-20) |

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
| S11 | Passivation | Works, off by default; 100,000 instances on 1,000 Store slots, wakes in about 64 to 101 µs; apps opt in since E7 ([spikes/s11](../spikes/s11/README.md)) |
| S12 | TLA+ model of remote links and monitors | Four ways for the nodes to disagree, each fixed by a rule (done in C6); with them no violation found: full queues and liveness exhaustive, closes and crashes up to about 840 and 890 million states ([spikes/s12](../spikes/s12/README.md)) |
| S13 | Host-owned TCP listeners and guest streams | Guest streams at native throughput with an instance per connection ([spikes/s13](../spikes/s13/README.md)) |
| S14 | Storage in the guest versus host services | About 140 ns per host import; keyvalue reads 1.2 µs over native ([spikes/s14](../spikes/s14/README.md)) |
| S15 | HTTP in the guest versus the host | Host HTTP plus an instance call keeps about 75% of native; an instance per request about 15% ([spikes/s15](../spikes/s15/README.md)) |
| S16 | TLA+ model of failover | Done; now layer 1 of [docs/tla](tla/VERIFICATION.md) ([spikes/s16](../spikes/s16/README.md)) |
| S17 | Moving a running instance to another node | Settled without a spike program (M1, 2026-10-20): the snapshot through the replicated log, a new pid, nothing else carried (DESIGN.md, "Moving a singleton with its state") |
| S18 | Hot-standby instances following the primary | Investigation |
| S19 | Ports or nodes for external processes | A port: only a process the node spawns can be limited and stopped, and a cluster member's stamps are trusted; about 37 µs a call, 1.9 ms to start, 3.8 MiB; Landlock and a cgroup limit it ([spikes/s19](../spikes/s19/README.md)) |
| S20 | TLA+ model of replication within the HA pair | Done (K1): rules R1 to R7; no committed write lost with one failure at a time, exhaustive to about 2.2 billion states, and each rule's absence gives a counterexample ([spikes/s20](../spikes/s20/README.md)) |
