# Review: HAppliance and Erlang/OTP against Paddock

2026-10-06 · For evaluation; nothing here is adopted until it moves into
[DESIGN.md](DESIGN.md) or [PHASE1_PLAN.md](PHASE1_PLAN.md).

## Scope

This review compares Paddock's plan with two sources and lists ideas worth
adopting, each marked as an action, a spike, or a note for a later phase.

- **HAppliance** (`../../HAppliance`, commit `f613206`, 2026-03-01): an HA
  key-value appliance in Rust. It runs two nodes and a witness, replicates
  synchronously, and runs colocated native apps in FreeBSD jails with
  Capsicum. The apps talk to the host in protobuf over Unix sockets. Read:
  `README.md`, `ARCHITECTURE.md`, `ROADMAP.md`, `CHANGELOG.md`, `docs/*.md`,
  `docs/tla/VERIFICATION.md`, `proto/ipc.proto`,
  `crates/haap-supervisor/src/manifest.rs`.
- **Erlang/OTP**: features of the runtime and libraries that Paddock's design
  does not mention.
- **Paddock**: `docs/DESIGN.md`, `docs/PHASE1_PLAN.md`, `wit/runtime.wit`, and
  the M7 work in progress (`paddock-node`, `paddock-api`), as of commit
  `e137f2e` plus uncommitted changes.

## Summary

HAppliance and Paddock solve the same problem in different ways: hosting
supervised apps next to host-owned state, with messages routed through the
host. HAppliance is further along in the areas Paddock leaves to Phases 2 and
3: failover, durable messaging, fencing, OS-level sandboxing and chaos
testing. Paddock is further along in the per-instance model: links, monitors,
OTP supervisors and snapshot upgrade.

Of the findings below, one needs a decision before `paddock:runtime@0.1.0` is
treated as stable (F1). Three are small additions that reuse Phase 1
machinery (F5 to F7). The rest feed Phase 2 and 3 design.

## Comparison

| Area | HAppliance | Erlang/OTP | Paddock (planned) |
| --- | --- | --- | --- |
| Isolation | Jail, Capsicum, rctl, ZFS quota, pf | None (shared VM) | Wasm Store per instance; OS limits for `native-process` only |
| Messaging | call/cast through the supervisor | At most once, ordered per pair | At most once, ordered per pair |
| Durable messages | Cast modes `Immediate`, `Persisted`, `Delivered`; replicated queue, TTL, replay on restart | None built in | Non-goal "by default" |
| Data-change events | `KeyChanged` for key globs | `mnesia:subscribe` | None |
| Role awareness | App modes `singleton`, `all-nodes`, `hot-standby`; `RoleChanged` message | Distributed applications (`dist_ac`, takeover, failover) | `placement.singleton: bool`, Phase 3 |
| Failure detection | Separate heartbeat link; serial link separates a partition from a crash | `net_ticktime` | QUIC idle timeout, 15 s default |
| Node watchdog | `watchdog(4)` planned (Milestone 10) | `heart` | Listed as an analogue only |
| Replication | Custom sync protocol with witness and epoch fencing, model-checked in TLA+ | Mnesia | Raft (openraft), Phase 3 |
| Node binary upgrade | Drain, rolling restart, resync, ZFS boot environment rollback | Rolling restart or relup | Not covered; app upgrades only |
| Listening sockets | Host binds, passes the fd with `SCM_RIGHTS` | `gen_tcp` | `wasi:sockets` with listen yes/no, Phase 3 |
| Bounded mailboxes | Yes, default 64 | No | Yes |

## Findings

Each finding has a type: Action (a change to a doc, the manifest model or
code), Spike (a question to answer first; see [Proposed spikes](#proposed-spikes)),
or Note (input for a later phase's design).

### Before the WIT is treated as stable

#### F1. The `info` variant cannot grow without a new world version

Spike S10, then Action.

- `variant info` has three cases: `exit`, `down`, `timer`
  (`wit/runtime.wit:86`). Component Model types are compared structurally,
  so adding a case should make the host's `handle-info` type differ from the
  one a guest built against `@0.1.0` exports. S10 confirms this.
- HAppliance already needed two more host-to-app events: `RoleChanged`
  (F2) and `KeyChanged` (F4). Likely Phase 2 and 3 additions are
  `role-changed`, `key-changed`, `node-up`/`node-down` (OTP's
  `monitor_node`) and `takeover`.
- Options:
  - Reserve a generic case now, for example
    `host-event(tuple<string, envelope>)`, so new events reach `@0.1.0` guests
    as a typed name plus an envelope.
  - Accept that each new event needs `@0.2.0` and the per-version linker
    selection already in the WASI migration plan.
- Changing the WIT is cheapest before M8 publishes `paddock-sdk` and before
  any app outside the repo exists.

### Manifest data model (record now, enforce later)

#### F2. App modes instead of a singleton flag

Action, data model only.

- HAppliance's `AppMode` has three values:
  - `singleton`: runs on the primary only, and moves when the primary
    changes.
  - `all-nodes`: runs on every node.
  - `hot-standby`: runs on every node, and each instance is told whether it
    leads.
- `hot-standby` is OTP's distributed application with `takeover`; Paddock
  has no equivalent. `placement.singleton` is a `bool`
  (`crates/paddock-manifest/src/sections.rs:210`). An enum such as
  `placement.mode = "spread" | "singleton" | "all-nodes" | "hot-standby"`
  costs little now, under the rule that the data model is complete from day
  one. The node already warns about unenforced fields.
- Enforcement needs placement (Phase 3) and an event to deliver (F1).

#### F3. Durable cast as an opt-in capability

Note for Phase 2 or 3, with a
DESIGN.md entry.

- HAppliance offers three cast modes and fixes the rules around them:
  - `Immediate`: in-memory delivery.
  - `Persisted`: written to a replicated queue, replayed when the app
    restarts.
  - `Delivered`: waits until the remote node accepts the message.

  Queues are bounded and reject new messages when full, persisted messages
  carry a TTL, and a dead-letter queue is planned. `call` is never durable;
  a workflow that needs persistence uses a persisted cast plus a separate
  reply message.
- For Paddock this belongs in a capability (`paddock:queue`, or a pattern on
  top of `wasi:keyvalue` with F4). `process` keeps Erlang's at-most-once
  promise, which the non-goals already state. The DESIGN.md non-goal could
  name the opt-in path.

#### F4. Keyvalue watch

Note for Phase 2, depends on F1.

- HAppliance's `[subscriptions] keys` sends `KeyChanged` for exact, prefix
  and `*` glob matches. It fires on any local write, including writes that
  arrived by replication, so replication needs no extra fan-out.
- In Paddock this would be a bucket watch delivered as an `info` event: the
  role `mnesia:subscribe` plays in OTP. `wasi:keyvalue@0.2.0-draft` has no
  watch, so it would be a `paddock:*` interface, with an `access-denied`
  error per the interface rule.

### Cheap with existing machinery

#### F5. Passivation through the snapshot pair

Spike S11.

- An idle instance runs `save-snapshot`. The host drops its Store and keeps
  the pid, mailbox, links, monitors, names and timers, which are host-side.
  The next message instantiates the component again and runs `load-snapshot`.
  This is OTP's `erlang:hibernate`, taken further (as in Orleans and Akka
  passivation).
- It addresses the limit S2 found: virtual address space per live Store
  (about 15,000 instances per node with default reservations). The host
  holds pids cheaply; only Stores cost address space.
- M6 already does save, drop and rebuild; S5 measured 0.2 to 0.35 ms per
  swap with pre-compiled components. S11 confirms the rules: an instance with
  a pending outbound call or an outstanding deferred reply stays live, and
  timers wake a passivated instance.

#### F6. Admin `state <pid>` through `save-snapshot`

Action, M7 or M9.

- This is OTP's `sys:get_state`. Between callbacks the node calls
  `save-snapshot` and returns the bytes. The CLI decodes them when the
  snapshot is CBOR or JSON (the SDK default in M8), and prints hex otherwise.
- Caveat: a trap in `save-snapshot` poisons the Store. The command has to
  treat that the way an upgrade does: the instance exits and its supervisor
  restarts it. The command should say so in its help text.
- `paddock-api` has `status`, `ps` and `kill`, but no per-instance state
  view yet.

#### F7. Node watchdog (OTP `heart`)

Action, late Phase 1 or Phase 2.

- The epoch ticker is already an OS thread. It could pat a watchdog only
  while the tokio runtime answers a probe, which catches a wedged node.
  Instance supervision cannot see that failure.
- Targets differ by OS: `sd_notify` `WATCHDOG=1` under systemd on Linux,
  `watchdog(4)` on FreeBSD; there is nothing standard on macOS or Windows.
  Any crate goes through `cargo deny` first.
- HAppliance plans the same thing (Milestone 10) and ties it to replication
  health. For Paddock, the first condition is the runtime answering; others
  can follow.

### Phase 2 and 3 design input

#### F8. Failure detection beyond one timeout

Note for Phase 2.

- HAppliance's most recent commit (`f613206`, `CHANGELOG.md` 0.2.23) added a
  dedicated heartbeat link. Its chaos test
  `chaos___primary_kill_with_load___promoted_secondary_serves_all_writes` was
  flaky when liveness came from the data path alone. With the heartbeat, a
  Secondary detects a dead Primary in about 500 ms in tests and about 1 s in
  production configuration (HAppliance's figures, not re-measured here).
- Paddock already keeps heartbeats on a control stream, apart from message
  streams. Worth adding to the Phase 2 design:
  - A failure-detector trait, as HAppliance's `HeartbeatLink` abstracts its
    serial link, so a second path can be added.
  - A rule for what `noconnection` waits for. A false node-down breaks every
    link through that node, the cost Erlang users know from `net_ticktime`.

#### F9. Fencing epochs for placement

Note for Phase 3.

- Pid incarnations already fence instances: a stale pid never reaches a new
  instance. Singleton placement and failover need the same per placement, so
  a node that returns after a partition cannot run a second copy of a
  singleton.
- HAppliance's model has a monotonic epoch stored by a witness, and a node
  holding a stale epoch is fenced on contact. TLA+ checks it as
  `AtMostOnePrimaryPerEpoch`.

#### F10. Raft is not the only option for replicated storage

Note for Phase 3.

- HAppliance replaced openraft with its own protocol. The main reason was
  topology: with Raft on 2 nodes plus a remote witness, losing one HA node
  makes every write wait for the WAN round trip
  (`docs/plan-option1-sync-replication.md`).
- A second issue stays open in `docs/CHAOS_TEST_INVESTIGATION.md`: openraft
  assertion failures and lost acknowledged writes in an aggressive chaos test
  run with `SyncNone`. Configuration may account for part of it. It was not
  resolved before the switch.
- Paddock plans openraft for global names and `replicated` keyvalue. If
  2-node-plus-witness edge sites are a target, the `replicated` durability
  should sit behind a backend trait, as HAppliance's `StorageEngine` and
  `WalEngine` do. Global names, which hold little data and can tolerate WAN
  latency, fit Raft well.

#### F11. Model-check the distributed protocols

Spike S12, before Phase 2
implementation.

- HAppliance specified its protocol in TLA+ in three layers. Model checking
  found 7 protocol bugs before implementation (`docs/tla/VERIFICATION.md`),
  among them stale promotion requests, divergent timelines after resync, and
  in-flight writes inherited across promotion.
- Paddock candidates:
  - Remote links and monitors with `noconnection`, and an incarnation reused
    after a reconnect.
  - Process-group merge after a partition heals.
  - Rollout stop-and-revert when a node is lost mid-batch.
  - Singleton handover (F9).

#### F12. Listening sockets owned by the host

Spike S13, Phase 3.

- HAppliance binds ports on the host and passes the fd to the jailed app.
  The app never needs bind permission, and the port stays open while the app
  restarts.
- For Paddock, this would remove bind rules from guests and let a listener
  survive instance restart and upgrade, which suits `blue-green` mode. It goes
  beyond what OTP offers.
- wasmtime-wasi 49 offers only `socket_addr_check`, `allow_tcp` and
  `allow_udp`. WASI 0.2 has no pre-opened sockets: the guest creates and binds
  its own. A host-owned listener therefore needs a `paddock:*` resource or a
  wasmtime-wasi extension. HTTP ingress (Phase 2) already covers the HTTP
  case, so S13 matters only for raw TCP apps such as protocol gateways
  (Modbus, MQTT, OPC UA in HAppliance's use cases).

#### F13. Reuse HAppliance's platform layer for `native-process`

Note for
Phase 3.

- `haap-platform` has a `Platform` trait with two backends:
  - `PortablePlatform`: plain child processes.
  - `FreeBsdPlatform`: jails, nullfs mounts, rctl limits, and a ZFS dataset
    with a quota per app.

  Paddock's plan for `native-process` (cgroups, seccomp, Landlock) has the
  same shape, with Linux backends instead.
- Per-app datasets or subvolumes (ZFS, btrfs) also answer DESIGN.md's note
  that filesystem quotas "need a custom `wasi:filesystem` backend or OS quotas
  per volume". That applies to Wasm apps' `/data` volumes too, not only to
  native processes.
- HAppliance uses `capsicum = "0.4"`; its licence has not been checked
  against `deny.toml`.

#### F14. Node drain and node binary upgrade

Note for Phase 2.

- Paddock covers app upgrades but not upgrading `paddock-node` itself.
  HAppliance's procedure, in order:
  1. Drain the node.
  2. Stop it.
  3. Deploy the new binary.
  4. Start it; it resyncs and rejoins.
  5. Check health, then move to the next node.

  ZFS boot environments give it a rollback.
- For Paddock, "drain" means stopping or moving instances, so it depends on
  placement. The Phase 2 wire format already negotiates a protocol version,
  which a rolling node upgrade needs. A drain-and-rejoin test would fit the
  Phase 2 exit tests next to partition and node loss.

#### F15. Test infrastructure for Phase 2

Note for Phase 2.

- HAppliance's test infrastructure:
  - An in-process cluster harness.
  - Fast and slow test tiers: the fast tier gates CI, and chaos tests run by
    hand.
  - libvirt scripts that build a real 3-node network
    (`scripts/vm/`).
- Lessons from its chaos investigation: a harness `kill_node` that did not
  wait for the old node to stop caused false data loss. HAppliance added a
  data-directory lock as a defense.
- Paddock already refuses a second node on the same data directory:
  `Runtime::new` opens `keyvalue.redb` before `restore()` runs, and redb holds
  a file lock. No test pins this yet; one would be a small Action for M9.

### Erlang/OTP features not in the plan

#### F16. OTP gaps

Notes; each is small and can be decided separately.

- **`gen_statem`**: state timeouts and `postpone`. Both fit in `paddock-sdk`
  on top of the existing callbacks and timers. `postpone` is the main case
  for the open question on selective receive; a deferred queue in the SDK may
  settle it.
- **`erlang:system_monitor`**: events for long schedules, large heaps and
  busy distribution links. The host sees all of these (callback time, memory
  growth, mailbox depth); they fit the anyprobe probes already noted in
  PHASE1_PLAN.md.
- **Backpressure between nodes**: OTP has `busy_dist_port` and
  `send_nosuspend`. DESIGN.md defines what a sender sees when the target's
  mailbox is full, but not when the link to the remote node is congested.
- **Broadcast and multicall** (`erpc:multicall`): HAppliance's broadcast is
  best-effort, returns partial results, and never lets one slow node block
  the others. That is a reasonable shape for a multicall over Phase 2 process
  groups.
- **Permanent applications**: in OTP, a permanent application that fails
  stops the node, and `heart` restarts it. Paddock marks the app `failed` and
  raises an alert. The difference is reasonable; DESIGN.md does not state it.

## Second pass (2026-10-07)

After M7 landed, the findings above were checked again against the code (all file and line references still hold), and HAppliance's `ROADMAP.md` and `docs/PLAN-production-readiness.md` were read for anything missed.

- **S10 has run.** F1 holds: one more `info` case makes `AppPre::new` refuse a guest built against `@0.1.0` ("expected variant of 4 cases, found 3 cases"). See [`spikes/s10/README.md`](../spikes/s10/README.md). A1 becomes the first step of M8.
- **F6 and A5 still apply.** The admin API has no per-instance state view, and no test pins the second-node refusal.

### Further findings

#### F17. Change an app's log level at runtime

Action, M9 or Phase 2.

- HAppliance's host has a `set_log_level` command per app. OTP has `logger:set_application_level`.
- Paddock takes the level from the manifest, and changing it means a redeploy. The log policy is already per instance in the host, so an admin `paddock log-level <app> <level>` is small.

#### F18. Key expiry in keyvalue

Note for Phase 2.

- HAppliance plans TTLs with a background reaper (Milestone 8, tier 2). `wasi:keyvalue@0.2.0-draft` has no expiry, and neither has ETS.
- Caches and session stores want it. It would be a `paddock:*` extension next to F4's watch: a per-bucket default TTL in the manifest scope, or a TTL on `set`.

#### F19. Backup and restore of disk buckets

Note for Phase 2.

- HAppliance has full-snapshot resync and plans ZFS send and receive. Paddock's disk buckets live in one redb file with no backup story.
- A consistent copy needs a read transaction held across the copy, or a redb savepoint. An admin `paddock backup` that writes the copy, plus restore on an empty node, would cover single-node sites until replication arrives.

#### F20. Config changes without a restart

Note for Phase 2.

- OTP calls `config_change/3` when application environment changes during a release upgrade. Paddock applies node config overrides only at deploy or start.
- A host-sent event (through F1's reserved case) telling an instance its config changed, with `wasi:config` reading the new values, would avoid restarts for config-only changes.

#### F21. Release upgrades across several apps

Note for M9 or Phase 2.

- OTP's `release_handler` applies one release, several applications at once, in dependency order. DESIGN.md says rollouts follow `dependencies`, but `Apps::upgrade` moves one app at a time and the operator orders them.
- A "release" (a set of app versions upgraded together, providers first, reverted together) is a small layer over M6.

#### F22. Node resource alarms

Note for Phase 2, with F7.

- OTP's `os_mon` raises alarms for memory, disk and CPU. HAppliance stops patting its watchdog when storage errors.
- Paddock's node can see address-space use (pool slots), disk space under `data_dir`, and keyvalue quota use. Alarms in the audit log and in `paddock status` would fit next to F7.

#### F23. Scheduling priority between apps

Note, no phase.

- OTP has process priorities. Paddock gives every instance the same epoch-based share of a worker.
- A per-app weight (more epochs before yielding) is possible but not needed by any current use case. Recorded so the question is not lost.

## Proposed spikes

Numbered after S9, which PHASE1_PLAN.md already proposes.

| # | Question | Done when | Time-box | If the answer is bad |
| --- | --- | --- | --- | --- |
| S10 (done) | Does adding a case to `info` (or any type a guest export uses) stop a guest built against the old WIT from instantiating? If so, does a reserved `host-event(tuple<string, envelope>)` case carry new events to old guests? | The echo fixture built against `@0.1.0` loaded by a host whose WIT has one extra `info` case, with the error recorded; then the same with a reserved generic case | 0.5 day | If old guests still load, nothing to reserve; record why |
| S11 | Can an idle instance be passivated (snapshot, drop the Store, rebuild on the next message) without changing what other instances see? | Counter fixture passivated and woken by a cast, a call, a timer and an exit signal; wake latency with pre-compiled components; instance count at the address-space limit with and without passivation; written rules on when an instance must stay live | 2 days | Keep instances live; document address space as the hard limit |
| S12 | Do the Phase 2 link, monitor and `noconnection` rules hold under partition, reconnect and node restart? | A TLA+ spec of remote links and monitors with pid incarnations, model-checked with a safe and an adversarial network, invariants listed (as `docs/tla/VERIFICATION.md` does in HAppliance) | 3 days | Narrow the Phase 2 guarantees to what the model shows holds |
| S13 | Can the host bind a TCP listener and hand it to a guest as a resource, kept across instance restart and upgrade? | A guest accepting connections on a host-owned listener through a `paddock:*` resource, or a wasmtime-wasi extension point that allows it; a restart with no refused connection | 1 day | Raw TCP apps bind their own sockets under bind rules; HTTP ingress covers HTTP |

## Proposed actions

| # | Finding | Change | Where | When |
| --- | --- | --- | --- | --- |
| A1 | F1 | Add `host-event(tuple<string, envelope>)` to `info` (S10 confirmed the need) | `wit/runtime.wit`, fixtures, DESIGN.md | First step of M8 |
| A2 | F2 | `placement.mode` enum instead of `singleton: bool`; unenforced warning | `paddock-manifest`, DESIGN.md manifest table | Phase 1, any time |
| A3 | F6 | Admin `state <pid>` through `save-snapshot` | `paddock-api`, `paddock-node`, `paddock-cli` | M7 or M9 |
| A4 | F7 | Watchdog pat from the epoch ticker thread, Linux first | `paddock-node` | Late Phase 1 or Phase 2 |
| A5 | F15 | Test: a second node on the same data directory is refused | `paddock-node` tests | M9 |
| A6 | F3, F16 | DESIGN.md: name the opt-in durable path in the non-goals; state the permanent-application difference from OTP | DESIGN.md | Done 2026-10-07 |
| A7 | F17 | Admin `log-level <app> <level>` at runtime | `paddock-host`, `paddock-node`, `paddock-cli` | M9 or Phase 2 |

## Checked and already aligned

- Bounded mailboxes with explicit backpressure; HAppliance reached the same
  rule independently ("bounded, not unbounded").
- Every message passes through the host, which stamps the sender; neither
  project allows direct app-to-app channels.
- Per-app log tagging, level caps and host-side log collection.
- Supervisor-owned restart policy with backoff. HAppliance has exponential or
  fixed backoff with max attempts. Paddock has OTP intensity and period, plus
  jitter, which covers it.
- mTLS between nodes. HAppliance makes TLS optional for air-gapped sites;
  Paddock requires it, which is simpler to reason about.

## Not recommended

- **Protobuf for envelopes or the wire.** HAppliance uses prost throughout.
  Paddock's envelopes are content-typed bytes (CBOR by default), and the
  frames are length-prefixed CBOR; adding protobuf would add a toolchain
  (`protoc`) without a need.
- **Role-based IPC lifecycle in place of supervision.** HAppliance's
  supervisor reconciles which apps should run on each role change; it has no
  links, monitors or supervision trees. Paddock's OTP model already covers
  more. Only the role signal itself (F1, F2) carries over.
