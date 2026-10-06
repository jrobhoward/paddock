# Spike S3: WASI 0.2 or 0.3?

**Decision: target WASI 0.2 with a synchronous guest ABI for `paddock:runtime@0.1.0`. Pin wasmtime 49.x. Add WASI 0.3 later as a new world version, not a rewrite.**

Checked 2026-10-05.

## Facts

| Item | Status | Source |
| --- | --- | --- |
| wasmtime latest | 49.0.2 (released 2026-10-05) | crates.io |
| WASI 0.3 host support | `wasmtime-wasi` 49 has `p1`, `p2`, `p3` features, **all on by default**; p3 ships `wasi:*@0.3.0` WIT for cli, clocks, filesystem, random, sockets | `wasmtime-wasi-49.0.2/Cargo.toml`, `src/p3/wit/deps` |
| Component-model async (the ABI under WASI 0.3) | `component-model-async` is a default wasmtime feature and enabled at runtime by default, but `Config::wasm_component_model_async` is still documented as "_very_ incomplete" | `wasmtime-49.0.2/src/config.rs` |
| Rust guest target | No `wasm32-wasip3` target in rustc 1.99; only `wasip1`, `wasip1-threads`, `wasip2` | `rustup target list` |
| Rust guest async bindings | wit-bindgen 0.62 has `async`, `async-spawn` and `futures-stream` features (CM async ABI on the wasip2 target) | crates.io feature list |
| Host async without guest async | Works: async host imports suspend a sync guest on a fiber (S1, E2) | `spikes/s1` |

## Why 0.2

1. **Paddock doesn't need guest-side async.** An instance is single-threaded and processes one message at a time, like a `gen_server`. S1 showed that host-side async gives us suspension, preemption and kill without any async in the guest.
2. **0.3 is not settled.** wasmtime still calls CM async very incomplete, and Rust has no wasip3 target. Building the guest ABI on it now would repeat wasmCloud's churn.
3. **Every Component Model language can target 0.2 today** (Rust, JS via componentize-js, Python via componentize-py, Go, C#), which matters for the multi-language goal.

## What 0.3 would add later

- Streams and futures in WIT: useful for large messages, HTTP bodies and file transfer without copying whole buffers.
- Several in-flight calls inside one instance. This conflicts with the one-message-at-a-time model, so it would be opt-in.
- Native async `wasi:http` (0.3 handlers are simpler than 0.2's).

## How to keep the door open

- Version the world: `paddock:runtime@0.1.0` (sync, WASI 0.2). A future `@0.2.0` can add async functions. The host links both, chosen by the component's declared world (recorded in the manifest's `component.world`).
- Keep messages as `envelope` byte lists so moving to `stream<u8>` for large bodies is additive.
- Build the host with wasmtime's default features (p3 compiled in) but don't require CM async from guests.
- Revisit when wasmtime drops the "incomplete" note or Rust adds `wasm32-wasip3`. Add that check to each Phase release review.

## Pins for Phase 1

- `wasmtime = "49"`, `wasmtime-wasi = "49"` (p2 used; p3 compiled but unused)
- `wit-bindgen = "0.62"` (no `async` feature in guests)
- Rust guest target `wasm32-wasip2`, toolchain 1.99+
