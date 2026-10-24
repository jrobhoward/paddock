# Writing an app

A Paddock app is a WebAssembly component that implements the `paddock:runtime/app` world ([`wit/runtime.wit`](../wit/runtime.wit)). In Rust, `paddock-sdk` hides the bindings: you implement the `App` trait and export it with `app!`. Apps in other languages implement the WIT directly; only the Rust SDK ships. Native code that cannot be a component (a vendor library, a driver) runs as a native-process app instead (below).

## The model

Like an Erlang `gen_server`:

- The host keeps each instance's mailbox and hands it **one message at a time**. Callbacks never run concurrently within an instance, so state needs no locks.
- **Calls** expect a reply and the caller waits, up to a timeout. **Casts** are fire and forget. **Infos** are notifications from the host: link exits, monitor downs, timers and named host events.
- Each callback has a **CPU deadline** (`limits.callback_deadline_ms`). Overrunning it kills the instance with reason `timeout`. Time spent waiting in a host call, such as a call to another instance, does not count.
- A **trap** (a panic in Rust), an out-of-memory error or a deadline kill ends the instance. Its supervisor restarts it from `init`, with fresh memory. State that must outlive a crash belongs in a keyvalue bucket or a file.
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
paddock-sdk = { path = "../paddock/crates/paddock-sdk" }
serde = { version = "1", features = ["derive"] }
```

`src/lib.rs`:

```rust
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};

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

paddock_sdk::app!(Greeter);
```

`paddock.toml`:

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
paddock bundle paddock.toml target/wasm32-wasip2/release/greeter.wasm --key signer.key
paddock deploy greeter-0.1.0.rbp
paddock call greeter world          # hello, world (#1)
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

A `CallError` is one of `Noproc` (dead pid or unknown name), `Timeout`, `Overloaded` (the target's mailbox is full), `TooLarge`, `AccessDenied` or `Exited(reason)` (the target died while handling the call). A call to your own pid traps, because it could never be answered.

A blocked `ctx.call` holds up only this instance. The host suspends it without tying up a thread.

## Links, monitors and exits

```rust
ctx.link(other);                  // if either exits abnormally, the other is killed...
ctx.trap_exits(true);             // ...unless it traps exits: then it gets Info::Exit instead
let m = ctx.monitor(other);       // one-way: Info::Down { monitor: m, pid, reason } when `other` exits
ctx.demonitor(m);
ctx.exit(other, &ExitReason::Shutdown);
```

Exit reasons: `Normal`, `Shutdown`, `Killed`, `Noproc`, `Noconnection`, `Timeout` (the CPU deadline), and `Crashed(text)`. The text is the trap message, `memory_limit`, or `init_timeout`.

The supervisor restarts an app's instances according to `supervision` (strategy, intensity, backoff). It follows OTP: too many restarts within `period_ms` and it gives up. The app is then marked `failed`, with the reason in `paddock status`.

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

`paddock_sdk::batch` does the buffering. A `Batcher` keeps one batch per destination and returns it when it is full by count or bytes, or, with `max_delay`, from the timer its first item armed:

```rust
use paddock_sdk::batch::{self, Batcher, Limits};

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
use paddock_sdk::{App, Snapshot};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize, Snapshot)]
#[snapshot(version = 2, migrate = migrate)]
struct State {
    count: u64,
    history: Vec<u64>,
}

fn migrate(from: u32, old: paddock_sdk::snapshot::Value) -> Result<State, String> {
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

The other modes need no snapshot: `restart` stops the old version and starts the new one from `init`, and `blue-green` starts the new version beside the old and moves traffic to it in steps (`paddock upgrade --steps`, `paddock traffic`), for a pool with no registered names.

`save_snapshot` also backs `paddock state <pid>`. An operator can read an SDK app's state there as JSON.

## Host services

Each service must be declared under `capabilities` in the manifest **and** granted by the node's operator policy. A component that imports an undeclared interface is refused at bundle and deploy time. A declared but ungranted interface is linked to a stub that returns `permission-denied` (or the interface's own error) at call time, so the app can degrade instead of failing to start.

| Capability | Use from Rust |
| --- | --- |
| `wasi:logging` | `paddock_sdk::info!("order {id} accepted")` (and `trace!`, `debug!`, `warn!`, `error!`). Records carry the app, version and pid. The host caps level and rate (`max_level`, `rate_limit_per_sec`), and the operator can change the level at run time with `paddock log-level`. The host tells the SDK the effective level, so a record below it is skipped before formatting (about 3 ns, against about 440 ns when the host drops it); records written during `init` are filtered by the host only. |
| `wasi:config` | `paddock_sdk::config::get("db.host")` and `get_all()`, with the SDK's `config` feature. Values come from `config.defaults`, overridden by the node's `[config.<app>]`. Without the capability granted they return `ConfigError::Denied`. |
| `wasi:keyvalue` | `paddock_sdk::kv::Bucket::open("orders")`, with the SDK's `keyvalue` feature: `get`, `set`, `delete`, `exists`, `keys` (all pages), `increment`, the batch calls, and CBOR-typed `get_value` and `set_value`. Only the buckets the manifest names open. `memory` buckets survive instance crashes; `disk` buckets also survive node restarts. Each bucket can have a quota. Shared buckets (manifest `shared`, defined by the node and granted by policy) open the same way and hold one copy for every app that lists them: the place for a point table or settings several apps read. A read-only one refuses writes with `KvError::AccessDenied`. With the SDK's `watch` feature, `paddock_sdk::watch` adds, on the same buckets: `get` with the value's revision, `compare_and_swap` and `compare_and_delete` on a revision (a lock-free update: read, then swap, retrying on `Conflict`), and `watch`, `watch_prefix` and `resume`. A watch delivers `kv-changed` host events to `handle_info` (`Change::from_info`) naming the key and revision, not the value; notices are conflated per key, so a slow instance skips intermediate values but always sees the latest. Store the last revision handled to `resume` after a restart; `TooOld` means reread the keys and watch from the revision it carries. Watches end when the instance exits and survive upgrades, so `init` is the place to start them. A shared bucket the node scopes to the cluster is replicated on an HA pair: writes return once committed on the primary, reads and watches use this node's copy, `get_linearizable` reads through the primary, and `Unavailable` means no primary could take the operation (for a write, it may still have committed: reread before retrying a compare-and-swap). |
| `paddock:groups` | `paddock_sdk::groups::join("prices")`, `leave`, `members` and `publish("prices", envelope)`, with the SDK's `groups` feature. `publish` casts one copy to each member on the node (the publisher too, if it is a member) and returns how many mailboxes took it; members receive it in `handle_cast`. The manifest lists the groups an app may join and publish to. An instance leaves its groups when it exits and keeps them across an upgrade, so `init` (not `load_snapshot`) is the place to join. In a cluster, members on every node receive it. |
| `paddock:metrics` | `paddock_sdk::metrics::{Counter, Gauge, Histogram}`, with the SDK's `metrics` feature: make them in `init` (`Counter::new("orders_total", "Orders taken.", &[("region", "eu")])?`) and keep them in state; `inc`, `add`, `set` and `observe` are one host call each. Names are exported as `<prefix>_<name>` with an `app` label, per app: all instances add into the same series. Keep label values few (no ids or timestamps); `max_series` caps them. |
| `paddock:sql` | `paddock_sdk::sql::query("main", "SELECT ... WHERE id = $1", &[id.into()])`, `execute`, and `begin` for a transaction (`commit`, or drop it to roll back), with the SDK's `sql` feature. Name each datasource in the manifest with its `max_connections`; the operator grants it and binds it to a Postgres database. Parameters are bound, never interpolated; columns of types the interface does not carry are refused, so cast them (`col::text`). |
| `paddock:secrets` | `paddock_sdk::secrets::get_string("db-password")` (or `get` for bytes), with the SDK's `secrets` feature. List the names in the manifest; the operator grants each and binds it to a file or variable. Read a secret when it is needed instead of keeping it in state (state goes into snapshots), and never log it. |
| `wasi:http/incoming-handler` | Declare `routes` (and optionally `max_body`, `timeout_ms`); the node's HTTP ingress sends each request on them to the app as a call. With the SDK's `http` feature, `paddock_sdk::http::Request::from_envelope(&msg)` recognises one in `handle_call`; reply `Reply::Now(Response::text(200, "hi").into())` or `Response::json(...)`, or `Reply::Later` and answer with `ctx.reply`. `req.scheme` is `https` when the client came over TLS. The request is spread over the pool, or routed by `key` (a route's `hash_header`) with `instances.routing = "hash:key"`. A full mailbox answers 503, a slow reply 504, a crash 502. See `examples/http-counter`. |
| `wasi:http/outgoing-handler` | `paddock_sdk::http_client::get(url)`, `post`, or `send(&Request::new("PUT", url).header(...).body(...).timeout(...))`, with the SDK's `http-client` feature. Only what the manifest's `allow` lists is reached; anything else is `HttpError::Denied`. Waiting for the response is host time, not counted against the deadline, but the instance handles nothing else meanwhile: use a pool for an app that must stay responsive. Prefer this to raw sockets for HTTP: the node pools connections, verifies TLS and refuses names that resolve to the node itself. |
| `wasi:sockets` | `paddock_sdk::net::TcpStream::connect(addr)` (or `connect_host`), `write_all`, and a non-blocking `read`; `TcpListener::bind` and `accept`, with the SDK's `net` feature. An instance never blocks waiting for bytes between callbacks: `stream.notify_readable(tag)` asks the host to deliver an `io-ready` event with that tag once bytes arrive, handled in `handle_info` (`net::ready_tag(&info)`); read what is there, then ask again. The manifest lists where the app may connect, bind and listen, and `max_sockets`. For a protocol the node does not terminate (MQTT, a database's wire protocol); prefer HTTP egress, listeners or an extension when one fits. UDP goes through the re-exported `wasip2` crate with `net::notify` on its pollables. |
| `wasi:filesystem` | `std::fs`. `/priv` holds the bundle's files read-only (`paddock bundle --priv <dir>`), and each preopen maps a guest path to a volume the operator configures. Paths cannot escape their preopen. |

The SDK's cargo features turn each wrapper on: `config`, `keyvalue`, `watch`, `groups`, `metrics`, `sql`, `secrets`, `http` (requests from the ingress), `http-client`, `net` and `connection`; logging and batches are always there. A component imports only the interfaces of the features it uses, so declare those and no more. An app may also generate its own bindings with `wit-bindgen` from the WIT in [`wit/`](../wit/) and [`wit/vendor/`](../wit/vendor/), as [`crates/paddock-fixture-services`](../crates/paddock-fixture-services/src/lib.rs) does.

Always available without a declaration: clocks, random, stdout and stderr (captured as log records), `paddock:runtime/process`, and `paddock:io` (readiness of pollables).

### Replicated buckets

On an HA pair with `[failover] replication = true`, a bucket declared with `durability = "replicated"` is kept whole on both nodes: a write returns once committed on the primary and its partner, reads and watches use this node's copy, and `get_linearizable` (with the `watch` feature) reads through the primary. It survives losing either node, so it is where a singleton keeps what must survive a failover. A write while no primary can take it fails with `Unavailable`; it may still have committed, so reread before retrying a compare-and-swap. Writes cost about a round trip between the nodes (about 2.3 ms at p50 on loopback, [BASELINE.md](BASELINE.md)), so batch them.

### Singletons on a pair

`placement.mode = "singleton"` runs one instance on the pair's primary. Its callbacks run only while the node holds its lease: out of lease a message waits, and a callback running when the lease ends is stopped with `crashed: lease_ended`, so two nodes never run it at once. After a failover it starts from `init` on the new primary, with what it kept in replicated buckets. In a planned switchover (`paddock failover switchover`), a singleton in `snapshot` upgrade mode on a pair with replication is moved with its state: the old primary takes its snapshot and the new one starts it with `load_snapshot` instead of `init`. Its pid changes and its links, monitors, names and mailbox do not move; callers should look it up by name again.

## Connection handlers

A connection handler serves TCP connections instead of messages: the node accepts each connection on a listener and starts an instance of the handler for it. Enable the SDK's `connection` feature, implement `Connection`, and export it with `paddock_sdk::connection!`:

```rust
use std::io::{BufRead, BufReader, Write};
use paddock_sdk::connection::{Connection, ConnectionInfo, Stream};
use paddock_sdk::Ctx;

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

paddock_sdk::connection!(Lines);
```

- `Stream` implements `std::io::Read` and `Write`; a read of 0 bytes means the client closed.
- Returning closes the connection; an error closes it as a crash. A connection idle for the listener's `idle_timeout_ms` is closed by the host.
- The handler has a `Ctx` for calling and casting to apps (`ctx.whereis("counter")`), but no mailbox. Keep state that must outlive a connection in an app or a keyvalue bucket, and keep the protocol in the handler: [`examples/line-server`](../examples/line-server) forwards `inc` to the counter example.
- Declare the listeners in the manifest (`[[appserver.listeners]]`, see [MANIFEST.md](MANIFEST.md)); the operator binds each to an address in node config.

## HTTP handlers

An unmodified `wasi:http` component, built with any toolchain that targets the `wasi:http/proxy` world, deploys as an HTTP handler: each request on its routes gets a new instance, and request and response bodies stream. In Rust, the `wasip2` crate's `wasip2::http::proxy::export!` exports one. Requests as calls to an app (above) are faster, so prefer them unless the component already exists or a body must stream (S15: about 15% of native throughput for a new instance per request).

- Declare routes under `wasi:http/incoming-handler` as for an app; see [MANIFEST.md](MANIFEST.md#http-handlers) for what else the manifest may hold.
- Nothing survives a request: keep state in an app (call it through `paddock:runtime/process`) or a keyvalue bucket.
- A trap before the response's headers is a 500; after them, the client sees the stream cut.

## Native-process apps

Native code runs as a process the node spawns and supervises like an instance, an Erlang-style port: `kind = "native-process"` in the manifest. It has a pid, a mailbox, links, monitors and supervision, and handles one callback at a time; it can declare only `wasi:logging` for now. The node runs it through `paddock-runner`, which restricts it first: it reads only the system libraries, its own directory and the app's `priv/` files, executes nothing else, opens no sockets, and signals only itself. Linux only.

Two shapes, chosen by `native.program`:

- **`"executable"`:** a program on `paddock-native`'s app API (feature `app`), with the process functions:

  ```rust
  use paddock_native::app::{App, CallRef, Ctx, serve};
  use paddock_native::protocol::Envelope;

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

  `Ctx` has `call`, `cast`, `reply`, `link`, `monitor`, `register`, `whereis`, `send_after`, `exit` and the rest; `paddock_native::app::log` logs from any thread. What the program prints goes to the node's log. It finds the bundle's `priv/` files in the directory `PADDOCK_PRIV` names. It must not read stdin, and a panic in a callback crashes the instance with its message.
- **`"library"`** (default): an existing rustbridge plugin (`.so`), unchanged. Calls and casts reach its `handle_request` with the envelope's schema id as the type tag, in JSON; the reply carries the same schema id, and an error the schema id `rustbridge.error`. It cannot call other apps, since rustbridge's plugin interface has no call into the host.

Limits: `callback_deadline_ms` bounds each callback's wall time (less time spent in requests to the node), and an idle process that misses a ping is killed; both end as `timeout`. `memory_max` is enforced by the node (a cgroup, or `RLIMIT_DATA`). Upgrades are `restart` or `blue-green`; there are no snapshots, so no passivation or moves. Build the program for the platform the node runs on and bundle it as below; [MANIFEST.md](MANIFEST.md#native-process-apps) has the details.

## Bundles

`paddock bundle paddock.toml <program> --key signer.key` writes a signed `.rbp`:

- The program is the component (`.wasm`, built for `wasm32-wasip2`), or for a native-process app the `.so` or executable built for this machine's platform (bundle on the platform the nodes run).
- `--priv <dir>` adds a directory's files, which a component reads at `/priv` and a native app in the directory its environment variable `PADDOCK_PRIV` names.
- The bundle is refused if the component imports an interface the manifest does not declare (`--skip-import-check` writes it anyway, for a node to refuse), and it is read back as a node would before the command succeeds.
- `paddock keygen <prefix>` makes the signing key pair; nodes trust the public key in `trusted_keys`.

## Testing

- **Unit-test the logic natively.** Keep message handling in plain functions, and leave the `App` impl as a thin shell around them.
- **Integration-test against a node.** The `paddock` crate's tests start a node in a temporary directory and drive it through `paddock::api::Client`. [`crates/paddock/tests/exit_criteria.rs`](../crates/paddock/tests/exit_criteria.rs) shows the pattern.
- **Read state while debugging:** `paddock state <pid>`. Note that an instance whose `save_snapshot` traps is restarted.

## Limits to design around

- Calls cost about 2 µs, plus the copy. Paddock suits work that takes tens of microseconds or more per message, not chatty fine-grained calls.
- Compute runs at about 1.0 to 2.4 times native speed, and further behind when native code would use SHA or AES instructions ([spike S8](../spikes/s8/README.md)).
- A full mailbox drops casts and fails calls with `overloaded`. Size `limits.mailbox_max` to absorb bursts, and treat `Overloaded` as back-pressure.
