# Paddock: Phase 1 Plan (single node)

2026-10-05 · Companion to [DESIGN.md](DESIGN.md)

Phase 1 delivers a single-node Paddock server. It deploys signed `.rbp` bundles containing Wasm components and runs them as supervised instances with mailboxes, call/cast, links and monitors. It enforces memory, deadline and mailbox limits, provides logging, config, term storage and jailed files, and supports hot upgrade with automatic revert. Clustering is Phase 2.

## Exit criteria

Phase 1 is done when all of the following pass in CI on Linux:

1. **Kill and restart.** Killing an instance (admin `kill`) restarts it under its supervisor with a new incarnation. Linked instances get `exit`, monitors get `down`, and stale pids return `noproc`.
2. **Crash loop.** A component that traps in `init` exceeds restart intensity. Its supervisor escalates, and the app is marked `failed` with the reason visible in `paddock status`.
3. **Runaway code.** An infinite loop in `handle-call` is killed by the epoch deadline (reason `timeout`). The caller gets `timeout` and the node stays responsive.
4. **Memory cap.** A component that allocates past `memory_max` crashes with `memory_limit`, and other apps are unaffected.
5. **Overload.** A full mailbox drops casts (counted in metrics) and fails calls fast with `overloaded`.
6. **Upgrade.** A stateful counter app upgrades v1 → v2 with state preserved and its pid, links and registered name unchanged. A deliberately broken v3 (`load-snapshot` fails) reverts to v2 with no lost messages.
7. **Capabilities.** An app whose component imports an interface its manifest doesn't declare is refused at deploy. An app with an ungranted (but declared) import gets `permission-denied` at call time.
8. **Storage and files.** Keyvalue data in a `disk` bucket survives instance crashes and node restarts. `/priv` is read-only, `/data` is writable, and path escapes fail.
9. **Signatures.** A tampered bundle (bad checksum or signature) is refused.

## Spikes (do first, time-boxed)

Each spike answers one question that could change the design. They're small throwaway programs in `spikes/`, and their findings go into `DESIGN.md`.

| # | Question | Done when | Time-box | If the answer is bad |
| --- | --- | --- | --- | --- |
| S1 | Does the callback model work with wasmtime's async component bindings? Specifically: a guest export (`handle-call`) calls a host import (`call`) that suspends for a long time without blocking a thread, while an epoch deadline can still kill it. | Rust guest + host where A calls B via the host, B sleeps, A's deadline fires and traps cleanly, and the host keeps serving other instances | 2 days | Fall back to a sync `call` that returns a ref plus a deferred `handle-info` reply (more guest-side complexity) |
| S2 | What does an instance cost? Instantiation time with the pooling allocator, resident memory per idle instance, and per-message host→guest overhead | Numbers recorded for 1, 100, 1,000 and 5,000 instances | 1 day | Revisit the default limits and pool sizing; still fine for the coarse-grained target |
| S3 | WASI 0.2 or 0.3? Check what the current wasmtime release supports, and what `wit-bindgen` and the Rust guest toolchain need for each | A written decision with the wasmtime version pinned | 0.5 day | Target 0.2 now and design the world so 0.3 async is an additive version |
| S4 | Can the host read a component's imports at deploy time and link ungranted imports to deny stubs? | Host lists a component's imports (via `wit-parser`/`wasmparser`); a component importing `wasi:http` instantiates with a stub that returns an error, or with traps via `define_unknown_imports_as_traps` | 1 day | Refuse deploy for any ungranted import (strict mode) and drop the degrade-gracefully goal |
| S5 | Snapshot upgrade end to end: swap a component instance mid-life while keeping the host-side pid, mailbox and links | v1 → v2 swap with state carried, plus a failing v3 that reverts | 1 day | Ship `restart` mode only in Phase 1; snapshot moves to Phase 2 |
| S6 | Which rustbridge pieces can be used as a library: bundle reading, minisign verification, manifest types? What must change for a `wasm32-wasip2` platform and an `appserver` section? | A list of rustbridge changes (or a small PR) and a Paddock loader reading a 1.2 bundle | 1 day | Paddock reads bundles itself for now; upstream later |
| S7 | Which WASI host implementations exist off the shelf (`wasmtime-wasi-keyvalue`, `wasmtime-wasi-config`, `wasi:logging`)? Are they on the same WIT versions? | A table of interface → crate → version → gaps | 0.5 day | Implement thin host bindings ourselves from the WIT |

S1, S3 and S4 matter most: they confirm the guest ABI, which is the hardest thing to change later. The total is about 7 days.

**Status (2026-10-05):** S1 done, passed ([spikes/s1/README.md](../spikes/s1/README.md)). S3 done, WASI 0.2 chosen ([spikes/S3-wasi-target.md](../spikes/S3-wasi-target.md)). S4 done, passed with typed deny stubs ([spikes/s4/README.md](../spikes/s4/README.md)). S5 done, passed ([spikes/s5/README.md](../spikes/s5/README.md)). S2 done: about 35 µs and 30–40 KiB per instance; address space is the limit ([spikes/s2/README.md](../spikes/s2/README.md)). S6 done: rustbridge-bundle is reusable, 7 upstream changes needed ([spikes/s6/README.md](../spikes/s6/README.md)). S7 done: write our own keyvalue and logging hosts ([spikes/S7-wasi-host-crates.md](../spikes/S7-wasi-host-crates.md)). **All spikes complete.**

## Decisions to make before M1

| Decision | Recommendation | Why |
| --- | --- | --- |
| Project name | **Decided:** paddock: crates `paddock-*`, WIT package `paddock:runtime`, CLI `paddock` (all free on crates.io as of 2026-10-05) | Settle it before any public crate, WIT package or repo exists |
| Repo layout | One Cargo workspace, new repo `paddock` | rustbridge stays independent |
| License | MIT OR Apache-2.0 | Matches rustbridge and the Rust ecosystem |
| Default envelope encoding | CBOR, with JSON allowed | Compact and self-describing; JSON for debugging and rustbridge parity |
| Admin API transport | JSON over HTTP on a Unix socket (Phase 1); TLS TCP in Phase 2 | Simple, scriptable with curl, no extra schema toolchain |
| WASI target | **Decided (S3):** WASI 0.2, sync guest ABI, wasmtime 49.x pinned; 0.3 later as `paddock:runtime@0.2.0` | See [spikes/S3-wasi-target.md](../spikes/S3-wasi-target.md) |
| Async runtime | tokio | Required by wasmtime async and quinn later |
| Persistent store | redb | Pure Rust, embedded, transactional |

Reserve the crate names on crates.io (publish empty `0.0.0` placeholders) and the GitHub repo once the name is final.

## Workspace layout

```
paddock/
├── wit/                    # paddock:runtime WIT package (versioned)
├── crates/
│   ├── paddock-manifest/   # full manifest data model, serde, validation
│   ├── paddock-bundle/     # .rbp loading, signature/checksum checks, import inspection
│   ├── paddock-host/       # runtime: engine, instances, mailboxes, links, supervisors, services
│   ├── paddock-node/       # daemon binary: config, admin API, deploy store
│   ├── paddock-cli/        # `paddock` admin CLI
│   └── paddock-sdk/        # Rust guest SDK (App trait, ProcessContext, snapshot derive)
├── examples/
│   ├── counter/            # stateful, v1/v2/v3 for upgrade tests
│   ├── ping-pong/          # call/cast/link/monitor
│   └── chaos/              # trap, infinite loop, memory hog, init failure
├── tests/                  # integration tests driving a real node
└── spikes/                 # throwaway; removed after Phase 1
```

## Milestones

Sizes are relative (S = days, M = about a week, L = more than a week). Each milestone ends with passing tests.

### M0 Workspace and CI (S)
- [x] Cargo workspace, MSRV 1.96.0, workspace lints, `clippy.toml`, `deny.toml`.
- [x] CI: clippy and tests on Linux, macOS and Windows; guest fixtures built for `wasm32-wasip2` and checked with `wasm-tools`; MSRV; cargo-deny; fmt and docs; weekly fresh-deps.
- [x] `wit/runtime.wit`: package `paddock:runtime@0.1.0`, its rules enforced by `crates/paddock/tests/wit_rules.rs`. `crates/paddock-fixture-echo` is the smallest app implementing it.

### M1 Manifest and bundles (M)
- [x] `paddock-manifest`: Rust types for the **entire** manifest data model in DESIGN.md (all sections, including Phase 2/3 fields), serde, defaults, validation errors with paths.
- [x] Mark unenforced fields: the node logs a warning when a manifest uses a field Phase 1 doesn't enforce yet. (`AppSpec::unenforced_fields` lists them; the node logs them once M7 exists.)
- [x] `paddock-bundle`: open `.rbp`, pick the `wasm32-wasip2` variant, verify checksums and minisign signature against trusted keys.
- [x] Import inspection: list component imports and fail deploy if any are missing from `capabilities` (from S4). Reads the component with `wasmparser`, without compiling it.
- [x] Effective-grant computation: imports ∩ manifest ∩ operator policy (operator policy file, Phase 1 format: TOML).

### M2 Instance runtime (L)
- [ ] Engine: async, epoch interruption with a ticker task, pooling allocator sized from node config (`max_instances`; `total_stacks = max_instances`; memory reservation is a node setting: large by default, 128 MiB for dense nodes; see S2).
- [ ] Instance = Store + limits (`StoreLimits`) + bounded mailbox + one tokio task running the callback loop.
- [ ] Pid allocation `{node, id, incarnation}`; process table; `self`.
- [ ] Callback loop: `init` with init timeout → loop over mailbox → `handle-call`/`handle-cast`/`handle-info` under the per-callback deadline → `terminate` on shutdown.
- [ ] Trap, memory and deadline → exit reason mapping; instance teardown.
- [ ] Lifecycle states (from rustbridge): Installed, Starting, Active, Upgrading, Stopping, Stopped, Failed, Restarting.

### M3 Messaging (M)
- [ ] `cast`, `call` with timeout, `reply` and deferred replies (`noreply`).
- [ ] `noproc` for dead or unknown pids; caller gets the target's exit reason if it dies mid-call; late replies dropped.
- [ ] Mailbox limits and `message_max_bytes`; `overloaded` behaviour.
- [ ] Timers: `send-after`, `cancel-timer`.
- [ ] Local registry: `register` (only names in `registered_names`), `whereis`, unregistration on exit.
- [ ] Envelope handling: content type and schema id passed through untouched.

### M4 Links, monitors, supervision (L)
- [ ] Link and monitor tables in the host; `trap-exits`; exit propagation rules matching OTP.
- [ ] Host-native supervisors: `one_for_one`, `one_for_all`, `rest_for_one`, `simple_one_for_one` pools.
- [ ] Restart types (`permanent`, `transient`, `temporary`), intensity/period, escalation, backoff with jitter.
- [ ] Root supervisor per app; app start order from `dependencies`; reverse order on stop.
- [ ] Instance pools with round-robin and hash routing (`instances.routing`).

### M5 Host services (M)
- [ ] `wasi:logging` → `tracing`, tagged with app, version, pid; level cap and rate limit per app. Paddock's own host from vendored proposal WIT; no wasmtime crate exists (S7).
- [ ] `wasi:config` from manifest defaults plus node-level overrides, via `wasmtime-wasi-config` (S7).
- [ ] `wasi:keyvalue`: `memory` and `disk` (redb) buckets, per-app namespaces, byte quotas, atomics. Paddock's own host on the vendored `wasi:keyvalue@0.2.0-draft` WIT; wasmtime's crate is a demo (S7).
- [ ] `wasi:filesystem`: `/priv` read-only from the bundle, `/data` read-write per app volume.
- [ ] `wasi:clocks`, `wasi:random`; stdout/stderr captured as tagged logs.
- [ ] Deny stubs for declared-but-ungranted imports.

### M6 Hot upgrade (M)
- [ ] `snapshot` mode: pause delivery → `save-snapshot` → instantiate v2 → `load-snapshot` → resume; revert on any failure.
- [ ] `restart` mode.
- [ ] Upgrade validation: signature, world compatibility, capability diff (new capabilities need `--approve`).
- [ ] Batch rollout across a pool, with stop-and-revert on restart threshold.
- [ ] `rollback` to the previous version.

### M7 Node daemon and CLI (M)
- [ ] `paddock-node`: node config (TOML), deploy store on disk, restore deployed apps on restart.
- [ ] Admin API on a Unix socket: deploy, list, status, stop, start, kill, upgrade, rollback, remove, logs, ps.
- [ ] `paddock` CLI wrapping the API, with human and `--json` output.
- [ ] Audit log of deploys, upgrades, grants and denials.

### M8 Guest SDK and examples (M)
- [ ] `paddock-sdk`: an `App` trait (init, handle_call, handle_cast, handle_info, terminate), `ProcessContext` (call, cast, link, monitor, timers, registry), typed envelopes with serde + CBOR.
- [ ] `#[derive(Snapshot)]` for versioned state with a `migrate(from_version, old)` hook.
- [ ] Examples: `counter` (v1, v2 and broken v3), `ping-pong`, `chaos`.
- [ ] Bundle tooling: `paddock bundle` or the rustbridge CLI producing a 1.2 `.rbp` from a component and manifest (per S6).

### M9 Exit test suite and baseline numbers (M)
- [ ] Integration tests for every exit criterion above.
- [ ] Baseline benchmarks: call latency (p50/p99), cast throughput, instantiate time, idle memory per instance. Recorded in the repo, not gated.
- [ ] Fuzz the host functions of `process` with arbitrary envelopes and pids (cargo-fuzz), run nightly.
- [ ] Docs: getting started, writing an app, manifest reference.

## Order and parallelism

```
Spikes ─► M0 ─► M1 ─┬─► M2 ─► M3 ─► M4 ─┬─► M6 ─┐
                    │                   ├─► M5 ─┼─► M9
                    └─► M8 (SDK starts  └─► M7 ─┘
                         once M3's WIT is stable)
```

M5 and M7 can run beside M4 once M2 and M3 are done. The SDK (M8) tracks the WIT and can start early against stubs.

## rustbridge work in Phase 1

Kept small; the rest waits for later phases:

From S6 (`rustbridge-bundle` 1.0.1 is already a usable library). Done on the rustbridge `bundle-1.2` branch, not yet published; `paddock-bundle` depends on a sibling checkout (`../rustbridge`) until it is, so CI cannot build the workspace in the meantime:
- [x] `Platform::Wasm32Wasip2` and `#[non_exhaustive]` on `Platform`.
- [x] Optional `component: { world }` on a variant.
- [x] `#[serde(flatten)]` extensions on `Manifest` so `appserver` round-trips; builder `with_extension`.
- [x] Accept `bundle_version` 1.2.
- [x] Verify against operator-trusted keys (`verify_manifest_signature_trusted`); never trust the key inside the bundle.
- [x] `read_variant_verified` (in-memory bytes, checksum verified).
- [x] `BundleBuilder::add_component`.
- [x] Also: `zip` built with Deflate only, which keeps two non-allow-listed licences (bzip2, ppmd) out of Paddock's graph.
- [ ] Publish the branch, then switch `paddock-bundle` to a git or crates.io dependency.
- [ ] Later (Phase 2+): the guest adapter (`Plugin` → `paddock:runtime/app`), the `Server` trait, and the native-process runner.

## Out of scope for Phase 1

Distribution and QUIC, process groups and global names, messaging ACLs, HTTP ingress and egress, sockets, secrets, metrics export, health exports, placement, native-process plugins, blue-green upgrades, guest SDKs other than Rust, and fuel metering.
