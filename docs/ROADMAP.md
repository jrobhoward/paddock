# Paddock: Roadmap

2026-10-07 · Companion to [DESIGN.md](DESIGN.md)

The single plan for remaining work. It replaces `docs/PHASE1_PLAN.md`, which tracked Phase 1 to completion (see git history up to commit `a1f84c8`). DESIGN.md holds the design and its rationale; this file holds what is left to build, in what order, and which part of the code each piece of work touches.

## How to use this plan

- Work is grouped in tracks. Each track names the code areas it owns ("Code areas" below). A change that needs an area outside its track is a signal to stop and record it here, not to widen the change.
- Tracks have numbered items. Tick an item when it lands with passing tests and docs (CLAUDE.md, "Definition of Done").
- Ideas that are not scheduled go under "Parked", with enough detail to pick them up later.
- Sizes are relative: S = days, M = about a week, L = more than a week.

## Status

Phase 1 (single node) is complete as of 2026-10-06. All nine exit criteria pass in CI on Linux, through a running node's admin API (`crates/paddock/tests/exit_criteria.rs`) and at the runtime level (`crates/paddock-host/tests/`):

1. Kill and restart: new incarnation, `exit` to links, `down` to monitors, `noproc` for stale pids.
2. Crash loop: restart intensity exceeded, escalation, app marked `failed`.
3. Runaway code: killed by the epoch deadline with `timeout`; the node stays responsive.
4. Memory cap: `memory_limit`, other apps unaffected.
5. Overload: dropped casts counted, calls fail fast with `overloaded`.
6. Upgrade: counter v1 → v2 with state, pid, links and name kept; a broken v3 reverts.
7. Capabilities: undeclared imports refused at deploy; ungranted imports denied at call time.
8. Storage and files: `disk` buckets survive crashes and restarts; `/priv` read-only, `/data` writable, no path escapes.
9. Signatures: tampered bundles refused.

Baseline numbers are in [BASELINE.md](BASELINE.md).

Phase 2 is split in two, so that single-node platform work does not wait on clustering:

| Phase | Tracks | Exit |
| --- | --- | --- |
| 2a: platform on one node | 0, A, B, E | An app serves HTTP, reads a secret, logs and exports metrics on one node, and an operator can trace its messages on a live node |
| 2b: cluster | C, D | A three-node cluster passes partition and node-loss tests (DESIGN.md roadmap) |
| 3: application server | see "Phase 3" | One real workload runs a full release cycle with zero downtime |

## Order

```
0 ─► A ─┬─► B ──────────┐
        ├─► E (E1, E2…) ┼─► Phase 2a done
        │               │
        └─► C ─► D ─────┴─► Phase 2b done
```

Track 0 comes first because the crate consolidation (0.1) moves files that every later track touches. Track A comes next because B and E add host code that should follow A's service pattern. B, C and E can then run in parallel: they touch different areas. D needs C.

## Code areas

An area is a unit of focus: a set of modules with a narrow interface to the rest. Areas are modules inside crates, with `pub(crate)` and a small `pub` surface, rather than one crate each.

| Area | Where (after 0.1) | Interface to the rest | Owned by |
| --- | --- | --- | --- |
| Contract | `wit/`, WIT rules tests | The `paddock:runtime` world; versioned, never changed in place | Any track, with the WIT rules |
| Manifest and bundle | `paddock-bundle` (`manifest`, `bundle`, `policy`, `grant`) | `AppSpec`, `VerifiedBundle`, the effective grant | 0, E (new manifest sections) |
| Process core | `paddock-host`: `pid`, `table`, `registry`, `links`, `message`, `instance`, `runtime`, `state` (process functions) | Spawn, call, cast, link, monitor, exit; after C1, a delivery interface | C, D |
| Host services | `paddock-host`: `service` (new in A1), then one module per service: `logging`, `config`, `kv`, `deny`, `output`, and E's services | The service trait (A1); each service owns its state, linking and capability check | A, E |
| Orchestration | `paddock-host`: `apps`, `pool`, `supervisor`, `upgrade`, `lifecycle`, `spec` | `Apps`, `AppHandle`, `UpgradeOptions` | 0 (rollback), D (placement later) |
| Observability | `paddock-host`: `probe` (new in B1) | Probe functions called from the other host areas | B |
| Node and admin | `paddock` (`node`, `api`, `cli`) | Admin API routes under `/v1`, the CLI | 0, B, D |
| Guest SDK | `paddock-sdk`, `paddock-sdk-macros` | `App`, `Ctx`, envelopes, snapshots, logging; services by cargo feature | A, E |
| Fixtures and examples | `crates/paddock-fixture-*`, `examples/` | Guest components for tests | Any track |

## Crates

Two audiences consume Paddock as a library, and each should depend on only what it uses:

- **App developers** depend on `paddock-sdk` (and its macros). Nothing from the host. They install the `paddock` binary for bundle tooling, and can build it without the node.
- **Edge-service developers** (a new host service such as a datasource or a message broker bridge) depend on `paddock-host` and implement its service trait (A1). They do not need the node, the CLI or the SDK.

Apart from that, fewer crates are preferred: modules and visibility keep areas apart, and fewer crates means less to version and publish. The target set is five published crates, down from nine:

| Crate | Contents | Depends on | Audience |
| --- | --- | --- | --- |
| `paddock-sdk` | Guest SDK | `paddock-sdk-macros` | App developers |
| `paddock-sdk-macros` | `#[derive(Snapshot)]` (a proc-macro must be its own crate) | none | App developers |
| `paddock-bundle` | Manifest model (`manifest`), `.rbp` loading and verification, operator policy, effective grant | rustbridge-bundle, wasmparser; no wasmtime | Host, binary |
| `paddock-host` | Runtime, built-in services, the service trait | `paddock-bundle`, wasmtime | Edge-service developers, binary |
| `paddock` | The `paddock` binary: node daemon (`paddock node`), admin API types and client, CLI; also a library to run a node with extra services (A5) | all of the above | Operators |

The `paddock` crate has a default `node` feature. Without it, the binary has the CLI and bundle tooling only and does not build wasmtime. Fixtures and examples stay unpublished (`publish = false`).

## Track 0: Housekeeping (S)

Small items left from Phase 1, done before the structural work.

- [x] **0.1 Crate consolidation.** Merge `paddock-manifest` into `paddock-bundle` (as a `manifest` module). Merge `paddock-node`, `paddock-api` and `paddock-cli` into `paddock`: one binary with a `node` subcommand in place of `paddock-node`, the API types and client as a module, and a default `node` feature. The WIT rules tests stay in `paddock`. No behavior change: every test passes unchanged, apart from paths and the node's command line. Update README, GETTING_STARTED, CLAUDE.md and CI.
- [x] **0.2 `paddock rollback --restart`.** Found in M9. A rollback is an upgrade to the previous version in that version's mode. In `snapshot` mode it runs the older release's `load-snapshot` on the newer release's snapshot. If that fails (the counter example's v1 cannot read v2's format 2), the rollback is refused and the newer version keeps running, so an operator cannot return to a known-good release. Fix: `--restart` (API: `restart=true`) stops the current version gracefully and starts the previous one from `init`, discarding in-memory state. It is audited as a restart rollback and refused without the flag, so losing state is always explicit. Also `paddock upgrade --restart` for the same choice on upgrades, and a refusal message that suggests the flag. Reuses `restart` mode's rollout path.
- [x] **0.3 `placement.mode`** in place of `placement.singleton: bool` (review action A2), still unenforced, with its warning.
- [x] **0.4 Missing baselines.** The cost of epoch interruption against a store without it; many concurrent clients against a pool. Added as a dated section of BASELINE.md. (Admin-API latency is B5; log-record cost is A3.)
- [x] **0.5 Fairness under load.** Done: the default epoch tick is 2 ms (was 10 ms). With 16 workers, a pool of 16 and 256 clients, p99 fell from 10.7 ms to 3.4 ms at the same throughput; a single overloaded instance gives up some throughput (21,900–27,400 calls/s against 24,800–26,400) ([BASELINE.md](BASELINE.md), 2026-10-07). A time slice between messages was tried and did not help. Found in 0.4 ([BASELINE.md](BASELINE.md), 2026-10-07). An instance with a full mailbox runs callbacks back to back and keeps its tokio worker until the cooperative budget (128 mailbox receives) runs out or the 10 ms epoch tick yields it. With 256 clients on a pool of 16, p50 is 51 µs but p99 is 10.7 ms. A `yield_now` after every callback halves p99 but costs over half the throughput, so the fix needs a policy, not a yield per message. Candidates: yield after a slice of guest time (about 1 ms, measured with the existing `CallbackClock`), or shorter epoch ticks while other tasks wait. Done when the 16-worker, 256-client p99 drops well below one tick with throughput within 10% of today's, rerun with `pool_load` and `baseline`. Process core area.
