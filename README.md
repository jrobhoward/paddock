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

Pre-release. The design and the Phase 1 spikes are done, and milestone M0
(workspace, CI, the WIT contract) is in place. There is no runnable server yet.

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
