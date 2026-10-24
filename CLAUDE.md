# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Paddock is a clustered application server for sandboxed WebAssembly
components, with Erlang/OTP-style semantics: mailboxes, call/cast, links,
monitors, supervisors and hot upgrade with explicit state migration. Nodes
connect over QUIC with mutual TLS. Apps are WebAssembly components (WASI 0.2)
that see only the capabilities their manifest declares and an operator grants.
Bundles use the rustbridge `.rbp` format.

Phase 1 (single node) is complete. `docs/ROADMAP.md` is the single plan for
remaining work: tracks, their order, and the code areas each owns. Read these
before changing anything structural:

- `docs/DESIGN.md`: the design, prior art, spike findings and risks.
- `docs/ARCHITECTURE.md`: where things are: crates, the paths of a call, a
  deploy and a failover, threads and tasks, and where state lives.
- `docs/SECURITY.md`: the threat model and its open findings.
- `docs/ROADMAP.md`: open work only, in stages, with code areas, decisions
  and spike status. Keep a change inside its item's areas; record anything
  outside them in the roadmap instead of widening the change.
- `spikes/*/README.md`: what each spike measured and what it changed.

## Local machines

Details of the machines this checkout tests on (the Raspberry Pi 5 for the ARM
baselines and the serial line: host, logins, toolchain) are in `CLAUDE.local.md`, which is
gitignored. Read it before work on real hardware. Never copy hostnames,
logins or addresses from it into tracked files or commit messages.

## Commands

```bash
# Every check below as one command, one line of result per check; logs in
# target/check/. `--quick` runs fmt, clippy, conventions and the tests; named
# checks run alone (`--list`). CI calls the same script.
scripts/check.sh
scripts/check.sh --quick
scripts/check.sh clippy guests

# Soak (not in CI): simulation seeds, failover between node processes under
# faults, links and monitors across three nodes, and the chaos app, in turn;
# each failure keeps a run directory under target/soak/ with its seed and
# logs.
scripts/soak.sh --hours 1

# Coverage per file (cargo install cargo-llvm-cov once). Tests that start
# `paddock node` as its own process are not counted.
cargo llvm-cov --workspace --no-fail-fast --summary-only

# Debug and test builds carry line tables only (backtraces keep file and
# line); `CARGO_PROFILE_DEV_DEBUG=true` for a debugger's variables.
# Build and test the main workspace. `paddock`'s features: `node`, `http`,
# `tls`, `sql`, `egress` and `probes` by default; `modbus` off. Each optional
# one compiles out (`scripts/check.sh features`).
cargo build --workspace --all-targets
cargo test --workspace --all-features
cargo test --workspace -- some____test____name     # single test

# Lint (must be clean before any change is considered done). CI will run the
# latest stable clippy, which adds lints each release; `rustup update stable`
# first so a lagging local toolchain doesn't pass code CI rejects.
cargo clippy --workspace --all-targets -- -Dwarnings
cargo fmt --all -- --check

# Rustdoc, with broken intra-doc links treated as errors. Set on this command
# only, never in a shared `env:` block (see "Never set RUSTFLAGS" below).
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Secrets and personal data: tracked files, plus (--push) the lines added by
# commits not yet pushed. Run before every push. Optional pre-push hook:
#   ln -s ../../scripts/check-secrets.sh .git/hooks/pre-push
scripts/check-secrets.sh --push

# Supply chain: run before adding or updating any dependency.
cargo deny check licenses bans sources advisories

# MSRV: the floor is wasmtime's (rust-version 1.96.0). Needs
# `rustup toolchain install 1.96.0` once.
cargo +1.96.0 check --workspace --all-targets

# Guest components (examples, test fixtures) build for WASI 0.2. Needs
# `rustup target add wasm32-wasip2` once. CI checks each one is a valid
# component exporting the app world (`cargo install wasm-tools` once).
cargo build -p paddock-fixture-echo --target wasm32-wasip2 --release
wasm-tools component wit target/wasm32-wasip2/release/paddock_fixture_echo.wasm

# paddock-host's tests build the guest fixtures themselves on first use, into
# target/fixtures (the running `cargo test` holds the lock on target/), so
# `cargo test --workspace` needs the wasm32-wasip2 target installed.
# paddock-bundle's fixture test predates that and is #[ignore]d; CI's guest
# job runs it.
cargo test -p paddock-bundle --test fixture_echo -- --ignored
```

**rustbridge comes from GitHub.** `rustbridge-bundle` is a git dependency on
rustbridge's `experiment/wasm` branch (bundle format 1.2), pinned by
`Cargo.lock`; `cargo update -p rustbridge-bundle` picks up new commits. For
changes to rustbridge itself, work in a checkout of that branch under
rustbridge's own `CLAUDE.md`, push it, then update the lock. A temporary
`[patch."https://github.com/jrobhoward/rustbridge"]` pointing at the local
checkout lets both repos be iterated together; don't commit it.

Fuzz targets are a separate workspace under `fuzz/` (cargo-fuzz, nightly);
`fuzz/README.md` has the commands.

Spikes are a separate workspace under `spikes/` with their own lock file. Each
`spikes/<name>/README.md` has its exact build and run commands.

## Repository layout

- `crates/paddock`: the `paddock` binary and its library. `api`: the admin
  API's types and Unix-socket client. `node` (default `node` feature): the
  node daemon (config, deploy store, admin server, audit log, log ring), run
  by `paddock node`. `src/cli/`: the binary's own subcommands (bundle
  building, output formatting, the node runner). Without the `node` feature
  the crate does not build wasmtime. `tests/wit_rules.rs` enforces the rules
  of `wit/runtime.wit`; the node and CLI tests require the `node` feature.
- `crates/paddock-bundle`: no wasmtime. `manifest`: the app manifest model
  (`AppSpec` is the bundle's `appserver` section; `AppManifest` adds
  identity, component and provenance), validation and capability/import
  classification. The rest opens and verifies `.rbp` bundles, reads
  component imports, operator policy and the effective grant.
- `crates/paddock-host`: the runtime: engine and epoch ticker, pids and the
  process table, instances and their callback loop, links and monitors,
  supervisors, and app sets with pools. `src/services/`: host services, one
  module each on the `Service` pattern (DESIGN.md, "Host service pattern");
  a new service goes there, not into `state.rs` or `runtime.rs`.
  `src/probe.rs`: USDT probes (anyprobe, `probes` feature); host code calls
  its functions, never anyprobe directly.
- `crates/paddock-sdk`, `crates/paddock-sdk-macros`: the Rust guest SDK and
  its `Snapshot` derive. The SDK owns the wit-bindgen bindings (with
  `default_bindings_module`), so app crates have no WIT.
- `examples/`: apps written with the SDK, each with a `paddock.toml`.
  `paddock`'s `tests/examples.rs` builds, bundles, deploys and drives them.
  `examples/greeter/` is an extension (a host service from outside
  Paddock): `service` depends on `paddock-host` alone, `app` calls it, and
  `node` is a node binary with it, whose test runs all three.
  `examples/board` is the tutorial's app (a counter on a shared disk
  bucket over HTTP; `paddock.toml`, `watch.toml` and `hot-standby.toml`, one
  component), run by `crates/paddock/tests/board.rs`; `examples/tutorial/` is its chapter
  scripts (`common.sh` shared), not a crate. `examples/tally` keeps a
  counter in Postgres (`paddock:sql`, tutorial chapter 4), run by
  `crates/paddock/tests/tally.rs` against a scratch Postgres
  (`crates/paddock-host/tests/common/pg.rs`, shared by path).
  `examples/ledger` puts and gets text entries in a shared bucket, by
  `paddock call`: the example for `cluster` buckets, driven by
  `tests/examples.rs` and written through by the replication soak.
  `examples/failover/` is a demo script (a witness and a pair as three
  `paddock node` processes, the primary killed), not a crate.
  `examples/tracing/` is a demo script and bpftrace scripts for the
  probes, not a crate.
- `crates/paddock-fixture-chaos`: a guest that traps, spins, allocates or
  fails on request, and drives every `process` function; the host's tests
  run against it.
- `crates/paddock-fixture-sdk`: a guest that uses `wasi:config`,
  `wasi:keyvalue`, `paddock:groups`, `wasi:http/outgoing-handler` and the
  other services through `paddock-sdk`'s wrappers (`config`, `kv`,
  `groups`, `http_client`, ...). Node tests that deploy it declare every
  capability it imports.
- `crates/paddock-fixture-services`: a guest that drives logging, config,
  keyvalue, the filesystem and stdout. Its `denied` feature adds an import no
  host implements, for deny-stub tests; nodes refuse that build, so node tests
  build it without the feature into `target/fixtures/services-plain`.
- `crates/paddock-fixture-net`: a guest that connects, listens and binds
  UDP through `paddock-sdk`'s `net` module and waits on its sockets with
  `paddock:io` (`paddock-host`'s `tests/sockets.rs`).
- `crates/paddock-native`: native-process apps: `protocol`
  (CBOR frames between a node and a native app), `app` (feature `app`: an
  executable's App trait and process functions) and `runner` (feature
  `runner`, default; Linux): `paddock-runner`, which restricts itself
  (Landlock, seccomp, non-dumpable) before loading a rustbridge plugin or
  executing the program. paddock-host depends on it without default
  features (the protocol only). The host side is
  `crates/paddock-host/src/native/` (the port, cgroups, conversions) and
  `src/instance_native.rs` (the lifecycle, a child module of `instance`).
- `crates/paddock-fixture-native` (an executable on the app API) and
  `crates/paddock-fixture-plugin` (a rustbridge `.so`): native test apps,
  built natively into `target/fixtures/native` by the tests that use them.
  The plugin's `[package.metadata.paddock] native-library = true` keeps
  `scripts/check.sh guests` from building it for wasm32.
- `crates/paddock-fixture-counter`: a stateful counter built three ways by
  feature (v1, v2, a broken v3) for upgrade and passivation tests
  (`tests/passivation.rs`, `examples/passivation_cost.rs`); each version
  builds into its own target directory.
- `crates/paddock-fixture-echo`: the smallest app implementing the `app`
  world; a guest test fixture. Generated bindings live in a `bindings` module
  with a scoped `#[allow(missing_docs, clippy::too_many_arguments)]`. 
- `wit/`: the `paddock:runtime` WIT package, versioned. Its rules are enforced
  by `crates/paddock/tests/wit_rules.rs`. `wit/connection/`: the
  `paddock:connection` package (connection handlers), separate because it
  uses `wasi:io` streams, which `paddock:runtime` excludes. `wit/groups/`:
  the `paddock:groups` package (process groups), and `wit/watch/`:
  `paddock:watch` (keyvalue revisions, compare-and-swap, watches), and
  `wit/secrets/`, `wit/metrics/` and `wit/sql/` (SQL datasources), separate
  so the app world stays frozen; every host bindgen lists all of them,
  since `crates/paddock-host/wit` imports them. The SQL service
  (`src/services/sql`, cargo feature `sql`) runs Postgres through
  `tokio-postgres` and `deadpool-postgres`; its tests start a scratch
  Postgres as the current user and skip when none is installed. `wit/io/`:
  `paddock:io` (readiness of a pollable as a host event), bound on its own
  (`src/ready.rs`, the SDK's `net`) since it uses `wasi:io` types; guest
  sockets are `src/services/sockets` (`wasi:sockets` narrowed, the count
  kept by redefined linker functions). `wit/vendor/`
  holds unmodified copies of the WASI interfaces Paddock binds against
  (keyvalue, config, logging, and `wasi:io` for connections); bindings name
  each package path explicitly.
- `crates/paddock-host/src/delivery.rs`: the one place a pid is routed
  (local, dead, or another node). Process-core code that names a pid calls
  it, never the process table directly. `src/transport/`: frames between
  nodes, the `Transport` trait, `liveness` (heartbeats and sessions; every
  node-down goes through `transport::end_session`), `membership` (joining,
  members, node creation numbers), and `memory`, the
  in-process mesh with fault injection that `tests/distribution.rs` runs on;
  `quic`, the real transport (pinned keys in `quic/identity.rs`, one
  connection per pair, `tests/quic.rs`), and `wire`, the versioned CBOR
  format. `tests/simulation/` runs
  the mesh on a paused clock: targeted races and seeded random scenarios
  (`PADDOCK_SIM_SEED=<n>` replays one).
- `crates/paddock-host/src/failover/`: an HA pair's roles, epochs and
  leases (`node.rs`: one type per standing; transitions consume the state
  and return the next, named after the actions in `docs/tla/FailoverL2.tla`)
  and the witness (`witness.rs`). The rules are R1 to R13 in
  `docs/tla/VERIFICATION.md`; a change to a transition means updating the
  matching action in the model and rerunning it. `tests/simulation/failover.rs` logs `may_run` and checks that a
  singleton never runs on both nodes. On the node, `crates/paddock/src/node/gate.rs`
  starts and kills `singleton` apps as `may_run` changes;
  `crates/paddock/tests/failover.rs` runs a pair and a witness over QUIC.
- `crates/paddock-host/src/replication/`: `cluster` shared buckets on the
  HA pair. `core.rs` is the replica as a state machine whose
  transitions are named after spike S20's actions
  (`spikes/s20/PairReplication.tla`); `log.rs` the log, in the keyvalue
  redb file; `mod.rs` the task around them. The rules are S20's R1 to R7
  and R13 in `docs/tla/VERIFICATION.md`; `tests/simulation/replication.rs`
  checks that no acknowledged write is lost under single faults.
- `crates/paddock-modbus`: the Modbus TCP extension (`paddock-modbus:modbus`,
  WIT in its `wit/`), depending on `paddock-host` alone; linked into
  `paddock node` by `paddock`'s `modbus` feature (off by default). Device tasks,
  change detection and the update format (`points`), the server.
  `examples/scada` is its demo and Phase 3's reference app (`points` pool,
  `hmi` HTTP pool, `control` singleton in two versions, `drill.sh`, the
  release drill on a pair and a witness); `crates/paddock/tests/scada.rs`
  runs it on one node and on a pair and a witness against a fake device, and
  `crates/paddock-modbus/examples/fake_device.rs` serves fake devices to
  the drill.
- `crates/paddock-host/src/metrics.rs`: app metrics (per app, lock-free
  series) and the host's per-app counters, rendered as Prometheus text;
  the `paddock:metrics` service is `src/services/metrics`. Metric names
  are an interface listed in `docs/OBSERVABILITY.md`.
- `crates/paddock-host/src/http.rs`: the HTTP ingress (node-level routes,
  hyper listeners, each request a call to the owning app's pool; the
  request and response envelope formats). `examples/http-counter` is its
  example, `crates/paddock/tests/http.rs` runs it through a node.
- `crates/paddock-host/src/listener.rs`: listeners and connection handlers,
  an instance per connection. `crates/paddock-fixture-conn` is its test
  guest; `examples/line-server` the SDK example.
- `crates/paddock-host/src/http/handler.rs`: HTTP handlers, an unmodified
  `wasi:http/proxy` component with an instance per request on its routes
  (needs the `http` and `egress` features); the node side is
  `crates/paddock/src/node/daemon/handlers.rs`. `crates/paddock-fixture-http`
  is its test guest (built with `wasip2`'s `proxy::export!`, plus a
  `paddock:runtime/process` import).
- `crates/paddock/src/serial/`: the tty layer (raw termios on `rustix`,
  Unix only; `serialport`/`tokio-serial` are MPL-2.0), the serial heartbeat
  frame format and a cuttable relay between two ttys (tests, the soak);
  `crates/paddock/src/node/serial_path.rs` is the failover pair's serial
  heartbeat path. Tests use pty pairs; `PADDOCK_SERIAL_PAIR=DEV_A,DEV_B`
  (two devices on a null-modem cable) also runs them on hardware.
- `crates/paddock-soak`: the `paddock-soak` binary behind `scripts/soak.sh`
  (Unix only): `failover` runs a witness and a pair as `paddock node`
  processes behind UDP proxies (each node binds `cluster.address` and
  advertises its proxy's) under seeded faults, `replication` the same
  cluster with a `cluster` bucket the ledger example writes through both
  nodes, checked after each heal that the copies agree and hold every
  acknowledged write, `cluster` three member nodes
  behind the same proxies whose `paddock-fixture-chaos` instances link and
  monitor each other at random, checked after each heal for agreement
  between every node's `ps` links and monitors, `scada` the failover
  cluster running `examples/scada` against fake devices with HTTP probes, between
  fault episodes releasing `control` across the pair and switching over
  (Phase 3's exit), `chaos` the chaos example on
  one node, and `check` reads a failover run's events and node logs for
  any moment when both nodes ran the singleton. It reads the gate's
  `singleton starting`, `singleton killed` and `singleton stopped` log
  records, the failover
  task's `failover lease ended` (`ended_ms_ago`) and `failover lease
  extended` (debug, target `paddock::lease`: `until_ms`, `slack_ms`), and an
  instance's `crashed: lease_ended` exit; changing their wording breaks the
  check (its tests hold sample lines).
- `scripts/`: `check.sh` (every check; `--help`), `check-secrets.sh` (run
  before pushing; `--help`) and `soak.sh` (hours of fault injection, not in
  CI; `--help`).
- `docs/`: design and plan. `docs/tla/`: TLA+ specifications of failover
  and their results (`VERIFICATION.md`); run with TLC, not in CI.
- `spikes/`: throwaway programs that answered design questions
  (`spikes/s16` is a TLA+ model, run with TLC; its README has the commands). Excluded from
  the main workspace, from the conventions below, and from the Definition of
  Done. Spike code uses `.unwrap()` freely and is never copied into `crates/`
  as is.

## Constraints established by the spikes

Each of these was measured; the spike README has the evidence. Don't undo one
without re-running its spike.

**Host instances on tokio's multi-threaded runtime, never `current_thread` +
`LocalSet`.** In that configuration timers and the top-level future starve
while a guest yields every epoch tick (S1). That includes tests: async tests that
run instances use `#[tokio::test(flavor = "multi_thread")]`, never the
default. The one exception is `paddock-host`'s `tests/simulation`, which
needs tokio's paused clock (`current_thread` only): it runs no spinning
guest and sets an hour-long epoch tick, and a guest that spins there would
hang the run.

**The epoch ticker is an OS thread, not a tokio task.** Guests yield only when
the epoch advances; if every worker ran a spinning guest, a ticker task would
never be scheduled and nothing would yield again.

**A Store whose call future was dropped, or that trapped, is poisoned.** The
next entry traps with `cannot enter component instance`. Killing an instance
means dropping the future and discarding the Store; supervisors rebuild from
`init`, never resume (S1).

**`callback_deadline_ms` measures guest execution only.** The Store's call
hook times every host import (`CallHook::CallingHost` to
`ReturningFromHost`, wasmtime's `call-hook` feature) and the epoch callback
subtracts that time, so waiting on another instance, a timer or I/O is not
runaway code. Host functions do not time themselves. The epoch callback
yields every tick for fairness (S1). The hook costs nothing measurable in
`baseline` (BASELINE.md).

**The epoch tick bounds how long a busy instance holds a worker.** Receiving
from a non-empty mailbox never suspends, so an instance draining a full
mailbox gives up its tokio worker only at an epoch tick (or when tokio's
cooperative budget runs out). Tasks queued on that worker wait up to a tick:
at 10 ms, p99 under oversubscription was 10.7 ms; at the 2 ms default it is
3.4 ms with the same throughput. A `yield_now` per callback instead costs
over half the throughput. Rerun `pool_load` and `baseline` before changing
the tick or the instance loop (0.5, `docs/BASELINE.md`).

**A component imports only what its code calls.** An app that never calls a
`process` function has no `process` import, so deploy-time checks treat an
absent import as unused, not as an error.

**`from` is a WIT keyword.** It can't be a parameter name (`handle-call` takes
`caller`).

**Guest ABI is synchronous WASI 0.2.** Host imports are async; guests are not.
`paddock:runtime` never exposes `wasi:io` pollables or streams, so a later
WASI 0.3 world is additive (S3, `docs/DESIGN.md` → WASI migration).

**Never call `Linker::define_unknown_imports_as_traps` next to the WASI
linker.** It redefines re-exported WASI resources and breaks instantiation.
Ungranted capabilities get typed deny stubs built with `func_new` (S4).

**Capability functions return `result<T, error>` with an `access-denied`
case.** That is what lets a generic deny stub return an error instead of
trapping. The stub also accepts `permission-denied` and `access` for WASI and
older interfaces (S4, S7).

**Socket policy has separate bind and connect rules.** Rust std binds
`0.0.0.0:0` before every connect, so a destination-only rule blocks every
connection (S4).

**Pooling allocator: `total_stacks` equals the instance limit.** Each live
Store holds a fiber stack. Virtual address space, not RAM, limits instance
count: default reservations cost about 4 to 8 GiB of address space per
instance (S2).

**Bundles are verified against operator-trusted keys only.**
`rustbridge-bundle` defaults to the key inside the bundle, which proves
integrity, not authorship (S6). Use `verify_manifest_signature_trusted` and
`read_variant_verified`, never `verify_manifest_signature`.

**Import inspection skips type imports.** A world that uses interface types
(`use types.{envelope}`) imports each type by its plain name (`envelope`); these
are bound to the interface instance, not linked separately. Counting them as
imports refuses every real component (found by the echo fixture).

**Unix-only code compiles cleanly on Windows.** The admin API and its client
are `cfg(unix)`; code reachable only from them must not warn on Windows, where
CI runs clippy with `-Dwarnings`. Scope Unix-only imports inside the function,
or allow dead code for the module with a comment.

**Fixtures and examples build per feature set.** Cargo unifies features within
one build, so each feature combination (counter v1/v2/v3, services with and
without `denied`) builds into its own `target/fixtures/<name>` directory, and
test binaries that build the same set share a directory.

**Unix socket paths are short.** About 107 bytes on Linux and 103 on macOS;
the node refuses a longer `admin_socket` with a message saying so, and tests
keep data directories short.

**wasmtime 49 has its own `wasmtime::Error`, not `anyhow::Error`.** Use
`wasmtime::Result` at the wasmtime boundary and convert into this crate's
`thiserror` types (S1).

## Licensing is a design constraint

The project is MIT OR Apache-2.0 with no copyleft anywhere in the dependency
graph. `deny.toml`'s licence allow-list is permissive-only and excludes
copyleft by omission; adding an MPL/LGPL/GPL dependency fails `cargo deny` by
design. The fix is a change that edits the allow-list and says why, never a
silent `exceptions` entry. Crates found to be incompatible go in
`[bans] deny` by name.

## Conventions

**Workspace lints:** lints live in the root `Cargo.toml` under
`[workspace.lints]`, and every crate opts in with `[lints] workspace = true`.
A crate-local `[lints]` table is not added; a lint that one crate needs to
relax is relaxed with a scoped `#[allow]` and a comment.

**Shared dependency versions** live in `[workspace.dependencies]`; crates use
`dep = { workspace = true }`. The wasmtime crates move together as one group.

**Test file layout:** tests live in separate `*_tests.rs` files, registered at
the bottom of the source file with:
```rust
#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod mailbox_tests;
```
Integration tests live in each crate's `tests/`. Guest components used as test
fixtures live in their own crates and are built for `wasm32-wasip2`.

**Test naming:** `subject____condition____result`, with exactly four
underscores between segments (`call____target_dead____returns_noproc`).
Because consecutive underscores trip `non_snake_case`, every `*_tests.rs` file
starts with:
```rust
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
```

**Test body:** Arrange, Act, Assert, separated by blank lines, without
`// Arrange` comments. `#[tokio::test]` for async tests. `rstest` for
parameterized tests; `proptest` when a pure function has a round-trip or
arithmetic invariant worth checking across many inputs. Prefer extracting a
pure function over faking a filesystem or a network.

**No `.unwrap()` / `.expect()` in production code**: use `?`. `clippy.toml`
allows them in tests only. No `panic!` in production code either; a guest's
failure is an exit reason, never a host panic. Workspace lints also warn on
`cognitive_complexity`, `must_use_candidate`, `return_self_not_must_use`,
`missing_errors_doc` and `undocumented_unsafe_blocks`: a new public method
that returns `Self` or a `Result` carries `#[must_use]` or an `# Errors`
section. These are lint gates, so none can be skipped quietly.

**Errors:** `thiserror`, in each crate's `error.rs`. Public error enums are
`#[non_exhaustive]`. `anyhow` only in binaries and examples.
An error ignored on a cleanup or rollback path (`let _ =`) is logged at
`error` or `warn` when it means the system is not back where it started.

**A new host service** has, besides its module under `src/services/`: a
test that an ungranted or unlisted use returns `access-denied`, a test for
each limit it enforces, a test that what an instance holds is released when
it exits, its manifest section in MANIFEST.md, its metric and probe names in
OBSERVABILITY.md if any, and a line in docs/SECURITY.md's boundaries if it
takes input from outside the node.

**Logging:** `tracing` macros. The host tags every record with app, version,
pid and node.

**Locks:** never hold a lock across `.await` (`await_holding_lock` is `deny`),
and never call external code (guest calls, logging callbacks, channel sends
that can block) while holding one.

**Unsafe:** `unsafe_op_in_unsafe_fn` is `forbid`. The host should need almost
no `unsafe`; the expected exception is loading pre-compiled components
(`Component::deserialize`). Every `unsafe` block carries a `// SAFETY:`
comment saying why it is sound.

**Public docs:** every public item needs a doc comment (`missing_docs` is on).
Rustdoc must not link to a repository-relative path like `docs/DESIGN.md`; use
the absolute GitHub URL there and keep relative links in the Markdown files.

**Prefer `use` imports over fully-qualified paths** in function bodies, except
where two crates export the same name.

**`Cargo.lock` is committed**, for the main workspace and the spikes
workspace, so a dependency change shows up in the diff.

**Never set `RUSTFLAGS=-Dwarnings` in CI** (or any shared `env:` block). It
applies to dependency compilation too, so a new stable rustc that adds one
warning anywhere in the graph reds every job. Pass `-Dwarnings` to
`cargo clippy` explicitly instead. A multi-line `run:` in a cross-OS matrix
sets `shell: bash`.

## Definition of Done

Before considering any change to the main workspace complete:

- `scripts/check.sh` passes. It runs, each with a line of result: the tests
  (`cargo test --workspace --all-features`), clippy with `-Dwarnings`, a
  native build of every target, the CLI built without the `node` feature,
  the node with no optional feature (`features`), rustdoc with warnings as errors, `cargo fmt
  --check`, `cargo deny`, the MSRV check, the guest builds with their
  `wasm-tools` checks and fixture test, `scripts/check-secrets.sh`, the test
  conventions (above), and a CHANGELOG change for any change to `crates/`,
  `examples/` or `wit/` (a commit line `Changelog: none` when nothing a user
  would notice changed). Its tools: the `wasm32-wasip2` target, the MSRV
  toolchain, `cargo-deny` and `wasm-tools`
- `Cargo.lock` is committed
- A secret-scan false positive is marked on its line with `secret-scan:
  allow`, never by loosening the script
- No `.unwrap()` / `.expect()` / `panic!` in production code
- New public items have doc comments
- `CHANGELOG.md` has an entry for anything a user would notice
- If the change touches a constraint above, the matching spike or a test that
  replaces it still passes
- If behaviour depends on the OS (sockets, filesystem preopens, address space
  limits), it was checked on that OS, or the fact that it was not is stated
  plainly

## Docs are part of "done"

Each file has one job; keep changes in the right one rather than restating
across them. CHANGELOG.md, MANIFEST.md, OBSERVABILITY.md and OPERATIONS.md (interfaces)
change with the code. A change that leaves a narrative doc (README,
GETTING_STARTED, WRITING_APPS, DESIGN prose, ARCHITECTURE, the tutorial) wrong may add a
line to ROADMAP.md's "Doc debt" instead of rewriting it then:

| File | Holds | Scope |
|---|---|---|
| `README.md` | What Paddock does, how to run it, caveats that change how it should be used | Link out rather than expand |
| `docs/DESIGN.md` | Design, rationale, prior art, spike findings, risks | Update when a decision changes; not a development log |
| `docs/ARCHITECTURE.md` | Map of the code as built: crates, data paths, threads and tasks, where state lives | Narrative; a change that leaves it wrong adds a line to ROADMAP's "Doc debt" |
| `docs/SECURITY.md` | Threat model: what is protected, whom Paddock trusts, each boundary's controls and gaps, fuzz targets | Update when a boundary or a control changes; findings are fixed through ROADMAP items |
| `docs/OBSERVABILITY.md` | `ps` columns, calling one instance, probe names and arguments, tracer one-liners | Update with every probe or `ps` change; probe names and arguments are an interface |
| `docs/OPERATIONS.md` | Paths under `data_dir`, environment variables, signals, backups, running as a service | Update with every path, variable or signal change |
| `docs/ROADMAP.md` | The single plan: open work in stages, doc debt, code areas, parked ideas, spike status | Remove items as they land (CHANGELOG records them); add new work here, never in a second plan file |
| `spikes/*/README.md` | What one spike measured, how to rerun it, what it changed | Written once; corrected, not extended |
| `CHANGELOG.md` | What changed, under `Unreleased` until the first release | Keep a Changelog format; one line per change |
| `CLAUDE.md` | Conventions and constraints a contributor needs before editing | Rules, not narrative |

## Writing style for `README.md`, `docs/**/*.md` and rustdoc

- **No second person, no first person.** Not "your app", "you can", "we
  chose". Describe Paddock and what it does: "the host suspends the instance",
  "the supervisor restarts it". Imperatives are fine in instructions.
- **Bold is for bullet lead-ins only**: the first word or phrase of a list
  item. No bold mid-sentence, none in table cells, none opening a paragraph.
- **No decorative icons.** Write "yes" and "no" in tables.
- **Do not sell.** Avoid "the whole point", "load-bearing", "genuinely",
  "crucially", "deliberately", "notably". State the fact and stop.
- **Plain words, short sentences.** Prefer "use" over "utilize", "about" over
  "approximately".
- **Understate the caveats.** "Not measured with a memory-heavy guest" beats
  "a critical unverified gap".
- **Measured numbers carry their machine and date**, and what has not been
  measured is said plainly.
