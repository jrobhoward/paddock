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
- **Isolation**: each app gets its own memory, a CPU deadline per callback,
  and only the capabilities its manifest declares and the operator grants.
- **Distribution** runs over QUIC with mutual TLS (Phase 2).

## Status

Pre-release. A single node runs: `paddock-node` deploys signed bundles and
supervises their instances, and the `paddock` CLI manages it. Phase 1
milestones M0 to M7 are done; the guest SDK (M8) and the exit test suite (M9)
remain. Until rustbridge's bundle format 1.2 is published, the workspace builds
only next to a rustbridge checkout on its `bundle-1.2` branch, so CI is red.

- [`docs/DESIGN.md`](docs/DESIGN.md): architecture, app model, manifest and
  capability model, failure handling, upgrades, distribution, security, risks.
- [`docs/PHASE1_PLAN.md`](docs/PHASE1_PLAN.md): Phase 1 exit criteria and
  milestones.
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
paddock-node --config node.toml        # admin socket: <data_dir>/admin.sock

export PADDOCK_SOCKET=/var/lib/paddock/admin.sock
paddock deploy orders-1.0.0.rbp
paddock list
paddock status orders
paddock call orders 'ping'
paddock upgrade orders orders-2.0.0.rbp --approve
paddock rollback orders
paddock ps
paddock kill 0.12.1
paddock logs orders -n 50
paddock stop orders && paddock start orders
paddock remove orders
```

Every command takes `--json`. The API is JSON over HTTP, so `curl
--unix-socket /var/lib/paddock/admin.sock http://paddock/v1/apps` works too;
`paddock-api` documents the routes. Deploys, upgrades and refusals are recorded
in `<data_dir>/audit.log`. Unix socket paths are limited to about 107 bytes;
set `admin_socket` if `data_dir` is long. Producing bundles is M8's job; until
then they are built with rustbridge's `BundleBuilder`.

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
