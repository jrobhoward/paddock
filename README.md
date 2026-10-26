# Paddock

[![CI](https://github.com/jrobhoward/paddock/actions/workflows/ci.yml/badge.svg)](https://github.com/jrobhoward/paddock/actions/workflows/ci.yml)

> **A personal research project, not production software.** The APIs, wire
> formats and bundle format change without notice, and there are no releases
> or compatibility guarantees. Issues, suggestions and feedback are welcome,
> but there is no support, and the project may stop at any time. Use it at
> your own risk.

Paddock is a clustered application server for sandboxed WebAssembly
components, with Erlang/OTP-style semantics:

- **Apps** are WebAssembly components (WASI 0.2) that implement a
  `gen_server`-style set of callbacks and receive messages one at a time.
- **Messaging** is call (request and reply, with a timeout) and cast (fire and
  forget), locally or across nodes.
- **Failure handling** uses links, monitors and supervisors. A crashed or
  runaway app is killed and restarted, and linked apps are told.
- **Hot upgrade** swaps an app's component between messages, carrying state
  through snapshot functions, and reverts if the new version fails to load it.
- **Blue-green upgrades** run both versions side by side and move traffic in
  steps, for apps whose state lives outside the instance.
- **Isolation**: each app gets its own memory, a CPU deadline per callback,
  and only the capabilities its manifest declares and the operator grants.
- **Host services** behind those capabilities: key-value buckets (memory,
  disk, or replicated across a pair), watches, process groups, config,
  secrets, logging, metrics, timers, HTTP ingress and allow-listed HTTP
  egress, Postgres datasources, guest sockets, and a Modbus TCP extension.
- **Connections**: a connection-handler app serves TCP connections on
  listeners the node owns, one sandboxed instance per connection, at native
  throughput (spike S13).
- **Native-process apps** run a native program or rustbridge plugin as a
  restricted process per instance (Linux), supervised like any other app.
- **Distribution** runs over QUIC with mutual TLS, nodes trusting each other
  by pinned public keys (no certificate authority). An HA pair and a witness
  fail over singletons and replicated state, and `paddock release` upgrades
  the pair without downtime.

## Status

Version 0.3.0, a research release: not feature complete, with unstable APIs
and formats. Phases 1 to 3 are done: a single node (`paddock node` deploys
signed bundles and supervises their instances, and the `paddock` CLI manages
it), clusters with an HA pair and a witness for failover, replicated shared
buckets, and releases across the pair without downtime. The docs were
checked against the code for all three on 2026-10-25; `docs/ROADMAP.md` has
what is left. rustbridge's
bundle format 1.2 comes from its `experiment/wasm` branch on GitHub until it
is released.

- [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md): run a node and
  deploy, upgrade and roll back an app.
- [`examples/tutorial`](examples/tutorial/README.md): one node, then a pair
  and a witness with failover, a hot standby, and a database, as scripts.
- [`docs/WRITING_APPS.md`](docs/WRITING_APPS.md): write an app with
  `paddock-sdk`.
- [`docs/MANIFEST.md`](docs/MANIFEST.md): every manifest field, capabilities,
  node configuration and operator policy.
- [`docs/OPERATIONS.md`](docs/OPERATIONS.md): running nodes and pairs:
  failover, upgrades of the builds, backups, the remote admin API.
- [`docs/BASELINE.md`](docs/BASELINE.md): baseline performance numbers.
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
- [`wit/runtime.wit`](wit/runtime.wit): the app contract,
  `paddock:runtime@0.1.0`.
- [`spikes/`](spikes/): the experiments behind the design, each with its
  results.

## Building

Rust 1.99.0 or newer. Guest components build for `wasm32-wasip2`:

```sh
cargo test --workspace --all-features
rustup target add wasm32-wasip2
cargo build -p paddock-fixture-echo --target wasm32-wasip2 --release
```

`paddock`'s cargo features choose what a node can do. The default build has
the node with its HTTP ingress (`http`), HTTPS and the remote admin API
(`tls`), HTTP egress (`egress`), SQL datasources (`sql`) and probes
(`probes`). The Modbus TCP extension is off by default (`--features modbus`).
`--no-default-features --features node` builds a node with none of them, and
`--no-default-features` the CLI and bundle tooling alone, without wasmtime.

## Running a node

A node runs on Linux or macOS: its admin API is a Unix socket, and on
Windows `paddock node` stops at start. Native-process apps need Linux. The
CLI runs anywhere, and reaches a node's remote admin API (`[admin]` in node
config) with `--remote`. A minimal `node.toml`:

```toml
data_dir = "/var/lib/paddock"
trusted_keys = ["RWQ..."]              # minisign public keys of bundle signers
policy = "/etc/paddock/policy.toml"    # which capabilities apps are granted

[volumes]
orders-data = "/srv/paddock/orders"    # host directories behind manifest volumes
```

```sh
paddock node --config node.toml        # admin socket: <data_dir>/admin.sock

export PADDOCK_SOCKET=/var/lib/paddock/admin.sock
paddock deploy orders-1.0.0.rbp
paddock list
paddock status orders
paddock call orders 'ping'
paddock upgrade orders orders-2.0.0.rbp --approve
paddock rollback orders                # --restart: from init, state discarded
paddock ps                             # memory, restarts, last exit per instance
paddock call --pid 0.12.1@1 'ping'     # one instance; or --name <registered name>
paddock state 0.12.1@1                 # an instance's state, via save-snapshot
paddock kill 0.12.1
paddock logs orders -n 50              # -f: then follow
paddock events                         # audit records, exits, members, failover
paddock shell                          # these commands at a prompt
paddock log-level orders debug         # until `paddock log-level orders reset`
paddock stop orders && paddock start orders
paddock remove orders
```

Every command takes `--json`. The API is JSON over HTTP, so `curl
--unix-socket /var/lib/paddock/admin.sock http://paddock/v1/apps` works too;
the `paddock::api` module documents the routes. Deploys, upgrades and refusals are recorded
in `<data_dir>/audit.log`. Unix socket paths are limited to about 107 bytes;
set `admin_socket` if `data_dir` is long.

## Writing an app

Apps are written with `paddock-sdk` and built for `wasm32-wasip2`; see
[`examples/`](examples/): `counter` and `ping-pong` to start, `http-counter`,
`board` (state in a shared disk bucket), `ledger` (a replicated bucket),
`tally` (Postgres), `line-server` (a connection handler), `greeter` (a host
extension built into a node) and `scada`, the reference app.

```rust
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};

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

paddock_sdk::app!(Counter);
```

The app's `paddock.toml` holds its name, version and `appserver` section
(limits, capabilities, upgrade mode). To build, key and bundle it:

```sh
cargo build -p example-counter --target wasm32-wasip2 --release
paddock keygen signer                   # signer.key, signer.pub; prints the public key
paddock bundle examples/counter/paddock.toml \
  target/wasm32-wasip2/release/example_counter.wasm --key signer.key
paddock deploy counter-1.0.0.rbp
```

`paddock bundle` refuses a component that imports an interface its manifest
does not declare. The signing key's password comes from
`PADDOCK_KEY_PASSWORD`, or a prompt.

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
