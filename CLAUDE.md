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
- `docs/ROADMAP.md`: status, tracks and items, code areas, crate plan, spike
  status. Keep work inside the current track's areas; record anything outside
  them in the roadmap instead of widening the change.
- `spikes/*/README.md`: what each spike measured and what it changed.

## Commands

```bash
# Build and test the main workspace
cargo build --workspace --all-targets
cargo test --workspace
cargo test --workspace -- some____test____name     # single test

# Lint (must be clean before any change is considered done). CI will run the
# latest stable clippy, which adds lints each release; `rustup update stable`
# first so a lagging local toolchain doesn't pass code CI rejects.
cargo clippy --workspace --all-targets -- -Dwarnings
cargo fmt --all -- --check

# Rustdoc, with broken intra-doc links treated as errors. Set on this command
# only, never in a shared `env:` block (see "Never set RUSTFLAGS" below).
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

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
  `examples/tracing/` is a demo script and bpftrace scripts for the
  probes, not a crate.
- `crates/paddock-fixture-chaos`: a guest that traps, spins, allocates or
  fails on request, and drives every `process` function; the host's tests
  run against it.
- `crates/paddock-fixture-sdk`: a guest that uses `wasi:config`,
  `wasi:keyvalue` and `paddock:groups` through `paddock-sdk`'s `config`,
  `kv` and `groups` wrappers.
- `crates/paddock-fixture-services`: a guest that drives logging, config,
  keyvalue, the filesystem and stdout. Its `denied` feature adds an import no
  host implements, for deny-stub tests; nodes refuse that build, so node tests
  build it without the feature into `target/fixtures/services-plain`.
- `crates/paddock-fixture-counter`: a stateful counter built three ways by
  feature (v1, v2, a broken v3) for upgrade tests; each version builds into
  its own target directory.
- `crates/paddock-fixture-echo`: the smallest app implementing the `app`
  world; a guest test fixture. Generated bindings live in a `bindings` module
  with a scoped `#[allow(missing_docs, clippy::too_many_arguments)]`. 
- `wit/`: the `paddock:runtime` WIT package, versioned. Its rules are enforced
  by `crates/paddock/tests/wit_rules.rs`. `wit/connection/`: the
  `paddock:connection` package (connection handlers), separate because it
  uses `wasi:io` streams, which `paddock:runtime` excludes. `wit/groups/`:
  the `paddock:groups` package (process groups), separate so the app world
  stays frozen; every host bindgen lists it, since `crates/paddock-host/wit`
  imports it. `wit/vendor/`
  holds unmodified copies of the WASI interfaces Paddock binds against
  (keyvalue, config, logging, and `wasi:io` for connections); bindings name
  each package path explicitly.
- `crates/paddock-host/src/delivery.rs`: the one place a pid is routed
  (local, dead, or another node). Process-core code that names a pid calls
  it, never the process table directly. `src/transport/`: frames between
  nodes, the `Transport` trait, `liveness` (heartbeats and sessions; every
  node-down goes through `transport::end_session`), `membership` (joining,
  members, node creation numbers), and `memory`, the
  in-process mesh with fault injection that `tests/distribution.rs` runs on. `tests/simulation/` runs
  the mesh on a paused clock: targeted races and seeded random scenarios
  (`PADDOCK_SIM_SEED=<n>` replays one).
- `crates/paddock-host/src/failover/`: an HA pair's roles, epochs and
  leases (`node.rs`: one type per standing; transitions consume the state
  and return the next, named after the actions in `docs/tla/FailoverL2.tla`)
  and the witness (`witness.rs`). The rules are R1 to R11 in
  `docs/tla/VERIFICATION.md`; a change to a transition means updating the
  matching action in the model and rerunning it. `tests/simulation/failover.rs` logs `may_run` and checks that a
  singleton never runs on both nodes.
- `crates/paddock-host/src/listener.rs`: listeners and connection handlers,
  an instance per connection. `crates/paddock-fixture-conn` is its test
  guest; `examples/line-server` the SDK example.
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
never be scheduled and nothing would yield again (M2).

**A Store whose call future was dropped, or that trapped, is poisoned.** The
next entry traps with `cannot enter component instance`. Killing an instance
means dropping the future and discarding the Store; supervisors rebuild from
`init`, never resume (S1).

**`callback_deadline_ms` measures guest execution only.** Host imports record
the time they spend and the epoch callback subtracts it, so waiting on another
instance is not runaway code. The epoch callback yields every tick for
fairness (S1).

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
absent import as unused, not as an error (M0).

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
imports refuses every real component (M1, found by the echo fixture).

**Unix-only code compiles cleanly on Windows.** The admin API and its client
are `cfg(unix)`; code reachable only from them must not warn on Windows, where
CI runs clippy with `-Dwarnings`. Scope Unix-only imports inside the function,
or allow dead code for the module with a comment (M7).

**Fixtures and examples build per feature set.** Cargo unifies features within
one build, so each feature combination (counter v1/v2/v3, services with and
without `denied`) builds into its own `target/fixtures/<name>` directory, and
test binaries that build the same set share a directory (M6 to M8).

**Unix socket paths are short.** About 107 bytes on Linux and 103 on macOS;
the node refuses a longer `admin_socket` with a message saying so, and tests
keep data directories short (M7).

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

- `cargo test --workspace` passes with zero failures
- `cargo clippy --workspace --all-targets -- -Dwarnings` is clean
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` is clean
- `cargo fmt --all -- --check` is clean
- `cargo deny check licenses bans sources advisories` passes
- `cargo +1.96.0 check --workspace --all-targets` (MSRV) passes
- `Cargo.lock` is committed
- No `.unwrap()` / `.expect()` / `panic!` in production code
- New public items have doc comments
- Guest fixtures used by the change build for `wasm32-wasip2`, and the tests
  that load them run
- `CHANGELOG.md` has an entry for anything a user would notice
- If the change touches a constraint above, the matching spike or a test that
  replaces it still passes
- If behaviour depends on the OS (sockets, filesystem preopens, address space
  limits), it was checked on that OS, or the fact that it was not is stated
  plainly

## Docs are part of "done"

Each file has one job; keep changes in the right one rather than restating
across them:

| File | Holds | Scope |
|---|---|---|
| `README.md` | What Paddock does, how to run it, caveats that change how it should be used | Link out rather than expand |
| `docs/DESIGN.md` | Design, rationale, prior art, spike findings, risks | Update when a decision changes; not a development log |
| `docs/OBSERVABILITY.md` | `ps` columns, calling one instance, probe names and arguments, tracer one-liners | Update with every probe or `ps` change; probe names and arguments are an interface |
| `docs/ROADMAP.md` | The single plan: status, tracks, code areas, crate plan, parked ideas, spike status | Tick items as they land; add new work here, never in a second plan file |
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
