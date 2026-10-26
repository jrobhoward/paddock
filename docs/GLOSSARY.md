# Glossary

Terms used across Paddock's docs and code, grouped by area. Each entry says what the word means in Paddock and points to the file that has the detail. Where Paddock uses an Erlang/OTP word, the meaning is close to OTP's unless the entry says otherwise.

Two words have more than one meaning:

- **Epoch:** the wasmtime epoch, which the epoch ticker advances (see "Epoch tick"), or a failover epoch, the HA pair's fencing token (see "Failover epoch"). They are unrelated. The docs write "epoch tick" or "epoch check" for the first and "failover epoch" for the second.
- **Snapshot:** the bytes an app's `save-snapshot` writes (its state), or a copy of the replicated log's buckets sent to a partner that has fallen too far behind. The second is called a log snapshot.
- **S and R numbers:** S1 to S23 are spikes, and S1 to S9 also the security findings in [SECURITY.md](SECURITY.md); R1 to R14 are the model-checked rules, and R1 to R5 also the 2026-10-10 review items in ROADMAP.md. The docs name the kind ("spike S8", "finding S4", "rule R6") where it is not clear from the file.

## Apps and instances

- **App:** a deployed WebAssembly component (or a native program) with its manifest, run as one or more instances. Named by the manifest, versioned by semver.
- **Bundle:** an `.rbp` file (rustbridge's bundle format) holding an app's component, manifest (the `appserver` section), checksums and signature. A node opens only bundles signed by a key in its `trusted_keys`. [MANIFEST.md](MANIFEST.md).
- **Manifest:** the app's declaration of what it needs: capabilities, limits, instances, placement, upgrade mode, routes. [MANIFEST.md](MANIFEST.md).
- **Component:** a WebAssembly component (WASI 0.2) exporting the `paddock:runtime/app` world, or the `paddock:connection` or `wasi:http/proxy` world for listeners and HTTP handlers.
- **Guest:** code running inside a component instance, as opposed to the host. "Guest time" is time spent running that code.
- **Host:** `paddock-host`, the runtime that runs guests and implements their imports. "Host time" is time a callback spends waiting in a host function a guest called; it does not count against the callback deadline. Work a host function does without waiting counts with the guest's time.
- **Instance:** one running copy of an app, with its own pid, mailbox, memory and state; Erlang's process. A component instance runs in a wasmtime Store; a native instance is an OS process.
- **Native-process app:** an app shipped as a native program (a rustbridge plugin or an executable on `paddock-native`'s app API) rather than a component. Each instance is a process started through `paddock-runner`, which sandboxes itself first. No snapshots, so no passivation or snapshot upgrades. [ARCHITECTURE.md](ARCHITECTURE.md), "Native-process apps".
- **Pid:** an instance's address, written `node.serial.incarnation@creation`. The serial stays across restarts; the incarnation grows by one on each restart. [OBSERVABILITY.md](OBSERVABILITY.md).
- **Incarnation:** the part of a pid counting restarts of one instance, so a message for a crashed incarnation never reaches its replacement.
- **Pool:** an app's set of instances behind one name. A message to the app picks one, round robin or by a hash of a routing key. During a blue-green upgrade a pool has two sides and a weight.
- **Routing key:** the field of a message body a hash-routed pool hashes to choose an instance, so messages with the same key reach the same instance.
- **App set:** the apps on a node, started in dependency order.
- **Placement:** where an app's instances run in a cluster: `any`, `singleton` (one instance in the cluster; on an HA pair, on the primary only), `all-nodes`, or `hot-standby` (on both pair nodes, each told its role). [MANIFEST.md](MANIFEST.md).
- **Singleton:** an app with `placement.mode = "singleton"`.
- **Extension:** a host service from a crate outside `paddock-host` (`paddock-modbus`, the greeter example), linked into a node binary.
- **Hot standby:** an app with `placement.mode = "hot-standby"`: an instance on both nodes of an HA pair, each told its role (`failover-role` host event), for an app that keeps a warm copy and takes over itself.
- **Priv files:** files bundled with an app (`paddock bundle --priv`), read-only at `/priv` in a component, or in the directory `PADDOCK_PRIV` names for a native app.

## Messages

- **Envelope:** a message: content type, an optional schema id and a body of bytes. Calls, casts, replies, timer messages and HTTP requests are all envelopes.
- **Call:** a message that waits for a reply, with a timeout; OTP's `gen_server:call`. Handled by `handle-call`.
- **Cast:** a message with no reply; OTP's `gen_server:cast`. Handled by `handle-cast`. A cast to a full mailbox is dropped and counted.
- **Info:** anything else an instance is told, handled by `handle-info`: a link's `exit`, a monitor's `down`, a `timer` message, or a `host-event`.
- **Host event:** an info with a name and an envelope, sent by the host: `log-level`, `failover-role`, `io-ready`, watch notices. Guests ignore names they do not know, so new events need no new world version.
- **Signal:** exits, timer messages and watch notices, which reach an instance ahead of its mailbox, on a queue of their own.
- **Mailbox:** an instance's bounded queue of calls and casts (`limits.mailbox_max`). Full, it refuses a call with `overloaded` and drops a cast.
- **Noreply:** a callback's answer meaning "the reply comes later": the guest keeps the call ref and answers with `process.reply`.
- **Send-after:** `process.send-after`, a timer that puts a message in the instance's own mailbox after a delay; Erlang's `erlang:send_after`. Each is a tokio sleep, so it fires on a 1 ms grain; how late it is handled is measured in [BASELINE.md](BASELINE.md) (`timer_cost`).
- **Batch:** many items sent as one message through `paddock_sdk::batch`, flushed by count, bytes or delay.
- **Group:** a named set of instances a message can be published to (`paddock:groups`); OTP's `pg`.

## Lifecycle and supervision

- **Callback:** one call from the host into a guest export: `init`, `handle-call`, `handle-cast`, `handle-info`, `terminate`, `save-snapshot`, `load-snapshot`. An instance runs one callback at a time.
- **Callback deadline:** `limits.callback_deadline_ms`, the most guest time one callback may take. Past it, the instance is killed with reason `timeout`, at the next epoch tick.
- **Exit reason:** why an instance ended: `normal`, `shutdown`, `killed`, `crashed: <text>` (a trap, `timeout`, `memory_limit`, `unhealthy`, `lease_ended` and others), or `noproc`.
- **Link:** a two-way tie between instances: when one exits abnormally, the other exits too, unless it traps exits and gets an `exit` info instead.
- **Monitor:** a one-way watch on an instance: the watcher gets a `down` info when it exits.
- **Trap exits:** an instance that traps exits turns exit signals from links into `exit` infos.
- **Supervisor:** a host task that starts children and restarts them by its strategy (`one_for_one`, `one_for_all`, `rest_for_one`, `simple_one_for_one`), within `intensity` restarts per `period`, after a backoff. Each app has a root supervisor.
- **Health check:** a call the host makes to an instance on a timer; `failure_threshold` failures in a row kill it as `crashed: unhealthy`.
- **Passivation:** an idle instance saving its state with `save-snapshot` and giving up its Store, keeping its pid, mailbox, links, monitors, names and timers. The next message wakes it with `load-snapshot`. A passivated instance's snapshot is held in host memory. [ARCHITECTURE.md](ARCHITECTURE.md), "Passivation".
- **Upgrade modes:** `restart` (stop, then start the new version from `init`), `snapshot` (each instance in place: `save-snapshot` in the old version, `load-snapshot` in the new, keeping pid and mailbox) and `blue-green` (the new version beside the old, traffic moved by weight). [MANIFEST.md](MANIFEST.md).
- **Weight:** during a blue-green upgrade, the percentage of messages a pool sends to the new side.
- **Rollback:** going back to the previous version, in the same mode as an upgrade.

## Runtime and engine

- **Store:** wasmtime's container for one component instance: its linear memory, globals, tables and host state. A Store that trapped or whose call was dropped cannot be entered again, so a crashed instance is rebuilt from `init`.
- **Store slot:** one of the `max_instances` places in the pooling allocator. Each live Store holds one; a passivated instance holds none. `max_processes` bounds instances, live and passivated.
- **Pooling allocator:** wasmtime's allocator that reserves address space for every slot when the runtime starts, so instantiating needs no `mmap`. The reservation is virtual, not resident: about 8 GiB per slot on the standard memory profile and 256 MiB on dense.
- **Memory profile:** `standard` (up to 4 GiB of linear memory per instance, with guard regions large enough that wasmtime compiles no bounds checks) or `dense` (up to 128 MiB, with small guards, so bounds checks are compiled in; less address space per slot, for nodes with tens of thousands of instances).
- **Linear memory:** a guest's heap: one byte array it can grow but never shrink. `ps` shows its size, which is the instance's high-water mark.
- **Epoch tick:** the moment the host's epoch ticker (an OS thread, `paddock-epoch`) advances wasmtime's epoch counter, every `epoch_tick_ms` (2 ms by default; 10 ms before 2026-10-07). Compiled guest code checks the counter at every function entry and loop back edge (the epoch check). At each tick, a running guest yields its tokio worker so other instances run, and a guest past its callback deadline traps. The tick bounds how long a busy instance holds a worker. Measured cost: [BASELINE.md](BASELINE.md), "Epoch interruption".
- **Epoch check:** the load, compare and branch that wasmtime compiles into guest code for epoch interruption.
- **Call hook:** wasmtime's hook on every entry to and return from a host function. Paddock does not install one: the callback clock times the task's polls of a callback instead (2026-10-24; the hook cost about 5% of an empty call).
- **Callback clock:** the guest time of the callback running now: the time the instance's task spends polling it, which stops while a host function waits. The epoch callback compares it with the callback deadline.
- **Compiled-code cache:** compiled components kept on disk (`compiled/` under the data directory), so a restarted node deserializes them instead of compiling again. Read only if the file and its directory belong to the node's user and neither is writable by group or others.
- **Deny stub:** a function that stands in for an import the operator did not grant and returns `access-denied`, so a component with an ungranted import still links.

## Capabilities and services

- **Capability:** a host interface an app declares under `capabilities` in its manifest, with the scope that narrows it (`wasi:keyvalue` with its buckets, `wasi:http/outgoing-handler` with its `allow` list, `paddock:sql` with its datasources). [MANIFEST.md](MANIFEST.md#capabilities-at-a-glance).
- **Policy:** the operator's file of what apps may be granted (`policy` in node config names it): defaults, per app and per signer, plus the shared buckets, secrets and datasources each may use.
- **Signer:** a minisign key whose bundles a node runs (`trusted_keys`); policy can grant by signer.
- **Grant:** the capabilities an instance gets: what the manifest asks for, narrowed by policy (the effective grant). Imports outside it get deny stubs.
- **Host service:** an interface the host implements for guests (config, keyvalue, groups, logging, secrets, metrics, egress, sockets, SQL), each on the `Service` pattern. [DESIGN.md](DESIGN.md), "Host services".
- **Ingress:** the node's HTTP server: a request to a route becomes a call to the owning app's pool.
- **HTTP handler:** a `wasi:http/proxy` component given a new instance per request, with no mailbox.
- **Listener, connection handler:** a TCP port whose every accepted connection gets a new instance of a `paddock:connection` component.
- **Egress:** outgoing HTTP from a guest through `wasi:http`, filtered by the manifest's `allow` and node config's ceiling, through the node's connection pool.
- **Volume:** a host directory node config names (`[volumes]`), mapped into a guest path by a manifest preopen, read-only or read-write.
- **Datasource:** a Postgres database node config binds (`[datasources.<name>]`), pooled, that granted apps query through `paddock:sql` by name.

## Storage

- **Bucket:** a key-value namespace through `wasi:keyvalue`, an app's own or shared from node config's `[buckets]`. Durability: `memory` (survives instance crashes, not node restarts), `disk` (redb, synced) or `replicated` (kept whole on both nodes of an HA pair, through the replicated log). A shared bucket with `scope = "cluster"` is replicated the same way. [MANIFEST.md](MANIFEST.md).
- **Watch:** a subscription to changes of a key or prefix in a bucket (`paddock:watch`); each change is delivered as a host event. Notices for a watcher that falls behind are conflated.
- **Replicated log:** the HA pair's log of writes to replicated and cluster buckets, in the same redb file as the disk buckets. A write is acknowledged once both nodes have synced it, or, with the standby gone, once the witness has recorded it out of date.
- **Group commit:** writers arriving together sharing one synced append.
- **Reserved buckets:** `@cluster` (the replicated buckets' namespace), `@handover` (snapshots of singletons moved in a switchover) and `@release` (the pair's formats version).

## Cluster and failover

- **Node:** one `paddock node` process with its data directory and node id.
- **Member:** a node that has joined the cluster, with its id, creation, address and role (`member`, or `witness`).
- **Creation:** a node's start count, kept in its data directory and raised on every start. It is part of every pid and every packet, so pids and frames from an earlier start are never mistaken for current ones; Erlang's `creation`.
- **Frame:** one message between nodes (a call, cast, reply, link, monitor, exit or down notice, heartbeat, replication message and so on), in the versioned CBOR wire format.
- **Packet:** a frame with the sender's creation, the receiver's creation as the sender knows it, and the session number.
- **Session:** the span during which two nodes consider each other up. A node-down ends it on both sides; every frame carries its session number, so frames from an ended session are discarded.
- **Lane:** one direction of a link to another node, with a byte budget for messages and a cap on queued frames. A congested lane drops casts and fails calls with `overloaded`, as a full mailbox does.
- **Liveness:** heartbeats between every pair of nodes, every `heartbeat_interval` on each path; a path silent for `node_timeout` is down.
- **Heartbeat path:** a way heartbeats travel: the cluster link, and for an HA pair, optionally a serial line.
- **HA pair:** two nodes with `[failover]`, one primary and one standby, plus a witness.
- **Witness:** a third node that runs no apps and decides which pair node may promote, when the two cannot hear each other.
- **Standing:** a pair node's state in failover: `Primary`, `Standby` or `Down`.
- **Lease:** the time a primary may run singletons without hearing from its partner or the witness, renewed each heartbeat through either. The witness grants a promotion only after the old primary's lease has ended.
- **Failover epoch:** a counter raised on every promotion and written to disk (`failover-epoch`) before it is acted on. A node at a lower failover epoch is fenced; it is the pair's fencing token. Not the wasmtime epoch.
- **may_run:** the failover task's answer to whether this node may run singletons now. The gate follows it.
- **Gate:** the node component that starts singletons when `may_run` is set and kills them when it drops.
- **Promotion, switchover, override, bootstrap:** a standby becoming primary on the witness's grant; a planned handover from primary to standby; a promotion on an operator's confirmation when the witness cannot be reached; the first start of a pair, where the lower node id leads.
- **Handover:** a singleton moved with its state in a switchover, through `@handover`.
- **Out of date:** the witness's record that the standby lacks replicated writes the primary committed alone. While it stands, the witness grants that node no promotion, and a switchover to it is refused.
- **Pair version (formats):** the version of what outlives a connection (the log, the `@cluster` records). It rises only by `paddock release finalize`.
- **Release:** `paddock release`, an app upgraded on both nodes of a pair, standby first, each step gated on the app running at the new version; also the build-upgrade sequence that ends in `release finalize`. [OPERATIONS.md](OPERATIONS.md).

## Operations and project terms

- **Admin API:** the node's JSON-over-HTTP API on its Unix socket (and over TLS with pinned keys), used by the `paddock` CLI.
- **Operator:** whoever can use the admin API: the node's user on the socket, or an operator whose key the `[admin]` operators file lists, with the same rights.
- **Contacts file:** the operator's list of nodes for `paddock shell` (`connect <name>`): each node's admin address and pinned key.
- **Probe:** a USDT probe on the message path, for bpftrace and similar tracers. [OBSERVABILITY.md](OBSERVABILITY.md).
- **Soak:** hours of fault injection against `paddock node` processes (`scripts/soak.sh`).
- **Baseline:** the recorded benchmark figures in [BASELINE.md](BASELINE.md), from `scripts/bench.sh`.
- **Spike (S1 to S23):** a throwaway program that answered one design question, under `spikes/`; its README has the answer.
- **R1 to R14:** the failover and replication rules model-checked in TLA+. [tla/VERIFICATION.md](tla/VERIFICATION.md).
