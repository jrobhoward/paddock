# Writing an app

A Paddock app is a WebAssembly component that implements the `paddock:runtime/app` world ([`wit/runtime.wit`](../wit/runtime.wit)). In Rust, `paddock-sdk` hides the bindings: you implement the `App` trait and export it with `app!`. Apps in other languages implement the WIT directly; Phase 1 ships only the Rust SDK.

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

`save_snapshot` also backs `paddock state <pid>`. An operator can read an SDK app's state there as JSON.

## Host services

Each service must be declared under `capabilities` in the manifest **and** granted by the node's operator policy. A component that imports an undeclared interface is refused at bundle and deploy time. A declared but ungranted interface is linked to a stub that returns `permission-denied` (or the interface's own error) at call time, so the app can degrade instead of failing to start.

| Capability | Use from Rust |
| --- | --- |
| `wasi:logging` | `paddock_sdk::info!("order {id} accepted")` (and `trace!`, `debug!`, `warn!`, `error!`). Records carry the app, version and pid. The host caps level and rate (`max_level`, `rate_limit_per_sec`), and the operator can change the level at run time with `paddock log-level`. The host tells the SDK the effective level, so a record below it is skipped before formatting (about 3 ns, against about 440 ns when the host drops it); records written during `init` are filtered by the host only. |
| `wasi:config` | `paddock_sdk::config::get("db.host")` and `get_all()`, with the SDK's `config` feature. Values come from `config.defaults`, overridden by the node's `[config.<app>]`. Without the capability granted they return `ConfigError::Denied`. |
| `wasi:keyvalue` | `paddock_sdk::kv::Bucket::open("orders")`, with the SDK's `keyvalue` feature: `get`, `set`, `delete`, `exists`, `keys` (all pages), `increment`, the batch calls, and CBOR-typed `get_value` and `set_value`. Only the buckets the manifest names open. `memory` buckets survive instance crashes; `disk` buckets also survive node restarts. Each bucket can have a quota. Shared buckets (manifest `shared`, defined by the node and granted by policy) open the same way and hold one copy for every app that lists them: the place for a point table or settings several apps read. A read-only one refuses writes with `KvError::AccessDenied`. With the SDK's `watch` feature, `paddock_sdk::watch` adds, on the same buckets: `get` with the value's revision, `compare_and_swap` and `compare_and_delete` on a revision (a lock-free update: read, then swap, retrying on `Conflict`), and `watch`, `watch_prefix` and `resume`. A watch delivers `kv-changed` host events to `handle_info` (`Change::from_info`) naming the key and revision, not the value; notices are conflated per key, so a slow instance skips intermediate values but always sees the latest. Store the last revision handled to `resume` after a restart; `TooOld` means reread the keys and watch from the revision it carries. Watches end when the instance exits and survive upgrades, so `init` is the place to start them. |
| `paddock:groups` | `paddock_sdk::groups::join("prices")`, `leave`, `members` and `publish("prices", envelope)`, with the SDK's `groups` feature. `publish` casts one copy to each member on the node (the publisher too, if it is a member) and returns how many mailboxes took it; members receive it in `handle_cast`. The manifest lists the groups an app may join and publish to. An instance leaves its groups when it exits and keeps them across an upgrade, so `init` (not `load_snapshot`) is the place to join. Members on other nodes arrive with clustering (ROADMAP D4). |
| `paddock:secrets` | `paddock_sdk::secrets::get_string("db-password")` (or `get` for bytes), with the SDK's `secrets` feature. List the names in the manifest; the operator grants each and binds it to a file or variable. Read a secret when it is needed instead of keeping it in state (state goes into snapshots), and never log it. |
| `wasi:http/incoming-handler` | Declare `routes` (and optionally `max_body`, `timeout_ms`); the node's HTTP ingress sends each request on them to the app as a call. With the SDK's `http` feature, `paddock_sdk::http::Request::from_envelope(&msg)` recognises one in `handle_call`; reply `Reply::Now(Response::text(200, "hi").into())` or `Response::json(...)`, or `Reply::Later` and answer with `ctx.reply`. The request is spread over the pool, or routed by `key` (a route's `hash_header`) with `instances.routing = "hash:key"`. A full mailbox answers 503, a slow reply 504, a crash 502. See `examples/http-counter`. |
| `wasi:filesystem` | `std::fs`. `/priv` holds the bundle's files read-only (`paddock bundle --priv <dir>`), and each preopen maps a guest path to a volume the operator configures. Paths cannot escape their preopen. |

The SDK wraps logging only. For config and keyvalue, generate bindings with `wit-bindgen` from the WIT vendored in [`wit/vendor/`](../wit/vendor/), as [`crates/paddock-fixture-services`](../crates/paddock-fixture-services/src/lib.rs) does. That fixture implements `paddock:runtime/app` directly, without the SDK; combining the SDK with separately generated WASI bindings is untested in Phase 1.

Always available without a declaration: clocks, random, stdout and stderr (captured as log records), and `paddock:runtime/process`.

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

## Testing

- **Unit-test the logic natively.** Keep message handling in plain functions, and leave the `App` impl as a thin shell around them.
- **Integration-test against a node.** The `paddock` crate's tests start a node in a temporary directory and drive it through `paddock::api::Client`. [`crates/paddock/tests/exit_criteria.rs`](../crates/paddock/tests/exit_criteria.rs) shows the pattern.
- **Read state while debugging:** `paddock state <pid>`. Note that an instance whose `save_snapshot` traps is restarted.

## Limits to design around

- Calls cost about 2 µs, plus the copy. Paddock suits work that takes tens of microseconds or more per message, not chatty fine-grained calls.
- Compute runs at about 1.0 to 2.4 times native speed, and further behind when native code would use SHA or AES instructions ([spike S8](../spikes/s8/README.md)).
- A full mailbox drops casts and fails calls with `overloaded`. Size `limits.mailbox_max` to absorb bursts, and treat `Overloaded` as back-pressure.
