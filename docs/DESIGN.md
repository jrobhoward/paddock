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
- Strong consistency or exactly-once delivery by default. Delivery is at most once, ordered per sender/receiver pair, as in Erlang.
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

- `types`: `pid` (`node`, `id`, `incarnation`), `call-ref`, `envelope`, `exit-reason`, `call-error`, `register-error`, `info` (link `exit`, monitor `down`, `timer`) and `call-result` (`reply` or `noreply`).
- `process` (imported by every app): `self`, `cast`, `call` (returns `result<envelope, call-error>`), `reply`, `link`/`unlink`, `trap-exits`, `monitor`/`demonitor`, `register` (returns `result<_, register-error>`), `whereis`, `send-after`/`cancel-timer`, `exit`.
- `world app` exports: `init`, `handle-call(caller, msg)`, `handle-cast`, `handle-info`, `terminate`, `save-snapshot`, `load-snapshot(from-version, snapshot)`.

`call-error` and `register-error` both carry an `access-denied` case (messaging ACLs, names missing from the manifest). `call-error` also has `too-large`, for a body over the target's `message_max_bytes`; it is separate from `overloaded` because retrying cannot help. `crates/paddock/tests/wit_rules.rs` enforces the package's rules: synchronous functions only, no `stream`/`future`, no `wasi:io` types, and an `access-denied` error on every capability function.

**Messages are envelopes of bytes.** WIT has no recursive types, so there is no clean Erlang-style "any term". An envelope carries a content type (CBOR by default; JSON and rustbridge binary allowed) and an optional schema id. Bytes are already serialized for the network, and the manifest can publish JSON Schemas the way `.rbp` bundles already do.

## App manifest and capability model

The manifest describes everything the server needs to place, limit, wire and upgrade an app. The data model is complete now; enforcement ships in phases (column 3). Field names follow `.rbp` style so the manifest can live in a rustbridge bundle as an extra section.

In a bundle, the `appserver` section holds every section below except three that the `.rbp` manifest already carries: `app` comes from `plugin`, `component` from the `wasm32-wasip2` variant, and `provenance` from `build_info`, the SBOM and the operator key that verified the signature. `paddock-manifest` models the section (`AppSpec`) and the combined result (`AppManifest`). Unknown fields are rejected with the path of the field, and a node logs a warning for each field a manifest sets that the current phase does not enforce.

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
| `limits` | memory_max, table_max, callback_deadline_ms, fuel_per_message, mailbox_max, message_max_bytes, storage quota, open files, outbound connections | Phase 1 memory/deadline/mailbox; rest later |
| `capabilities` | requested host interfaces with scopes (table below) | Phase 1 subset |
| `messaging_acl` | which apps or names may call/cast this app, and which it may call | Phase 2 |
| `config` | schema for config keys, defaults, secret references (never secret values) | Phase 1 |
| `upgrade` | snapshot format version, supported from-versions, upgrade mode (snapshot or restart) | Phase 1 |
| `placement` | node labels required or preferred, singleton, spread | Phase 3 |
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
| CPU, wall clock | Epoch interruption: a deadline per callback (callback_deadline_ms) | Cheap, a few percent. A blown deadline kills the instance with reason `timeout`. |
| CPU, deterministic | Fuel per message (optional) | Costs more. Use for untrusted tenants or metering. |
| Threads | Host-owned tokio pool; instances are single-threaded | Wasm threads support in WASI is unsettled, so don't depend on it. Scale with instance pools. |
| Mailbox | mailbox_max per instance; message_max_bytes | When full: cast is dropped and counted, call fails fast with `overloaded`. |
| Filesystem | WASI preopens per manifest grant; read-only or read-write | cap-std stops path escapes. Quotas need a custom `wasi:filesystem` backend or OS quotas per volume. |
| Network | `wasi:sockets` address check hook; `wasi:http` through a filtering proxy | Default deny. Prefer HTTP egress over raw sockets. |
| Storage | `wasi:keyvalue` buckets with byte quotas | Host-owned, so data survives instance crashes. |
| Host calls | Every import runs a policy check and a rate limit | Each host function is attack surface; keep the set small. |

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

**Supervisors**

- Supervisors are host-native, not Wasm. They are trusted, cheap, and can't crash from guest bugs.
- Strategies: `one_for_one`, `one_for_all`, `rest_for_one`. A dynamic `simple_one_for_one` covers instance pools.
- Restart types per child: `permanent`, `transient`, `temporary`.
- Intensity and period, for example 5 restarts in 10 s. Past it, the supervisor gives up and escalates to its parent. At the top, the app is marked `failed` and an alert fires.
- Restart backoff with jitter is an addition beyond OTP, to avoid hot crash loops against a failing dependency.
- Each app gets a root supervisor. Apps that declare the `supervisor` capability can start their own children within manifest limits.

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

**SDK support.** The Rust SDK derives snapshot code from a versioned state type (serde plus a version tag), with a `migrate(from_version, old)` hook. Most apps never hand-write snapshot bytes.

**Native-process plugins** upgrade with `restart` or `blue-green` only, because their memory can't be snapshotted by the host. A plugin may implement the snapshot messages itself over its pipe.

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

- Phase 2: a static seed list in node config, like Erlang hostnames. Joining nodes learn the rest of the mesh from any seed.
- Later: DNS SRV and Kubernetes headless services. mDNS for development.
- Node identity is the certificate subject. A cluster CA signs node certs, replacing Erlang's shared cookie. Revocation uses short-lived certs.
- Node-down detection uses QUIC idle timeout (default 15 s, tunable like `net_ticktime`).

**Registry**

| Scope | Mechanism | Consistency | Phase |
| --- | --- | --- | --- |
| Local names | Per-node map | Strong (single node) | 1 |
| Process groups | Each node owns its members and gossips them, like OTP `pg` | Eventual; never blocks | 2 |
| Global unique names | Raft (openraft) on a small set of voter nodes | Strong; unavailable in a minority partition | 3, opt-in |

Apps should prefer process groups. Erlang's `global` shows that global locks during partitions cause most distribution pain, so unique global names stay opt-in.

**Partitions.** When a partition heals, process groups merge naturally. Global names conflict only if Raft is off; with Raft on, the minority side can't register. Paddock does not attempt automatic split-brain resolution for app state; that is the app's concern.

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
| Logging | `wasi:logging`; host adds app, version, pid, node, trace id | `logger` | Logging subsystem | 1 |
| Config | `wasi:config` | `application:get_env` | System properties | 1 |
| Secrets | `paddock:secrets` (resolved from Vault, files, or env) | — | Elytron credential store | 2 |
| Clocks, random | `wasi:clocks`, `wasi:random` | `erlang:monotonic_time` | — | 1 |
| In-memory term storage | `wasi:keyvalue` bucket, durability `memory`; host-owned so it outlives instance crashes | ETS (with `heir`) | Infinispan local cache | 1 |
| Persistent term storage | `wasi:keyvalue`, durability `disk` (redb) | DETS | — | 1 |
| Replicated storage | `wasi:keyvalue`, durability `replicated` (Raft) | Mnesia | Infinispan distributed | 3 |
| Files | `wasi:filesystem`: read-only `/priv` from the bundle, writable `/data` per app | `code:priv_dir` | — | 1 |
| HTTP ingress | `wasi:http/incoming-handler`; host terminates TLS and routes by host/path | Cowboy, inets | Undertow web container | 2 |
| HTTP egress | `wasi:http/outgoing-handler` via filtering proxy | `httpc` | — | 2 |
| Sockets | `wasi:sockets` with allow-list | `gen_tcp`, `gen_udp` | — | 3 |
| Database pools | `paddock:sql` against named datasources | — | JDBC datasources | 3 |
| Metrics and tracing | `paddock:metrics`; OpenTelemetry export | `telemetry` | Metrics subsystem | 2 |
| Introspection | Admin API: apps, pids, mailbox depth, memory, restarts, message trace | `observer`, `recon`, `dbg` | Management console | 2 |
| Placement and failover | Cluster controller | `dist_ac`, takeover | HA singleton | 3 |
| Distributed transactions | Not provided | — | JTA | Never |

**Term storage notes.** Keys are strings and values are bytes, matching `wasi:keyvalue`. Buckets are per app and namespaced by the host, so apps can't read each other's data. Sharing data between apps goes through messages or an explicitly shared bucket granted to both. Atomic increment and compare-and-swap come from `wasi:keyvalue/atomics`.

**Logging notes.** Apps never write to stdout as a log channel. The host captures stdout and stderr anyway and tags them, so legacy code still produces useful output. Rate limits per app stop one noisy app from flooding the node.

## Security model

Security rests on three hard boundaries: the Wasm sandbox around each app, mTLS between nodes, and signatures on every bundle. Policy narrows what crosses those boundaries; it never substitutes for them.

**Trust levels**

| Principal | Trusted for | Not trusted for |
| --- | --- | --- |
| Operator (admin API, signed by operator key) | Grants, deployments, cluster config | — |
| Node (cluster CA cert) | Routing messages, holding link and registry state | Inventing messages from pids it doesn't host |
| Bundle signer (minisign key from `.rbp`) | Authorship and integrity of code | Permissions: signing doesn't grant anything |
| App instance | Its own state and granted capabilities | Anything else |

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
- **S7 (WASI hosts):** use wasmtime's crates for core WASI, `wasi:config` and `wasi:http`. `wasmtime-wasi-keyvalue` is a demo (writes don't persist past a bucket handle), and there is no `wasi:logging` host, so Paddock writes both. Details: `spikes/S7-wasi-host-crates.md`.
- **S8 (guest versus native speed): 1.0x to 2.4x for compute; about 2 µs per call.** Measured through `paddock-host` with identical source compiled both ways. Plain integer code matches native (software SHA-256 at 1.02x, fannkuch-redux 1.56x). The gaps come from CPU features wasm lacks: native SHA-256 with SHA extensions is about 6.8x faster, and n-body runs at 2.35x scalar and 1.82x with wasm `simd128`. A call into an instance costs 2 µs on the same worker and 4.8 µs across threads, so Paddock suits messages carrying more than tens of microseconds of work. Details: `spikes/s8/README.md`.
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

## Open questions

- [ ] Default envelope encoding: CBOR, or rustbridge JSON for familiarity?
- [ ] Should supervisors ever be written in Wasm, or always host-native?
- [ ] Is selective receive needed, or are call refs plus deferred reply enough?
- [ ] Messaging ACL by app name, by registered name, or by capability token?
- [ ] Ship the server as a library (embedded in another binary) as well as a daemon?
- [ ] How far to go with native-process plugins before Phase 3?

## Roadmap

1. **Phase 1: single node.** Deploy `.rbp` with a Wasm component; instances, mailboxes, call/cast, timers; links, monitors, host supervisors; memory, deadline and mailbox limits; logging, config, keyvalue (memory and disk), preopened files; snapshot upgrade with revert; admin CLI. *Exit: two sample apps survive kill, crash-loop and upgrade tests.*
2. **Phase 2: cluster.** QUIC mesh with mTLS, static seeds; remote pids, links, monitors and `noconnection`; process groups; messaging ACLs; HTTP ingress and egress; metrics and tracing; introspection. *Exit: three-node cluster passes partition and node-loss tests.*
3. **Phase 3: application server.** Placement and failover, opt-in global names (Raft), replicated storage, SQL pools, sockets, native-process plugins, blue-green upgrades, more guest SDKs. *Exit: one real workload runs a full release cycle with zero downtime.*

## Sources

- [Erlang/OTP design principles](https://www.erlang.org/doc/system/design_principles.html)
- [Lunatic repository](https://github.com/lunatic-solutions/lunatic) and [open issues](https://github.com/lunatic-solutions/lunatic/issues)
- [Lunatic ProcessConfig](https://docs.rs/lunatic/latest/lunatic/struct.ProcessConfig.html)
- [wasmCloud application manifest](https://wasmcloud.com/docs/ecosystem/wadm/model/)
- [Golem agents](https://learn.golem.cloud/concepts/agents) and [updating running agents](https://learn.golem.cloud/how-to-guides/common/golem-update-running-agents)
- [WildFly developer guide](https://docs.wildfly.org/32/Developer_Guide.html)
- [rustbridge](https://github.com/jrobhoward/rustbridge): bundle format, plugin lifecycle, transport and architecture docs
