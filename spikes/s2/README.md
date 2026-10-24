# Spike S2: what does an instance cost?

**Answer: cheap enough for the coarse-grained target, and well past it with one config change.**
- About 35 µs to instantiate.
- About 30–40 KiB of resident memory per idle instance.
- About 0.57 µs per host→guest call; about 1.1 µs per actor round trip on the same worker.

The limit is virtual address space, not RAM. Default reservations cap a node at roughly 15,000 instances; 128 MiB reservations lift that past 20,000 with no measured slowdown.

Run on 2026-10-05: wasmtime 49.0.2, Linux x86_64, 16 cores, 62 GiB RAM, 1 tokio worker. Guest: the S5 counter v1 (Rust, `wasm32-wasip2`, WASI linked, 2.9 MB component). Each instance has its own Store, created through `InstancePre`, then runs `init` and one `inc`.

## How to run

```sh
cd spikes
cargo build -p s2-bench --release
./target/release/s2-bench target/s5/counter_v1.wasm ondemand
./target/release/s2-bench target/s5/counter_v1.wasm pooling
./target/release/s2-bench target/s5/counter_v1.wasm pooling-small 1 1000 5000 20000
```

## Results

| Allocator | Instances | Instantiate mean / p99 (µs) | RSS per instance | Virtual per instance | Call (ns) |
| --- | --- | --- | --- | --- | --- |
| on-demand | 1 | 129 / 129 | 344 KiB (first instance, cold) | 4.06 GiB | 571 |
| on-demand | 100 | 37.4 / 60.8 | 24.0 KiB | 4.06 GiB | 574 |
| on-demand | 1,000 | 37.5 / 49.7 | 31.4 KiB | 4.06 GiB | 575 |
| on-demand | 5,000 | 39.8 / 83.0 | 35.5 KiB | 4.06 GiB | 567 |
| pooling (default reservations) | 100 | 32.3 / 62.7 | 28.0 KiB | 9.4 GiB (reserved up front) | 565 |
| pooling (default reservations) | 1,000 | 33.5 / 48.4 | 34.8 KiB | 8.2 GiB | 570 |
| pooling (default reservations) | 5,000 | 35.1 / 47.0 | 39.3 KiB | 8.1 GiB | 567 |
| pooling, 128 MiB reservation, 64 KiB guard | 1,000 | 33.3 / 68.0 | 34.3 KiB | 263 MiB | 571 |
| pooling, 128 MiB reservation, 64 KiB guard | 5,000 | 34.7 / 63.3 | 39.3 KiB | 260 MiB | 569 |
| pooling, 128 MiB reservation, 64 KiB guard | 20,000 | 33.9 / 47.3 | 38.6 KiB | 259 MiB | 571 |

Actor round trip (mpsc send + `handle-call` + oneshot reply), 100,000 calls: **1.06–1.08 µs** when the client runs on the same worker, **20–22 µs** when it runs on another thread (OS wake-up latency).

## Findings

1. **RAM is not the constraint.** Idle instances cost tens of KiB thanks to copy-on-write memory images and lazy faulting. A real app's RSS will be dominated by its own heap. The first instance costs about 125 µs and 350 KiB more (code pages, first-touch).
2. **Virtual address space is the constraint.** By default each linear memory reserves about 4 GiB plus guards, so bounds checks can be elided. The pooling allocator reserves all slots up front: 2 memory slots per instance in this config came to about 8 GiB each. At about 8 GiB per instance, a 128 TiB user address space caps a node near 15,000 instances. With `memory_reservation(128 MiB)` and a 64 KiB guard it drops to about 260 MiB per instance, so 20,000 instances use about 5 TiB.
3. **Small reservations didn't change call or instantiation time here.** This trivial guest doesn't stress memory access; a memory-heavy guest will pay for bounds checks. Measure that in the baseline benchmarks before making small reservations the default.
4. **Pooling is about 12% faster to instantiate with a tighter p99**, but it must be sized up front. Each live Store keeps an async fiber stack: with the default `total_stacks` (1,000), the 1,001st Store failed with `maximum concurrent limit of 1000 for fibers reached`. Size `total_stacks` to the max instance count.
5. **Call overhead is about 0.57 µs** (async fiber switch, string marshalling, epoch checks), which is negligible next to app work.
6. **Cross-thread wake-ups dominate messaging latency.** Same-worker delivery costs about 1 µs; cross-thread about 20 µs. Phase 2's scheduler could keep chatty instances on the same worker, but that's an optimization, not a Phase 1 need.

## Effect on the design

- Default: pooling allocator, sized from node config (`max_instances`), with `total_stacks = max_instances`.
- Memory reservation is a node setting. Default to large reservations (fast, about 8 GiB virtual each) up to a few thousand instances; switch to 128 MiB reservations for dense nodes. The manifest's `limits.memory_max` must not exceed the reservation.
- Alert when the pool is near full; instantiation fails hard at the limit.
