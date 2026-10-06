# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Paddock is a clustered application server for sandboxed WebAssembly
components, with Erlang/OTP-style semantics: mailboxes, call/cast, links,
monitors, supervisors and hot upgrade with explicit state migration. Nodes
connect over QUIC with mutual TLS. Apps are WebAssembly components (WASI 0.2)
that see only the capabilities their manifest declares and an operator grants.
Bundles use the rustbridge `.rbp` format.

The project is pre-implementation: the design and the seven Phase 1 spikes
are done, and the Phase 1 crates have not been written. Read these before
changing anything structural:

- `docs/DESIGN.md`: the design, prior art, spike findings and risks.
- `docs/PHASE1_PLAN.md`: exit criteria, milestones M0 to M9, spike status.
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
# `rustup target add wasm32-wasip2` once.
cargo build -p <guest-crate> --target wasm32-wasip2 --release
```

Spikes are a separate workspace under `spikes/` with their own lock file. Each
`spikes/<name>/README.md` has its exact build and run commands.

## Repository layout

- `crates/paddock`: placeholder that holds the workspace together. The Phase 1
  crates (`paddock-manifest`, `paddock-bundle`, `paddock-host`,
  `paddock-node`, `paddock-cli`, `paddock-sdk`) land under `crates/` per the
  plan's workspace layout.
- `wit/` (from M0): the `paddock:runtime` WIT package, versioned.
- `docs/`: design and plan.
- `spikes/`: throwaway programs that answered design questions. Excluded from
  the main workspace, from the conventions below, and from the Definition of
  Done. Spike code uses `.unwrap()` freely and is never copied into `crates/`
  as is.

## Constraints established by the spikes

Each of these was measured; the spike README has the evidence. Don't undo one
without re-running its spike.

**Host instances on tokio's multi-threaded runtime, never `current_thread` +
`LocalSet`.** In that configuration timers and the top-level future starve
while a guest yields every epoch tick (S1).

**A Store whose call future was dropped, or that trapped, is poisoned.** The
next entry traps with `cannot enter component instance`. Killing an instance
means dropping the future and discarding the Store; supervisors rebuild from
`init`, never resume (S1).

**`callback_deadline_ms` measures guest execution only.** Host imports record
the time they spend and the epoch callback subtracts it, so waiting on another
instance is not runaway code. The epoch callback yields every tick for
fairness (S1).

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
integrity, not authorship (S6).

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

### Relaxed during the spike phase

Until the spike phase is declared over (remove this subsection then):

- `spikes/` is exempt from everything above.
- `CHANGELOG.md` is not required; there is no release yet.
- The MSRV check is not required until the first crate depends on wasmtime.
- `docs/` may describe planned behaviour, as long as it says it is planned.
- `docs/DESIGN.md` and `docs/PHASE1_PLAN.md` predate the writing-style rules
  below (they use "we"); bring each in line when it is next edited.

The rest of the list applies to `crates/` from the first commit.

## Docs are part of "done"

Each file has one job; keep changes in the right one rather than restating
across them:

| File | Holds | Scope |
|---|---|---|
| `README.md` (from M0) | What Paddock does, how to run it, caveats that change how it should be used | Link out rather than expand |
| `docs/DESIGN.md` | Design, rationale, prior art, spike findings, risks | Update when a decision changes; not a development log |
| `docs/PHASE1_PLAN.md` | Phase 1 exit criteria, milestones, spike status | Tick boxes as milestones land; retire when Phase 1 ships |
| `spikes/*/README.md` | What one spike measured, how to rerun it, what it changed | Written once; corrected, not extended |
| `CHANGELOG.md` (from first release) | What changed in each release | Keep a Changelog format |
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
