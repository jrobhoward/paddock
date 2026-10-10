# Paddock

[![CI](https://github.com/jrobhoward/paddock/actions/workflows/ci.yml/badge.svg)](https://github.com/jrobhoward/paddock/actions/workflows/ci.yml)

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
- **Connections**: a connection-handler app serves TCP connections on
  listeners the node owns, one sandboxed instance per connection, at native
  throughput (spike S13).
- **Isolation**: each app gets its own memory, a CPU deadline per callback,
  and only the capabilities its manifest declares and the operator grants.
- **Distribution** runs over QUIC with mutual TLS, nodes trusting each other by pinned public keys (no certificate authority).

## Status

Pre-release. A single node runs: `paddock node` deploys signed bundles and
supervises their instances, and the rest of the `paddock` CLI manages it. Phase 1
milestones M0 to M9 are done. rustbridge's bundle format 1.2 comes from its
`experiment/wasm` branch on GitHub until it is released.

- [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md): run a node and
  deploy, upgrade and roll back an app.
- [`docs/WRITING_APPS.md`](docs/WRITING_APPS.md): write an app with
  `paddock-sdk`.
- [`docs/MANIFEST.md`](docs/MANIFEST.md): every manifest field, capabilities,
  node configuration and operator policy.
- [`docs/BASELINE.md`](docs/BASELINE.md): baseline performance numbers.
- [`docs/OBSERVABILITY.md`](docs/OBSERVABILITY.md): instance stats, calling
  one instance, and tracing a live node with its probes.
- [`docs/DESIGN.md`](docs/DESIGN.md): architecture, app model, manifest and
  capability model, failure handling, upgrades, distribution, security, risks.
- [`docs/ROADMAP.md`](docs/ROADMAP.md): status, remaining work and its
  order.
- [`wit/runtime.wit`](wit/runtime.wit): the app contract,
  `paddock:runtime@0.1.0`.
- [`spikes/`](spikes/): the experiments behind the design, each with its
  results.

## Building

Rust 1.96.0 or newer. Guest components build for `wasm32-wasip2`:

```sh
cargo test --workspace
rustup target add wasm32-wasip2
cargo build -p paddock-fixture-echo --target wasm32-wasip2 --release
```

## Running a node

Linux or macOS: the admin API is a Unix socket, and Windows support is not in
Phase 1. A minimal `node.toml`:

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
paddock logs orders -n 50
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
[`examples/`](examples/) (`counter`, `ping-pong`, `chaos`).

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
