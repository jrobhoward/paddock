# Bulkhead: Architecture

2026-11-01 · Companion to [DESIGN.md](DESIGN.md) · How Bulkhead differs from Paddock: [PADDOCK.md](PADDOCK.md)

A map of the code as built: the crates, the paths a call, a deploy, a failover, a replicated write and a release take through them, the threads and tasks that run, and where state lives. DESIGN.md says why; this file says where.

## Crates

```mermaid
flowchart TD
    operator(["operator"]) -->|"CLI, over the Unix socket or TLS"| bulkhead
    bulkhead["bulkhead (bin + lib)<br/>api: admin types, client<br/>node: daemon, store, gate, events, audit, serial path<br/>cli: commands, shell"]
    host["bulkhead-host<br/>runtime, process core, services, supervisors,<br/>transport, failover, replication, http, listeners, native"]
    bundle["bulkhead-bundle<br/>manifest, .rbp, verify, policy, grant<br/>(no wasmtime)"]
    native["bulkhead-native<br/>protocol, channel, snapshot, app, bulkhead-runner"]
    ext["extensions<br/>bulkhead-modbus, greeter service"]
    guests["guests (Wasm)<br/>bulkhead-sdk, fixtures, examples"]
    napps["native apps<br/>rustbridge plugins, executables<br/>on bulkhead-native's app API"]
    bulkhead --> host
    bulkhead --> bundle
    bulkhead -->|"modbus feature"| ext
    host --> bundle
    host -->|"protocol, channel, snapshot"| native
    ext -->|"Extension trait"| host
    guests -.->|"WIT imports"| host
    napps -.->|"run by bulkhead-runner"| native
```

Solid arrows point from a crate to the crates it depends on; dotted arrows are what runs on the host, not Cargo dependencies.

| Crate | Builds | What it holds |
| --- | --- | --- |
| `bulkhead` | binary and library | `api` (admin API types, and the client over the Unix socket or TLS), `node` (the daemon, feature `node`), `serial` (the tty layer and frames for the failover serial line), `cli` (commands, bundle building, the shell). Without `node` it does not build wasmtime. |
| `bulkhead-bundle` | library | The app manifest model and validation, `.rbp` opening and signature checks against operator keys (a component, or a native app's platform variant), component import inspection, operator policy and the effective grant. |
| `bulkhead-host` | library | The runtime: engine, epoch ticker, process table, instances (components and native processes), passivation, links and monitors, supervisors, app sets and pools, host services, the HTTP ingress and handlers, listeners, the cluster transport, liveness, membership, failover, replication, moving singletons and release versions. |
| `bulkhead-native` | library and `bulkhead-runner` | The frame protocol between a node and a native app, the channel it travels over (shared-memory rings, the pipes that wake each side, the descriptor socket; feature `channel`), published buckets' snapshot format, the app API for native executables (feature `app`), and `bulkhead-runner` (feature `runner`, default), the launcher that sandboxes itself and loads or executes a native app. `bulkhead-host` takes the protocol, the channel and the snapshot format. |
| `bulkhead-modbus` | library | The Modbus TCP extension; depends on `bulkhead-host` alone. |
| `bulkhead-modbus-client` | app library | Apps' client for the Modbus extension's WIT, the same functions in both builds: the generated bindings in a component, calls through the node (`bulkhead_sdk::extension`) natively. |
| `bulkhead-sdk`, `bulkhead-sdk-macros` | app libraries | The Rust SDK: the `App` trait, envelopes, snapshots, service wrappers by feature. On `wasm32` it owns the generated bindings; on Unix the same functions go over `bulkhead-native`'s protocol, and `app!` also generates `bulkhead_main()`, so one source builds as a component and as a native program. |
| `bulkhead-fixture-*` | guests and native test apps | Components (echo, chaos, counter in three versions, services, sdk, conn, http, net), a native executable (`native`) and a rustbridge plugin (`plugin`). |
| `bulkhead-soak` | binary | Fault injection against `bulkhead node` processes (`scripts/soak.sh`): failover, replication and the SCADA release cycle (its apps as components or, `--native`, native programs). |
| `examples/*` | guests and scripts | SDK examples, the tutorial's apps, the SCADA reference app, the greeter extension, the failover demo, systemd units. |

The WIT contracts live in `wit/`: `bulkhead:runtime` (the app world, frozen per version) and separate packages for connections, groups, watch, secrets, metrics, SQL and I/O readiness. `wit/vendor/` holds the WASI interfaces bound against.

Code areas and their interfaces are listed in ROADMAP.md, "Code areas".

## Inside `bulkhead-host`

| Area | Modules | Role |
| --- | --- | --- |
| Engine | `runtime`, `config`, `bindings`, `deny`, `compiled` | `Runtime` and its `RuntimeInner`: the wasmtime engine with a pooling allocator sized by `max_instances` (a freed slot keeps up to `keep_resident` of its written pages for the next instance, given back once starts have been quiet for `release_idle_after`, `pool_memory`), the epoch ticker, the linker (WASI p2 with the socket functions redefined, `bulkhead:runtime`, `bulkhead:io`, services, extensions), deny stubs for ungranted imports, compiled components kept on disk |
| Process core | `pid`, `table`, `registry`, `links`, `message`, `instance`, `instance_native`, `state`, `process`, `ready`, `delivery`, `acl`, `lifecycle`, `output`, `health` | Instances, their mailboxes and callback loop (a Store, or a native process), passivation, the per-instance host state and `process` functions, readiness waits, pid routing, messaging ACLs (checked on the target's node), links and monitors, health checks |
| Orchestration | `apps`, `pool`, `supervisor`, `upgrade`, `spec` | App sets in dependency order, a root supervisor per app, pools with routing and a blue-green second side, snapshot, restart and blue-green upgrades |
| Services | `services::{config, keyvalue, groups, logging, secrets, metrics, egress, sockets, sql, extension}`, `metrics` | Guest-facing interfaces on the `Service` pattern; extensions from other crates |
| I/O | `http` (`http::tls`, `http::handler`), `listener` | The HTTP ingress (a request is a call to a pool), HTTP handlers (an instance per request) and listeners (an instance per connection, or a native app's instances, many each) |
| Native apps | `native::{port, cgroup, convert}` | A native instance's process through `bulkhead-runner`, its limits, and the conversions to the protocol's types |
| Cluster | `transport::{liveness, membership, wire, quic, memory, lane}`, `failover::{node, witness}`, `replication::{core, log}`, `handover`, `formats` | Frames between nodes, heartbeats and sessions, joining, QUIC with pinned keys, the in-memory mesh for tests, the HA pair and witness, replicated buckets, singletons moved with their state, the pair's version for what is kept |
| Observability | `probe`, `metrics` | USDT probes on the message path; Prometheus text |

`delivery` is the one place a pid is routed: a live local instance, a dead local pid (`noproc`), or another node (a frame on the transport, or `noconnection`). Process-core code goes through it, never the process table directly.

`LoadedApp` holds what an app runs: a component linked and ready to instantiate (`Runtime::load`, `load_cached`), or a native program (`Runtime::load_native`). The instance task takes the same channels and handle either way and branches once, at the start of its lifecycle.

## A call

### From the operator

```mermaid
sequenceDiagram
    participant CLI as bulkhead call app body
    participant Node as node::server, Node::send
    participant H as InstanceHandle
    participant T as instance task (serve loop)
    participant G as guest (Store)
    CLI->>Node: HTTP over the admin socket (or TLS), POST /v1/apps/{app}/call
    Note over Node: resolve(target)<br/>app: the app's pool picks an instance (round robin, or a hash of a key,<br/>during a blue-green upgrade the new side by weight)<br/>pid: the process table<br/>name: the registry
    Node->>H: call
    Note over H: size check, a full mailbox is an error at once
    H->>T: mailbox try_send(Call{msg, oneshot})
    Note over T: takes, in order: control changes, host ops (upgrade, snapshot, move),<br/>signals (exits, timers, watch notices), readiness of the guest's<br/>pollables (io-ready), then the mailbox<br/>starts the callback clock (callback_deadline_ms counts guest time only)
    T->>G: handle-call
    G-->>T: reply, or noreply and process.reply later
    T-->>Node: oneshot
    Node-->>CLI: HTTP response
```

The caller waits with its timeout; a reply after the timeout is dropped. A guest that exceeds its deadline traps at an epoch tick, and the instance exits with a reason its supervisor acts on. Guest time excludes host time: the callback clock (`state::CallbackClock`) counts only the time the instance task spends polling the callback, so a guest waiting in `process.call` or a keyvalue read is not charged for it, and host work that does not wait is. wasmtime's call hook stays off (it cost about 5% of an empty call, B12).

### Between instances

A guest calls `process.call`. The host function (`process`) hands it to `delivery::call`, and the time the caller waits counts as host time.

- **Local:** the target's mailbox, as above.
- **Remote:** a `Frame::Call` with an id goes to the target's node over the transport, and the caller waits on `pending_calls`. The receiving node runs `delivery::call` in a task of its own and answers with `Frame::Reply`. A session that ends meanwhile fails the call with `noconnection`. Calls from one peer handled at once are capped (`[cluster] incoming_calls_max`, `RuntimeConfig::incoming_calls_per_peer`); past it they are answered `overloaded`.

Casts follow the same routes without a reply; a full mailbox drops the cast and counts it. A congested link drops casts and fails calls with `overloaded` past `send_buffer_bytes`, as a full mailbox does.

### Between two native apps

A native app's `process.call` is a request over its channel. Its instance task runs the same `process` host code a component's import does, so routing, ACLs, limits and errors are the same; only the two ends differ. Three processes take part: the caller's, the node and the target's.

```mermaid
sequenceDiagram
    participant A as caller app (process)
    participant AT as caller's instance task
    participant D as delivery
    participant BT as target's instance task
    participant B as target app (process)
    Note over A: in a callback, process.call(to, msg, timeout)
    A->>AT: Request Call{to, msg, timeout_ms}, through A's ring
    Note over A: the callback thread waits for the Answer:<br/>spins for spin_us, then sleeps on its door
    AT->>D: process Host::call, delivery::call
    Note over D: route: a local pid in the process table,<br/>the messaging ACL, the size check
    D->>BT: mailbox try_send(Call{msg, oneshot}), a full mailbox fails at once
    Note over BT: takes it once B is between callbacks,<br/>keeps the oneshot under a new call_ref
    BT->>B: Call{call_ref, msg}, through B's ring
    Note over B: handle_call
    B-->>BT: Reply(Some(envelope))
    BT-->>AT: the oneshot
    AT-->>A: Answer Called(Ok(envelope)), through A's ring
    Note over A: the callback goes on
```

- **The caller waits inside its callback,** as a `gen_server:call` does. Its instance task is busy answering the request, so the caller takes no other message until the reply, a timeout or an error (`noproc`, `overloaded`, `too-large`, `access-denied`, `exited`). The wait is host time: the caller's callback deadline does not count it.
- **A later reply:** the target answers `Reply(None)` and keeps `call_ref`. In any later callback it sends `Request Reply{call_ref, msg}`, and its instance task completes the oneshot it kept. A reply after the caller's timeout is dropped.
- **A cast** is `Request Cast{to, msg}`: `delivery::cast` queues it in the target's mailbox, and the caller gets `Answer Unit` at once if it was queued, or `Answer CastFailed` with the reason if it was dropped (a full mailbox, a dead pid). `cast` ignores the answer; `try_cast` returns it. `cast_timeout` is `Request CastWait{to, msg, timeout_ms}`: `delivery::cast_wait` waits, inside the request, for the target mailbox's bytes and then a slot (`mailbox::Sender::send_timeout`), and the answer is `Unit` or `CastFailed(Timeout)`. The target's task hands it on as `Cast{msg}`, and the app answers `Done`.
- **What each message costs:** two requests and two copies through rings, besides the mailbox. About 13 µs at p50 for a call between two native instances on one node, on the 16-core machine and the Raspberry Pi 5, against 5 to 7 µs for components (BASELINE.md, T5, 2026-11-01).

### Between nodes

A pid names its node, so the same `process.call` reaches an instance on another node: `delivery::route` finds `Route::Remote`, and the call crosses the QUIC connection between the two nodes. Four processes take part: the caller, both nodes and the target.

```mermaid
sequenceDiagram
    participant A as caller app (node 1)
    participant AT as caller's instance task (node 1)
    participant T1 as node 1 transport
    participant T2 as node 2 transport
    participant BT as target's instance task (node 2)
    participant B as target app (node 2)
    A->>AT: Request Call{to: a pid on node 2, msg, timeout_ms}
    AT->>T1: delivery::call finds Route::Remote, transport::call
    Note over T1: pending_calls.add(id, oneshot),<br/>the link's lane: past send_buffer_bytes, overloaded
    T1->>T2: Frame::Call{id, from, sender, to, msg, timeout_ms},<br/>on the data stream (QUIC, mutual TLS, pinned keys)
    Note over T2: admits: the messaging ACL, on the target's node.<br/>incoming_calls_max per peer: past it, Reply overloaded
    Note over T2: a task of its own runs delivery::call, now local
    T2->>BT: mailbox try_send(Call{msg, oneshot})
    BT->>B: Call{call_ref, msg}, through B's ring
    B-->>BT: Reply(Some(envelope))
    BT-->>T2: the oneshot
    T2->>T1: Frame::Reply{id, result}
    Note over T1: pending_calls.take(id)
    T1-->>AT: the oneshot
    AT-->>A: Answer Called(Ok(envelope))
```

- **The connection** is one per pair of nodes, with a control stream (heartbeats, membership, failover) and a data stream (messages, links, monitors), so frames between two nodes keep their order and heartbeats never wait behind a large message (`transport::quic`).
- **Failures:** a session that ends while the call waits fails it with `noconnection` (`PendingCalls::fail_node`), as it ends links and monitors with that node. A caller that times out drops its entry, and a late `Frame::Reply` finds nothing. A remote cast is `Frame::Cast`, `Ok` once the transport takes it, and dropped if the target's mailbox is full.
- **A waited cast** (`cast_timeout`) is `Frame::CastWait`, sent as a call is (`pending_calls`, `incoming_calls_per_peer`): the receiving node runs `delivery::cast_wait` in a task of its own and answers `Frame::Reply` with an empty envelope once the target's mailbox took it, or why not. The sender waits 250 ms longer than the receiving node (`CAST_WAIT_GRACE`), so a timeout is normally the receiving node's answer. A peer one release older cannot read the frame, and the cast fails with `noconnection`.
- **What each message costs:** the transport dominates. About 160 to 170 µs at p50 for a call between native instances on two node processes on one host (167 µs on the 16-core machine, 158 µs on the Pi), against 13 µs within a node (BASELINE.md, C4, 2026-11-01). Native apps between the two machines have not been measured since T1.

### From HTTP

The ingress (`http`; in the node, `daemon/ingress.rs`, behind the `http` and `tls` features) accepts on several tasks per address, matches a route by host and longest path prefix, and reads the body up to the route's limit.

- **To an app:** the request becomes a CBOR envelope, encoded straight from the borrowed request, and a call to the owning app's pool; the reply envelope becomes the response. On a standby, a singleton's route redirects to the primary (`partner_url`) or forwards the call over the cluster (`standby = "forward"`, `Frame::AppCall`).
- **To an HTTP handler** (`http::handler`; the node's `daemon/handlers.rs`): a `wasi:http/proxy` component gets a new instance per request, with streaming bodies. The route's timeout runs until the response headers, then again as an idle timer while either body moves; past it the stream is cut. The instance takes a Store slot for the request's life (`503` with `Retry-After` when none is free) and has no mailbox.

Listeners (`listener`) are like HTTP handlers for raw TCP: each accepted connection gets a new instance of a connection-handler component, which reads and writes the socket through `wasi:io` streams. A native app's listener hands each connection to an instance of the app's pool instead: an operation ahead of the instance's mailbox (`Op::Connection`), whose stream the instance task passes on the descriptor socket and keeps a copy of in the instance's sockets (`services/sockets/native.rs`), as for a `wasi:sockets` stream; the listener's own copy watches `TCP_INFO` for the idle timeout and is shut down on stop.

### Out of an app

- **HTTP egress** (`services::egress`): wasmtime-wasi-http's send hook is the filter. A request goes out only if it matches the manifest's `allow` (inside node config's `[egress] allow`), through the node's connection pool, at most `max_concurrent` per app at once. Its resolver refuses names that resolve only to loopback or link-local addresses unless `allow_loopback`.
- **Guest sockets** (`services::sockets`): wasmtime-wasi implements `wasi:sockets`; the service narrows it with an address check on every use (the manifest's `connect` and `bind` rules, `listen`, and the guard on loopback, link-local and unspecified destinations). The count per app (`max_sockets`) has no hook in wasmtime-wasi, so `shadow` redefines `create-tcp-socket`, `create-udp-socket`, `accept` and both socket destructors on the linker, taking a semaphore permit per socket.
- **Waiting on a socket** (`ready`, `bulkhead:io`): a guest hands the host a pollable and a tag with `ready.notify`; the serve loop polls the instance's pollables beside its mailbox and delivers the host event `io-ready` once one is ready. The `wasi:io/poll` pollable destructor is redefined to forget waits on a dropped pollable.
- **SQL** (`services::sql`, feature `sql`): a Postgres pool per datasource, with a connection cap per app.

## Passivation

An app with `instances.passivate_after_ms` lets an idle instance give up its Store (spike S11). After that long with nothing to do, the instance runs `save-snapshot`, drops its Store and keeps its data (`InstanceState`: timers, waiting callers, services' state); its pid, mailbox, links, monitors and names live outside the Store and stay. Its state is `Passive`.

Stores are bounded by `RuntimeInner::slots`, a semaphore of `max_instances` permits, while `max_processes` bounds instances. The next message, signal or operation for a passive instance takes a slot, builds a Store on the kept data and runs `load-snapshot`. An instance that finds no free slot signals `slot_wanted`, and idle instances that may passivate do so early to free one. A snapshot operation on a passive instance is answered from the kept bytes without waking it.

## Native-process apps

A native app (`kind = "native-process"`) runs as a process, one per instance, through `bulkhead-runner`:

```mermaid
sequenceDiagram
    participant I as instance task (instance_native)
    participant R as bulkhead-runner (one process)
    Note over I: spawn: pre_exec sets the parent-death signal,<br/>no core dumps, the cgroup or RLIMIT_DATA
    I->>R: spawn
    Note over R: non-dumpable, Landlock (read libraries and the app's files,<br/>execute only the program), seccomp (signals to itself only,<br/>no ptrace, no sockets), then load the rustbridge .so<br/>or execve the executable (bulkhead-native app)
    R->>I: Hello
    I->>R: Init
    I->>R: Call / Cast / Info, one callback at a time
    R->>I: Request (process.*, services), during a callback
    I->>R: Answer
    R->>I: Reply / Done / Failed
    I->>R: Ping (idle), answered between callbacks
```

- **The port** (`native::port`) holds the child, its channel and a task copying its stdout and stderr into log lines, as a component's stderr is. The channel (`bulkhead_native::channel`, T1) is two byte rings in a memfd the app inherits as descriptor 3, each waiting side spinning for `[native] spin_us`, then sleeping on a pipe the other writes a byte to when it said it sleeps: the app on its door (descriptor 5), the node on the bell (the app holds its write end as descriptor 4), whose end of file also means the app has exited. The instance task writes frames into the ring itself as far as they fit, keeping the rest until the app makes room, so a stalled app never blocks it, and reads and decodes the app's frames itself through a buffer, so a call wakes no other task on the node's side (Z1). It spins between callbacks too, so its worker stays awake for the instance's next message, except after a callback it hinted idle: nothing was queued behind it and it came longer than the spin after the frame before, so it writes 1 to the ring's `idle_hint` word and neither side spins after it (T1b). On the app's side, its callback thread reads the ring itself. Descriptors the node passes (T3) go on a socket of their own, the app's end at descriptor 6, each before the frame that names it (`Port::send_fd`, `app::take_fd`). A published bucket's snapshots go that way (T4): the instance task passes the latest of each before `Init` and whenever a new one is made, and the app maps it and reads `get`, `get_many` and `exists` there (`bulkhead_native::snapshot`).
- **The lifecycle** (`instance_native`, a child module of `instance`) uses the instance's own channels, so pids, links, monitors, supervision and exit reasons work as for a component. Requests are answered by the same host code a component's imports run (`process::Host`, and the keyvalue, groups, config, secrets and metrics services), on an `InstanceState` the task holds without a Store; metric updates come in one unanswered frame before the callback's result. One callback-deadline timer and one ping timer per instance are re-armed when they fire early, rather than a timer per frame. A callback's deadline is wall time less the time spent answering requests; past it, or past a ping's deadline while idle, the process is killed (`timeout`). Its exit is a crash with its status or signal, a panic is a crash with the panic's message, and an OOM kill in its cgroup is `memory_limit`.
- **Limits** (`native::cgroup`): with node config `[native] cgroup`, the node moves itself into `<cgroup>/node` at start and gives each instance `<cgroup>/native/<pid>` with `memory.max`, no swap and `pids.max`; the child is placed there between fork and exec. Without it, `memory_max` is the process's `RLIMIT_DATA`.

Snapshots are frames (`SaveSnapshot` between callbacks, `LoadSnapshot` in place of `Init`), and every load is in a new process: a snapshot upgrade starts the new version's process (with a cgroup of its own) beside the old and swaps the port once it has loaded, handing the `InstanceState`'s timers and pending callers over as for a component; passivation saves, kills and reaps the process, and `passive` waits as for a component, answering snapshots from the kept bytes; a wake or a moved singleton starts a new process from the snapshot. A rustbridge plugin's instance refuses snapshot operations. A singleton's process is killed when the node's lease ends, idle or not (`crashed: lease_ended`), since it can run threads between callbacks.

### Processes and shared memory

A node with native apps is one `bulkhead node` process and one process per native instance. `bulkhead-runner` does not add one: it sandboxes itself and then `execve`s the app's executable (or loads its rustbridge `.so`) in the same process. Two apps with one instance each, a writer and a watcher of a shared bucket:

```mermaid
flowchart LR
    subgraph node["bulkhead node (one process)"]
        WT["writer's instance task"]
        RT["watcher's instance task"]
        core["process table, mailboxes and signal queues,<br/>delivery, registry, links"]
        KV["KvStore and Watches<br/>(bucket data: heap maps, redb file)"]
        PUB["publishing thread"]
        TR["transport (QUIC)"]
    end
    subgraph W["writer app (process)"]
        WA["callback thread"]
    end
    subgraph R["watcher app (process)"]
        RA["callback thread"]
    end
    WT <-->|"memfd: two 64 KiB rings,<br/>bell and door pipes"| WA
    RT <-->|"memfd: two 64 KiB rings,<br/>bell and door pipes"| RA
    WT --- core
    RT --- core
    WT --- KV
    RT --- KV
    core --- TR
    KV --> PUB
    PUB -.->|"sealed memfd snapshot,<br/>mapped read-only"| WA
    PUB -.-> RA
    TR <-->|"one connection per peer"| peer(["other nodes"])
```

- **Shared memory is between the node and one app only.** Each instance has a channel of its own: a memfd holding a 64 KiB ring each way (`CHANNEL_BYTES`), mapped by the node and that app, with the bell and door pipes that wake a side that sleeps. Two apps never map each other's channel. A message or a value from one app to another is copied out of the sender's ring into the node's memory and then into the receiver's ring. A frame larger than a ring (up to 17 MiB) goes through in pieces as the reader makes room.
- **The one mapping several apps share** is a published bucket's snapshot (`publish_ms`): the node's publishing thread writes it into a memfd, seals it and passes it to each native reader on its descriptor socket, and each maps it read-only. Writes to that bucket still go to the node as requests.
- **What an app inherits:** the channel's memfd (descriptor 3), the write end of the node's bell (4), the read end of its own door (5) and its end of the descriptor socket (6). It opens no sockets and no bucket files of its own: seccomp refuses sockets, and the node owns every bucket.
- **The node's side of an instance is a tokio task, not a thread or a process.** The instance task reads the app's frames, answers its requests with the same host code a component's imports run, and feeds it calls, casts and infos from the mailbox and signal queues, which are node memory (`mpsc` channels). An app sees only frames.
- **A crash stays in its process.** An app that exits or is killed closes its ends of the pipes; the bell's end of file tells the instance task, which turns it into an exit reason for links, monitors and the supervisor. The node, the other apps and the bucket data keep running. If the node dies, the parent-death signal ends every app, and the door's end of file tells an app that is still reading.

## A deploy

`bulkhead deploy app.rbp` sends the bundle (`POST /v1/apps`). The node holds the change lock for the whole deploy; reads do not wait for it.

1. The upload is written to `<data_dir>/tmp`.
2. `VerifiedBundle::open`, on the blocking pool: the signature against `trusted_keys`, the `appserver` section, checksums, and the `wasm32-wasip2` variant and its world, or a native app's platform variant.
3. The effective grant: manifest ∩ operator policy. A native program declares its capabilities but has no imports to inspect.
4. The ceilings: `[egress] allow`, `[sockets]`, datasources bound.
5. `Runtime::load_cached`, on the blocking pool: compile with Cranelift, inspect imports, link deny stubs, and keep the compiled code under `compiled/`. A native app's program is written under `compiled/` (mode 0500) and loaded with `Runtime::load_native`.
6. The `InstanceSpec`, from the manifest, narrowed by the grant.
7. The store: `priv/` extracted, the bundle kept as `bundles/<app>/<version>.rbp`.
8. Run: HTTP routes, if granted (an HTTP handler only routes). A singleton on a failover node goes to `gate.hold` and starts only while `may_run`; anything else goes to `Apps::add`: the dependency check, the root supervisor and a pool of instances, each instantiated in a new Store (or spawned as a native process), then `init`, then serving.
9. `state.json` saved, and an `audit.log` record.

On restart a node re-verifies every stored bundle and starts the apps `state.json` marks running, in deploy order, reading compiled components from `compiled/` instead of compiling them.

### Upgrades

An upgrade takes the same verify and compile steps, then one of three modes, as the new version's manifest says:

- **`snapshot`:** each instance in place: save-snapshot in the old Store, a new Store from the new component, load-snapshot; a failure reverts that instance with its state intact.
- **`restart`:** the instances stop and start again from `init` at the new version.
- **`blue-green`:** the new version's instances start beside the old under a root supervisor of their own, as a second side of the app's pool (`pool`), with a weight: the percentage of messages routed to the new side. `bulkhead upgrade --steps` moves the weight in steps, and `bulkhead traffic` sets it, retires the old side or stops the new one. A watcher in `apps` follows the new side's supervisor. The node's `daemon/blue_green.rs` keeps the deploy store in step, so a restart brings back both sides at the same weight.

Rollback goes to the previous version the same way.

## A failover

```mermaid
sequenceDiagram
    participant N1 as node 1 (primary)
    participant W as witness (node 3)
    participant N2 as node 2 (standby)
    Note over N1,N2: liveness loop, on the LAN and the serial line
    N1->>N2: beats: standing, epoch
    N2->>N1: beats: standby, epoch e, confirming (renews 1's lease)
    N1->>W: renews its lease
    Note over W: records the renewal
    Note over N1,N2: node 1 is cut off
    Note over N1: at lease end − margin: may_run = false,<br/>the gate kills singletons
    Note over N2: hears 1 on no path: stops confirming
    N2->>W: asks
    Note over W: waits lease + margin since 1's last renewal,<br/>writes witness-state
    W->>N2: grants
    Note over N2: promotes at epoch e+1. The gate writes failover-epoch,<br/>waits for replication, then starts singletons
```

Each node's failover task moves between three standings. The transitions are named after the actions in `docs/tla/FailoverL2.tla`:

```mermaid
stateDiagram-v2
    [*] --> Down
    Down --> Standby: Rejoin, a primary at no lower an epoch
    Down --> Primary: Granted (Resume), or Bootstrap
    Standby --> Primary: Granted after Ask, Override, or Paths
    Standby --> Down: Refused or StandDown
    Primary --> Down: Fenced or Switchover
```

A standby asks the witness once its partner is silent on every path, or at once when the partner yields in a switchover. `Override` promotes on an operator's confirmation, when the witness cannot be reached either; `Paths` promotes on silence alone, for a pair without a witness. `Bootstrap` is the start of a pair: with both nodes down, the lower node id leads.

- **Liveness** (`transport::liveness`) beats every peer each heartbeat interval, on the cluster link and any extra path, with a payload failover sets (standing, epoch, whether it confirms). A path silent for `node_timeout` raises an event; a peer that stops being reachable ends its session.
- **The serial path** (`node/serial_path.rs`, on `bulkhead::serial`): one task per configured device writes this node's beats as serial frames and reports the partner's to the runtime as a heartbeat path. A device that fails is reopened a second later, the path silent meanwhile; frames from or for another node are dropped. The tty layer is Bulkhead's own, on `rustix`.
- **The failover task** (`failover::node::run`) is a state machine, one type per standing, stepping on node events, witness messages, each heartbeat tick, and the moment its lease would end. It sees liveness and the transport through the `Env` trait, which unit tests fake to drive one transition at a time. It publishes a `Leadership` (standing, epoch, `may_run`) on a watch channel. Its transitions are named after the actions in `docs/tla/FailoverL2.tla`.
- **The witness** (`failover::witness`) answers one request at a time under a lock, and writes its epoch, promoted node and record of a partner out of date to disk before a grant or record change leaves. A primary yielding in a planned switchover releases its lease there (`Release`), so the standby need not wait it out.
- **The gate** (`node::gate`) follows the leadership on the node: it writes the epoch to `failover-epoch` before acting on it, and starts the held singletons when `may_run` is set, from a moved snapshot if one waits, else from `init`. A singleton with replicated buckets (its own or a cluster bucket it uses) waits until `ReplicationStatus::ready` at the current epoch. It checks held singletons as a start would and retries failed starts. When `may_run` drops it kills them without `terminate`. It logs `singleton starting` and `singleton killed`, which the soak's checker reads.
- **Out of lease, in the instance:** an instance bound to the lease checks it before each callback and at each epoch tick (`failover::RunUntil`), so the window between the leadership and the gate's kill runs no guest code.

The rules are R1 to R14 in `docs/tla/VERIFICATION.md`; the assumptions the model makes, and which the code does not meet (A8: process pauses), are listed there.

### A planned switchover, with state

`bulkhead failover switchover` on the primary hands its singletons to the standby. A singleton in `snapshot` upgrade mode, on a pair with `[failover] replication = true`, is moved with its state (`handover`): the instance takes its snapshot (the `MoveOut` operation; afterwards it handles nothing more), and the old primary writes it to the reserved replicated bucket `@handover`, one key per app, through the pair's log. Every start of a singleton that moves takes the record first (`Runtime::take_handover`): it deletes the record, waits for the delete to commit, and loads the snapshot in place of `init`. The gate's `hand_over` and `start` do the two halves; `RuntimeInner::handovers` holds what was taken until the instance starts.

## A watch between two apps

Two native apps share a bucket from node config (`[buckets.<name>]`), each listing it in its manifest under `wasi:keyvalue` `shared`; the watcher may hold it read-only. The watcher starts a watch, usually in `init`; the writer writes as usual and knows nothing of it. Three processes take part, as in "Processes and shared memory" above: the writer and the watcher, each talking only to the node through its own channel, and the node, which holds the bucket and does the write:

```mermaid
sequenceDiagram
    participant W as writer app (process)
    participant WT as writer's instance task
    participant KV as KvStore (services::keyvalue)
    participant WS as Watches
    participant RT as watcher's instance task
    participant R as watcher app (process)
    R->>RT: Request Kv Watch(bucket, key or prefix, after), in init
    RT->>WS: add (checks the grant and limits.watches_max)
    WS-->>RT: id, the bucket's revision
    RT-->>R: Answer: Started
    W->>WT: Request Kv Set(key, value), during a callback
    WT->>KV: run(bucket, Set)
    Note over KV: applies the write: the bucket's revision rises,<br/>the key is stamped with it
    KV->>WS: notify(bucket, changed keys, revision), after the commit
    Note over WS: each matching watch (exact key, or a prefix the key starts with):<br/>a notice already queued for the key takes the new revision<br/>(conflated), or else a notice is queued
    WS->>RT: signal(Info::Late(notice)), on the unbounded signal queue
    KV-->>WT: Ok
    WT-->>W: Answer: Unit, the writer's callback goes on
    Note over RT: takes signals ahead of the mailbox, between callbacks.<br/>The notice takes the key's latest revision<br/>(none if the watch ended)
    Note over RT: native_notice: reads the key's value and revision now
    RT->>R: Info KvChanged{watch, bucket, key, entry, revision}
    Note over R: the SDK makes it the host event kv-changed<br/>(carried: true, value) and calls handle_info
    Note over R: the app calls Change::from_info(info),<br/>then its own code for that key
    R-->>RT: Done
```

- **The watch** (`services::keyvalue::watches`) names the bucket by its storage key, so an app's own bucket and a shared bucket of the same name are told apart; an app's own buckets are its alone, so only a shared bucket (or a `cluster` one) links two apps. It ends with `unwatch` or when the instance exits; an upgrade keeps it, and a restarted instance watches again in `init`, with `after` set to the last revision it saw to replay what it missed (`too-old` if the bucket can no longer replay that far).
- **Notices are conflated per key, never dropped:** a slow watcher misses intermediate values but always gets the latest. A notice that waits on a passivated instance wakes it.
- **The data stays in the node.** The writer's value is copied out of its ring into the bucket (the node's heap, or its redb file for a disk bucket); the watcher's copy is read from the bucket and copied into the watcher's ring. Neither app maps the other's memory or the bucket's (a published bucket's snapshot is the exception, read-only).
- **The value comes with the notice** for native apps (`Change::carried`, `Change::value`; `None` for a delete), read by the watcher's task when it delivers the notice, so the revision is the value's own and may be past the write that queued it. A component's notice carries only the revision; it reads the value with `watch::get`.
- **A replicated bucket** (`scope = "cluster"`, or an app's `replicated` one) notifies watches when the write commits on each node, from that node's copy (below), so a watcher on either node of the pair hears of it.
- **Running code on a change** is `handle_info`'s job: the SDK has no callback per key, so an app matches `change.key` (or `change.watch`) to its own handlers.

## A replicated write

```mermaid
sequenceDiagram
    participant I as instance on node 2 (standby)
    participant R2 as node 2 replication task
    participant R1 as node 1 replication task (primary)
    I->>R2: wasi:keyvalue set, through KvStore::run
    R2->>R1: forwards Write
    Note over R1: appends at epoch e (synced)
    R1->>R2: Append
    Note over R2: appends (synced)
    R2->>R1: Ack
    Note over R1: commits: applies to its @cluster copy,<br/>notifies watches
    R1->>R2: Done
    R2->>I: set returns
    R1->>R2: the next Append or heartbeat carries the commit index
    Note over R2: applies to its copy, notifies watches
```

- **What replicates:** `cluster` buckets from node config, apps' buckets declared `replicated` (kept as `<app>/<bucket>` in the `@cluster` namespace), and the reserved records `@handover` and `@release`. Replication runs on a pair with `[failover] replication = true`.
- **`KvStore`** sends a write to a replicated bucket (`Durability::Replicated`) to the replication task and waits for its answer; reads are local, from this node's copy. `get-linearizable` goes to the task too, which serves it on the primary (or forwards it).
- **The replication task** (`replication::Task`) owns the replica, a state machine named after spike S20's actions (`replication::core`), over the log (`replication::log`), which lives in the same redb file as the disk buckets: entries, and the commit index applied with the data in one transaction. It follows the failover leadership, takes frames from the partner (`Frame::Replication`), forwards the standby's writes and linearizable reads to the primary, asks the failover task to set or clear the witness's record (`failover::record_partner`), and ticks each heartbeat interval to resend, send the commit index and compact. It publishes `ReplicationStatus` for the gate and `bulkhead failover`. Its redb work runs under `block_in_place`.

The rules are spike S20's R1 to R7, mapped to the code in `docs/tla/VERIFICATION.md` with the witness's record (R13).

## A release across the pair

Each build speaks its own wire protocol version and the one before. A connection agrees on the highest version both speak (`wire::Hello`, `Transport::version`), and a frame is written only on a connection at or above the version that introduced it (`wire::since`), so the two nodes of a pair can run builds one release apart.

What outlives a connection (the replicated log, the `@cluster` records) follows the pair's version instead (`formats`): it is kept in the reserved replicated bucket `@release` (key `formats`), so both nodes apply a change at the same log index, and it rises only by `Runtime::finalize_formats` (`bulkhead release finalize`), once every node named speaks this build's version. A node refuses to start a build older than the pair's version.

`bulkhead release` (the CLI, over each node's admin API) shows the versions; `bulkhead release upgrade` upgrades an app on the standby, then the primary, gating each step on the app running at the new version there; `release rollback` returns both to their common previous version.

## The node's admin side

- **The admin API** (`node::server`) serves JSON over HTTP on the admin socket, and over TLS with pinned keys on `[admin]`. Long answers stream (`server.rs`, `stream`): `bulkhead events`, `bulkhead logs --follow`, and long polls such as `GET /v1/failover?after=...&wait_ms=...`.
- **The event hub** (`node/events.rs`) merges audit records as they are written, instance exits and restarts (`Runtime::instance_events`), members heard and lost, and failover changes into one stream of JSON lines (`bulkhead events`).
- **The shell** (`cli/shell.rs`, `cli/contacts.rs`) runs `bulkhead` commands against one node at a time, moving between nodes named in a contacts file.

## Threads and tasks

A node runs one tokio multi-threaded runtime, one worker per core by default. Nothing runs guests on a current-thread runtime (CLAUDE.md, spike S1).

| Thread or task | Count | Started by | Ends |
| --- | --- | --- | --- |
| `bulkhead-epoch` OS thread | 1 per runtime | `Runtime::new` | runtime drop |
| Pool memory release | 1 per runtime, once an instance starts (unless `release_idle_after` is unset) | `pool_memory::start_once` | runtime drop |
| tokio workers | 1 per core | `node::run` | process exit |
| blocking pool | on demand | bundle verify and compile, `disk` bucket transactions (redb), secret file reads | per job |
| Instance task (callback loop) | 1 per instance | `Runtime::spawn` | instance exit |
| Health check | 1 per instance of an app with `health` | the instance task | instance exit |
| Native app process (an OS process, not a node thread) | 1 per native instance | the instance task, through `native::port` | instance exit, or the node's (parent-death signal) |
| Native process task: stdout and stderr to log lines | 1 per native instance | `native::port` | process exit |
| Publishing thread (an OS thread): published buckets' snapshots | 1 per node, from the first native instance that reads a published bucket | `services::keyvalue::publish` | the keyvalue store's drop |
| Timer | 1 per pending `process.send-after` | `process` | fires or is cancelled |
| Supervisor | 1 per supervisor, plus 1 per child watching its exit | `supervisor` | supervisor exit |
| App watcher | 1 per app, and 1 per blue-green new side | `apps` | root supervisor exit |
| HTTP accept, connection; HTTP handler instance | several per address; 1 per connection; 1 per request | `http` | stop; connection close; response sent or cut |
| Listener accept, connection instance | several per listener; 1 per connection | `listener` | stop; connection close or idle timeout |
| HTTP egress connections | pooled, per host | `services::egress` | idle timeout |
| Liveness loop | 1, once a node has a peer | `Liveness::add_peer` | runtime drop |
| Serial path | 1 per configured device | `node/serial_path.rs` | stop |
| Failover node loop | 1 on a pair member | `Runtime::start_failover` | runtime drop |
| Replication task; a record request to the failover loop | 1 on a pair member with replication; 1 per request | `Runtime::start_failover` | runtime drop; answer |
| QUIC accept, new-member dialer | 1 each | `QuicTransport` | stop |
| QUIC per connection: 2 writers, 2 readers, close watcher | 5 per peer | connection setup | connection close |
| QUIC redial | 1 per lost peer or seed | connection loss | reconnected or stop |
| Remote call | 1 per incoming `Frame::Call` (capped per peer) | `transport` | reply sent |
| Admin server accept, connection; streams | 1; 1 per connection | `node::server` | stop |
| Event hub | 1 | daemon | stop |
| Metrics endpoint | 1 if configured | daemon | stop |
| Seed join, identity reload, certificate watcher, SIGHUP reload | 1 each if configured | daemon | stop |
| Gate follower | 1 on a pair member | daemon | stop |
| Modbus device, server | 1 per device; 1 per server | `bulkhead-modbus` | stop |

The epoch ticker is an OS thread so that a node whose workers all run spinning guests still advances the epoch, and the guests still yield.

## Where state lives

### In memory, per node (`RuntimeInner`)

| State | Holder | Lock |
| --- | --- | --- |
| Live instances by id | `table::ProcessTable` | std `RwLock` |
| Store slots, and the wish for one | `RuntimeInner::slots`, `slot_wanted` | semaphore; `Notify` |
| Registered names | `registry::Registry` | std `RwLock` |
| Links and monitors (both local sides of remote ones) | `links::LinkTable` | std `Mutex` |
| App pools, each with a possible blue-green side and weight | `RuntimeInner::pools` | std `RwLock` |
| Remote calls awaiting a reply | `transport::PendingCalls` | std `Mutex` |
| Peers and sessions; paths and this node's payload | `transport::liveness::Liveness` | std `Mutex`; std `RwLock` |
| Members and creations | `transport::membership::Membership` | std `Mutex` |
| Failover inbox; the witness's record and its persistence | `failover::Shared` | std `Mutex` each |
| Services' shared state: buckets (memory, and the redb handle), watches, groups, log rate buckets, metric series, secret sources, the egress pool, socket counts per app, SQL pools | `services::Shared` | per service |
| The replica: role, mode, the partner's match index, writes waiting to commit, a snapshot in transfer | `replication::Task` (owned by its task, no lock) | none |
| A moved singleton's snapshot taken for its start | `RuntimeInner::handovers` (`handover::Pending`) | std `Mutex` |
| Failover role for `hot-standby` instances | `RuntimeInner::failover_role` | std `RwLock` |

No std lock is held across an `.await` (`await_holding_lock` is denied).

### Per instance

| State | Where |
| --- | --- |
| Guest memory and globals | the Store (pooling allocator slot); for a native app, its process |
| WASI context, resource table, timers, pending `noreply` callers, readiness waits, the callback clock, service instance states | `state::InstanceState`, the Store's data (kept while passive; held by the task for a native app) |
| A native app's channel: a memfd with a 64 KiB ring each way, the bell and door pipes, the descriptor socket | shared by the node and that app's process only; ends with the process |
| Mailbox | bounded `mpsc` (`limits.mailbox_max`) |
| Signals (exits, timers, watch notices) | unbounded `mpsc` |
| Host ops (upgrade, snapshot, move, announce) | unbounded `mpsc` |
| Run, shutdown or kill | `watch` channel |
| Lifecycle state, exit reason, counters | `InstanceHandle`, shared |

A guest's state lives only in its Store (or its process). A crash or restart starts from `init`; a snapshot upgrade, a passivation and a singleton moved in a planned switchover carry it across. State that must survive anything else goes in a `disk` or replicated bucket.

### In the node daemon (`node::Node`)

| State | Holder |
| --- | --- |
| Desired apps, blue-green sides and deploy info | `NodeState`, behind a `std::sync::Mutex` taken briefly and never across an await; a separate tokio `Mutex`, the change lock, keeps admin changes (deploy, upgrade, stop, start, remove, rollback) to one at a time across their awaits, and reads (`list`, `status`) do not take it |
| Connection apps and their listeners; HTTP handlers | std `Mutex` |
| HTTP route table, TLS certificates | `HttpIngress` |
| Held singletons | `gate::Gate` (std `Mutex`, and a tokio `Mutex` serializing starts and stops) |
| Event subscribers | `node/events.rs` (a broadcast channel) |
| Recent log records | `LogRing` (10,000 records) |

### On disk (`data_dir`)

```
<data_dir>/
├── state.json                 apps, versions, running or stopped, blue-green sides
├── bundles/<app>/<version>.rbp
├── priv/<app>/<version>/...   files the app sees at /priv
├── compiled/<app>/<version>/  compiled components (<checksum>.cwasm) and native programs (.native)
├── kv/                        disk buckets and the replicated log with its replicated buckets (one redb file)
├── tmp/                       uploads being verified
├── creation                   raised on every start; part of every pid
├── failover-epoch             the highest epoch reached; written and synced before acting
├── witness-state              a witness's epoch, promoted node and record; synced before a grant or record change
├── node.lock                  held while a node has the directory open
├── admin.sock                 the admin API, unless admin_socket names another path
└── audit.log                  one JSON object per line
```

Node configuration, the node's cluster key and the peers file live wherever the configuration names them. A compiled component is read back only if it and its directory belong to the node's user and neither is writable by group or others (`bulkhead_host::compiled`), since deserializing runs its code. [OPERATIONS.md](OPERATIONS.md) lists every path under `data_dir`.
