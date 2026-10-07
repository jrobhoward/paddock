# Spike S8: work inside an instance versus native code

**Question.** How much slower is code running inside a Paddock instance than the same code compiled natively, and what does one call into an instance cost?

**Answer: compute-bound code runs at about 1.0x to 2.4x native time, depending on the workload; one call into an instance costs about 2 µs.** The large gaps come from native CPU features the wasm build can't use: SHA extensions (about 6.8x on SHA-256) and auto-vectorization (n-body improves from 2.35x to 1.82x with wasm `simd128`).

Run on 2026-10-06: AMD Ryzen Threadripper 1950X (16 cores, SHA-NI), Linux x86_64, rustc 1.99.0, wasmtime 49.0.2 through `paddock-host` (pooling allocator, epoch interruption on, 10 ms tick). Native code is built for generic x86-64 with no `target-cpu` flag.

## How to run

```sh
cd spikes
cargo build -p s8-guest --target wasm32-wasip2 --release
cargo build -p s8-bench --release
./target/release/s8-bench target/wasm32-wasip2/release/s8_guest.wasm 1   # 1 tokio worker

# Native SHA-256 without SHA extensions
cargo build -p s8-bench --release --features soft --target-dir target/soft
# Guest with wasm SIMD
RUSTFLAGS="-C target-feature=+simd128" cargo build -p s8-guest --target wasm32-wasip2 --release --target-dir target/simd
```

## Method

- `kernels/` holds the workloads. The same source is compiled natively into the bench binary and to `wasm32-wasip2` inside `guest/`, a Paddock app.
- "In Paddock" is a full round trip: `InstanceHandle::call`, the mailbox, `handle-call` and the reply.
- Each sample repeats a workload until it has run for at least 20 ms. The table shows the median of 5 samples, per run of the workload.
- Native and guest results are compared and must match exactly. They do, including n-body's floating-point energy.
- The SHA-256 rows hash a buffer repeatedly, chaining each digest into the next. "1 per call" means one call per hash, so call overhead is included. "100k per call" amortizes it.
- Workloads:
  - SHA-256 (`sha2` 0.10)
  - n-body and fannkuch-redux from the Computer Language Benchmarks Game, plain single-threaded versions

## Results

Empty call (no work): native function 15 ns. Paddock call 1.98 µs with 1 tokio worker; 4.8 µs with 2 workers, where the reply crosses threads.

Native SHA-256 with SHA extensions (the default build), scalar guest, 1 worker:

| Workload | Native | In Paddock | Ratio |
| --- | --- | --- | --- |
| sha256 64 B, 1 per call | 226 ns | 2.98 µs | 13.2x |
| sha256 1 KiB, 1 per call | 772 ns | 6.60 µs | 8.6x |
| sha256 64 B, 100k per call | 90 ns | 519 ns | 5.8x |
| sha256 1 MiB, 50 per call | 576 µs | 3.89 ms | 6.8x |
| n-body, 5M steps | 241 ms | 568 ms | 2.35x |
| fannkuch-redux, n=10 | 220 ms | 344 ms | 1.56x |

Native SHA-256 in software (`--features soft`), same scalar guest:

| Workload | Native | In Paddock | Ratio |
| --- | --- | --- | --- |
| sha256 64 B, 1 per call | 615 ns | 2.95 µs | 4.8x |
| sha256 1 KiB, 1 per call | 4.12 µs | 6.55 µs | 1.59x |
| sha256 64 B, 100k per call | 485 ns | 519 ns | 1.07x |
| sha256 1 MiB, 50 per call | 3.81 ms | 3.89 ms | 1.02x |

Guest built with `+simd128`:

| Workload | Native | In Paddock | Ratio |
| --- | --- | --- | --- |
| n-body, 5M steps | 241 ms | 438 ms | 1.82x |
| fannkuch-redux, n=10 | 222 ms | 348 ms | 1.57x |
| sha256 (any size) | | | unchanged |

## Findings

1. **Plain integer code runs at native speed.** Software SHA-256 is within 2–7% of native once call overhead is amortized. Cranelift's code for this kind of work matches LLVM's closely.
2. **CPU extensions are most of the gap.** With SHA-NI, native SHA-256 is about 6.7x faster than any wasm build can be: wasm has no SHA instructions, and WASI offers no host crypto interface. A host-provided hash or crypto interface would close it; that is not on the roadmap.
3. **SIMD matters for float code.** n-body drops from 2.35x to 1.82x with `simd128`. The remaining gap is not broken down. Likely causes are epoch-interruption checks at loop back-edges, and register allocation. Both are unmeasured.
4. **Array-heavy code costs about 1.6x.** fannkuch-redux is bounded by memory access and branches; SIMD does not help it.
5. **A call costs about 2 µs on the same worker, 4.8 µs across threads.** S2 measured 1.1 µs for a bare actor round trip. The rest is Paddock's call path: building the envelope, the oneshot reply channel, the timeout wrapper, and pending-reply bookkeeping. For a 64-byte hash that overhead is 13x the native cost; for anything over about 50 µs of work it is in the noise.

## Effect on the design

- Paddock suits coarse-grained work: tasks taking more than tens of microseconds per message. Chatty, tiny calls pay a 2–5 µs floor each.
- Consider enabling `simd128` by default for guests built with `paddock bundle` (M8). Every current wasm runtime supports it.
- Crypto-heavy apps should expect native SHA and AES to be several times faster. A `paddock:crypto` host interface would be a later-phase option.
- M9's baseline benchmarks should reuse this harness, and split out the cost of epoch interruption by comparing against a raw wasmtime store without it.
