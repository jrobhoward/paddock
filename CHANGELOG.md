# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## Unreleased

### Added

- `wit/runtime.wit`: the `paddock:runtime@0.1.0` app contract (`types`,
  `process`, and the `app` world).
- Tests that enforce the contract's rules: synchronous functions only, no
  `stream`/`future`, no `wasi:io` types, and an `access-denied` error on every
  capability function.
- `paddock-fixture-echo`, the smallest app implementing the `app` world.
- CI: tests on Linux, macOS and Windows, guest component checks, MSRV 1.96.0,
  cargo-deny, fmt and docs, and a weekly run against fresh dependencies.
- `paddock-manifest`: the full app manifest model (every section, including
  fields for later phases), with defaults, path-aware parse errors, validation
  that reports every broken rule, and a list of fields Phase 1 does not enforce.
- `paddock-bundle`: opens `.rbp` 1.2 bundles, verifies signatures against
  operator-trusted keys only, checks checksums and the component's world, and
  reads the `appserver` section.
- Deploy-time import inspection: a component importing an interface its
  manifest does not declare, or that no host provides, is refused.
- Operator policy (TOML) and the effective grant: imports ∩ manifest ∩ policy,
  with ungranted capabilities marked for deny stubs.
- `paddock-host`: the instance runtime. A pooling-allocator engine sized from
  node config (standard or dense memory profile), an epoch ticker, pids with
  incarnations, a process table, and instances that run `init`, handle calls
  and casts one at a time under a per-callback deadline, and run `terminate`
  on a graceful stop. Traps, memory and table limits, deadlines and kills end
  an instance with an exit reason; a full mailbox fails casts and calls with
  `overloaded`.
- `paddock-fixture-chaos`: a guest that traps, spins, allocates or fails on
  request, for exit-path tests.
- Messaging: guest `cast`, `call` (with timeout, `noproc` for dead or stale
  pids, the target's exit reason if it dies mid-call) and `reply` for deferred
  answers; `send-after` and `cancel-timer` timers delivered to `handle-info`;
  a node-local registry (`register` limited to the manifest's
  `registered_names`, `whereis`, names released on exit). `Runtime` gains
  `whereis`, `call` and `cast`.
- `message_max_bytes` is enforced; an oversized body fails with the new
  `too-large` call error. Casts dropped for a full mailbox are counted
  (`InstanceHandle::dropped_messages`).
