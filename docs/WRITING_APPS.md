# Writing an app

A Bulkhead app implements a `gen_server`-style set of callbacks. In Rust, `bulkhead-sdk` gives you the `App` trait and the `app!` macro, and one source builds two ways: as a native program, run as one restricted process per instance (Linux; "Native-process apps" below), and as a WebAssembly component implementing the `bulkhead:runtime/app` world ([`wit/runtime.wit`](../wit/runtime.wit)). Native programs are where Bulkhead is going; components still run beside them until native apps reach parity (ROADMAP Z8 and Z9), and some features are components only until then. Go has an SDK of its own for components ([`sdks/go`](../sdks/go/README.md)); apps in other languages implement the WIT directly. An existing rustbridge plugin runs natively too.

## The model

Like an Erlang `gen_server`:

- The host keeps each instance's mailbox and hands it **one message at a time**. Callbacks never run concurrently within an instance, so state needs no locks.
- **Calls** expect a reply and the caller waits, up to a timeout. **Casts** are fire and forget. **Infos** are notifications from the host: link exits, monitor downs, timers and named host events.
- Each callback has a **deadline** (`limits.callback_deadline_ms`): CPU time for a component, wall time for a native process. Overrunning it kills the instance with reason `timeout`. Time spent waiting in a host call, such as a call to another instance, does not count.
- A **trap** (a panic in Rust, or an abort in a native program), an out-of-memory error or a deadline kill ends the instance. Its supervisor restarts it from `init`, with fresh memory. State that must outlive a crash belongs in a keyvalue bucket or a file.
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
cargo build --target wasm32-wasip2 --release
bulkhead bundle bulkhead.toml target/wasm32-wasip2/release/greeter.wasm --key signer.key
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
| `terminate(&mut self, ctx, reason)` | no | Before a graceful stop, within `lifecycle.shutdown_timeout_ms`. Not called on kills or traps. |
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

A `CallError` is one of `Noproc` (dead pid or unknown name), `Noconnection` (the target's node was lost, or cannot be reached), `Timeout`, `Overloaded` (the target's mailbox is full), `TooLarge`, `AccessDenied` (refused by `messaging_acl`) or `Exited(reason)` (the target died while handling the call). A call to your own pid traps, because it could never be answered.

A blocked `ctx.call` holds up only this instance. The host suspends it without tying up a thread.

## Links, monitors and exits

```rust
ctx.link(other);                  // if either exits abnormally, the other is killed...
ctx.trap_exits(true);             // ...unless it traps exits: then it gets Info::Exit instead
let m = ctx.monitor(other);       // one-way: Info::Down { monitor: m, pid, reason } when `other` exits
ctx.demonitor(m);
ctx.exit(other, &ExitReason::Shutdown);
```

Exit reasons: `Normal`, `Shutdown`, `Killed`, `Noproc`, `Noconnection`, `Timeout` (the CPU deadline), and `Crashed(text)`. The text is the trap message, or one of the host's: `memory_limit`, `table_limit`, `init_timeout`, `unhealthy` (failed health checks), `lease_ended` (a singleton whose node lost its lease), `no_slot` (no Store slot for a start or wake), or a host-object limit (`timer_limit`, `monitor_limit`, `link_limit`, `pending_call_limit`).

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

## Idle instances

An app with many instances that are mostly idle (one per device or per user) can let them give up their memory: with `instances.passivate_after_ms`, an instance with nothing to do for that long is passivated (`save_snapshot`, then its Store is dropped; its pid, mailbox, links, monitors, names, timers, watches and groups stay), and the next message wakes it with `load_snapshot`. It needs the snapshot callbacks (below). A wake costs about 60 to 100 µs, so passivate instances idle for seconds, not milliseconds.

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
- A batch is one message: it counts once against the receiver's mailbox, and a full mailbox drops it whole. Keep `max_bytes` well under the receiver's `limits.message_max_bytes`.
- Batch per destination, not per app: with `instances.routing = "hash:..."`, keep items that must stay in order on one key in one batch to one instance.
- Items buffered when the sender exits are lost, as unsent casts would be.
- Protocol extensions already batch: the Modbus extension sends one update per device and scan with every point that changed.
- A pool writing into one shared bucket does not get faster with more instances. Writes to a bucket take turns, so past a few writers more instances add contention: with 64 devices polled every millisecond, a pool of 4 stored more updates than a pool of 8 on the same bucket (`modbus_load`, [BASELINE.md](BASELINE.md), 2026-10-16). Keep the writing pool small, batch each update into one write, or give groups of devices buckets of their own.

## State across upgrades

In `snapshot` mode (`upgrade.mode = "snapshot"`), the host pauses the mailbox, calls `save_snapshot` on the old version and `load_snapshot` on the new one, then resumes. Pid, registered names, links, monitors and queued messages are kept. If `load_snapshot` returns `Err`, or the new version traps, the old version keeps running.

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

Each service must be declared under `capabilities` in the manifest **and** granted by the node's operator policy. A component that imports an undeclared interface is refused at bundle and deploy time. A declared but ungranted interface is linked to a stub that returns `access-denied` (or the interface's own permission error) at call time, so the app can degrade instead of failing to start. [MANIFEST.md](MANIFEST.md#capabilities-at-a-glance) lists, for each service, what the operator configures on the node and which SDK feature wraps it.

| Capability | Use from Rust |
| --- | --- |
| `wasi:logging` | `bulkhead_sdk::info!("order {id} accepted")` (and `trace!`, `debug!`, `warn!`, `error!`). Records carry the app, version and pid. The host caps level and rate (`max_level`, `rate_limit_per_sec`), and the operator can change the level at run time with `bulkhead log-level`. The host tells the SDK the effective level, so a record below it is skipped before formatting (about 3 ns, against about 440 ns when the host drops it); records written during `init` are filtered by the host only. |
| `wasi:config` | `bulkhead_sdk::config::get("db.host")` and `get_all()`, with the SDK's `config` feature. Values come from `config.defaults`, overridden by the node's `[config.<app>]`. Without the capability granted they return `ConfigError::Denied`. |
| `wasi:keyvalue` | `bulkhead_sdk::kv::Bucket::open("orders")`, with the SDK's `keyvalue` feature: `get`, `set`, `delete`, `exists`, `keys` (all pages), `increment`, the batch calls, and CBOR-typed `get_value` and `set_value`. Only the buckets the manifest names open. `memory` buckets survive instance crashes; `disk` buckets also survive node restarts. Each bucket can have a quota. Shared buckets (manifest `shared`, defined by the node and granted by policy) open the same way and hold one copy for every app that lists them: the place for a point table or settings several apps read. A read-only one refuses writes with `KvError::AccessDenied`. A memory bucket many instances read at once can take `read_concurrency = true` (MANIFEST.md): reads scale with cores, single operations cost a little more, and readers may see part of a batch. With the SDK's `watch` feature, `bulkhead_sdk::watch` adds, on the same buckets: `get` with the value's revision, `compare_and_swap` and `compare_and_delete` on a revision (a lock-free update: read, then swap, retrying on `Conflict`), `watch`, `watch_prefix` and `resume`, and `scan`: entries in key order, as ETS's `ordered_set` (`watch::scan("orders").prefix("c42/").reverse().entries()`, or a page at a time; `first`, `last`, `next` and `prev` for one entry). Each page is read at one revision and carries it, so a scan followed by `resume` from that revision misses no change. A watch delivers `kv-changed` host events to `handle_info` (`Change::from_info`) naming the key and revision, not the value; notices are conflated per key, so a slow instance skips intermediate values but always sees the latest. Store the last revision handled to `resume` after a restart; `TooOld` means reread the keys and watch from the revision it carries. Watches end when the instance exits and survive upgrades, so `init` is the place to start them. A shared bucket the node scopes to the cluster is replicated on an HA pair: writes return once committed on the primary, reads and watches use this node's copy, `get_linearizable` reads through the primary, and `Unavailable` means no primary could take the operation (for a write, it may still have committed: reread before retrying a compare-and-swap). |
| `bulkhead:groups` | `bulkhead_sdk::groups::join("prices")`, `leave`, `members` and `publish("prices", envelope)`, with the SDK's `groups` feature. `publish` casts one copy to each member, on every node of the cluster (the publisher too, if it is a member; not across a `messaging_acl` refusal), and returns how many members took it (on another node, all its members once the copy is sent); members receive it in `handle_cast`. The manifest lists the groups an app may join and publish to. An instance leaves its groups when it exits and keeps them across an upgrade, so `init` (not `load_snapshot`) is the place to join. |
| `bulkhead:metrics` | `bulkhead_sdk::metrics::{Counter, Gauge, Histogram}`, with the SDK's `metrics` feature: make them in `init` (`Counter::new("orders_total", "Orders taken.", &[("region", "eu")])?`) and keep them in state; `inc`, `add`, `set` and `observe` are one host call each. Names are exported as `<prefix>_<name>` with an `app` label, per app: all instances add into the same series. Keep label values few (no ids or timestamps); `max_series` caps them. |
| `bulkhead:sql` | `bulkhead_sdk::sql::query("main", "SELECT ... WHERE id = $1", &[id.into()])`, `execute`, and `begin` for a transaction (`commit`, or drop it to roll back), with the SDK's `sql` feature. Name each datasource in the manifest with its `max_connections`; the operator grants it and binds it to a Postgres database. Parameters are bound, never interpolated; columns of types the interface does not carry are refused, so cast them (`col::text`). |
| `bulkhead:secrets` | `bulkhead_sdk::secrets::get_string("db-password")` (or `get` for bytes), with the SDK's `secrets` feature. List the names in the manifest; the operator grants each and binds it to a file or variable. Read a secret when it is needed instead of keeping it in state (state goes into snapshots), and never log it. |
| `wasi:http/incoming-handler` | Declare `routes` (and optionally `max_body`, `timeout_ms`); the node's HTTP ingress sends each request on them to the app as a call. With the SDK's `http` feature, `bulkhead_sdk::http::Request::from_envelope(&msg)` recognises one in `handle_call`; reply `Reply::Now(Response::text(200, "hi").into())` or `Response::json(...)`, or `Reply::Later` and answer with `ctx.reply`. `req.scheme` is `https` when the client came over TLS. The request is spread over the pool, or routed by `key` (a route's `hash_header`) with `instances.routing = "hash:key"`. A full mailbox answers 503, a slow reply 504, a crash 502. See `examples/http-counter`. |
| `wasi:http/outgoing-handler` | `bulkhead_sdk::http_client::get(url)`, `post`, or `send(&Request::new("PUT", url).header(...).body(...).timeout(...))`, with the SDK's `http-client` feature. Only what the manifest's `allow` lists is reached; anything else is `HttpError::Denied`. Waiting for the response is host time, not counted against the deadline, but the instance handles nothing else meanwhile: use a pool for an app that must stay responsive. Prefer this to raw sockets for HTTP: the node pools connections, verifies TLS and refuses names that resolve to the node itself. |
| `wasi:sockets` | `bulkhead_sdk::net::TcpStream::connect(addr)` (or `connect_host`), `write_all`, and a non-blocking `read`; `TcpListener::bind` and `accept`, with the SDK's `net` feature. An instance never blocks waiting for bytes between callbacks: `stream.notify_readable(tag)` asks the host to deliver an `io-ready` event with that tag once bytes arrive, handled in `handle_info` (`net::ready_tag(&info)`); read what is there, then ask again. The manifest lists where the app may connect, bind and listen, and `max_sockets`. For a protocol the node does not terminate (MQTT, a database's wire protocol); prefer HTTP egress, listeners or an extension when one fits. UDP goes through the re-exported `wasip2` crate with `net::notify` on its pollables. |
| `wasi:filesystem` | `std::fs`. `/priv` holds the bundle's files read-only (`bulkhead bundle --priv <dir>`), and each preopen maps a guest path to a volume the operator configures. Paths cannot escape their preopen. |

The SDK's cargo features turn each wrapper on: `config`, `keyvalue`, `watch`, `groups`, `metrics`, `sql`, `secrets`, `http` (requests from the ingress), `http-client`, `net` and `connection`; logging and batches are always there. A component imports only the interfaces of the features it uses, so declare those and no more. An app may also generate its own bindings with `wit-bindgen` from the WIT in [`wit/`](../wit/) and [`wit/vendor/`](../wit/vendor/), as [`crates/bulkhead-fixture-services`](../crates/bulkhead-fixture-services/src/lib.rs) does.

Always available without a declaration: clocks, random, stdout and stderr (captured as log records), `bulkhead:runtime/process`, and `bulkhead:io` (readiness of pollables).

### Replicated buckets

On an HA pair with `[failover] replication = true`, a bucket declared with `durability = "replicated"` is kept whole on both nodes: a write returns once committed on the primary and its partner, reads and watches use this node's copy, and `get_linearizable` (with the `watch` feature) reads through the primary. It survives losing either node, so it is where a singleton keeps what must survive a failover. A write while no primary can take it fails with `Unavailable`; it may still have committed, so reread before retrying a compare-and-swap. A write waits for the standby to have it on disk: about 2.2 to 2.8 ms at p50 with both nodes on NVMe, on one machine or across a LAN, where the drive's sync sets the pace ([BASELINE.md](BASELINE.md), 2026-10-25). Batch writes that belong together into one.

### Singletons on a pair

`placement.mode = "singleton"` runs one instance on the pair's primary. Its callbacks run only while the node holds its lease: out of lease a message waits, and a callback running when the lease ends is stopped with `crashed: lease_ended`, so two nodes never run it at once. After a failover it starts from `init` on the new primary, with what it kept in replicated buckets. In a planned switchover (`bulkhead failover switchover`), a singleton in `snapshot` upgrade mode on a pair with replication is moved with its state: the old primary takes its snapshot and the new one starts it with `load_snapshot` instead of `init`. Its pid changes and its links, monitors, names and mailbox do not move; callers should look it up by name again.

## Connection handlers

A connection handler serves TCP connections instead of messages: the node accepts each connection on a listener and starts an instance of the handler for it. Enable the SDK's `connection` feature, implement `Connection`, and export it with `bulkhead_sdk::connection!`:

```rust
use std::io::{BufRead, BufReader, Write};
use bulkhead_sdk::connection::{Connection, ConnectionInfo, Stream};
use bulkhead_sdk::Ctx;

struct Lines;

impl Connection for Lines {
    fn serve(ctx: &mut Ctx, _info: ConnectionInfo, stream: Stream) -> Result<(), String> {
        let mut out = stream.clone_writer();
        for line in BufReader::new(stream).lines() {
            let line = line.map_err(|e| e.to_string())?;
            writeln!(out, "{line}").map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

bulkhead_sdk::connection!(Lines);
```

- `Stream` implements `std::io::Read` and `Write`; a read of 0 bytes means the client closed.
- Returning closes the connection; an error closes it as a crash. A connection idle for the listener's `idle_timeout_ms` is closed by the host.
- The handler has a `Ctx` for calling and casting to apps (`ctx.whereis("counter")`), but no mailbox. Keep state that must outlive a connection in an app or a keyvalue bucket, and keep the protocol in the handler: [`examples/line-server`](../examples/line-server) forwards `inc` to the counter example.
- Declare the listeners in the manifest (`[[appserver.listeners]]`, see [MANIFEST.md](MANIFEST.md)); the operator binds each to an address in node config.

## HTTP handlers

An unmodified `wasi:http` component, built with any toolchain that targets the `wasi:http/proxy` world, deploys as an HTTP handler: each request on its routes gets a new instance, and request and response bodies stream. In Rust, the `wasip2` crate's `wasip2::http::proxy::export!` exports one. Requests as calls to an app (above) are faster, so prefer them unless the component already exists or a body must stream (S15: about 15% of native throughput for a new instance per request).

- Declare routes under `wasi:http/incoming-handler` as for an app; see [MANIFEST.md](MANIFEST.md#http-handlers) for what else the manifest may hold.
- Nothing survives a request: keep state in an app (call it through `bulkhead:runtime/process`) or a keyvalue bucket.
- A trap before the response's headers is a 500; after them, the client sees the stream cut.

## Apps in other languages

Go apps use the Go SDK, [`sdks/go`](../sdks/go/README.md), built with TinyGo: the same callbacks, process functions, envelopes, snapshot format and services as `bulkhead-sdk`, with S21's two fixes inside it (it frees what the host passes, and its `build.sh` sets `-scheduler=none`) and a manifest template that declares what TinyGo's runtime imports. [`examples/board-go`](../examples/board-go/main.go) is the tutorial's board app on it, run by the same node tests as the Rust board.

Any language whose tooling builds a WASI 0.2 component can implement `bulkhead:runtime/app` from [`wit/runtime.wit`](../wit/runtime.wit). Go (TinyGo), Python (componentize-py) and JavaScript (jco) are tried: [`examples/ping-pong/languages`](../examples/ping-pong/languages/README.md) has the `pong` app in each, called by the Rust `ping`, with a demo that upgrades one app across all four languages on a running node. Each language's runtime brings a requirement of its own:

- **Go:** without the SDK, build with `-scheduler=none`, and free each string and list the host passes to an export, or every message leaks.
- **Python:** node config's `max_core_instances_per_component` at 14 or more.
- **JavaScript:** `limits.memory_max` of about 96 MiB.
- **All three:** their runtimes import capabilities the app may not use (`wasi:filesystem`, and `wasi:sockets` or `wasi:http`). The manifest declares them, with no scope; ungranted, the node links deny stubs.

These builds are not in CI. Spikes S21 to S23 measured what each costs next to Rust. [GUEST_FORMATS.md](GUEST_FORMATS.md) has the bodies an app in any language reads and writes beyond the WIT: snapshots, HTTP requests and responses, health checks, batches, hash routing and host events.

### Conformance

An SDK shows what it supports by passing the conformance suite, `bulkhead-host`'s [`tests/conformance.rs`](../crates/bulkhead-host/tests/conformance.rs). Its cases drive an app that speaks a small text command protocol (`echo:<text>`, `link:<pid>`, `kv-set:<bucket>:<key>:<value>` ...), given in full in [`bulkhead-fixture-conformance`](../crates/bulkhead-fixture-conformance/src/lib.rs), the Rust SDK's app. An SDK in another language builds the same app with its own API, and the suite runs against it:

```bash
BULKHEAD_CONFORMANCE_APP=path/to/app.wasm cargo test -p bulkhead-host --test conformance
```

A relative path is taken from the workspace root. Without the variable, the suite runs against the Rust app, as in CI. The snapshot the app writes is GUEST_FORMATS.md's `[1, state]` with a state the protocol fixes, so the snapshot cases also check that an SDK writes that format.

| Cases (tests named) | What | Rust | Go |
| --- | --- | --- | --- |
| `init____*` | Init arguments, an init error, a trap in init | yes | yes |
| `call____*`, `cast____*`, `reply____*`, `self____*`, `guest_call_and_cast____*` | Calls and casts both ways, envelopes kept, `noreply`, deferred and in-callback replies, the instance's pid | yes | yes |
| `timers____*` | Timers fire in order; a cancelled one never does | yes | yes |
| `register____*` | Names listed in the manifest; taken and unlisted names | yes | yes |
| `link____*`, `unlink____*`, `monitor____*`, `exit_signal____*`, `terminate____*` | Links with and without trapped exits, monitors and demonitor, exit signals, `terminate` on a graceful stop | yes | yes |
| `snapshot____*`, `upgrade____*`, `rollback____*` | The snapshot format, a snapshot upgrade that keeps state and deferred callers, a refused snapshot, a rollback | yes | yes |
| `trap____*`, `memory_max____*`, `callback_deadline____*` | A trap, `memory_max` (128 MiB here) and the callback deadline end the instance with the right reason | yes | yes |
| `memory____*` | 10,000 calls and casts leave memory where it was: what the host passes each callback is freed | yes | yes |
| `health____*` | Health checks answered, and a failing one kills the instance | yes | yes |
| `log_level____*` | Records reach the host at their level; the app follows the `log-level` event | yes | yes |
| `keyvalue____*`, `watch____*`, `scan____*` | Keyvalue operations, an unlisted bucket, a watch's change notices, scans in key order with a prefix, bounds and either direction | yes | yes |
| `groups____*` | Join, members, publish and leave, an unlisted group | yes | yes |
| `config____*`, `secrets____*`, `metrics____*` | Config values, bound, unbound and unlisted secrets, counters, gauges and histograms as the node renders them | yes | yes |

The Go column is from a local run (Linux x86-64, TinyGo 0.43.0, 2026-10-30); CI runs the Rust app only until ROADMAP J5.

## Native-process apps

Native code runs as a process the node spawns and supervises like an instance, an Erlang-style port: `kind = "native-process"` in the manifest. It has a pid, a mailbox, links, monitors and supervision, and handles one callback at a time; it can declare logging, keyvalue, process groups, config, secrets, metrics and HTTP routes (MANIFEST.md). The node runs it through `bulkhead-runner`, which restricts it first: it reads only the system libraries, its own directory and the app's `priv/` files, executes nothing else, opens no sockets, and signals only itself. Linux only.

An app written with `bulkhead-sdk` builds both ways from one source. The crate keeps its `app!` line; it adds `"rlib"` to its `crate-type`, a binary that calls the `bulkhead_main()` `app!` generates on Unix, and a feature that gates the binary, so a `wasm32-wasip2` build skips it:

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[[bin]]
name = "my-app"
path = "src/main.rs"
required-features = ["native"]

[features]
native = []
```

```rust
// src/main.rs
fn main() -> std::process::ExitCode {
    my_app::bulkhead_main()
}
```

`cargo build --release --features native --bin my-app` builds the program, bundled with `native.program = "executable"`; `examples/http-counter` and `examples/board` are built both ways (their `bulkhead.toml` has the native lines), the board's node tests run each case against both, and `BOARD=native` runs [the tutorial](../examples/tutorial/README.md) with the native board. The SDK's calls go over the native protocol there, with these differences from a component: each host call is a request to the node (about 13 µs on a Raspberry Pi 5, against about 1.2 µs for a component's bucket read), so read many keys with `get_many` or a scan; `kv::Bucket::open` checks nothing, and an unlisted bucket's error comes with its first operation; a panic ends the instance as `crashed: panic: ...` where a component's trap reads `crashed: trap: ...`; `save_snapshot` and `load_snapshot` work as for a component, each load in a new process (an upgrade starts the new version's process beside the old until it has loaded, and passivation ends the process until a message wakes it); a watch notice carries the key's value (`watch::Change::current` reads it either way); and `memory_max` without a node cgroup is the process's data limit, so going over it aborts the app. The conformance suite runs the same reference app both ways (`BULKHEAD_CONFORMANCE_NATIVE=1`).

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

  `Ctx` has `call`, `cast`, `reply`, `link`, `monitor`, `register`, `whereis`, `send_after`, `exit` and the rest; `bulkhead_native::app::log` logs from any thread. What the program prints goes to the node's log. It finds the bundle's `priv/` files in the directory `BULKHEAD_PRIV` names. It must not read stdin, and a panic in a callback crashes the instance with its message.
- **`"library"`** (default): an existing rustbridge plugin (`.so`), unchanged. Calls and casts reach its `handle_request` with the envelope's schema id as the type tag, in JSON; the reply carries the same schema id, and an error the schema id `rustbridge.error`. It cannot call other apps, since rustbridge's plugin interface has no call into the host.

Limits: `callback_deadline_ms` bounds each callback's wall time (less time spent in requests to the node), and an idle process that misses a ping is killed; both end as `timeout`. `memory_max` is enforced by the node (a cgroup, or `RLIMIT_DATA`). An executable may upgrade in `snapshot` mode, be passivated and move with its state, through its `save_snapshot` and `load_snapshot` (on `bulkhead-native`'s API, `App::save_snapshot` and `App::load_snapshot`); a rustbridge plugin upgrades in `restart` or `blue-green` mode only. Build the program for the platform the node runs on and bundle it as below; [MANIFEST.md](MANIFEST.md#native-process-apps) has the details.

## Bundles

`bulkhead bundle bulkhead.toml <program> --key signer.key` writes a signed `.rbp`:

- The program is the component (`.wasm`, built for `wasm32-wasip2`), or for a native-process app the `.so` or executable built for this machine's platform.
- A native-process app may instead carry a program per platform. Pass `--program linux-x86_64=PATH --program linux-aarch64=PATH` in place of the argument, for a cluster of both architectures or for nodes of another one. Each program must be an ELF file for its platform's architecture. A node runs the program for its own platform, and refuses a bundle without one with a message naming the platforms the bundle has.
- Cross-building for a Raspberry Pi from an x86-64 machine needs the target and a cross linker:
  - `rustup target add aarch64-unknown-linux-gnu`;
  - a cross linker, such as Ubuntu's `gcc-aarch64-linux-gnu`;
  - `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc cargo build --release --target aarch64-unknown-linux-gnu --features native`.

  The program lands under `target/aarch64-unknown-linux-gnu/release/`. It needs a glibc no newer than the nodes'. Built on Ubuntu 24.04, `examples/board` asks for glibc 2.34 and ran on a Raspberry Pi 5 (2026-11-01). Building on a machine of each architecture works as well.
- `--priv <dir>` adds a directory's files, which a component reads at `/priv` and a native app in the directory its environment variable `BULKHEAD_PRIV` names.
- The bundle is refused if the component imports an interface the manifest does not declare (`--skip-import-check` writes it anyway, for a node to refuse), and it is read back as a node would before the command succeeds.
- `bulkhead keygen <prefix>` makes the signing key pair; nodes trust the public key in `trusted_keys`.

## Testing

Three levels, from fastest to most complete:

- **The logic, natively.** Keep message handling in plain functions over plain types, and the `App` impl a thin shell that decodes the envelope, calls them and encodes the reply. `cargo test` runs them on the host target. The `Ctx` methods and the service wrappers call host imports, which only a node provides, so test code does not call them.
- **The component, in a runtime.** A test can build the component (`cargo build --target wasm32-wasip2`, as [`crates/bulkhead/tests/board.rs`](../crates/bulkhead/tests/board.rs) does from its `build()`) and run it in a `bulkhead_host::Runtime`: `Runtime::new(RuntimeConfig::default())`, `rt.load(&bytes)`, `rt.spawn(&app, InstanceSpec::new("name", "1.0.0"))`, then `handle.call(Envelope::new("text/plain", "inc"), timeout)`. That covers the callbacks, snapshots (`handle.upgrade`), traps and limits, with no services beyond the always-available ones. [`crates/bulkhead-host/examples/app_cost.rs`](../crates/bulkhead-host/examples/app_cost.rs) is a short example of the calls.
- **The app on a node.** For services, manifests and policy, start a `bulkhead::node::Node` in a temporary directory with a node config written by the test, bundle the app with the CLI and drive it through `bulkhead::api::Client`, as `board.rs` (HTTP, a shared bucket, a node restart) and [`scada.rs`](../crates/bulkhead/tests/scada.rs) (an HA pair and a witness in one process) do.

While debugging, `bulkhead state <pid>` shows an SDK app's state through its `save_snapshot`; an instance whose `save_snapshot` traps is restarted. `bulkhead logs <app> -f` follows its records, and `bulkhead events` shows exits and restarts as they happen.

## Limits to design around

- Calls to a component cost about 2 µs, plus the copy. Bulkhead suits work that takes tens of microseconds or more per message, not chatty fine-grained calls.
- A native instance is a process: a call to it costs about 30 µs on a Raspberry Pi 5 (frames through two pipes and two wake-ups), a request from it to the node about 11 to 13 µs, and an instance about 0.25 to 0.5 MiB (1.4 to 2.4 MiB on the Pi) and a few milliseconds to start (BASELINE.md, 2026-10-31; spike S30). Its code runs at native speed. Keep instances long-lived, and batch reads (`get_many`, scans) rather than reading key by key.
- A component's compute runs at about 1.4 to 2.6 times native time on ordinary app code (JSON with serde, regex, Markdown, maps; `app_cost`, [BASELINE.md](BASELINE.md)), and further behind when native code would use SHA or AES instructions ([spike S8](../spikes/s8/README.md)). Byte-at-a-time loops such as string comparison in sorted maps pay most.
- A full mailbox drops casts and fails calls with `overloaded`. Size `limits.mailbox_max` to absorb bursts, and treat `Overloaded` as back-pressure.
