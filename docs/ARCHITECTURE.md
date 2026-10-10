# Paddock: Architecture

2026-10-10 · Draft (ROADMAP R1); W1 finishes it · Companion to [DESIGN.md](DESIGN.md)

A map of the code as built: the crates, the paths a call, a deploy and a failover take through them, the threads and tasks that run, and where state lives. DESIGN.md says why; this file says where. The stage 2 reviews (R2 to R5) work from it.

## Crates

```
                       ┌──────────────────────────────┐
  operator ──CLI──────►│ paddock (bin + lib)          │
  (Unix socket, HTTP)  │  api: admin types, client    │
                       │  node: daemon, store, gate,  │
                       │        audit, log ring       │
                       └──────┬───────────────┬───────┘
                              │               │
              ┌───────────────▼──┐      ┌─────▼────────────┐
              │ paddock-host     │◄─────┤ paddock-bundle   │
              │  runtime, process│      │  manifest, .rbp  │
              │  core, services, │      │  verify, policy, │
              │  supervisors,    │      │  grant (no       │
              │  transport,      │      │  wasmtime)       │
              │  failover, http, │      └──────────────────┘
              │  listeners       │
              └──▲────────────▲──┘
                 │            │ Extension trait
   ┌─────────────┴───┐   ┌────┴───────────────┐
   │ guests (Wasm):  │   │ paddock-modbus,    │
   │ paddock-sdk,    │   │ examples/greeter/  │
   │ -sdk-macros,    │   │ service            │
   │ fixtures,       │   └────────────────────┘
   │ examples        │
   └─────────────────┘
```

| Crate | Builds | What it holds |
| --- | --- | --- |
| `paddock` | binary and library | `api` (admin API types and the Unix-socket client), `node` (the daemon, feature `node`), `cli` (bundle building, output). Without `node` it does not build wasmtime. |
| `paddock-bundle` | library | The app manifest model and validation, `.rbp` opening and signature checks against operator keys, component import inspection, operator policy and the effective grant. |
| `paddock-host` | library | The runtime: engine, epoch ticker, process table, instances, links and monitors, supervisors, app sets and pools, host services, the HTTP ingress, listeners, the cluster transport, liveness, membership and failover. |
| `paddock-modbus` | library | The Modbus TCP extension; depends on `paddock-host` alone. |
| `paddock-sdk`, `paddock-sdk-macros` | guest libraries | The Rust guest SDK: the `App` trait, envelopes, snapshots, service wrappers by feature. The SDK owns the generated bindings. |
| `paddock-fixture-*` | guests | Test components: echo, chaos, counter (three versions), services, sdk, conn. |
| `paddock-soak` | binary | Fault injection against `paddock node` processes (`scripts/soak.sh`). |
| `examples/*` | guests and scripts | SDK examples, the tutorial's board app, the SCADA demo, the greeter extension, the failover demo. |

The WIT contracts live in `wit/`: `paddock:runtime` (the app world, frozen per version) and separate packages for connections, groups, watch, secrets and metrics. `wit/vendor/` holds the WASI interfaces bound against.

Code areas and their interfaces are listed in ROADMAP.md, "Code areas".

## Inside `paddock-host`

| Area | Modules | Role |
| --- | --- | --- |
| Engine | `runtime`, `config`, `bindings`, `deny` | `Runtime` and its `RuntimeInner`: the wasmtime engine with a pooling allocator sized by `max_instances`, the epoch ticker, the linker (WASI p2, `paddock:runtime`, services, extensions), deny stubs for ungranted imports |
| Process core | `pid`, `table`, `registry`, `links`, `message`, `instance`, `state`, `delivery`, `lifecycle`, `output` | Instances, their mailboxes and callback loop, the per-Store host state and `process` functions, pid routing, links and monitors |
| Orchestration | `apps`, `pool`, `supervisor`, `upgrade`, `spec` | App sets in dependency order, a root supervisor per app, pools and routing, snapshot and restart upgrades |
| Services | `services::{config, keyvalue, groups, logging, secrets, metrics, extension}`, `metrics` | Guest-facing interfaces on the `Service` pattern; extensions from other crates |
| I/O | `http` (and `http::tls`), `listener` | The HTTP ingress (a request is a call) and listeners (an instance per connection) |
| Cluster | `transport::{liveness, membership, wire, quic, memory}`, `failover::{node, witness}` | Frames between nodes, heartbeats and sessions, joining, QUIC with pinned keys, the in-memory mesh for tests, the HA pair and witness |
| Observability | `probe`, `metrics` | USDT probes on the message path; Prometheus text |

`delivery` is the one place a pid is routed: a live local instance, a dead local pid (`noproc`), or another node (a frame on the transport, or `noconnection`). Process-core code goes through it, never the process table directly.

## A call

### From the operator

```
paddock call app body
  │  HTTP over the admin Unix socket: POST /v1/apps/<app>/call
  ▼
node::server ──► Node::send ──► resolve(target)
  │                 app  → the app's pool picks an instance (round robin, or hash of a key)
  │                 pid  → the process table
  │                 name → the registry
  ▼
InstanceHandle::call ──► size check ──► mailbox try_send(Call{msg, oneshot})
  │                                       full mailbox → error at once
  ▼
instance task (serve loop)
  │  takes, in order: control changes, host ops (upgrade, snapshot),
  │  signals (exits, timers, watch notices), then the mailbox
  │  starts the callback clock (callback_deadline_ms counts guest time only)
  ▼
guest handle-call in the Store ──► reply, or noreply and process.reply later
  ▼
oneshot ──► Node::send ──► HTTP response ──► CLI
```

The caller waits with its timeout; a reply after the timeout is dropped. A guest that exceeds its deadline traps at an epoch tick, and the instance exits with a reason its supervisor acts on.

### Between instances

A guest calls `process.call`. The host function (`state`) hands it to `delivery::call`, and the time the caller waits counts as host time, not against its deadline.

- **Local:** the target's mailbox, as above.
- **Remote:** a `Frame::Call` with an id goes to the target's node over the transport, and the caller waits on `pending_calls`. The receiving node runs `delivery::call` in a task of its own and answers with `Frame::Reply`. A session that ends meanwhile fails the call with `noconnection`.

Casts follow the same routes without a reply; a full mailbox drops the cast and counts it.

### From HTTP

`http` accepts on several tasks per address, matches a route by host and longest path prefix, reads the body up to the route's limit, and calls the owning app's pool with a CBOR request envelope. The reply envelope becomes the HTTP response. Listeners (`listener`) are different: each accepted connection gets a new instance of a connection-handler component, which reads and writes the socket through `wasi:io` streams and has no mailbox.

## A deploy

```
paddock deploy app.rbp
  │  POST /v1/apps (bundle bytes)
  ▼
Node (holds the NodeState lock for the whole deploy)
  ├─ write the upload to <data_dir>/tmp
  ├─ blocking pool: VerifiedBundle::open
  │     signature against trusted_keys, checksums, manifest, component world
  ├─ effective grant: manifest ∩ operator policy
  ├─ blocking pool: Runtime::load
  │     compile (Cranelift), inspect imports, link deny stubs
  ├─ InstanceSpec from the manifest, narrowed by the grant
  ├─ store: extract priv/, keep bundles/<app>/<version>.rbp
  ├─ run:
  │     HTTP routes, if granted
  │     singleton on a failover node → gate.hold (starts only while may_run)
  │     otherwise                    → Apps::add
  │        dependency check, root supervisor, pool of instances;
  │        each instance: instantiate in a new Store, init, then serve
  ├─ state.json saved
  └─ audit.log record
```

On restart a node re-verifies every stored bundle and starts the apps `state.json` marks running, in deploy order.

An upgrade takes the same verify and compile steps, then either swaps each instance in place (save-snapshot in the old Store, a new Store from the new component, load-snapshot; a failure reverts that instance with its state intact) or restarts the instances from `init`. Rollback goes to the previous version the same way.

## A failover

```
          node 1 (primary)              witness (node 3)             node 2 (standby)
 liveness  beats: standing, epoch  ───────────────────────►  beats: "standby, epoch e,
 loop      ◄───────────────────────────────────────────────  confirming"  (renews 1's lease)
           renews at the witness ─────►  record renewal
 failover  lease end − margin:                                hears 1 on no path:
 node::run   may_run = false                                  stops confirming, asks ──►
                                       waits lease + margin
                                       since 1's last renewal
                                       writes witness-state
                                       grants ────────────────►  promotes: epoch e+1
 gate      kills singletons                                      gate::follow:
                                                                   writes failover-epoch
                                                                   then starts singletons
```

- **Liveness** (`transport::liveness`) beats every peer each heartbeat interval, on the cluster link and any extra path, with a payload failover sets (standing, epoch, whether it confirms). A path silent for `node_timeout` raises an event; a peer that stops being reachable ends its session.
- **The failover task** (`failover::node::run`) is a state machine, one type per standing, stepping on node events, witness messages, each heartbeat tick, and the moment its lease would end. It publishes a `Leadership` (standing, epoch, `may_run`) on a watch channel. Its transitions are named after the actions in `docs/tla/FailoverL2.tla`.
- **The witness** (`failover::witness`) answers one request at a time under a lock, and writes its epoch and promoted node to disk before a grant leaves.
- **The gate** (`node::gate`) follows the leadership on the node: it writes the epoch to `failover-epoch` before acting on it, starts the held singletons from `init` when `may_run` is set, and kills them without `terminate` when it drops. It logs `singleton starting` and `singleton killed`, which the soak's checker reads.

The rules are R1 to R11 in `docs/tla/VERIFICATION.md`; the assumptions the model makes, and which the code does not meet (A8: process pauses), are listed there.

## Threads and tasks

A node runs one tokio multi-threaded runtime, one worker per core by default. Nothing runs guests on a current-thread runtime (CLAUDE.md, spike S1).

| Thread or task | Count | Started by | Ends |
| --- | --- | --- | --- |
| `paddock-epoch` OS thread | 1 per runtime | `Runtime::new` | runtime drop |
| tokio workers | 1 per core | `node::run` | process exit |
| blocking pool | on demand | bundle verify and compile, `disk` bucket transactions (redb), secret file reads | per job |
| Instance task (callback loop) | 1 per instance | `Runtime::spawn` | instance exit |
| Timer | 1 per pending `process.send-after` | `state` | fires or is cancelled |
| Supervisor | 1 per supervisor, plus 1 per child watching its exit | `supervisor` | supervisor exit |
| App watcher | 1 per app | `apps` | root supervisor exit |
| HTTP accept, connection | several per address; 1 per connection | `http` | stop; connection close |
| Listener accept, connection instance | several per listener; 1 per connection | `listener` | stop; connection close or idle timeout |
| Liveness loop | 1, once a node has a peer | `Liveness::add_peer` | runtime drop |
| Failover node loop | 1 on a pair member | `Runtime::start_failover` | runtime drop |
| QUIC accept, new-member dialer | 1 each | `QuicTransport` | stop |
| QUIC per connection: 2 writers, 2 readers, close watcher | 5 per peer | connection setup | connection close |
| QUIC redial | 1 per lost peer or seed | connection loss | reconnected or stop |
| Remote call | 1 per incoming `Frame::Call` | `transport` | reply sent |
| Admin server accept, connection | 1; 1 per connection | `node::server` | stop |
| Metrics endpoint | 1 if configured | daemon | stop |
| Seed join, identity reload, certificate watcher, SIGHUP reload | 1 each if configured | daemon | stop |
| Gate follower | 1 on a pair member | daemon | stop |
| Modbus device, server | 1 per device; 1 per server | `paddock-modbus` | stop |

The epoch ticker is an OS thread so that a node whose workers all run spinning guests still advances the epoch, and the guests still yield.

## Where state lives

### In memory, per node (`RuntimeInner`)

| State | Holder | Lock |
| --- | --- | --- |
| Live instances by id | `table::ProcessTable` | std `RwLock` |
| Registered names | `registry::Registry` | std `RwLock` |
| Links and monitors (both local sides of remote ones) | `links::LinkTable` | std `Mutex` |
| App pools | `RuntimeInner::pools` | std `RwLock` |
| Remote calls awaiting a reply | `transport::PendingCalls` | std `Mutex` |
| Peers and sessions; paths and this node's payload | `transport::liveness::Liveness` | std `Mutex`; std `RwLock` |
| Members and creations | `transport::membership::Membership` | std `Mutex` |
| Failover inbox; the witness's record and its persistence | `failover::Shared` | std `Mutex` each |
| Services' shared state: buckets (memory, and the redb handle), watches, groups, log rate buckets, metric series, secret sources | `services::Shared` | per service |
| Failover role for `hot-standby` instances | `RuntimeInner::failover_role` | std `RwLock` |

No std lock is held across an `.await` (`await_holding_lock` is denied).

### Per instance

| State | Where |
| --- | --- |
| Guest memory and globals | the Store (pooling allocator slot) |
| WASI context, resource table, timers, pending `noreply` callers, the callback clock, service instance states | `state::InstanceState`, the Store's data |
| Mailbox | bounded `mpsc` (`limits.mailbox_max`) |
| Signals (exits, timers, watch notices) | unbounded `mpsc` |
| Host ops (upgrade, snapshot, announce) | unbounded `mpsc` |
| Run, shutdown or kill | `watch` channel |
| Lifecycle state, exit reason, counters | `InstanceHandle`, shared |

A guest's state lives only in its Store. A crash or restart starts from `init`; only a snapshot upgrade carries it across, and only within one node. State that must survive goes in a `disk` bucket.

### In the node daemon (`node::Node`)

| State | Holder |
| --- | --- |
| Desired apps and deploy info | `NodeState`, behind a tokio `Mutex`: one admin change (deploy, upgrade, stop, start, remove, rollback) at a time, held across its awaits |
| Connection apps and their listeners | std `Mutex` |
| HTTP route table, TLS certificates | `HttpIngress` |
| Held singletons | `gate::Gate` (std `Mutex`, and a tokio `Mutex` serializing starts and stops) |
| Recent log records | `LogRing` (10,000 records) |

### On disk (`data_dir`)

```
<data_dir>/
├── state.json                 apps, versions, running or stopped
├── bundles/<app>/<version>.rbp
├── priv/<app>/<version>/...   files the app sees at /priv
├── kv/                        disk buckets (one redb file)
├── tmp/                       uploads being verified
├── failover-epoch             the highest epoch reached; written and synced before acting
├── witness-state              a witness's epoch and promoted node; synced before a grant
├── node.lock                  held while a node has the directory open
└── audit.log                  one JSON object per line
```

Node configuration, the node's cluster key and the peers file live wherever the configuration names them. Compiled components are not cached on disk; a restart compiles again (ROADMAP G4, with Q2).

## Open questions for the reviews

Found while drafting; each review decides.

- **R2:** the daemon's `NodeState` lock is held across bundle verification and compilation, and `list` and `status` take it too, so a slow deploy or upgrade also stalls read-only admin calls (and the soak's checks, which call `status`). One change at a time is intended; reads waiting behind it may not be.
- **R3:** an incoming `Frame::Call` spawns a task with no bound on how many run at once; a peer can start as many as it sends frames.
- **R3:** signals and host-op queues are unbounded per instance; watches conflate per key, but exit signals from many linked instances do not.
- **R5:** `daemon.rs` is 1,860 lines and holds deploy, upgrade, rollback, connections, cluster start and failover start; it is the first candidate for splitting.
- **R4 (done):** the gate is outside `failover/` and outside the model: it writes the epoch, then starts or kills singletons. The model's `may_run` is the leadership's; the window between them (and Q8's pause window) is not modelled. The review's results are in `docs/tla/VERIFICATION.md`, "Conformance with the code".
