# Writing an app

A Bulkhead app implements a `gen_server`-style set of callbacks. In Rust, `bulkhead-sdk` provides the `App` trait and the `app!` macro, and the app is a native program, run as one restricted process per instance (Linux; "Native-process apps" below). An existing rustbridge plugin runs too.

## The model

Like an Erlang `gen_server`:

- The host keeps each instance's mailbox and hands it **one message at a time**. Callbacks never run concurrently within an instance, so state needs no locks.
- **Calls** expect a reply and the caller waits, up to a timeout. **Casts** are fire and forget. **Infos** are notifications from the host: link exits, monitor downs, timers and named host events.
- Each callback has a **deadline** (`limits.callback_deadline_ms`), in wall time. Overrunning it kills the instance with reason `timeout`. Time spent waiting for the node, such as a call to another instance, does not count.
- A panic, an abort, an out-of-memory error or a deadline kill ends the instance. Its supervisor restarts it from `init`, with fresh memory. State that must outlive a crash belongs in a keyvalue bucket or a file.
- Instances share nothing. They talk only through messages, by **pid** or by **registered name**.

## A minimal app

`Cargo.toml`:

```toml
[package]
name = "greeter"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
bulkhead-sdk = { path = "../bulkhead/crates/bulkhead-sdk" }
serde = { version = "1", features = ["derive"] }
```

`src/lib.rs`:

```rust
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

struct Greeter {
    greeted: u64,
}

impl App for Greeter {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self { greeted: 0 })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        self.greeted += 1;
        let name = msg.as_text().unwrap_or("stranger");
        Reply::Now(Envelope::text(format!("hello, {name} (#{})", self.greeted)))
    }
}

bulkhead_sdk::app!(Greeter);
```

`bulkhead.toml`:

```toml
[app]
name = "greeter"
version = "0.1.0"

[appserver.limits]
memory_max = "16MiB"
```

Build, bundle and deploy:

```sh
cargo build --release
bulkhead bundle bulkhead.toml target/release/greeter --key signer.key
bulkhead deploy greeter-0.1.0.rbp
bulkhead call greeter world          # hello, world (#1)
```

## Callbacks

| Callback | Required | Called |
| --- | --- | --- |
| `init(ctx, args)` | yes | Once per start, with `lifecycle.start_args` as JSON bytes (empty if unset). `Err` fails the start, and the supervisor counts it. |
| `handle_call(&mut self, ctx, call, msg)` | yes | For each call. Return `Reply::Now(envelope)`, or `Reply::Later` and answer later with `ctx.reply(call, envelope)`, possibly from another callback. |
| `handle_cast(&mut self, ctx, msg)` | no | For each cast. |
| `handle_info(&mut self, ctx, info)` | no | For `Info::Exit`, `Info::Down`, `Info::Timer` and `Info::Event`. Ignore event names you don't know; later hosts add more. |
| `terminate(&mut self, ctx, reason)` | no | Before a graceful stop, within `lifecycle.shutdown_timeout_ms`. Not called on kills or crashes. |
| `save_snapshot(&self)` / `load_snapshot(ctx, from_version, bytes)` | no | During a `snapshot`-mode upgrade; see below. The defaults refuse, so such an app upgrades in `restart` mode only. |

## Messages

An `Envelope` is a content type, an optional schema id, and a body of bytes. The host does not look inside, except to read a hash-routing key.

```rust
Envelope::text("ping");                          // text/plain
Envelope::json(&order)?;                         // application/json, via serde
Envelope::cbor(&order)?;                         // application/cbor, via serde
msg.as_text();                                   // Option<&str>
let order: Order = msg.decode()?;                // by its content type: JSON or CBOR
```

Bodies over `limits.message_max_bytes` are refused with `too-large`. Messages are copied into and out of each instance. That costs about one `memcpy` per direction, which is negligible next to the roughly 2 µs per call ([BASELINE.md](BASELINE.md)).

## Talking to other instances

Everything goes through `Ctx`:

```rust
let pong = ctx.whereis("pong").ok_or("pong is not running")?;
let reply = ctx.call(pong, Envelope::text("ping"), Duration::from_secs(1))?;   // Result<Envelope, CallError>
ctx.cast(pong, Envelope::text("hello"));
ctx.register("orders")?;          // the name must be listed in registered_names
let me = ctx.pid();
```

A `CallError` is one of `Noproc` (dead pid or unknown name), `Noconnection` (the target's node was lost, or cannot be reached), `Timeout`, `Overloaded` (the target's mailbox is full), `TooLarge`, `AccessDenied` (refused by `messaging_acl`) or `Exited(reason)` (the target died while handling the call). A call to an instance's own pid ends it, because it could never be answered.

A blocked `ctx.call` holds up only this instance. The host suspends it without tying up a thread.

### Casts that must arrive

A `cast` is delivered at most once, and its sender is not told when it is dropped: at a full mailbox (`limits.mailbox_max` messages or `limits.mailbox_max_bytes` of bodies), at a dead pid, over `message_max_bytes`, refused by `messaging_acl`, or at a congested link to another node. An app has two more sends:

```rust
ctx.try_cast(to, msg)?;                                  // Result<(), CastError>: queued, or why not
ctx.cast_timeout(to, msg, Duration::from_millis(500))?;  // waits up to 500 ms for room in a full mailbox
```

| Send | What the sender learns | For |
| --- | --- | --- |
| `cast` | Nothing | Updates the next one replaces: readings, heartbeats, telemetry |
| `try_cast` | Whether the message was queued: in the target's mailbox, or, to another node, on the link to it | Shedding load: drop, count or slow down when a target falls behind |
| `cast_timeout` | That the message is in the target's mailbox, on any node; on credit, that room was reserved for it there and it is on the way | Messages that must not be lost to a full mailbox |
| `call` | That the target handled it, and its reply | Work whose result matters, or a receipt |

A `CastError` is one of `Noproc`, `Noconnection`, `Overloaded` (a full mailbox, or the link to the target's node over its budget), `TooLarge`, `AccessDenied` or `Timeout` (`cast_timeout` only: no room came in time, and the message was not queued).

- **Accepted is not handled.** A message in a mailbox is lost if the target crashes before taking it, as in Erlang. Data that must survive that goes by `call`, whose reply is the receipt, or into a bucket.
- **The wait:** `cast_timeout` waits for the body's bytes and then a slot, in arrival order with other waiters, up to the timeout. The node caps the timeout at the sender's callback deadline, and the wait does not count against it. The sender handles nothing else meanwhile. A cast to its own pid never waits, since the sender cannot empty its mailbox while waiting. Two instances waiting on each other's full mailboxes are freed when the first times out.
- **Between nodes** the other node waits and answers, and the sender waits 250 ms longer, so `Timeout` normally means the message was not queued. Only an answer later than that leaves it unknown, and a retry may then deliver it twice. `try_cast`'s `Ok` to another node means only that the link took the message; that node's full mailbox may still drop it.
- **Credit between nodes:** once a waited cast from an instance to a target on another node is queued, that node reserves room in the target's mailbox for the sender (`[cluster] cast_credit`, 64 casts and 1 MiB by default), and the sender's next waited casts to that target return `Ok` at once, until the room is used up; more is granted as they arrive. Such an `Ok` means room was reserved and the message is on the link, which keeps its order: it is lost if the session between the nodes ends before it arrives, if the target exits first (as the messages in its mailbox are), or if the target's messaging ACL changed since and refuses it. Credit lapses after a second unused. A node one release older grants none, and each waited cast to it waits for its answer.
- **Costs** (16 senders, [BASELINE.md](BASELINE.md), 2026-11-05): `try_cast` costs what `cast` does. `cast_timeout` moves 26 to 47% fewer casts per second within a node. Between nodes, without credit, it costs a round trip per cast: about 30,000 a second for 16 senders between two processes or two machines; with credit, 146,000 to 149,000, two thirds to three quarters of `cast`'s rate, when the targets keep up ([BASELINE.md](BASELINE.md), 2026-11-10). Where many small messages must all arrive, batch them (below): a batch is one waited cast.

## Links, monitors and exits

```rust
ctx.link(other);                  // if either exits abnormally, the other is killed...
ctx.trap_exits(true);             // ...unless it traps exits: then it gets Info::Exit instead
let m = ctx.monitor(other);       // one-way: Info::Down { monitor: m, pid, reason } when `other` exits
ctx.demonitor(m);
ctx.exit(other, &ExitReason::Shutdown);
```

Exit reasons: `Normal`, `Shutdown`, `Killed`, `Noproc`, `Noconnection`, `Timeout` (the callback deadline), and `Crashed(text)`. The text is the panic message, or one of the host's: `memory_limit`, `init_timeout`, `unhealthy` (failed health checks), `lease_ended` (a singleton whose node lost its lease), `killed_at_lease_end` (a singleton `bulkhead-runner` killed at its lease's end while its node did not run), or a host-object limit (`timer_limit`, `monitor_limit`, `link_limit`, `pending_call_limit`).

The supervisor restarts an app's instances according to `supervision` (strategy, intensity, backoff). It follows OTP: too many restarts within `period_ms` and it gives up. The app is then marked `failed`, with the reason in `bulkhead status`.

## Timers

```rust
let t = ctx.send_after(Duration::from_secs(30), Envelope::text("tick"));
ctx.cancel_timer(t);
// later: handle_info(Info::Timer { timer, msg })
```

## Health checks

With a `health` section in the manifest, the host asks each instance every `interval_ms` whether it is healthy, and kills it as `crashed: unhealthy` after `failure_threshold` failures in a row; its supervisor restarts it. Implement `App::health` to say so (healthy by default):

```rust
fn health(&mut self, _ctx: &mut Ctx) -> Result<(), String> {
    if self.device_seen.elapsed() > Duration::from_secs(30) {
        return Err("no data from the device for 30 s".into());
    }
    Ok(())
}
```

The check is a call through the mailbox, so an instance too busy to answer within the interval fails it too.

## Many small updates

A cast costs about the same whatever it carries, so an app that sends many small updates (points, events, rows) to the same instance should batch them. Between two instances on one node, one cast per point moved about 310,000 to 350,000 points per second; batches of 100 moved 0.94 to 1.5 million (1 and 4 workers), and four producer and consumer pairs on 4 workers moved 3.7 million (`batch_cost`, [BASELINE.md](BASELINE.md)). Past about 100 items the work on the items themselves dominates, so larger batches gain little.

`bulkhead_sdk::batch` does the buffering. A `Batcher` keeps one batch per destination and returns it when it is full by count or bytes, or, with `max_delay`, from the timer its first item armed:

```rust
use bulkhead_sdk::batch::{self, Batcher, Limits};

// In the sender's state:
out: Batcher<Pid, Point>,   // Batcher::new(Limits { max_items: 100, max_bytes: 64 * 1024, max_delay: Some(Duration::from_millis(20)) })

// For each update:
for (to, msg) in self.out.push(ctx, collector, &point)? {
    ctx.cast(to, msg);
}

// In handle_info:
if let Info::Timer { timer, .. } = &info {
    if let Some((to, msg)) = self.out.on_timer(*timer) {
        ctx.cast(to, msg);
    }
}

// In the receiver's handle_cast:
if let Some(points) = batch::items::<Point>(&msg) {
    for point in points? { /* ... */ }
}
```

- `max_delay` bounds how stale an update can get: a lone point waits at most that long. Without it, call `flush` or `flush_all` (at the end of a burst, in `terminate`).
- A batch is one message: it counts once against the receiver's mailbox, and a full mailbox drops it whole, unless the app sends it with `cast_timeout`. Keep `max_bytes` well under the receiver's `limits.message_max_bytes`.
- Batch per destination, not per app: with `instances.routing = "hash:..."`, keep items that must stay in order on one key in one batch to one instance.
- Items buffered when the sender exits are lost, as unsent casts would be.
- Protocol extensions already batch: the Modbus extension sends one update per device and scan with every point that changed.
- A pool writing into one shared bucket does not get faster with more instances. Writes to a bucket take turns, so past a few writers more instances add contention: with 64 devices polled every millisecond, a pool of 4 stored more updates than a pool of 8 on the same bucket (`modbus_load`, [BASELINE.md](BASELINE.md), 2026-10-16). Keep the writing pool small, batch each update into one write, or give groups of devices buckets of their own.

## State across upgrades

In `snapshot` mode (`upgrade.mode = "snapshot"`), the host pauses the mailbox, calls `save_snapshot` on the old version and `load_snapshot` on the new one, then resumes. Pid, registered names, links, monitors and queued messages are kept. If `load_snapshot` returns `Err`, or the new version fails, the old version keeps running.

`#[derive(Snapshot)]` writes `[format_version, state]` as CBOR, and migrates older formats through a function you provide:

```rust
use bulkhead_sdk::{App, Snapshot};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize, Snapshot)]
#[snapshot(version = 2, migrate = migrate)]
struct State {
    count: u64,
    history: Vec<u64>,
}

fn migrate(from: u32, old: bulkhead_sdk::snapshot::Value) -> Result<State, String> {
    #[derive(Deserialize)]
    struct V1 { count: u64 }
    match from {
        1 => {
            let v1: V1 = old.deserialized().map_err(|e| e.to_string())?;
            Ok(State { count: v1.count, history: Vec::new() })
        }
        other => Err(format!("unknown snapshot format {other}")),
    }
}

// In the App impl:
//   fn save_snapshot(&self) -> Result<Vec<u8>, String> { self.state.to_snapshot() }
//   fn load_snapshot(_ctx: &mut Ctx, _from: &str, bytes: &[u8]) -> Result<Self, String> {
//       Ok(Self { state: State::from_snapshot(bytes)? })
//   }
```

Set `upgrade.snapshot_version` to the format your release writes, and `upgrade.from_versions` to the app versions it accepts. [`examples/counter`](../examples/counter) has all three versions, including one that refuses on purpose.

The other modes need no snapshot: `restart` stops the old version and starts the new one from `init`, and `blue-green` starts the new version beside the old and moves traffic to it in steps (`bulkhead upgrade --steps`, `bulkhead traffic`), for a pool with no registered names.

`save_snapshot` also backs `bulkhead state <pid>`. An operator can read an SDK app's state there as JSON.

## Host services

Each service must be declared under `capabilities` in the manifest **and** granted by the node's operator policy. A request to a service declared but not granted is answered `access-denied` (or the service's own permission error), so the app can degrade instead of failing to start. [MANIFEST.md](MANIFEST.md#capabilities-at-a-glance) lists, for each service, what the operator configures on the node and which SDK feature wraps it.

| Capability | Use from Rust |
| --- | --- |
| `bulkhead:logging` | `bulkhead_sdk::info!("order {id} accepted")` (and `trace!`, `debug!`, `warn!`, `error!`). Records carry the app, version and pid. The host caps level and rate (`max_level`, `rate_limit_per_sec`), and the operator can change the level at run time with `bulkhead log-level`. The host tells the SDK the effective level, so a record below it is skipped before formatting (about 3 ns, against about 440 ns when the host drops it); records written during `init` are filtered by the host only. |
| `bulkhead:config` | `bulkhead_sdk::config::get("db.host")` and `get_all()`, with the SDK's `config` feature. Values come from `config.defaults`, overridden by the node's `[config.<app>]`. Without the capability granted they return `ConfigError::Denied`. |
| `bulkhead:keyvalue` | `bulkhead_sdk::kv::Bucket::open("orders")`, with the SDK's `keyvalue` feature: `get`, `set`, `delete`, `exists`, `keys` (all pages), `increment`, the batch calls, and CBOR-typed `get_value` and `set_value`. Only the buckets the manifest names open. `memory` buckets survive instance crashes; `disk` buckets also survive node restarts. Each bucket can have a quota. Shared buckets (manifest `shared`, defined by the node and granted by policy) open the same way and hold one copy for every app that lists them: the place for a point table or settings several apps read. A read-only one refuses writes with `KvError::AccessDenied`. A memory bucket many instances read at once, far more than it is written, can take `read_concurrency = true` (MANIFEST.md): reads scale with cores (on a machine with cores to spare; on 4 cores native apps gained nothing), single operations and writes cost more, and readers may see part of a batch. With the SDK's `watch` feature, `bulkhead_sdk::watch` adds, on the same buckets: `get` with the value's revision, `compare_and_swap` and `compare_and_delete` on a revision (a lock-free update: read, then swap, retrying on `Conflict`), `watch`, `watch_prefix` and `resume`, and `scan`: entries in key order, as ETS's `ordered_set` (`watch::scan("orders").prefix("c42/").reverse().entries()`, or a page at a time; `first`, `last`, `next` and `prev` for one entry). Each page is read at one revision and carries it, so a scan followed by `resume` from that revision misses no change. A watch delivers `kv-changed` host events to `handle_info` (`Change::from_info`) naming the key and revision, not the value; notices are conflated per key, so a slow instance skips intermediate values but always sees the latest. Store the last revision handled to `resume` after a restart; `TooOld` means reread the keys and watch from the revision it carries. Watches end when the instance exits and survive upgrades, so `init` is the place to start them. A shared bucket the node scopes to the cluster is replicated on an HA pair: writes return once committed on the primary, reads and watches use this node's copy, `get_linearizable` reads through the primary, and `Unavailable` means no primary could take the operation (for a write, it may still have committed: reread before retrying a compare-and-swap). |
| `bulkhead:groups` | `bulkhead_sdk::groups::join("prices")`, `leave`, `members` and `publish("prices", envelope)`, with the SDK's `groups` feature. `publish` casts one copy to each member, on every node of the cluster (the publisher too, if it is a member; not across a `messaging_acl` refusal), and returns how many members took it (on another node, all its members once the copy is sent); members receive it in `handle_cast`. The manifest lists the groups an app may join and publish to. An instance leaves its groups when it exits and keeps them across an upgrade, so `init` (not `load_snapshot`) is the place to join. |
| `bulkhead:metrics` | `bulkhead_sdk::metrics::{Counter, Gauge, Histogram}`, with the SDK's `metrics` feature: make them in `init` (`Counter::new("orders_total", "Orders taken.", &[("region", "eu")])?`) and keep them in state; `inc`, `add`, `set` and `observe` are one host call each. Names are exported as `<prefix>_<name>` with an `app` label, per app: all instances add into the same series. Keep label values few (no ids or timestamps); `max_series` caps them. |
| `bulkhead:sql` | `bulkhead_sdk::sql::query("main", "SELECT ... WHERE id = $1", &[id.into()])`, `execute`, and `begin` for a transaction (`commit`, or drop it to roll back), with the SDK's `sql` feature. Name each datasource in the manifest with its `max_connections`; the operator grants it and binds it to a Postgres database. Parameters are bound, never interpolated; columns of types the interface does not carry are refused, so cast them (`col::text`). The same calls work in a native build, each a request to the node. |
| `bulkhead:secrets` | `bulkhead_sdk::secrets::get_string("db-password")` (or `get` for bytes), with the SDK's `secrets` feature. List the names in the manifest; the operator grants each and binds it to a file or variable. Read a secret when it is needed instead of keeping it in state (state goes into snapshots), and never log it. |
| `bulkhead:http-routes` | Declare `routes` (and optionally `max_body`, `timeout_ms`); the node's HTTP ingress sends each request on them to the app as a call. With the SDK's `http` feature, `bulkhead_sdk::http::Request::from_envelope(&msg)` recognises one in `handle_call`; reply `Reply::Now(Response::text(200, "hi").into())` or `Response::json(...)`, or `Reply::Later` and answer with `ctx.reply`. `req.scheme` is `https` when the client came over TLS. The request is spread over the pool, or routed by `key` (a route's `hash_header`) with `instances.routing = "hash:key"`. A full mailbox answers 503, a slow reply 504, a crash 502. See `examples/http-counter`. |
| `bulkhead:http-egress` | `bulkhead_sdk::http_client::get(url)`, `post`, or `send(&Request::new("PUT", url).header(...).body(...).timeout(...))`, with the SDK's `http-client` feature; the same natively, where the node sends each request (a response body up to 16 MiB). Only what the manifest's `allow` lists is reached; anything else is `HttpError::Denied`. Waiting for the response is host time, not counted against the deadline, but the instance handles nothing else meanwhile: use a pool for an app that must stay responsive. Prefer this to raw sockets for HTTP: the node pools connections, verifies TLS and refuses names that resolve to the node itself. |
| `bulkhead:sockets` | `bulkhead_sdk::net::TcpStream::connect(addr)` (or `connect_host`), `write_all`, and a non-blocking `read`; `TcpListener::bind` and `accept`, with the SDK's `net` feature. The node makes each socket and passes the stream (UDP through `Ctx::net`). An instance never blocks waiting for bytes between callbacks: `stream.notify_readable(tag)` asks the host to deliver an `io-ready` event with that tag once bytes arrive, handled in `handle_info` (`net::ready_tag(&info)`); read what is there, then ask again. The manifest lists where the app may connect, bind and listen, and `max_sockets`. For a protocol the node does not terminate (MQTT, a database's wire protocol); prefer HTTP egress, listeners or an extension when one fits. |
| `bulkhead:files` | `std::fs`, on the path `bulkhead_sdk::files::path("/data/orders.json")` returns: the host file, under the runner's Landlock rules. `/priv` holds the bundle's files read-only (`bulkhead bundle --priv <dir>`), and each preopen maps a guest path to a volume the operator configures. Paths cannot escape their preopen. |

The SDK's cargo features turn each wrapper on: `config`, `keyvalue`, `watch`, `groups`, `metrics`, `sql`, `secrets`, `http` (requests from the ingress), `http-client`, `net` and `connection`; logging and batches are always there.

Always available without a declaration: clocks, random, stdout and stderr (captured as log records), `bulkhead:runtime/process`, and `bulkhead:io` (readiness of pollables).

### Extensions

An extension is a host service an operator builds into a node from outside Bulkhead (the Modbus extension, [`examples/greeter`](../examples/greeter/README.md)), serving one package. Declare it under `capabilities.extensions."<package>"` with the table it reads; the operator grants it by package name. An app calls its functions through the node with `bulkhead_sdk::extension::call(package, "greet#hello", &(name,))`: the function by name, the parameters as a tuple, and the result decoded from CBOR. A client module gives the functions their names and types; `examples/greeter/app/src/greeting.rs` is one, and `bulkhead-modbus-client` is the Modbus extension's (`bulkhead_modbus_client::client::write_holding(...)`).

### Replicated buckets

On an HA pair with `[failover] replication = true`, a bucket declared with `durability = "replicated"` is kept whole on both nodes: a write returns once committed on the primary and its partner, reads and watches use this node's copy, and `get_linearizable` (with the `watch` feature) reads through the primary. It survives losing either node, so it is where a singleton keeps what must survive a failover. A write while no primary can take it fails with `Unavailable`; it may still have committed, so reread before retrying a compare-and-swap. A write waits for the standby to have it on disk: about 2.2 to 2.8 ms at p50 with both nodes on NVMe, on one machine or across a LAN, where the drive's sync sets the pace ([BASELINE.md](BASELINE.md), 2026-10-25). Batch writes that belong together into one.

### Singletons on a pair

`placement.mode = "singleton"` runs one instance on the pair's primary. Its callbacks run only while the node holds its lease: out of lease a message waits, and a callback running when the lease ends is stopped with `crashed: lease_ended`, so two nodes never run it at once. After a failover it starts from `init` on the new primary, with what it kept in replicated buckets. In a planned switchover (`bulkhead failover switchover`), a singleton in `snapshot` upgrade mode on a pair with replication is moved with its state: the old primary takes its snapshot and the new one starts it with `load_snapshot` instead of `init`. Its pid changes and its links, monitors, names and mailbox do not move; callers should look it up by name again.

### Reading shared data

Which way to read data several instances share, cheapest first (16-core machine and Raspberry Pi 5, BASELINE.md, 2026-11-01):

| Way | Cost per read: native | For |
| --- | --- | --- |
| The instance's own state | nanoseconds | Data only this instance writes; it survives upgrades through `save_snapshot`, and restarts only if kept in a bucket |
| A published bucket (`publish_ms`, MANIFEST.md) | 0.16 to 0.39 µs, from the app's own memory | Data many instances read and few write: points, prices, settings, routing tables. Native reads scale with cores (30 million a second from 16 instances on the 16-core machine). Reads may be up to an interval old, except the instance's own writes |
| A bucket read (`get`) | 2 to 3.5 µs, a request to the node (16-core machine, 2026-11-04; 5 to 7 µs before T7 on both machines) | Any data, always current |
| A batch (`get_many`) or a scan page | 1.3 to 2 µs per key, 100 at a time | Many keys at once; scan pages hold up to 1,000 entries or about 1 MiB |
| A call to an instance that owns the data | 13 µs within a node | Data one instance guards with rules of its own, where a read must see its latest decisions |

A watch (`watch` feature) says when a key changes, for reacting to changes or keeping a copy of a few keys; a notice carries the value. ARCHITECTURE.md's "A watch between two apps" shows a notice's path from the writer to the watcher's `handle_info`. Writes always go to the node: as requests (about 7 µs).

## Connection handlers

A connection handler serves TCP connections instead of messages: the node accepts each connection on a listener and hands it to the handler. Enable the SDK's `connection` feature, implement `connection::Handler`, and export it with `bulkhead_sdk::connection_handler!`:

```rust
use std::io::Write;
use bulkhead_sdk::connection::{ConnectionInfo, Flow, Handler, Output};
use bulkhead_sdk::Ctx;

struct Lines {
    pending: Vec<u8>,
}

impl Handler for Lines {
    fn open(_ctx: &mut Ctx, _info: &ConnectionInfo) -> Result<Self, String> {
        Ok(Lines { pending: Vec::new() })
    }

    fn data(&mut self, _ctx: &mut Ctx, bytes: &[u8], out: &mut Output<'_>) -> Result<Flow, String> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            out.write_all(&line).map_err(|e| e.to_string())?;
        }
        Ok(Flow::Continue)
    }
}

bulkhead_sdk::connection_handler!(Lines);
```

- `Self` is one connection's state: `open` makes it, `data` gets up to 64 KiB of what the peer sent at a time and answers through `out` (`std::io::Write`, blocking until the bytes are taken), and `closed` hears that the peer (or the node, after the idle timeout) closed. `Flow::Close` closes the connection.
- `connection_handler!` adds `bulkhead_main()`, which a binary calls. Each instance of the app's pool holds many connections, `data` runs as a callback when the node says a connection has bytes, an error from it closes that connection (logged as a warning), a panic crashes the instance and closes all of its connections, and writes count against `callback_deadline_ms`. A connection idle for the listener's `idle_timeout_ms` is closed by the node.
- The handler has a `Ctx` for calling and casting to apps (`ctx.whereis("counter")`). Keep state that must outlive a connection in an app or a keyvalue bucket, and keep the protocol in the handler: [`examples/line-server`](../examples/line-server) forwards `inc` to the counter example.
- Declare the listeners in the manifest (`[[appserver.listeners]]`, see [MANIFEST.md](MANIFEST.md)); the operator binds each to an address in node config.

## Native-process apps

Native code runs as a process the node spawns and supervises like an instance, an Erlang-style port: `kind = "native-process"` in the manifest. It has a pid, a mailbox, links, monitors and supervision, and handles one callback at a time; it can declare logging, keyvalue, process groups, config, secrets, metrics, HTTP routes, files, SQL, sockets, HTTP egress, listeners and extensions (MANIFEST.md). The node runs it through `bulkhead-runner`, which restricts it first: it reads only the system libraries, its own directory, the app's `priv/` files and its volumes, executes nothing else, opens no sockets of its own (the node makes them and passes the streams), and signals only itself. Linux only.

A crate on `bulkhead-sdk` keeps its `app!` line and has a binary that calls the `bulkhead_main()` it generates:

```rust
// src/main.rs
fn main() -> std::process::ExitCode {
    my_app::bulkhead_main()
}
```

`cargo build --release --bin my-app` builds the program, bundled with `native.program = "executable"` (the default). The SDK's calls go over the protocol: each is a request to the node (about 5 µs), so read many keys with `get_many` or a scan; `kv::Bucket::open` checks nothing, and an unlisted bucket's error comes with its first operation; a panic ends the instance as `crashed: panic: ...`; a system call outside the runner's allow-list ends it as `crashed: syscall <name>` (SECURITY.md lists what is allowed: no sockets of its own, no namespaces, no `memfd_create`); each snapshot load is in a new process (an upgrade starts the new version's process beside the old until it has loaded); a watch notice carries the key's value (`watch::Change::current` reads it); and `memory_max` without a node cgroup is the process's data limit, so going over it aborts the app. The conformance suite (`BULKHEAD_CONFORMANCE_NATIVE=1`) runs a reference app on a node's protocol.

Two shapes, chosen by `native.program`:

- **`"executable"`:** a program on `bulkhead-sdk` as above, or directly on `bulkhead-native`'s app API (feature `app`), with the process functions:

  ```rust
  use bulkhead_native::app::{App, CallRef, Ctx, serve};
  use bulkhead_native::protocol::Envelope;

  struct Driver;

  impl App for Driver {
      fn init(_ctx: &mut Ctx, _args: Vec<u8>) -> Result<Self, String> {
          Ok(Driver)
      }

      fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Option<Envelope> {
          let me = ctx.self_pid();
          Some(Envelope::new("text/plain", format!("{} from {}", msg.body.len(), me.id)))
      }
  }

  fn main() -> std::process::ExitCode {
      serve::<Driver>()
  }
  ```

  `Ctx` has `call`, `cast`, `reply`, `link`, `monitor`, `register`, `whereis`, `send_after`, `exit` and the rest; `bulkhead_native::app::log` logs from any thread. What the program prints goes to the node's log. It finds the bundle's `priv/` files and its volumes where `BULKHEAD_PREOPENS` says (`bulkhead_native::preopens::path("/data/orders.json")`, or `bulkhead_sdk::files::path` on the SDK), with `bulkhead:files` declared. Its stdin is empty, and a panic in a callback crashes the instance with its message.
- **`"library"`** (default): an existing rustbridge plugin (`.so`), unchanged. Calls and casts reach its `handle_request` with the envelope's schema id as the type tag, in JSON; the reply carries the same schema id, and an error the schema id `rustbridge.error`. It cannot call other apps, since rustbridge's plugin interface has no call into the host.

Limits: `callback_deadline_ms` bounds each callback's wall time (less time spent in requests to the node), and an idle process that misses a ping is killed; both end as `timeout`. `memory_max` is enforced by the node (a cgroup, or `RLIMIT_DATA`). An executable may upgrade in `snapshot` mode and move with its state, through its `save_snapshot` and `load_snapshot` (on `bulkhead-native`'s API, `App::save_snapshot` and `App::load_snapshot`); a rustbridge plugin upgrades in `restart` or `blue-green` mode only. Build the program for the platform the node runs on and bundle it as below; [MANIFEST.md](MANIFEST.md#native-process-apps) has the details.

## Bundles

`bulkhead bundle bulkhead.toml <program> --key signer.key` writes a signed `.rbp`:

- The program is the `.so` or executable built for this machine's platform.
- A native-process app may instead carry a program per platform. Pass `--program linux-x86_64=PATH --program linux-aarch64=PATH` in place of the argument, for a cluster of both architectures or for nodes of another one. Each program must be an ELF file for its platform's architecture. A node runs the program for its own platform, and refuses a bundle without one with a message naming the platforms the bundle has.
- Cross-building for a Raspberry Pi from an x86-64 machine needs the target and a cross linker:
  - `rustup target add aarch64-unknown-linux-gnu`;
  - a cross linker, such as Ubuntu's `gcc-aarch64-linux-gnu`;
  - `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc cargo build --release --target aarch64-unknown-linux-gnu`.

  The program lands under `target/aarch64-unknown-linux-gnu/release/`. It needs a glibc no newer than the nodes'. Built on Ubuntu 24.04, `examples/board` asks for glibc 2.34 and ran on a Raspberry Pi 5 (2026-11-01). Building on a machine of each architecture works as well.
- `--priv <dir>` adds a directory's files, which the app reads at `bulkhead_sdk::files::path("/priv/...")` (the directory `BULKHEAD_PRIV` names), when the manifest declares `bulkhead:files`.
- The bundle is read back as a node would before the command succeeds.
- `bulkhead keygen <prefix>` makes the signing key pair; nodes trust the public key in `trusted_keys`.

## Testing

Three levels, from fastest to most complete:

- **The logic.** Keep message handling in plain functions over plain types, and the `App` impl a thin shell that decodes the envelope, calls them and encodes the reply. `cargo test` runs them on the host target. The `Ctx` methods and the service wrappers go to the node, which only a running instance has, so test code does not call them.
- **The app on a node.** For services, manifests and policy, start a `bulkhead::node::Node` in a temporary directory with a node config written by the test, bundle the app with the CLI and drive it through `bulkhead::api::Client`, as `board.rs` (HTTP, a shared bucket, a node restart) and [`scada.rs`](../crates/bulkhead/tests/scada.rs) (an HA pair and a witness in one process) do.

While debugging, `bulkhead state <pid>` shows an SDK app's state through its `save_snapshot`; an instance whose `save_snapshot` fails is restarted. `bulkhead logs <app> -f` follows its records, and `bulkhead events` shows exits and restarts as they happen.

## Limits to design around

- An instance is a process: a call to it costs 5 to 7 µs while it is busy and 27 to 28 µs once it has gone idle (two sleeping threads to wake), a keyvalue `get`, `set` or `delete` from it to the node 2 to 3.5 µs and other requests about 4 µs (16-core machine, 2026-11-04; the Pi not measured since T7), and an instance about 0.25 to 0.5 MiB (1.4 to 2.4 MiB on the Pi) and a few milliseconds to start (BASELINE.md, 2026-10-31 and 2026-11-01; spike S30). Its code runs at native speed. Keep instances long-lived, and batch reads (`get_many`, scans) rather than reading key by key. Data many instances read and few write belongs in a published bucket (MANIFEST.md, `publish_ms`), which a native app reads from a snapshot in its own memory in 0.1 to 0.4 µs, its own writes read back at once and others' up to an interval late.
- A message's body is at most `limits.message_max_bytes` (1 MiB by default); a larger one fails with `too-large`. A native app's frames to and from the node are at most 17 MiB each, so a message, a keyvalue value or a whole `set_many` or `get_many` over about 16 MiB ends the instance today (ROADMAP, "Smaller open items"): split large batches. A bucket's values are bounded by its quota alone.
- A full mailbox drops casts and fails calls with `overloaded`. Size `limits.mailbox_max` to absorb bursts, and treat `Overloaded` as back-pressure.
