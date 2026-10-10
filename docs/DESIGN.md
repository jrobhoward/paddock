# Paddock: Design

2026-10-05 · Living version: https://claude.ai/code/artifact/095fba1a-7b9a-4585-96f3-e2e43d26628d


## Summary

Paddock is a clustered application server that hosts sandboxed WebAssembly apps with OTP-style semantics: mailboxes, call/cast, links, monitors, supervisors, and hot upgrade. Nodes connect over QUIC with mutual TLS. Apps are WebAssembly components that see only the capabilities their manifest declares and an operator grants. rustbridge supplies the bundle format, signing, and a native-plugin path.

The model is coarse-grained: tens to a few thousand app instances per node, not millions of processes. That choice removes Wasm's main weakness (per-instance memory and address-space cost) and keeps BEAM's main strength (fault isolation and supervision).

## Goals

- Host Wasm apps written in any Component Model language, Rust first.
- Give each app enforced limits on memory, CPU time, filesystem, and network.
- Let apps send each other messages with call (request/reply with timeout) and cast (fire and forget), locally or across nodes.
- Detect crashed or unresponsive apps, restart them under supervision, and notify linked and monitoring apps.
- Deploy, upgrade, roll back, and remove apps on a running cluster, with explicit state migration.
- Provide OTP/JBoss-style host services: registry, logging, config, term storage, jailed files, HTTP in and out.
- Keep the manifest and permission data model complete from day one, even where enforcement ships later.

## Non-goals

- Millions of lightweight processes per node.
- Running unmodified Erlang or BEAM bytecode.
- Shared memory between apps, or threads shared across app boundaries.
- Strong consistency or exactly-once delivery by default. Delivery is at most once, ordered per sender/receiver pair, as in Erlang. Durable delivery, when it comes, is an opt-in capability (a persisted queue, or keyvalue plus a watch), never a change to `process` (see `docs/REVIEW_HAPPLIANCE_OTP.md`, F3).
- Distributed transactions (JTA-style).

## Background and prior art

Paddock takes the failure model from Erlang/OTP, the deployment model from JBoss/WildFly, and the sandbox from WebAssembly. No existing project combines all three. Each one below shaped part of the design.

| Project | What it got right | What went wrong or is missing | What Paddock takes |
| --- | --- | --- | --- |
| [Erlang/OTP](https://www.erlang.org/doc/system/design_principles.html) | Links, monitors, supervisors; `.app` metadata (deps, registered names, env); appup/relup upgrades with `code_change/3` | One TCP connection per node pair causes head-of-line blocking; cookie auth is weak; `global` struggles in partitions | The failure semantics and the app metadata shape; fix transport and auth |
| [Lunatic](https://github.com/lunatic-solutions/lunatic) | Wasm instance per process; per-process `ProcessConfig` with max memory, max fuel, and spawn/compile permissions, all denied by default | Custom host ABI from before the Component Model; open issues for node authentication (#128), a global registry (#127), hot reload (#150), and socket access control (#99); maintenance stalled (#232) | Deny-by-default config per process; build on Component Model and WASI instead of a private ABI; design auth, registry and reload in from day one |
| [wasmCloud](https://wasmcloud.com/docs/ecosystem/wadm/model/) | Declarative manifests (wadm, OAM-based) with scalers and label-based placement; links bind an interface to a provider with per-link config | Dropped stateful actors for stateless components in 1.0; big rewrites while tracking WASI; earlier signed "claims" in modules turned out redundant with WIT imports | Manifest-driven deploy, label placement, per-binding config; derive permissions from WIT imports, not a parallel claims list |
| [Golem](https://learn.golem.cloud/concepts/agents) | Stateful addressable instances; sequential invocation queue; `save-snapshot`/`load-snapshot` upgrades that revert on failure; quotas per instance | Durability via an oplog adds latency and weight; not a supervision model | The upgrade protocol (snapshot when idle, revert on failure) and lifecycle states |
| [JBoss/WildFly](https://docs.wildfly.org/32/Developer_Guide.html) | Isolated deployments by default; explicit module dependencies (`jboss-deployment-structure.xml`); server-managed resources (datasources, JNDI) | The Java SecurityManager tried to sandbox code inside a shared heap and was disabled for good in JDK 24 | Isolation by default, declared dependencies, host-managed resources; enforce sandboxing at a hard boundary (Wasm), never by in-process policy |
| Microkernels ([seL4](https://sel4.systems/About/seL4-whitepaper.pdf), [QNX](https://www.qnx.com/developers/docs/7.1/#com.qnx.doc.neutrino.sys_arch/topic/ipc.html), [Fuchsia](https://fuchsia.dev/fuchsia-src/concepts/components/v2/capabilities)) | Services as isolated processes talking only by IPC; endpoints are capabilities, and the receiver learns a sender's identity from an unforgeable badge; the caller pays for kernel objects it creates; scheduling contexts give each component a CPU budget per period; restartable drivers (MINIX 3) | Shared-memory grants and synchronous-only IPC do not fit isolated apps on a network | Host-stamped sender identity; messaging ACLs by declared routes (Fuchsia's `use` and `expose`); a quota on every host object an instance creates; CPU budgets and priority classes as later work |
| [Mnesia](https://www.erlang.org/doc/apps/mnesia/mnesia_chap1.html), [Khepri](https://github.com/rabbitmq/khepri), [etcd](https://etcd.io/docs/v3.5/learning/api/), [Redis](https://redis.io/docs/latest/operate/oss_and_stack/management/replication/) | Replicated tables read from a local copy, with change subscriptions (Mnesia); Raft with local projections for fast reads (Khepri); revisioned keys, resumable watches and leases (etcd); fast async replicas and keyspace notifications (Redis) | Mnesia has no consensus and handles partitions poorly; Redis can lose acknowledged writes on failover | Shared and replicated buckets owned by the host: revisioned watches and compare-and-swap on revision, reads from the local copy; the replication protocol is chosen in Phase 3 (see "Shared buckets and watches") |
| [rustbridge](https://github.com/jrobhoward/rustbridge) | Signed multi-platform `.rbp` bundles with SBOM and provenance; lifecycle state machine; JSON and binary message transport with schemas | Native libraries can't be unloaded safely in-process; no permission model yet | Bundle format, signing and lifecycle, extended with a Wasm payload and a permissions section |

## Architecture

Each node is one OS process: a host runtime in Rust that runs many sandboxed Wasm app instances and joins a QUIC mesh with other nodes. Apps reach the outside world only through WIT imports the operator granted.

```
┌──────────────────────────── Node A ────────────────────────────┐
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────┐        │
│  │    orders    │   │   billing    │   │     web      │        │
│  │Wasm component│   │Wasm component│   │Wasm component│        │
│  │ own memory,  │   │ own memory,  │   │ own memory,  │        │
│  │   mailbox    │   │   mailbox    │   │   mailbox    │        │
│  └──────┬───────┘   └──────┬───────┘   └──────┬───────┘        │
│         │  WIT imports, granted only          │                │
│  ┌──────▼─────────────────▼───────────────────▼─────────────┐  │      ┌────────┐
│  │ Host runtime (wasmtime, tokio)                           │  │      │ Node B │
│  │  Mailboxes, timers │ Supervision     │ Registry          │  │  ┌──►│        │
│  │  Term storage      │ Logging, config │ Capability policy │  │  │   └───▲────┘
│  └──────────────────────────────────────────────────────────┘  │  │       │
│  ┌──────────────────────────────────────────────────────────┐  │  │   ┌───▼────┐
│  │ Distribution: QUIC + mTLS, heartbeats, registry sync     ├──┼──┴──►│ Node C │
│  └──────────────────────────────────────────────────────────┘  │      └────────┘
└────────────────────────────────────────────────────────────────┘
```

Apps sit inside the sandbox; mailboxes, supervision, registry, storage, logging and policy all live in the trusted host. The distribution layer carries messages, links and registry updates between nodes. Native rustbridge plugins run as separate OS processes that the host supervises like any instance.

## App model

An app is a WebAssembly component that implements a `gen_server`-style callback world. The host drives it one message at a time, so timeouts, preemption, crash attribution and upgrades all happen between callbacks.

**Terms**

| Term | Meaning | OTP analogue |
| --- | --- | --- |
| Bundle | Signed, versioned artifact: manifest plus component(s) | Release package, `.ez` |
| App | A named, versioned unit described by one manifest | Application (`.app`) |
| Instance | One running copy of an app's component, with its own Store, memory and mailbox | Process running a `gen_server` |
| Pid | `{node, instance-id, incarnation}`; incarnation changes on every restart so stale pids never reach a new instance | Pid |
| Supervisor | Host-side process that owns child specs and restart policy | `supervisor` |
| Node | One app server process in a cluster | Node |

**Callback, not receive loop.** Lunatic gave each process a blocking `receive()`, which needs async stack switching and makes upgrades hard. Paddock uses callbacks: `init`, `handle-call`, `handle-cast`, `handle-info`, `terminate`, and the snapshot pair. The host never interrupts a callback except to kill it.

**Concurrency inside an instance.** An instance is single-threaded. While it waits on an outbound `call`, the host suspends it (wasmtime async) without blocking a host thread. It does not process other messages meanwhile, matching `gen_server`. Parallelism comes from running a pool of instances under one supervisor.

**Deferred replies.** `handle-call` may return `noreply` and answer later with `reply(ref, msg)`, so an app can serve a call after its own outbound call finishes.

**Start and stop.** `init` receives the manifest's `lifecycle.start_args` encoded as JSON (an empty list when unset) and must finish within `init_timeout_ms`; overrunning it, returning an error or trapping all end the instance with `crashed`. A graceful stop lets the current callback finish, drops messages still queued (their callers get `exited`), then runs `terminate` within `shutdown_timeout_ms`; overrunning that is a kill. A kill abandons the callback in progress and skips `terminate`.

### WIT contract

The contract is the `paddock:runtime@0.1.0` package in [`wit/runtime.wit`](../wit/runtime.wit), the source of truth; this section summarizes it.

- `types`: `pid` (`node`, `id`, `incarnation`), `call-ref`, `envelope`, `exit-reason`, `call-error`, `register-error`, `info` (link `exit`, monitor `down`, `timer`, and a reserved `host-event(name, envelope)`) and `call-result` (`reply` or `noreply`). Every type in an export signature is frozen for a world version (spike S10); `host-event` lets later hosts send new kinds of event to `@0.1.0` guests, which ignore names they do not know.
- `process` (imported by every app): `self`, `cast`, `call` (returns `result<envelope, call-error>`), `reply`, `link`/`unlink`, `trap-exits`, `monitor`/`demonitor`, `register` (returns `result<_, register-error>`), `whereis`, `send-after`/`cancel-timer`, `exit`.
- `world app` exports: `init`, `handle-call(caller, msg)`, `handle-cast`, `handle-info`, `terminate`, `save-snapshot`, `load-snapshot(from-version, snapshot)`.

`call-error` and `register-error` both carry an `access-denied` case (messaging ACLs, names missing from the manifest). `call-error` also has `too-large`, for a body over the target's `message_max_bytes`; it is separate from `overloaded` because retrying cannot help. `crates/paddock/tests/wit_rules.rs` enforces the package's rules: synchronous functions only, no `stream`/`future`, no `wasi:io` types, and an `access-denied` error on every capability function.

### Connection handlers

A second world serves connections instead of messages: `paddock:connection/connection-handler` in [`wit/connection/connection.wit`](../wit/connection/connection.wit). It exports `serve(info, input-stream, output-stream)` and imports `process`. It is a separate package because it uses `wasi:io` streams, which `paddock:runtime` excludes so a WASI 0.3 world can be added beside it; in WASI 0.3 the streams become `stream<u8>`.

- **Listeners.** The manifest declares listeners by name (`[[appserver.listeners]]`, with an idle timeout and a connection cap), and the node binds each name to an address in its config (`[listeners.<name>]`), as with volumes. A name the node does not bind refuses the deploy. The host owns the socket and accepts on several tasks (the Ranch pattern); crashes and upgrades never touch it.
- **An instance per connection** (spike S13: native throughput). It has the app's limits and host services and a pid, so it can call, cast and link, but no mailbox: messages, timers and exit signals sent to it are dropped. A component that exports `paddock:connection/handler` is bundled with this world; a bundle's world and its listeners must agree.
- **No per-callback deadline:** `serve` lasts as long as the connection. A connection with no bytes in either direction for the listener's idle timeout is closed and its instance dropped, which also ends a handler spinning without I/O.
- **Failures** end their connection only, as `crashed` (a trap or an error from `serve`) or `timeout` (idle). Nothing restarts: the client reconnects.
- **Upgrades** apply to new connections; open connections keep the version they started with until they close. A rollback does the same in reverse. Listener names must stay the same across versions.
- **Instance slots.** Each open connection uses one slot of the node's `max_instances`; size it for apps and connections together.

**Messages are envelopes of bytes.** WIT has no recursive types, so there is no clean Erlang-style "any term". An envelope carries a content type (CBOR by default; JSON and rustbridge binary allowed) and an optional schema id. Bytes are already serialized for the network, and the manifest can publish JSON Schemas the way `.rbp` bundles already do.

## App manifest and capability model

The manifest describes everything the server needs to place, limit, wire and upgrade an app. The data model is complete now; enforcement ships in phases (column 3). Field names follow `.rbp` style so the manifest can live in a rustbridge bundle as an extra section.

In a bundle, the `appserver` section holds every section below except three that the `.rbp` manifest already carries: `app` comes from `plugin`, `component` from the `wasm32-wasip2` variant, and `provenance` from `build_info`, the SBOM and the operator key that verified the signature. `paddock_bundle::manifest` models the section (`AppSpec`) and the combined result (`AppManifest`). Unknown fields are rejected with the path of the field, and a node logs a warning for each field a manifest sets that the current phase does not enforce.

| Section | Fields | Enforced in |
| --- | --- | --- |
| `app` | name, version (semver), description, authors, license, repository | Phase 1 |
| `kind` | `wasm-component` or `native-process` (rustbridge `.so` run out of process) | Phase 1 wasm; native later |
| `component` | path, sha256, WIT world and version, exported interfaces | Phase 1 |
| `api` | message types: name, content type, request/response schema, call or cast | Phase 1 (docs), validation later |
| `dependencies` | other apps required (name + version range), start order; like OTP `applications` and WildFly module deps | Phase 1 |
| `registered_names` | names the app may register, local or global; registration of other names is refused | Phase 1 local, Phase 2 global |
| `lifecycle` | restart (permanent, transient, temporary), shutdown timeout, init timeout, start args | Phase 1 |
| `supervision` | children (for supervisor apps): strategy, intensity, period | Phase 1 |
| `instances` | count or min/max, pool routing (round-robin, hash key) | Phase 1 count; scaling later |
| `limits` | memory_max, table_max, callback_deadline_ms, fuel_per_message, mailbox_max, message_max_bytes, timers_max, monitors_max, links_max, pending_calls_max, storage quota, open files, outbound connections | Phase 1 memory/deadline/mailbox; host objects in Phase 2a (H1); rest later |
| `capabilities` | requested host interfaces with scopes (table below) | Phase 1 subset |
| `messaging_acl` | which apps or names may call/cast this app, and which it may call | Phase 2 |
| `config` | schema for config keys, defaults, secret references (never secret values) | Phase 1 |
| `upgrade` | snapshot format version, supported from-versions, upgrade mode (snapshot or restart) | Phase 1 |
| `placement` | node labels required or preferred, spread label, mode (`any`, `singleton`, `all-nodes`, `hot-standby`) | Phase 2: `singleton` and `hot-standby` on a node with `[failover]`; the rest Phase 3 |
| `health` | optional `health` export, interval, failure threshold | Phase 2 |
| `observability` | log level default, metrics enabled, trace sampling | Phase 1 level only |
| `provenance` | inherited from `.rbp`: build info, git commit, SBOM, signature, signer key | Phase 1 verify signature |

**Capabilities.** A component can call only what it imports, so its import list is the upper bound of its permissions. The manifest narrows each import with scopes, and the operator's deployment policy narrows further. Effective permission = imports ∩ manifest request ∩ operator grant. An import not granted is linked to a stub that returns `access-denied`, so an app can degrade instead of failing to load.

| Capability | Interface | Scope fields |
| --- | --- | --- |
| Messaging | `paddock:runtime/process` | always granted; `messaging_acl` narrows it |
| Spawn and supervise | `paddock:runtime/supervisor` | max children, allowed child apps |
| Logging | `wasi:logging` | max level, rate limit |
| Config | `wasi:config` | key prefixes |
| Secrets | `paddock:secrets` | named secret refs |
| Clocks, random | `wasi:clocks`, `wasi:random` | always granted |
| WASI plumbing | `wasi:io`, `wasi:cli` | always granted; Rust's standard library imports them. Stdout and stderr become tagged logs, and the environment holds only what the host sets |
| Term storage | `wasi:keyvalue` | buckets, durability (memory, disk, replicated), quota bytes |
| Files | `wasi:filesystem` | preopens: guest path → host volume, read-only or read-write, quota |
| HTTP out | `wasi:http/outgoing-handler` | allowed hosts and ports, methods, max concurrent |
| HTTP in | `wasi:http/incoming-handler` | routes (host, path prefix), max body size |
| Sockets | `wasi:sockets` | allowed CIDRs and ports, listen yes/no |
| SQL pool | `paddock:sql` | named datasource, max connections |
| Metrics | `paddock:metrics` | metric name prefix |
| Admin | `paddock:admin` | read-only or control; for operator tooling apps only |

A capability key covers a whole WIT package (`wasi:keyvalue` covers `store`, `atomics` and `batch`) except where noted by interface: `wasi:http/types` is covered by either HTTP key, and `paddock:runtime/supervisor` is the only `paddock:runtime` interface that needs declaring. An import that no capability covers and that is not always granted fails the deploy. In Phase 1, operator policy grants or withholds whole capabilities; the scope the manifest requests is used as written.

## Isolation and resource limits

Every instance gets its own wasmtime `Store`, linear memory and limits, and sees no host resource it was not granted. A violation traps the instance, and the host treats a trap as a crash.

| Resource | Mechanism | Notes |
| --- | --- | --- |
| Memory | `ResourceLimiter` / `StoreLimits` per Store: memory_max, table_max | Growth past the cap fails or traps. Use the pooling allocator for fast instantiation. |
| CPU, wall clock | Epoch interruption: a deadline per callback (callback_deadline_ms), 2 ms tick | About 2% on S8's Rust kernels; up to about 20% on short store-bound loops (BASELINE.md). The tick also bounds how long a busy instance holds a worker. A blown deadline kills the instance with reason `timeout`. |
| CPU, deterministic | Fuel per message (optional) | Costs more. Use for untrusted tenants or metering. |
| Threads | Host-owned tokio pool; instances are single-threaded | Wasm threads support in WASI is unsettled, so don't depend on it. Scale with instance pools. |
| Mailbox | mailbox_max per instance; message_max_bytes | When full: cast is dropped and counted, call fails fast with `overloaded`. |
| Filesystem | WASI preopens per manifest grant; read-only or read-write | cap-std stops path escapes. Quotas need a custom `wasi:filesystem` backend or OS quotas per volume. |
| Network | `wasi:sockets` address check hook; `wasi:http` through a filtering proxy | Default deny. Prefer HTTP egress over raw sockets. |
| Storage | `wasi:keyvalue` buckets with byte quotas | Host-owned, so data survives instance crashes. |
| Host calls | Every import runs a policy check and a rate limit | Each host function is attack surface; keep the set small. |
| Host objects | A quota per instance on timers, monitors, links, registered names, pending calls, group memberships and watches | These live in host memory, outside `memory_max` (a timer holds its envelope), so the instance that creates them is charged for them, as seL4 charges a caller for kernel objects. The `process` functions that create them return no error, so over quota the instance crashes with `timer_limit`, `monitor_limit`, `link_limit` or `pending_call_limit`; later interfaces (groups, watches) return an error instead. Node config sets each limit's default and ceiling. Registered names need no quota: an instance holds only names its manifest lists. |

**Address space.** Wasm32 memories reserve large guard regions (several GB of virtual space each) to avoid bounds checks. At thousands of instances per node this is fine. Configure the pooling allocator's guard size and the max instance count together, and alert near the limit.

**Native plugins.** A rustbridge `.so` can't be sandboxed or unloaded safely in-process. Run it as a separate OS process (`native-process` kind) and talk to it over a pipe with the same envelope protocol, as Erlang does with ports. Limits then come from the OS: cgroups for memory and CPU, seccomp or Landlock for syscalls and files, and kill for timeouts.

## Failure handling

The host detects every failure, converts it to an exit reason, and propagates it through links and monitors exactly as OTP does. Supervisors restart according to policy. State in instance memory is lost on crash; durable state belongs in term storage.

**Detection**

| Failure | Detected by | Exit reason |
| --- | --- | --- |
| Trap (panic, unreachable, bad memory access) | wasmtime returns a trap | `crashed(msg)` |
| Memory cap exceeded | ResourceLimiter | `crashed("memory_limit")` |
| Callback runs too long (loop, deadlock in guest) | Epoch deadline | `timeout` |
| `init` fails or times out | Return value or init timeout | `crashed(...)`; supervisor counts it as a restart |
| Health check fails N times (optional `health` export) | Host timer | `crashed("unhealthy")` |
| Mailbox stays full past threshold | Host metric | Alarm only by default; kill is a policy option |
| Native-process plugin exits or stops answering pings | Pipe EOF, ping timeout | `crashed(...)` or `timeout` |
| Remote node lost | QUIC idle timeout or connection close | `noconnection` for every remote link and monitor through that node |

This is stronger than BEAM in one way: BEAM cannot stop a NIF stuck in a loop, while epoch interruption always gets control back.

**Links and monitors**

- Links are bidirectional. An abnormal exit kills linked instances unless they trap exits; a trapping instance receives `info::exit`.
- Monitors are one-way and always deliver `info::down`.
- The host owns the link and monitor tables, so they survive app code bugs. Pids carry an incarnation, so a restarted instance never inherits old links.
- Links to remote pids are stored on both nodes and cleared with `noconnection` when the connection drops.
- Only `normal` is a normal exit for links: an instance stopped with `shutdown` takes non-trapping linked instances down with it, as in OTP. Linking to, or monitoring, a dead pid delivers `noproc` at once.
- `process.exit(pid, reason)` follows OTP's exit-signal rules. `killed` is untrappable (the role OTP's `kill` plays). A trapping target receives `info::exit`. A non-trapping target ignores `normal` and stops with any other reason, without `terminate`. An exit signal to oneself stops a non-trapping instance even with `normal`.
- Exit and down notifications travel on their own unbounded queue, ahead of the mailbox, so a full mailbox never drops them. Trapping starts when `trap-exits` is called, usually in `init`; a signal that arrives before then stops the instance.

**Supervisors**

- Supervisors are host-native, not Wasm. They are trusted, cheap, and can't crash from guest bugs.
- Strategies: `one_for_one`, `one_for_all`, `rest_for_one`. A dynamic `simple_one_for_one` covers instance pools.
- Restart types per child: `permanent`, `transient`, `temporary`.
- Intensity and period, for example 5 restarts in 10 s. Past it, the supervisor gives up and escalates to its parent. At the top, the app is marked `failed` and an alert fires.
- Restart backoff with jitter is an addition beyond OTP, to avoid hot crash loops against a failing dependency.
- Each app gets a root supervisor. Apps that declare the `supervisor` capability can start their own children within manifest limits.
- Unlike an OTP permanent application, a Paddock app whose root supervisor gives up does not stop the node: the app is marked `failed`, its dependents are not started, and other apps keep running. A node-level watchdog (OTP's `heart`) is a separate concern (review F7).
- The root supervisor runs `instances.count` instances of the app, then any `supervision.children` (instances of other deployed apps, with optional restart, shutdown and start-argument overrides). Under `simple_one_for_one`, `supervision.children[0]` is the pool template and `instances.count` copies start from it.
- A restarted instance keeps its id with the next incarnation. Siblings stopped by a group restart stop gracefully in reverse start order and start again in start order.
- Escalation exits with `crashed`, naming the supervisor, the intensity and the last child exit, so `paddock status` shows why an app failed.
- Apps start in dependency order and stop in reverse. An app counts as started once every instance is `Active`. An app that fails to start is marked `failed`, and apps depending on it are not started; unrelated apps still start.

**Instance pools.** An app's running instances form a pool; `whereis` on the app's name, when no instance has registered that name, returns a member round-robin. With `routing = "hash:<key>"`, host-side routing reads the top-level field `<key>` from a JSON or CBOR map body. Strings, integers and byte strings hash to the same member in either encoding. Other bodies, or a missing field, fall back to round-robin. Positions in the pool survive restarts, so a key keeps its member while the pool size is unchanged. Guests address pids, so guest-side hash routing needs a WIT addition; Phase 1 offers round-robin to guests.

**Calls during failure.** A `call` to a dead or unknown pid returns `noproc` immediately. If the target dies mid-call, the caller gets its exit reason. Otherwise the caller gets `timeout`. A late reply after a timeout is dropped, so callers never see stale replies.

**Messaging details.**
- A `call` to the caller's own pid traps the caller, as `gen_server:call(self())` exits in OTP: the instance cannot read its mailbox until the callback returns.
- A `cast` that cannot be delivered (dead pid, full mailbox, too large) is dropped without telling the sender. Casts and timer messages dropped for a full mailbox are counted per instance.
- `message_max_bytes` is checked where a message enters the target's mailbox, so it applies to calls and casts from guests and from the host alike. Replies and timer messages are not checked.
- `process.reply` works during `handle-call` as well as after it returns `noreply`; a second reply to the same call is dropped.
- Timers belong to the instance that started them and are cancelled when it exits. `cancel-timer` on a timer that already fired does not remove its message from the mailbox.
- An instance may register several of the names its manifest lists. Its names are released before its exit is published, so the next incarnation can register them again in `init`.

## Hot upgrade and state migration

An upgrade swaps an instance's component between messages, carrying state through `save-snapshot` and `load-snapshot`. If anything fails, the instance reverts to the old version. This is OTP's `code_change/3` made explicit, using Golem's snapshot-and-revert protocol.

**Per-instance protocol**

1. The controller pre-compiles v2 and validates it: signature, WIT world compatible, requested capabilities within the current grant (or an operator approves new ones).
2. The host pauses delivery to the instance after its current callback; new messages queue in the mailbox.
3. The host calls `save-snapshot` on v1 under a deadline.
4. The host instantiates v2 and calls `load-snapshot("1.4.0", bytes)` under a deadline.
5. On success, delivery resumes against v2. The pid stays the same, so links, monitors and registered names are untouched.
6. On any failure, the host drops v2, resumes v1, and reports the error. No messages are lost.

**Rollout across instances and nodes**

- Canary: upgrade one instance, watch crash rate and health for a set window, then continue.
- Batches: configurable batch size; stop and roll back automatically if restarts exceed a threshold.
- Mixed versions are normal during rollout. Message schemas must stay compatible for one version step: add fields, don't change meanings. The envelope's schema id makes violations visible.
- Order follows `dependencies`: providers upgrade before consumers, the reverse of the order they stop in.

**Upgrade modes per app** (manifest `upgrade.mode`)

| Mode | Behaviour | Use when |
| --- | --- | --- |
| `snapshot` | Protocol above | Stateful apps |
| `restart` | Terminate v1 gracefully, start v2 from `init` | Stateless apps, or state lives in term storage |
| `blue-green` | Start v2 instances beside v1, move the registered name, drain v1 | Apps serving HTTP or long calls |

**Phase 1 details.**
- An instance upgrade runs `save-snapshot` on the old Store, then instantiates the new component in a fresh Store and calls `load-snapshot`; `init` does not run.
- What belongs to the instance rather than the Store moves across: callers awaiting deferred replies and pending timers. The pid, links, monitors and registered names never change.
- Guest-held resources (open buckets) do not move; the new version reopens them. Deferred call references survive only if the app puts them in its snapshot.
- The new version's limits and capabilities apply from the swap. Mailbox capacity stays as created.
- A trap in `save-snapshot` poisons the old Store, so that instance exits and its supervisor restarts it as the old version: the supervisor's child spec changes only after an instance has moved.
- Validation: same app, a different version, the same WIT world, the old version inside the new manifest's `upgrade.from_versions`, and new capabilities only with approval. The node verifies the new bundle's signature when it opens it, as for any deploy.
- An app rolls out in batches. A failed instance, or more supervisor restarts than allowed during the rollout (counted after each batch's watch window), stops the rollout. Every instance already moved goes back by snapshot, or, if the old version cannot read the new snapshot, by restart from `init`.
- Rollback returns to the previous version by snapshot when that version's manifest asks for it and accepts the running version, otherwise by restart. If the previous version's `load-snapshot` refuses the newer snapshot, the rollback is refused and the running version stays. `rollback --restart` (and `upgrade --restart`) moves the instances by restart whatever the manifest's mode, discarding in-memory state; losing state is always the operator's explicit choice, and the audit log records it.

**SDK support.** `paddock-sdk` (Rust) wraps the app world:
- An app implements the `App` trait. Only `init` and `handle_call` are required; `handle_cast`, `handle_info` and `terminate` default to doing nothing, and the snapshot pair defaults to refusing snapshot upgrades, so `restart` mode works with no extra code. `paddock_sdk::app!(Type)` exports it.
- `Ctx` exposes every `process` function. `Envelope` encodes and decodes CBOR, JSON and text by content type.
- `#[derive(Snapshot)]` with `#[snapshot(version = N, migrate = path)]` writes state as CBOR `[format_version, state]`. Loading an older format hands the raw CBOR value to `migrate`. The snapshot's own format version is independent of the app version, which `load_snapshot` also receives.
- The bindings live inside the SDK, so app crates carry no WIT. A component still imports only what its code calls: an app that never logs has no `wasi:logging` import.

**Native-process plugins** upgrade with `restart` or `blue-green` only, because their memory can't be snapshotted by the host. A plugin may implement the snapshot messages itself over its pipe.

## Scaling and where code runs

**An instance runs on one core.** WASI 0.2 has no threads, and WASI 0.3's async gives an instance concurrency, not parallelism. As in Erlang, parallelism comes from many instances: an app runs as a pool (`instances.count`), and `instances.routing = "hash:<key>"` keeps each key's messages on one instance, in order. Sharding by device, account or session gives per-key state and ordering with parallelism across keys. "A guest with four cores" is a pool of four or more instances sharing state through messages and host services.

**Terminate I/O and protocols in the host; decide in guests.** Spikes S14 and S15 measured the split:
- Host-terminated HTTP calling a pooled instance keeps about 75% of native throughput; a guest that owns its socket is capped at one core.
- A host call costs about 0.15 µs and a message about 1.3 to 2.3 µs; guest compute runs at 1.0 to 2.4x native (S8).

So high-rate work with little logic stays native and multi-core, and logic that benefits from isolation, supervision and hot upgrade runs in guests:

| Host (native) | Guests |
| --- | --- |
| Accepting connections, TLS, framing, high-rate parsing | Per-key state machines, rules, alarms, control decisions |
| Protocol stacks (Modbus, DNP3, ICCP), serial ports, as extensions | Calculations, routing between subsystems |
| Bulk paths that need no app logic (device data to a historian) | Anything that must be upgraded live or isolated from other apps |

**Getting data in and out.** The host owns listeners (ROADMAP F2): several accept tasks per listener, the Ranch pattern, kept across instance restarts and upgrades. A connection becomes either an instance of its own (about 77 µs to start, about 56 KiB idle) or framed messages to a hash-routed pool. Protocols can also be parsed inside the sandbox from host-owned streams, which confines a parser bug to one instance: with an instance per connection this matched native throughput (S13), so it is not limited to low-rate protocols. Keep connections per instance few: under WASI 0.2 every connection an instance holds adds to the cost of each wake-up (S15: one instance with 32 connections managed 7.3k requests/s; eight instances with four each, 106k–120k). Devices such as serial lines are never opened by guests: a node binds a named port to a device path, apps declare the name, and an extension owns the device (the same rule as volumes and datasources).

**Throughput comes from batching and cores, not clock rate.** A message costs about the same whatever it carries, so high-rate data moves in batches (many point updates per cast). Scale up adds cores to tokio's worker pool and instances to pools; past 16 cores this is not yet measured, and two-socket machines may need a runtime per socket or several nodes per machine (ROADMAP G1, tabled until such a machine is available). Until then, designs must not be inherently single-core: no global lock or single task on a message or I/O path that every request crosses, listeners with several accept tasks, and pools rather than single instances for high-rate apps.

**Nodes for availability first.** When nodes are added for availability rather than load, failover comes before a general cluster: `placement.mode = "singleton"` or `"hot-standby"`, with apps recovering state from their sources (a SCADA app re-polls its field devices) or from restored storage until replicated storage exists (ROADMAP D10).

**Two nodes cannot break a tie alone.** With a primary and a standby joined only by the network, a partition looks like node loss to both sides, and both promote. Failover does not need consensus (OTP's distributed applications fail over by node-down and a priority list, with no Raft or Paxos); it needs independent evidence that the partner is dead, and one place that orders promotions. Paddock follows HAppliance's protocol (HA pair plus witness, model-checked in TLA+), which replaced OpenRaft there because Raft's symmetric quorum made every write wait on the witness's WAN link while one HA node was down:

- Heartbeats on several independent paths: the cluster link (QUIC) and a serial line between the pair, owned by the node and set in node config. The standby counts the primary as dead only when every path is silent; network lost while serial answers is a partition, and nobody promotes.
- An epoch, raised by every promotion and carried as a fencing token: a node that meets a higher epoch steps down, and frames stamped with a stale epoch are refused.
- A witness on a third site (a disaster-recovery node, or a small machine such as a Raspberry Pi 5 that runs no apps), which grants each epoch increase once. A standby promotes only when every heartbeat path is silent and the witness grants the new epoch.
- A primary keeps running `singleton` apps only while it holds a lease, renewed through its partner or the witness. A primary cut off from both stops its singletons when the lease runs out, and the witness grants the standby's promotion only after that lease has expired. This covers the case HAppliance's model leaves to fencing on reconnect (network and serial cut while both nodes live): with only fencing, the old primary would keep driving devices until it reconnects. The lease assumes bounded clock drift between nodes.

Spike S16 model-checked this in TLA+, starting from HAppliance's specifications: with the lease, no reachable state runs a singleton on two nodes, including with network and serial cut while both live; without it, that case gives two runners. Its README lists the rules the implementation must follow, among them that the standby's serial heartbeat carries its role, epoch and whether it is promoting, so a primary cut off from the network renews its lease over serial. `paddock_host::failover` implements it, tested on the in-memory mesh against D10's cases and random fault schedules; a primary that dies hands its singletons over in about node timeout + lease + 2 margins (9 to 10 s with the defaults: 5 s, 3 s, 500 ms). Each layer protects against a different failure: the second heartbeat path against one cut link, the witness against both links cut at once, the lease against a primary that loses everything but keeps running. One failure of any part leaves at most one primary. Losing the witness only stops automatic promotion, which an operator can confirm instead. A pair without a serial line or a witness can run with operator-confirmed promotion only. The failover rules start from HAppliance's TLA+ specifications (ROADMAP D10), extended with the lease and with apps started and stopped on role changes.

On a node, `[failover]` names the partner and the arbiter, and each app is deployed to both nodes through their own admin APIs. The node holds every `singleton` app and runs it only while its leadership says `may_run`: on promotion it starts from `init` (no state crosses nodes; apps re-read their sources or `disk` buckets), and when `may_run` drops it is killed at once, without `terminate`, since the lease may be about to end. `hot-standby` apps run on both nodes and are told the node's role as `host-event("failover-role", …)` after `init` and on every change. Two things are kept on disk, written and synced before they are acted on: a node's highest epoch, so its fencing token never goes back across a restart (the model's `PersistEpoch`), and the witness's epoch and promoted node, written before a grant is answered (a write that fails withholds the grant). The model also keeps the witness's record of renewals across its crashes (R10 in `docs/tla/VERIFICATION.md`). Renewals come every heartbeat, too often to sync each one, so a restarted witness counts the node it promoted as renewed at the moment it restarts instead: no real renewal can be later, so a grant waits at least as long as with the kept record, at the cost of one more lease before a promotion after a witness restart. An app that is not a singleton may not depend on one, since on the standby that dependency never runs.

**Scaling down.** On a small machine (a Raspberry Pi 5: 4 cores, 8 GB) the same node runs fewer, smaller apps: the dense memory profile, a 4-worker runtime, and compiled components cached in the deploy store so restarts do not recompile (ROADMAP G2 to G4). wasmtime and anyprobe support aarch64 Linux; Paddock has no numbers there yet.

## Distribution

Nodes form a full mesh over QUIC (quinn + rustls) with mutual TLS. Delivery is at most once and ordered per sender/receiver pair, the same promise Erlang makes. Anything stronger is opt-in.

**Transport: QUIC rather than SCTP**

| Need | QUIC | SCTP |
| --- | --- | --- |
| Independent streams, no head-of-line blocking | Yes | Yes |
| Authentication and encryption | TLS 1.3 built in; mTLS gives node identity | Needs DTLS, awkward |
| OS and network support | Userspace, runs anywhere, passes firewalls | No native macOS or Windows stack; middleboxes drop it |
| Liveness | Keepalive and idle timeout built in | Heartbeats built in |
| Multi-homing | Connection migration | Native multi-homing |

SCTP only wins on controlled telecom networks, so it is not worth a second transport.

**Streams and ordering**

- One control stream per node pair: membership, link and monitor tables, registry updates, heartbeats.
- One stream per sender→receiver pid pair, opened lazily and closed when idle. This keeps per-pair ordering without one large message blocking unrelated traffic.
- Large messages (above a threshold such as 1 MiB) are chunked with a size cap from `message_max_bytes`.

**Discovery and membership**

- Phase 2: a static seed list in node config, like Erlang hostnames. A joining node sends a join to a seed; the seed answers with every member it knows and passes the newcomer on, and each node joins any member it had not met, so every pair is connected (Erlang's `connect_all`). Members carry their node id, creation, address and role (`member`, or `witness`: a node that runs no apps and takes part in failover decisions). Membership and liveness are separate: a member that stops answering stays a member, with its session ended, until it answers or leaves.
- Every start of a node has a creation number, kept in its data directory and raised on each start, as Erlang's `creation`. Frames carry the sender's creation and the receiver's creation as the sender knows it, so a peer that restarted ends its old sessions everywhere and frames from or for an earlier start are discarded. The creation is also the high 24 bits of every instance id (the low 40 are a serial number), so a pid an app kept from before a node restarted never names a later instance; pids print as `node.serial.incarnation@creation`.
- Later: DNS SRV and Kubernetes headless services. mDNS for development.
- Node identity is a pinned public key, as WireGuard does, replacing Erlang's shared cookie (decided 2026-10-09, to avoid running a certificate authority). Each node holds an Ed25519 key (`paddock cluster keygen`), and a peers file on every node lists each node's public key against its id. The QUIC handshake is TLS 1.3 with raw public keys (RFC 7250): each side proves it holds its key and accepts the other only if its key is listed; after the handshake, each side's hello names its node id and the other checks the key belongs to that node. Nothing expires. Rotating a key lists the new key beside the old at every peer, then switches the node, then drops the old; adding a node adds its key to every peers file. Both files are reloaded on change, SIGHUP or `paddock reload-tls`. HAppliance chose a self-signed CA for its switched network to make rotation a single step; with a pair and a witness, three peers files are less to run than a CA, and a CA mode can be added if a site needs one.
- The wire format is CBOR, `[version, packet]`, after a hello each way that agrees on the highest protocol version both speak; one connection per pair carries a control stream (heartbeats, membership, failover) and a data stream (messages, links, monitors), so heartbeats never wait behind a large message. A node leaving cleanly sends `Leave` and finishes both streams; the peer closes the connection once it has read both to the end, and the leaving node waits for that close (up to a second) before closing its endpoint, since closing first would discard frames received but not yet read.
- Node-down detection uses heartbeats between every pair of nodes (every `heartbeat_interval`, 1 s by default; a link silent for `node_timeout`, 5 s by default, ends the session, tunable like `net_ticktime`), with QUIC's idle timeout as a backstop.

**Registry**

| Scope | Mechanism | Consistency | Phase |
| --- | --- | --- | --- |
| Local names | Per-node map | Strong (single node) | 1 |
| Process groups | Each node owns its members and gossips them, like OTP `pg` | Eventual; never blocks | 2 |
| Global unique names | Raft (openraft) on a small set of voter nodes | Strong; unavailable in a minority partition | 3, opt-in |

Apps should prefer process groups. Erlang's `global` shows that global locks during partitions cause most distribution pain, so unique global names stay opt-in.

**Partitions.** When a partition heals, process groups merge naturally. Global names conflict only if Raft is off; with Raft on, the minority side can't register. Paddock does not attempt automatic split-brain resolution for app state; that is the app's concern.

**Delivery seam.** Every process-core operation that names a pid (call, cast, exit signal, link, unlink, monitor, demonitor, exit and down notices) goes through one routing function: a live local pid gets it directly, a dead local pid gets `noproc`, and a pid on another node becomes a frame for that node's transport. Without a transport, or with the node unreachable, the operation gets `noconnection`. Each node keeps its own side of a link or monitor that crosses nodes. Linking or monitoring a remote pid is recorded at once and confirmed by the other node only when it fails: a dead target answers with a `noproc` exit or down. When a node is lost, the transport reports it and every link and monitor with its pids ends with `noconnection`, and calls waiting on it fail. Links lost to a partition stay lost after it heals, as in OTP. An in-memory transport joins runtimes in one process with injected delay, jitter, drops, partitions and stalls, for tests; QUIC implements the same `Transport` trait.

**Sessions.** A node-down is the end of a session between two nodes, and both sides must agree it ended: if one side dropped its half of a link while the other kept it, an exit notice would be lost for good. Erlang gets this from TCP, where a closed connection is seen by both ends. Paddock numbers sessions instead. Every frame carries the sender's session number for the pair, and ending a session raises it. A frame from an older session is discarded, so nothing sent before a node-down (a held cast, a late link) is acted on after it; a frame from a newer session makes the receiver end the old one too. Each heartbeat also says whether the sender hears the receiver, so a one-way failure ends the session on both sides: the side that stops hearing ends it, and the other side learns from the heartbeats that still reach it. A peer is reachable while its link is heard and it hears this node; frames to an unreachable peer are refused with `noconnection`. Extra heartbeat paths (a serial line) report only whether they are heard: they never end a session, and failover reads them (ROADMAP D10). Every beat carries a small payload (failover puts the node's standing and epoch there), timed by when it arrived, not by when the link was last heard; a heartbeat's payload is read even from an ended session, since it says what its sender is now.

**Wire format.** A frame carries source pid, destination pid or name, call ref (if any), envelope content type, schema id, and body. Frames are length-prefixed CBOR. Protocol version is negotiated at connect time so nodes can roll.

## Host services

The server provides OTP's runtime services and JBoss's managed resources as WIT interfaces. Standard WASI interfaces are used where they exist, so apps stay portable; `paddock:*` covers the rest.

| Service | Interface | OTP analogue | JBoss analogue | Phase |
| --- | --- | --- | --- | --- |
| Deploy, start, stop, upgrade, rollback | Admin API (not guest-facing) | `application`, `release_handler` | Deployment scanner, management CLI | 1 |
| Messaging, timers | `paddock:runtime/process` | `gen_server`, `send_after` | JMS (loosely) | 1 |
| Supervision | Host-native; `paddock:runtime/supervisor` for dynamic children | `supervisor` | — | 1 |
| Failure detection | Host-native | Links, `heart` | Health checks | 1 |
| Registry | `process` (local); groups and global later | `register`, `pg`, `global` | JNDI | 1–3 |
| Process groups and publish | `paddock:groups`: join, leave, members, and `publish`, which the host fans out to every member; members leave when they exit. Local first, gossiped across nodes later | `pg`, `gen_event` | JMS topics (loosely) | 2a (local), 2b (cluster) |
| Shared buckets and watches | `wasi:keyvalue` buckets bound in node config and granted to several apps; `paddock:watch` for revisioned change notices and compare-and-swap on revision | ETS (public tables), `mnesia:subscribe` | Infinispan listeners | 2a |
| Logging | `wasi:logging`; host adds app, version, pid, node, trace id | `logger` | Logging subsystem | 1 |
| Config | `wasi:config` | `application:get_env` | System properties | 1 |
| Secrets | `paddock:secrets`: names granted by policy, bound in node config to files or environment variables (Vault later) | — | Elytron credential store | 2 |
| Clocks, random | `wasi:clocks`, `wasi:random` | `erlang:monotonic_time` | — | 1 |
| In-memory term storage | `wasi:keyvalue` bucket, durability `memory`; host-owned so it outlives instance crashes | ETS (with `heir`) | Infinispan local cache | 1 |
| Persistent term storage | `wasi:keyvalue`, durability `disk` (redb) | DETS | — | 1 |
| Replicated storage | `wasi:keyvalue`, durability `replicated` (protocol chosen in Phase 3); every node reads its local copy and delivers watches from it | Mnesia, Khepri | Infinispan distributed | 3 |
| Files | `wasi:filesystem`: read-only `/priv` from the bundle, writable `/data` per app | `code:priv_dir` | — | 1 |
| HTTP ingress | Node-level ingress delivering each request on an app's routes (`wasi:http/incoming-handler` scope) as a call; host terminates HTTP (TLS next) and routes by host/path (see "HTTP ingress") | Cowboy, inets | Undertow web container | 2 |
| HTTP egress | `wasi:http/outgoing-handler` via filtering proxy | `httpc` | — | 2 |
| Sockets | `wasi:sockets` with allow-list | `gen_tcp`, `gen_udp` | — | 3 |
| Database pools | `paddock:sql` against named datasources | — | JDBC datasources | 3 |
| Metrics and tracing | `paddock:metrics` (per-app counters, gauges, histograms) and host counters, exported as Prometheus text (`[metrics]`, `/v1/metrics`); OTLP push later | `telemetry` | Metrics subsystem | 2 |
| Introspection | Admin API: apps, pids, mailbox depth, memory, restarts; message trace through USDT probes (OBSERVABILITY.md) | `observer`, `recon`, `dbg` | Management console | 1 (probes, `ps`); 2 (cluster-wide) |
| Placement and failover | Cluster controller | `dist_ac`, takeover | HA singleton | 3 |
| Distributed transactions | Not provided | — | JTA | Never |

**Node and admin API (Phase 1).**
- The node (`paddock node`) keeps a deploy store under its data directory: each app's signed bundles by version (the current one and one previous, for rollback), extracted `priv/` files, a desired-state file, the keyvalue database, and an append-only JSON-lines audit log.
- On restart the node verifies every bundle again against its current trusted keys and policy, then starts the apps that were running, in deploy order. An app that no longer verifies is reported as failed.
- The admin API is JSON over HTTP/1.1 on a Unix socket created with mode 0600. Access to the socket is the authorization; there is no other authentication in Phase 1. Routes are under `/v1`.
- Besides the planned operations, the API has `call`, which sends a message to an app through its pool's routing, for operators and integration tests. Windows has no Unix sockets, so its admin API (a named pipe) is deferred.
- A deploy runs the deployment checks in order: signature against trusted keys, checksums (component and `priv/` files), world, `appserver` section, imports against declared capabilities, then the effective grant under operator policy. The resulting grants, denials, deny-stubbed imports and unenforced fields are recorded in the audit log and shown by `paddock status`.

**Term storage notes.** Keys are strings and values are bytes, matching `wasi:keyvalue`. Buckets are per app and namespaced by the host, so apps can't read each other's data. Sharing data between apps goes through messages or a shared bucket granted to each ("Shared buckets and watches" below). Atomic increment and compare-and-swap come from `wasi:keyvalue/atomics`.

**Logging notes.** Apps never write to stdout as a log channel. The host captures stdout and stderr anyway and tags them, so legacy code still produces useful output. Rate limits per app stop one noisy app from flooding the node. The logging service announces each instance's effective level as `host-event("log-level", "<level>")` after `init`, after a snapshot upgrade and on every operator change; `paddock-sdk` caches it and skips records below it before formatting. The host still filters every record, so a guest that ignores the event is capped the same way.

**Denial by capability.** A declared capability the operator does not grant is still linked, and fails softly:
- `wasi:logging` drops records.
- `wasi:config` returns `io("access-denied")`; that interface has no denial case.
- `wasi:keyvalue` returns `access-denied`, as does opening a bucket the manifest does not list.
- `wasi:filesystem` sees no preopens.
- An imported interface no host implements (`wasi:http` and `paddock:*` services before their phase, or anything unknown) gets deny stubs: functions return their error type's `access-denied` case (or `permission-denied`, `access`, `HTTP-request-denied`), or trap when they have no error channel.

**Host service pattern.** Each guest-facing service other than `process` is a module under `paddock-host`'s `services`, with the same shape:
- A capability and the WIT interfaces it links. Imports of those interfaces are never deny-stubbed.
- A `Spec`: what the manifest asks of the service for one app, in `InstanceSpec::services`.
- `Shared` state, one per runtime (pools, limits, operator overrides), and `Instance` state, one per instance.
- Its own `bindgen!` world, linked with the per-interface `add_to_linker` and a getter that hands each host function a `ServiceCtx`: the service's own instance state plus the facilities every service shares (app, version and pid tags; the callback clock; the resource table). A service never sees the rest of the instance.

Services from outside `paddock-host` are extensions: an object-safe `Extension` trait serving one WIT package outside the reserved `wasi` and `paddock` namespaces, registered with `Runtime::with_extensions` (or `paddock::node::run`). Apps declare the package under `capabilities.extensions`, with a scope table the extension reads; operators grant it by package name. An extension builds its bindings with `paddock_host::wasmtime` (`wasmtime_crate` in `bindgen!`), so it cannot link against a different wasmtime, and reaches its per-instance state through the same `ServiceCtx`, found by type. Extensions are native code in the node process: they are trusted like the node itself, and only operators choose which a node runs.

An extension that works on its own schedule, such as a protocol stack polling devices or serving a port, does it from `Extension::start`, called once the node's apps are up. It gets its own node configuration (`[extensions."<package>"]`, the place for site bindings such as device addresses, as volumes and datasources are bound), a way to cast to an app by name through the app's pool routing, and a stop signal for shutdown. Messages it sends carry no sender pid; the receiving app is named by the operator, not by the extension's code.

**Modbus TCP (ROADMAP F6).** `crates/paddock-modbus` is the first protocol extension and follows "terminate I/O in the host, decide in guests": one task per device owns its connection, polls the configured ranges, and casts one CBOR update per device and scan carrying only the points that changed, so a pool routed by `hash:device` keeps each device on one instance in order and pays one message per scan. Every point is sent again after a connect, after an update that could not be delivered (the app not running, a full mailbox) and, optionally, on an integrity interval, so a receiver that starts late or drops one catches up; a device that stops answering produces one `offline` update per outage. Guests read and write the devices their manifest lists through the same connection, between polls, and can set the registers the node serves to Modbus masters, whose writes are cast to an app. Modbus RTU waits for the serial extension (F5); DNP3 and ICCP are parked on licence review.

Linking happens once per engine, so a host call is a direct call on the service's state, with no dynamic dispatch. A registry in `services/mod.rs` lists the services; adding one is a module plus a line in each registry, with no change to the process core or orchestration. Every service follows these rules:
- An ungranted capability fails softly, as an `access-denied` error value where the interface has one, never a trap.
- Time spent waiting in host I/O is host time and does not count against the callback deadline.
- Named resources (buckets, datasources) are declared in the manifest and bound in node configuration.
- Usage is limited per app (rate limits, quotas, connection caps), and each operation is a probe point once B1 exists.

**Host service details (Phase 1).**
- Log records and captured stdout (info) and stderr (warn) go to the `paddock::guest` target, each tagged with app, version and pid. Records below the app's level are dropped. A token bucket per app, shared by its instances, enforces `rate_limit_per_sec` and reports how many records it dropped.
- Keyvalue counters (`increment`) are decimal strings, as in wasmtime's host. Each batch call is one transaction and passes or fails its quota check as a whole. `list-keys` pages 256 keys at a time.
- Disk buckets share one redb file under the node's data directory. Writes run on tokio's blocking pool (they fsync); reads run on the calling worker, usually from redb's page cache, because the hop to the blocking pool costs about 20 µs (S14). Either way the time counts as host time, not against the callback deadline.

### Shared buckets and watches

A shared bucket is the bulletin board between apps: the role of a public ETS table on one node, and of a Mnesia table read from a local copy in a cluster. Reading it is a host call (2.4 µs for a 16-byte read from a `disk` bucket on the 16-core development machine, 2026-10-07, ROADMAP A6), not a message to another instance, and a write is seen by every reader at once.

- Shared buckets are named and bound in node configuration (`[buckets.<name>]`), like volumes and datasources. An app's manifest lists the shared buckets it uses (`wasi:keyvalue`'s `shared`), each read-only or read-write, and operator policy grants each one (`buckets`). Writes check the bucket's quota as for app buckets; the quota covers every app writing it. Built in ROADMAP H3.
- Each bucket has a `scope`: `node` (one node's copy) now, `cluster` (replicated) later. Apps written against a `node` bucket keep working when the operator moves it to `cluster`; only the consistency and failure rules below change.
- Every write gives the key a new revision, a number that increases per bucket. Compare-and-swap takes an expected revision (as etcd's transactions do), so a writer can update a value without a lock and without comparing the value's bytes.
- A watch subscribes an instance to a key or a key prefix. The host delivers `host-event("kv-changed", …)` with the bucket, key and revision, not the value; the subscriber reads the current value. Notices are conflated per key: while a notice for a key is waiting in the subscriber's mailbox, later writes to that key update its revision instead of adding notices. A slow subscriber misses intermediate values but always sees the latest, and watch notices never count as dropped casts.
- A subscriber that restarts can resume from the last revision it handled; if the bucket no longer keeps changes that old, the host says so and the subscriber rereads the keys it watches. Watches end when the instance exits, like monitors.
- `paddock:watch` is a Paddock interface next to `wasi:keyvalue` (which has no watch), with an `access-denied` error, and counts against the instance's host-object quota. It is covered by the `wasi:keyvalue` capability and reaches the same buckets on the same terms (ROADMAP H4).
- Notices travel on the instance's signal queue, not its mailbox, so a full mailbox never drops one; conflation bounds the queue by the keys a watch matches. A notice reads its key's latest revision when the instance takes it.
- Resuming needs no change log: each key keeps its latest revision and each bucket the revision of its latest delete. A watch resuming after revision R gets a notice for every matching key with a later revision, unless a delete happened after R (or R is ahead of the bucket), in which case it gets `too-old` with the current revision and rereads. A delete anywhere in the bucket counts, even outside a prefix watch's keys.
- Ordered range scans (ETS `ordered_set`, redb's order) and key expiry are later additions to the same interface.

This is the platform's job rather than an app's: an app holding the board would serialize every read through one instance on one core, lose the board when it crashes, and turn every subscriber into a message across the network.

For a `cluster` bucket (Phase 3), the replication protocol is chosen then: Raft (openraft), or synchronous replication between an HA pair with a witness as HAppliance does, which keeps writes on the LAN while one HA node is down. The guest interface above fits either. Writes from any node are forwarded to the leader and commit once replicated: a network round trip plus an fsync, so a replicated write costs more than a cast, and the board suits state (point tables, configuration, leadership) rather than streams. Every node applies the log to a full local copy. Reads come from that copy by default (they may trail the leader slightly), and a read can ask to be linearizable, which goes through the leader. Watches are delivered by each node from its own copy as it applies changes, so a notice never crosses the network per subscriber. In a minority partition, reads keep working and writes fail with an `unavailable` error, so apps can degrade. Two later options: keys owned by a pid (etcd leases, ZooKeeper ephemeral nodes), removed when the owner exits or its node is lost, for presence and election; and a `mirrored` durability, replicated asynchronously from the leader like Redis, for high-rate data whose last few updates may be lost on failover because the app re-reads its sources (a SCADA app re-polls its devices).

A bridge to an external Redis or etcd is an extension (A5) for sites that run one already, not the platform's board.

**Secrets notes.** A secret is named in the manifest, granted by name in operator policy (as shared buckets are), and bound in node configuration to a source: a file (as Kubernetes and systemd credentials mount them) or an environment variable of the node. The host reads the source on every call, so rotating a file needs no restart, and never logs a value; errors name the secret, not the path. Once read, the value is in the guest's memory, so an app should read it when needed rather than keep it in state, which ends up in snapshots, and should not log it. A secret store such as Vault is a later source kind or an extension.

### HTTP ingress

Decided 2026-10-09 (ROADMAP E1), from S15's measurements: the host terminates HTTP and delivers each request to an app as a call, the shape that kept about 75% of native throughput; `wasi:http/incoming-handler` with an instance per request kept about 15%.

- **Ingress is node-level.** Node config names addresses (`[http.<name>]`), each with several accept tasks; every address serves every app's routes, as a web container or Cowboy does. Apps claim routes in their manifest (`wasi:http/incoming-handler`'s `routes`: an optional host and a path prefix). The longest prefix wins, matched on path segments, and a route naming the host beats one that does not. A deploy that claims another app's exact host and prefix is refused.
- **A request is a call.** The request is a CBOR map (`method`, `path`, `query`, `headers`, `body`, and `key`) in an `application/cbor` envelope with schema `paddock/http-request`; the app's pool picks the instance by its routing, and a route's `hash_header` copies a header (a tenant or customer id) into `key` for `hash:key` pools. The app answers from `handle-call`, at once or later with `process.reply`, with a `paddock/http-response` envelope (`status`, `headers`, `body`); any other reply is sent as `200` with its content type. An app serves HTTP and ordinary calls from the same instances, with its state, supervision and upgrades unchanged, and the frozen app world gains nothing.
- **Limits and failures are the call's.** The callback deadline bounds guest execution; the route's `timeout_ms` bounds the wait for the reply (504). A full mailbox is 503 with `Retry-After`, an app not running 503, an instance exiting before it replies 502, a body over `max_body` 413 (checked from `content-length` before reading, and while reading). Bodies are buffered whole: no streaming.
- **Slow and abusive clients.** Each ingress address caps its open connections (`max_connections`, more are closed on accept and counted), the time a client may take to send its headers (`header_timeout_ms`, also between requests on a kept-alive connection) and its body (`body_timeout_ms`, then 408), and the size of an app's reply (`max_response`, then 502, checked before the reply is parsed). A route's `hash_header` lets a client choose the instance its requests reach, so one client can concentrate its load on one instance; that is the nature of hash routing, and `max_connections` and the instance's mailbox bound it.
- **Granting without an import.** An app that serves HTTP imports nothing, so its grant cannot be read from its imports. Its routes go live only if operator policy grants `wasi:http/incoming-handler`; otherwise the deploy reports it denied and the routes stay unrouted.
- **Where the parser runs, and what a parser bug reaches.** With requests as calls, HTTP is parsed by hyper in the node process, and so are the routing and the envelopes around it. The app's own logic runs in the sandbox either way, so a bug in how an app handles a request reaches only that app's grants in every design. The parser is what differs: a memory-safety bug in it, or in the ingress code, is a node compromise (every app's data, the node's keys), where a parser inside the sandbox would give an attacker one instance and leave a WebAssembly escape still to find. Paddock accepts this for the default path, as it does for the other host code exposed to untrusted input (wasmtime, the WASI host, redb, `tokio-modbus` in F6): hyper is Rust, widely deployed and fuzzed, and its published advisories have been denial of service and request smuggling rather than code execution. `wasi:http` (E6) does not change it either: wasmtime's `wasi:http` host also parses with hyper in the node and hands the guest a parsed request. For an endpoint where that residual risk matters (exposed to the internet, guarding sensitive data), parse in the sandbox instead: a connection handler (F3) on a listener, an instance per connection, reading the socket through `wasi:io` streams with an HTTP parser compiled into the guest. S13 measured that shape at native throughput, at the cost of about 40–65 µs to start each connection's instance, no shared state in the handler (it calls a pool app, one more hop), and TLS in the guest if the connection is encrypted (keys in guest memory). Choosing per endpoint is the operator's and author's decision; the node supports both.
- **TLS (ROADMAP E1b)** is terminated in the host with rustls on ring: certificates per ingress address, chosen by SNI (exact host or one wildcard label, the first as default), reloaded when their files change, on SIGHUP or on `paddock reload-tls`, keeping the old set if a file is broken. Private keys stay in the node process, never in a guest; the handshake counts against `header_timeout_ms`. Apps see `scheme` (`http` or `https`) in each request.
- **Later:** a separate `wasi:http` handler world for unmodified `wasi:http` components and streaming bodies, an instance per request as connection handlers are (E6).

## Security model

Security rests on three hard boundaries: the Wasm sandbox around each app, mutually authenticated TLS between nodes (pinned keys), and signatures on every bundle. Policy narrows what crosses those boundaries; it never substitutes for them.

**Trust levels**

| Principal | Trusted for | Not trusted for |
| --- | --- | --- |
| Operator (admin API, signed by operator key) | Grants, deployments, cluster config | — |
| Node (pinned key in every peers file) | Routing messages, holding link and registry state | Inventing messages from pids it doesn't host |
| Bundle signer (minisign key from `.rbp`) | Authorship and integrity of code | Permissions: signing doesn't grant anything |
| App instance | Its own state and granted capabilities | Anything else |
| Extension (native code built into a node by its operator) | Everything the node is | — |

**Deployment checks**

1. Verify the manifest and component signatures against trusted signer keys, plus checksums.
2. Compare the component's actual WIT imports against `capabilities` in the manifest. An import the manifest doesn't declare fails the deploy.
3. Intersect with operator policy (for example, "apps from signer X may use HTTP egress to \*.internal only"). Ungranted imports are linked to `access-denied` stubs.
4. Record the effective grant with the deployment, for audit. An upgrade that asks for more needs explicit approval.

**At runtime**

- Messages are stamped with the sender pid by the host; apps can't forge the source.
- A node accepts frames whose source pid belongs to the sending node only.
- `messaging_acl` is checked on the receiving node.
- Secrets are resolved by the host at the moment of use and never written to the manifest, logs or snapshots.
- Every grant check, denial, deploy and upgrade is logged to an audit stream.

**Out of scope for now:** multi-tenant isolation strong enough for mutually hostile customers on shared nodes. Wasm gives a good base, but side channels (Spectre-class timing) need further mitigation before that claim. The first target is one organization's apps with mixed trust levels.

## rustbridge integration

Paddock reuses rustbridge's bundle format, signing, lifecycle and transport, and extend them. A `.rbp` bundle gains a `wasm` platform and an `appserver` manifest section. The same plugin source can then ship as a native library for Java/C#/Python hosts and as a Wasm app for the server.

**What maps directly**

| rustbridge today | App server use |
| --- | --- |
| `.rbp` ZIP with `manifest.json`, minisign signatures, SHA-256 checksums | Deployment artifact; signature and checksum checks at deploy |
| `build_info`, git provenance, CycloneDX/SPDX SBOM | Provenance shown in the admin API and audit log |
| `api.messages`: `type_tag`, request/response JSON Schema | Message catalog for the app; envelope schema ids point here |
| JSON transport and binary transport | Envelope content types `application/json` and `application/x-rustbridge-binary` |
| Lifecycle Installed → Starting → Active → Stopping → Stopped / Failed | Instance lifecycle, plus Upgrading and Restarting states |
| `on_start`, `handle_request`, `on_stop`, shutdown timeout | `init`, `handle-call`, `terminate`, shutdown timeout |
| Logging callback | Routed to `wasi:logging` |
| Concurrency limits and backpressure | Mailbox limits and `overloaded` |

**Proposed extensions** (bundle version 1.2)

```json
{
  "bundle_version": "1.2",
  "plugin": { "name": "orders", "version": "1.4.0" },
  "platforms": {
    "wasm32-wasip2": { "variants": { "release": {
      "library": "lib/wasm32-wasip2/release/orders.wasm",
      "checksum": "sha256:...",
      "component": { "world": "paddock:runtime/app@0.1.0" }
    } } },
    "linux-x86_64": { "variants": { "release": { "library": "lib/linux-x86_64/release/liborders.so", "checksum": "sha256:..." } } }
  },
  "appserver": {
    "kind": "wasm-component",
    "dependencies": [{ "app": "billing", "version": "^2" }],
    "registered_names": ["orders"],
    "lifecycle": { "restart": "permanent", "shutdown_timeout_ms": 5000 },
    "instances": { "count": 2, "routing": "hash:customer_id" },
    "limits": { "memory_max": "64MiB", "callback_deadline_ms": 2000, "mailbox_max": 10000 },
    "capabilities": {
      "wasi:logging": { "max_level": "info" },
      "wasi:keyvalue": { "buckets": { "orders": { "durability": "disk", "quota": "1GiB" } } },
      "wasi:http/outgoing-handler": { "allow": ["https://payments.internal"] },
      "wasi:filesystem": { "preopens": { "/data": { "volume": "orders-data", "mode": "rw", "quota": "5GiB" } } }
    },
    "messaging_acl": { "accept_from": ["web", "billing"] },
    "upgrade": { "mode": "snapshot", "snapshot_version": 3, "from_versions": [">=1.2 <1.4"] },
    "config": { "schema": "schema/config.json", "secrets": ["payments-api-key"] }
  }
}
```

**Code changes in rustbridge**

- A guest adapter crate: a `Plugin` implementation compiled to `wasm32-wasip2` that exports the `paddock:runtime/app` world. `handle_request(type_tag, payload)` maps to `handle-call`/`handle-cast`; the adapter adds `handle-info` and snapshot defaults.
- An optional `Server` trait beside `Plugin` for apps that want links, monitors, timers and outbound calls through a `ProcessContext`.
- `rustbridge bundle create` accepts a `.wasm` component and an `appserver` section, and validates imports against declared capabilities.
- A native-process runner: a small host binary that loads a `.so` from the bundle and speaks the envelope protocol over stdin/stdout or a Unix socket. The server supervises it like any instance.

**What stays out of rustbridge.** The server, distribution, registry and supervision live in a separate project. rustbridge remains a packaging and plugin-ABI library with no dependency on the server.

## Spike findings

- **S1 (callback model): confirmed.** Async host imports with a synchronous guest ABI let a guest wait in `call` without holding a thread. Epoch interruption with a per-tick `Yield` gives fair preemption (1 ms worst probe latency on one worker thread while a guest spins). `callback_deadline_ms` measures guest execution time, excluding time inside host imports. Killing an instance means dropping its call future and discarding the Store; a dropped Store is poisoned and must never be reused. Host instances on tokio's multi-threaded runtime, not `current_thread` + `LocalSet`. Details: `spikes/s1/README.md`.
- **S3 (WASI target): WASI 0.2.** wasmtime 49 compiles WASI 0.3 in by default but still calls component-model async very incomplete, and Rust has no `wasm32-wasip3` target. `paddock:runtime@0.1.0` stays synchronous on WASI 0.2; a later `@0.2.0` world can add async. Details: `spikes/S3-wasi-target.md`.
- **S4 (capabilities): confirmed, with Paddock's own stubs.** `component_type().imports()` lists every import at deploy time; undeclared `paddock:*` imports are refused. Ungranted capabilities get typed deny stubs built with `func_new` from the function's type. wasmtime's `define_unknown_imports_as_traps` breaks WASI resource types and must not be used. WASI is always linked and narrowed through `WasiCtx` (preopens, socket checks, env). Socket policy needs separate bind and connect rules, because Rust std binds `0.0.0.0:0` before every connect. Details: `spikes/s4/README.md`.
- **S5 (snapshot upgrade): confirmed.** An upgrade is a mailbox message handled between callbacks, so senders keep the same pid and nothing is lost. 4,000 of 4,000 concurrent messages applied exactly once across three upgrades; each swap paused the instance 0.2 to 0.35 ms with pre-compiled components; a failing `load-snapshot` reverted with state intact. Details: `spikes/s5/README.md`.
- **S2 (instance cost): cheap.** About 35 µs to instantiate (pooling) and 30–40 KiB resident per idle instance; 0.57 µs per host→guest call; 1.1 µs per same-worker actor round trip. The limit is virtual address space: about 4–8 GiB reserved per instance by default (about 15,000 instances per node), about 260 MiB with 128 MiB reservations (20,000 tested). Pooling needs `total_stacks` sized to the instance count, because each live Store holds a fiber stack. Details: `spikes/s2/README.md`.
- **S6 (bundles): reusable with upstream changes.** `rustbridge-bundle` 1.0.1 works as a library but rejects bundle version 1.2 and the `wasm32-wasip2` platform, and drops unknown sections such as `appserver`. Its signature check defaults to a key carried inside the bundle; Paddock always verifies against operator-trusted keys. Seven rustbridge changes are listed in `spikes/s6/README.md`.
- **S7 (WASI hosts):** use wasmtime's crates for core WASI, `wasi:config` and `wasi:http`. `wasmtime-wasi-keyvalue` is a demo (writes don't persist past a bucket handle), and there is no `wasi:logging` host, so Paddock writes both. Details: `spikes/S7-wasi-host-crates.md`. (M5 also writes its own `wasi:config` host, two functions, because the crate's host cannot deny access.)
- **S8 (guest versus native speed): 1.0x to 2.4x for compute; about 2 µs per call.** Measured through `paddock-host` with identical source compiled both ways. Plain integer code matches native (software SHA-256 at 1.02x, fannkuch-redux 1.56x). The gaps come from CPU features wasm lacks: native SHA-256 with SHA extensions is about 6.8x faster, and n-body runs at 2.35x scalar and 1.82x with wasm `simd128`. A call into an instance costs 2 µs on the same worker and 4.8 µs across threads, so Paddock suits messages carrying more than tens of microseconds of work. Message bodies are copied in and out through the canonical ABI at about one `memcpy` each (about 28 µs per MiB). Against languageshootout.com's figures, guest compute sits between the JVM and Node and is 5–15x faster than BEAM; the methods differ, so that comparison is rough. Details: `spikes/s8/README.md`.
- **S13 (host-owned listeners, connections as guest streams): works at native throughput.** One listener with several accept tasks hands each connection to a new instance as `wasi:io` streams; this matched native hyper from 4 to 64 connections, a new connection costs about 40–65 µs more (instantiation), a trapping guest loses only its connection, and swapping the component under load lost no requests. A connection handler needs its own world and timeout rules (ROADMAP F3). Details: `spikes/s13/README.md`.
- **S15 (HTTP in the guest versus the host): terminate protocols in the host.** Host hyper calling a pooled instance per request keeps about 75% of native throughput (115k against 152k requests/s, 32 connections); `wasi:http` with a new instance per request about 15%; a guest that owns its socket matches native latency on one connection but is capped at one core. `wasi:http` egress costs what a native new-connection request costs, because wasmtime's sender does not pool connections. Details: `spikes/s15/README.md`.
- **S14 (storage and host calls): host services are cheap to call; I/O and scheduling dominate.** A host import round trip costs about 140 ns; a small keyvalue read through the whole path about 1.2 µs more than native redb, once the service stops sending reads to tokio's blocking pool (today about 20 µs per read; ROADMAP A6). Moving a 64 KiB value through the keyvalue path costs 10–22 µs, mostly allocations and copies around the canonical ABI copy. redb runs inside a guest (1.0–1.5x native in memory), but on files every write is a WASI call (unsynced writes 4–6x native), so durable storage stays a host service. Details: `spikes/s14/README.md`.
- **S16 (failover): a lease keeps one runner.** A TLA+ model of an HA pair and a witness, from HAppliance's specifications: with a lease renewed through the partner or the witness (only at the witness's epoch), and promotions granted only once the old primary's lease is gone, no reachable state runs a singleton on two nodes under crashes, restarts, and network, serial, WAN and witness failures. Without the lease, network and serial cut together while both nodes live gives two runners; with a serial line that fails only with a node, the lease is not needed. Each of the four lease guards is needed. Details: `spikes/s16/README.md`.
- **Interface rule (from S4, S7):** every `paddock:*` capability function returns `result<T, error>` where `error` has an `access-denied` case (the name `wasi:keyvalue` and `wasi:sockets` use), so a denied capability is an error value, not a trap. The generic deny stub also accepts `permission-denied` and `access`.

## WASI 0.2 to 0.3 migration

Apps will move from WASI 0.2 to 0.3 one at a time, with no cluster-wide cutover. Phase 1 targets WASI 0.2 with a synchronous guest ABI (`paddock:runtime@0.1.0`); see Spike findings.

**Why migration is feasible**

- wasmtime-wasi already ships p1, p2 and p3 side by side. A component's imports declare its WASI version, and the host links the matching implementation, so 0.2 and 0.3 apps run on the same node.
- WASI 0.1 is still supported years after 0.2 shipped, so 0.2 components won't break when 0.3 stabilizes.
- `paddock:runtime` is versioned separately from WASI. A future `@0.2.0` world can add async functions while the host keeps serving `@0.1.0`, chosen per app from the manifest's `component.world`.
- Envelopes and snapshots are plain bytes, so 0.2 and 0.3 apps can message each other locally and across nodes.
- Moving an app to 0.3 is an ordinary upgrade: v1 (0.2) runs `save-snapshot`, v2 (0.3) runs `load-snapshot`, and a failure reverts automatically.

**Rules that keep it cheap**

1. **No 0.2-specific types in `paddock:runtime`.** WASI 0.2 models async I/O with `wasi:io` pollables and stream resources; 0.3 replaces them with native `stream<T>` and `future<T>`. The world uses only plain values (strings, records, `list<u8>` envelopes) so it never depends on either model.
2. **Apps use `paddock-sdk`, not raw bindings.** Rust apps go through the `App` trait and `ProcessContext`, so a WASI change is absorbed inside the SDK and most apps migrate by bumping it.
3. **The host selects its linker per world version** from Phase 1 on, even while only one version exists. Adding the 0.3 linker is then additive.
4. **CI covers mixed versions** once 0.3 is usable: a 0.2 app and a 0.3 app calling each other, and a 0.2 to 0.3 snapshot upgrade.

**Limits**

- 0.3's final shape may still change before it stabilizes. That is why Phase 1 waits.
- Async inside guests is a semantic change. If `@0.2.0` allows several in-flight calls per instance, apps that opt in must handle concurrent access to their state. It is opt-in, not part of migration.
- wasmtime will eventually deprecate 0.2, likely years out. The per-app upgrade path is how the fleet gets off it.
- **Revisit trigger:** wasmtime drops the "very incomplete" note on component-model async, or Rust adds a `wasm32-wasip3` target. Check at each phase review.

## Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| WASI churn (0.2 to 0.3 async, proposals still moving) | Guest ABI breaks; wasmCloud went through large rewrites for this reason | Version the `paddock` world; host supports two world versions at once; keep custom interfaces small; see WASI 0.2 to 0.3 migration |
| Distributed semantics (partitions, false node-down, name conflicts) | Hard bugs, lost trust | Erlang-level guarantees only; global names opt-in behind Raft; partition tests (e.g. with toxiproxy) in CI |
| Upgrade developer experience | Teams avoid upgrades or ship broken snapshots | SDK-derived snapshots, automatic revert, canary rollouts, `restart` mode as the easy default |
| Small-team maintenance (Lunatic stalled) | Project dies | Build on wasmtime, quinn, rustls, redb, openraft; minimal custom runtime code; keep scope phased |
| Host API attack surface | Sandbox escape through a host function | Small import set, fuzz host functions, policy check on every call |
| Guest language support beyond Rust | Narrow adoption | Rust first; JS and Python via componentize tools; Go via wit-bindgen-go; test each per release |
| Address space at high instance counts | Instantiation failures | Pool sizing, guard-size tuning, alerts |
| Split brain with two nodes | Primary and standby both promote during a partition and act on the same devices | Independent heartbeat paths (network and serial), witness-granted epochs as fencing tokens, a lease for singletons, or operator-confirmed promotion (ROADMAP D10) |
| Host objects outside the memory cap | A guest creates timers, monitors or watches until the node runs out of memory | A quota per instance on every host object: timers, monitors, links, pending calls (ROADMAP H1), group memberships (H2) and watches (H4) |

## Open questions

- [ ] Default envelope encoding: CBOR, or rustbridge JSON for familiarity?
- [ ] Should supervisors ever be written in Wasm, or always host-native?
- [ ] Is selective receive needed, or are call refs plus deferred reply enough?
- [x] Messaging ACL by app name, by registered name, or by capability token? By app name, through routes declared in the manifest (`messaging_acl.may_call`, `accept_from`), checked against the sender identity the host stamps on every message, which a guest cannot forge. This follows Fuchsia's declared `use` and `expose` routes. Capability tokens (endpoint handles passed in messages, as seL4 does) are parked (ROADMAP D5).
- [x] Ship the server as a library (embedded in another binary) as well as a daemon? Yes, for host services: `paddock::node::run` and `Node::open_with` take extensions (ROADMAP A5).
- [ ] How far to go with native-process plugins before Phase 3?
- [ ] Should a member that stays silent be dropped from membership after a while? Today it stays a member, with its session ended, until it answers or leaves ("Membership"), so a node that crashed and never returns is listed forever and redialed (by the lower node id, or as a seed). Dropping it after a timeout would tidy the list and stop the redials, but a node back from a long outage would then have to rejoin through a seed instead of being redialed, and a too-short timeout would turn a network cut into a membership change. A clean leave is delivered reliably since 2026-10-09, so only crashed or retired nodes linger; an operator command to forget a node is the other option.

## Roadmap

The phases in outline; [ROADMAP.md](ROADMAP.md) is the plan, with tracks, order and status.

1. **Phase 1: single node.** Deploy `.rbp` with a Wasm component; instances, mailboxes, call/cast, timers; links, monitors, host supervisors; memory, deadline and mailbox limits; logging, config, keyvalue (memory and disk), preopened files; snapshot upgrade with revert; admin CLI. *Exit: two sample apps survive kill, crash-loop and upgrade tests.*
2. **Phase 2a: platform on one node.** Host-service pattern, I/O and protocols (host-owned listeners, extensions for protocols and serial lines), HTTP ingress and egress, secrets, metrics and tracing, introspection, scale and platform baselines. **Phase 2b: cluster.** Failover of singleton and hot-standby apps first; QUIC mesh with mTLS, static seeds; remote pids, links, monitors and `noconnection`; process groups; messaging ACLs. Phase 2a also adds coordination between apps on one node: host-object quotas, local process groups with publish, shared buckets and revisioned watches. *Exit: three-node cluster passes partition and node-loss tests.*
3. **Phase 3: application server.** Placement and failover, opt-in global names and replicated buckets (local reads, watches from the local copy; Raft or witness-based replication), SQL pools, sockets, native-process plugins, blue-green upgrades, more guest SDKs. *Exit: one real workload runs a full release cycle with zero downtime.*

## Sources

- [Erlang/OTP design principles](https://www.erlang.org/doc/system/design_principles.html)
- [Lunatic repository](https://github.com/lunatic-solutions/lunatic) and [open issues](https://github.com/lunatic-solutions/lunatic/issues)
- [Lunatic ProcessConfig](https://docs.rs/lunatic/latest/lunatic/struct.ProcessConfig.html)
- [wasmCloud application manifest](https://wasmcloud.com/docs/ecosystem/wadm/model/)
- [Golem agents](https://learn.golem.cloud/concepts/agents) and [updating running agents](https://learn.golem.cloud/how-to-guides/common/golem-update-running-agents)
- [WildFly developer guide](https://docs.wildfly.org/32/Developer_Guide.html)
- [rustbridge](https://github.com/jrobhoward/rustbridge): bundle format, plugin lifecycle, transport and architecture docs
- [seL4 whitepaper](https://sel4.systems/About/seL4-whitepaper.pdf), [QNX message passing](https://www.qnx.com/developers/docs/7.1/#com.qnx.doc.neutrino.sys_arch/topic/ipc.html), [Fuchsia capabilities](https://fuchsia.dev/fuchsia-src/concepts/components/v2/capabilities)
- [Mnesia](https://www.erlang.org/doc/apps/mnesia/mnesia_chap1.html), [Khepri](https://github.com/rabbitmq/khepri), [etcd API and watches](https://etcd.io/docs/v3.5/learning/api/), [Redis replication](https://redis.io/docs/latest/operate/oss_and_stack/management/replication/)
