# Spike S7: off-the-shelf WASI host implementations

**Answer: use wasmtime's crates for core WASI, `wasi:config` and `wasi:http`. Write our own host for `wasi:keyvalue` (wasmtime's is a demo) and `wasi:logging` (no crate exists).** All wasmtime crates are on the same release, 49.0.2.

Checked 2026-10-05 by reading the published crate sources.

| Interface | Crate | WIT version | State | Paddock action |
| --- | --- | --- | --- | --- |
| cli, clocks, filesystem, random, sockets, io | `wasmtime-wasi` 49.0.2 | `wasi:*@0.2.12` (and `@0.3.0` behind `p3`) | Complete. S4 exercised preopens, socket checks and env | Use as is, configured per app through `WasiCtx` |
| `wasi:config` | `wasmtime-wasi-config` 49.0.2 | `wasi:config@0.2.0-rc.1` (`get`, `get-all`) | Complete for a static key/value map (`WasiConfigVariables`) | Use as is: build each app's map from manifest defaults plus node overrides. Its `error` has no permission case, which is fine because config is always granted |
| `wasi:keyvalue` | `wasmtime-wasi-keyvalue` 49.0.2 | `wasi:keyvalue@0.2.0-draft` (store, atomics, batch) | **Demo only.** One store id (`""`); each `open()` clones the initial map into a new bucket handle, so writes are invisible to other handles and lost when the handle is dropped. No persistence, quotas or namespaces | Reuse its WIT; write our own host with per-app buckets (memory, redb), quotas and atomics. Its `error` variant already has `access-denied` |
| `wasi:logging` | none (`wasi-logging` 0.0.1 on crates.io is an empty placeholder) | proposal WIT only (`wasi:logging/logging`, draft) | Not implemented in wasmtime | Vendor the proposal WIT and write a small host that maps to `tracing` with app/pid tags and a rate limit |
| `wasi:http` | `wasmtime-wasi-http` 49.0.2 | `wasi:http@0.2.12` and `@0.3.0` | Complete. `WasiHttpHooks` lets the host override `send_request` and `is_forbidden_header` | Phase 2: egress policy via `send_request`; ingress via the incoming handler |

## Findings

1. **One version train.** All wasmtime host crates are released together (49.0.2), so a wasmtime bump moves them all; pin them as a group.
2. **Proposal interfaces are still drafts** (`keyvalue 0.2.0-draft`, `config 0.2.0-rc.1`, `logging` draft). Their WIT may change. Vendoring the WIT files we implement (rather than tracking upstream heads) keeps Paddock's guest ABI stable.
3. **Denial naming differs across WASI.** `wasi:keyvalue` and `wasi:sockets` say `access-denied`, `wasi:filesystem` says `access` / `not-permitted`, and `wasi:http` says `HTTP-request-denied`. S4's convention said `permission-denied`.
   - **Recommendation:** adopt `access-denied` for `paddock:*` interfaces, to match keyvalue and sockets.
   - Have the generic deny stub accept `access-denied`, `permission-denied` and `access`.

## Effect on the plan

- M5 scope: our own `wasi:keyvalue` and `wasi:logging` hosts. `wasi:config` and core WASI come from wasmtime.
- Interface rule update: the denial case is named `access-denied`.
