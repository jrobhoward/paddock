# Bulkhead

[![CI](https://github.com/jrobhoward/bulkhead/actions/workflows/ci.yml/badge.svg)](https://github.com/jrobhoward/bulkhead/actions/workflows/ci.yml)

> **A personal research project, not production software.** The APIs, wire
> formats and bundle format change without notice, and there are no releases
> or compatibility guarantees. Issues, suggestions and feedback are welcome,
> but there is no support, and the project may stop at any time. Use it at
> your own risk.

> Bulkhead is a fork of Paddock (commit 657ae61, 2026-10-31) that runs
> apps as native OS processes instead of WebAssembly components. Components
> were removed on 2026-11-08 (`docs/ROADMAP.md`, Z9).
> [`docs/PADDOCK.md`](docs/PADDOCK.md) compares the two.

Bulkhead is a clustered application server that runs each app instance as
a restricted native process, with Erlang/OTP-style semantics:

- **Apps** implement a `gen_server`-style set of callbacks and receive
  messages one at a time. Each instance is one long-lived OS process
  (Linux), started through `bulkhead-runner`, which fences it with
  Landlock and seccomp before the app's code runs; it talks to its node
  through rings in shared memory, each side spinning briefly before it
  sleeps, so a request to the node costs 4 to 5 µs and a call between
  instances 13 µs. Instances are meant to live long and pass messages and
  reach databases efficiently, not to be spawned by the thousand as
  Erlang's processes are.
- **Messaging** is call (request and reply, with a timeout) and cast (fire and
  forget), locally or across nodes. Mailboxes are bounded, so a cast to a full
  one is dropped; an app can instead ask whether a cast was queued, or
  wait for room ([WRITING_APPS.md](docs/WRITING_APPS.md#casts-that-must-arrive)).
- **Failure handling** uses links, monitors and supervisors. A crashed or
  runaway app is killed and restarted, and linked apps are told.
- **Hot upgrade** moves an instance to a new version's process between
  messages, carrying state through snapshot functions, and reverts if the new
  version fails to load it (rustbridge plugins upgrade by restart or
  blue-green).
- **Blue-green upgrades** run both versions side by side and move traffic in
  steps, for apps whose state lives outside the instance.
- **Isolation**: each instance is its own process with a memory limit, a
  deadline per callback, and only the
  capabilities its manifest declares and the operator grants.
- **Host services** behind those capabilities: key-value buckets (memory,
  disk, or replicated across a pair), watches, process groups, config,
  secrets, logging, metrics, timers, HTTP ingress and allow-listed HTTP
  egress, Postgres datasources, sockets, and a Modbus TCP extension. A
  shared bucket can be published: apps then read it from a snapshot mapped into their own memory, in 0.16 to
  0.39 µs a read, without asking the node.
- **Connections**: a connection handler serves TCP connections on
  listeners the node owns, each instance of its pool holding many.
- **Distribution** runs over QUIC with mutual TLS, nodes trusting each other
  by pinned public keys (no certificate authority). An HA pair and a witness
  fail over singletons and replicated state, and `bulkhead release` upgrades
  the pair without downtime.

## Status

Version 0.3.0, a research release: not feature complete, with unstable APIs
and formats. Phases 1 to 3 are done: a single node (`bulkhead node` deploys
signed bundles and supervises their instances, and the `bulkhead` CLI manages
it), clusters with an HA pair and a witness for failover, replicated shared
buckets, and releases across the pair without downtime. The docs were
checked against the code for all three on 2026-10-25. Phase 5's exit,
native apps, was met on 2026-11-01; `docs/DESIGN.md` ("Instance model")
says why a process per instance, and `docs/ROADMAP.md` has what is left.
The same day, a comparison with BEAM showed native apps furthest behind
within a node, and a shared-memory channel and published
buckets brought a call between native instances from 41 to 13 µs and a
read of read-mostly data into ETS's range (`docs/BASELINE.md`). rustbridge's
bundle format 1.2 comes from its `experiment/wasm` branch on GitHub until it
is released.

- [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md): run a node and
  deploy, upgrade and roll back an app.
- [`examples/tutorial`](examples/tutorial/README.md): one node, then a pair
  and a witness with failover, a hot standby, and a database, as scripts.
- [`docs/WRITING_APPS.md`](docs/WRITING_APPS.md): write an app with
  `bulkhead-sdk`.
- [`docs/MANIFEST.md`](docs/MANIFEST.md): every manifest field, capabilities,
  node configuration and operator policy.
- [`docs/OPERATIONS.md`](docs/OPERATIONS.md): running nodes and pairs:
  failover, upgrades of the builds, backups, the remote admin API.
- [`docs/BASELINE.md`](docs/BASELINE.md): baseline performance numbers,
  and against Erlang/OTP.
- [`docs/PADDOCK.md`](docs/PADDOCK.md): how Bulkhead differs from Paddock,
  what it costs, and when to choose which.
- [`docs/GLOSSARY.md`](docs/GLOSSARY.md): the terms the docs and code use.
- [`docs/OBSERVABILITY.md`](docs/OBSERVABILITY.md): instance stats, calling
  one instance, and tracing a live node with its probes.
- [`docs/DESIGN.md`](docs/DESIGN.md): architecture, app model, manifest and
  capability model, failure handling, upgrades, distribution, security, risks.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): the code as built: crates,
  data paths, threads and tasks, where state lives.
- [`docs/SECURITY.md`](docs/SECURITY.md): the threat model, each boundary's
  controls and gaps, and the fuzz targets.
- [`docs/ROADMAP.md`](docs/ROADMAP.md): status, remaining work and its
  order.
- [`protocol.rs`](crates/bulkhead-native/src/protocol.rs): the app contract.
- [`spikes/`](spikes/): the experiments behind the design, each with its
  results.

## Building

Rust 1.99.0 or newer. A node runs native apps through `bulkhead-runner`
(Linux), by default the one beside the `bulkhead` executable, so build the
two together:

```sh
cargo test --workspace --all-features
cargo build --release -p bulkhead -p bulkhead-native   # bulkhead, bulkhead-runner
```

`bulkhead`'s cargo features choose what a node can do. The default build has
the node with its HTTP ingress (`http`), HTTPS and the remote admin API
(`tls`), HTTP egress (`egress`), SQL datasources (`sql`) and probes
(`probes`). The Modbus TCP extension is off by default (`--features modbus`).
`--no-default-features --features node` builds a node with none of them, and
`--no-default-features` the CLI and bundle tooling alone.

## Running a node

A node runs apps on Linux: each is a process started through
`bulkhead-runner`. The
CLI runs anywhere, and reaches a node's remote admin API (`[admin]` in node
config) with `--remote`. A minimal `node.toml`:

```toml
data_dir = "/var/lib/bulkhead"
trusted_keys = ["RWQ..."]              # minisign public keys of bundle signers
policy = "/etc/bulkhead/policy.toml"    # which capabilities apps are granted

[volumes]
orders-data = "/srv/bulkhead/orders"    # host directories behind manifest volumes
```

```sh
bulkhead node --config node.toml        # admin socket: <data_dir>/admin.sock

export BULKHEAD_SOCKET=/var/lib/bulkhead/admin.sock
bulkhead deploy orders-1.0.0.rbp
bulkhead list
bulkhead status orders
bulkhead call orders 'ping'
bulkhead upgrade orders orders-2.0.0.rbp --approve
bulkhead rollback orders                # --restart: from init, state discarded
bulkhead ps                             # memory, restarts, last exit per instance
bulkhead call --pid 0.12.1@1 'ping'     # one instance; or --name <registered name>
bulkhead state 0.12.1@1                 # an instance's state, via save-snapshot
bulkhead kill 0.12.1
bulkhead logs orders -n 50              # -f: then follow
bulkhead events                         # audit records, exits, members, failover
bulkhead shell                          # these commands at a prompt
bulkhead log-level orders debug         # until `bulkhead log-level orders reset`
bulkhead stop orders && bulkhead start orders
bulkhead remove orders
```

Every command takes `--json`. The API is JSON over HTTP, so `curl
--unix-socket /var/lib/bulkhead/admin.sock http://bulkhead/v1/apps` works too;
the `bulkhead::api` module documents the routes. Deploys, upgrades and refusals are recorded
in `<data_dir>/audit.log`. Unix socket paths are limited to about 107 bytes;
set `admin_socket` if `data_dir` is long.

## Writing an app

Apps are written with `bulkhead-sdk` and built as native programs; see [`examples/`](examples/): `counter` and `ping-pong` to start, `http-counter`,
`board` (state in a shared disk bucket), `ledger` (a replicated bucket),
`tally` (Postgres), `line-server` (a connection handler), `greeter` (a host
extension built into a node) and `scada`, the reference app.

```rust
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

#[derive(Default)]
struct Counter { count: u64 }

impl App for Counter {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        if msg.as_text() == Some("inc") {
            self.count += 1;
        }
        Reply::Now(Envelope::text(self.count.to_string()))
    }
}

bulkhead_sdk::app!(Counter);
```

The app's `bulkhead.toml` holds its name, version and `appserver` section
(limits, capabilities, upgrade mode). The crate has a binary that calls
the `bulkhead_main()` that `app!` generates. To build, key and bundle it:

```sh
cargo build -p example-counter --release
bulkhead keygen signer                   # signer.key, signer.pub; prints the public key
bulkhead bundle examples/counter/bulkhead.toml \
  target/release/example-counter --key signer.key
bulkhead deploy counter-1.0.0.rbp
```

The signing key's password comes from
`BULKHEAD_KEY_PASSWORD`, or a prompt.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
