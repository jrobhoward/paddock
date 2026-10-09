# Baseline numbers

Phase 1 reference figures for the host runtime. They are recorded, not gated: CI does not fail on them. Rerun after changes to the call path, the instance loop or the engine configuration, and add a dated section rather than overwriting this one.

## How to run

```sh
cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
cargo run --release -p paddock-host --example baseline -- 1 1000 standard
cargo run --release -p paddock-host --example baseline -- 4 1000 standard
cargo run --release -p paddock-host --example baseline -- 1 5000 dense
```

Arguments: tokio worker threads, instances to spawn, and memory profile. The harness (`crates/paddock-host/examples/baseline.rs`) drives a `Runtime` directly, with no node or admin API in the path. The guest is the chaos fixture (114 KiB component), and the client runs as a task on the runtime, as an instance does.

- **Call:** 100,000 sequential `InstanceHandle::call`s to one instance. That is the full call path: envelope, mailbox, `handle-call`, reply and timeout wrapper.
- **Cast:** 200,000 `count` casts into a 1,024-slot mailbox. The sender yields when the mailbox is full. A final call checks that every cast was handled exactly once.
- **Instantiate:** `spawn` to the first reply, which takes in Store creation, WASI context, `init` and one call. Instances are spawned one after another.
- **Idle memory:** the change in process RSS and virtual size from `/proc/self/statm` after spawning N idle instances, divided by N.

## 2026-10-06

AMD Ryzen Threadripper 1950X (16 cores, 62 GiB), Linux 6.8 x86_64, rustc 1.99.0, wasmtime 49.0.2, release build, epoch tick 10 ms, pooling allocator.

| Measure | 1 worker, standard | 4 workers, standard | 1 worker, dense |
| --- | --- | --- | --- |
| Empty call p50 / p99 / p99.9 | 2.33 / 2.95 / 7.06 µs | 3.83 / 6.83 / 13.1 µs | 2.35 / 2.98 / 7.19 µs |
| 1 KiB echo call p50 / p99 | 4.47 / 5.18 µs | 5.27 / 7.15 µs | 4.41 / 5.14 µs |
| Sequential calls per second | 417,000 | 251,000 | 415,000 |
| Casts handled per second | 787,000 | 530,000 | 759,000 |
| Instantiate to first reply, p50 / p99 (1,000) | 78.9 / 219 µs | 77.0 / 239 µs | 76.3 / 195 µs |
| Idle resident memory per instance (1,000) | 56 KiB | 56 KiB | 52 KiB |
| Process virtual size, 1,000-slot pool | 8,129 GiB | 8,129 GiB | 254 GiB |

At 5,000 instances on the dense profile: instantiate p50 75.8 µs, p99 213 µs; 58 KiB resident per instance; 312 MiB resident and 1,265 GiB virtual for the whole process.

## Reading the numbers

- **Calls.** About 2.3 µs on one worker, matching spike S8's 1.98 µs. With more workers the reply often lands on another thread, so latency roughly doubles and the tail grows. A 1 KiB echo adds about 2 µs for the copy in and out, plus building the reply.
- **Casts** cost about half a call (around 1.3 µs each handled). No reply channel, and the instance drains its mailbox in a batch. With 4 workers the sender filled the mailbox 300,000 times. Sender and instance run in parallel, the instance is the bottleneck, and the sender spins on `overloaded`.
- **Instantiation** costs about 77 µs, against S2's 35 µs for a bare wasmtime instance. The rest is Paddock's own work per instance: the WASI context and preopens, host-service state, the mailbox and task, `init`, and the first call's round trip.
- **Idle memory** is 52–58 KiB per instance (S2: 30–40 KiB for bare instances). The extra is host-side state and the guest's first heap pages after `init`. 1,000 idle instances cost about 55 MiB.
- **Virtual size** is reserved up front by the pooling allocator for every slot (`max_instances`), so it does not grow per instance. The standard profile reserves about 8 GiB per slot; the dense profile about 256 MiB, which lets one node hold tens of thousands of slots (S2).

## 2026-10-07: epoch interruption and concurrent clients

Same machine and toolchain as 2026-10-06 (AMD Ryzen Threadripper 1950X, 16 cores, Linux 6.8 x86_64, rustc 1.99.0, wasmtime 49.0.2, release build).

```sh
cargo run --release -p paddock-host --example epoch_cost
cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
cargo run --release -p paddock-host --example pool_load -- 1    # and 4, 16
```

### Epoch interruption

`epoch_cost.rs` runs three hand-written WAT kernels on two wasmtime engines that differ only in `epoch_interruption`. The epoch side is driven as `paddock-host` drives it: a 10 ms tick and a yield to tokio on every tick. Both sides call through `call_async`. Median of 7 runs, consistent across three invocations:

| Kernel | Epoch off | Epoch on | Ratio |
| --- | --- | --- | --- |
| Tight loop (LCG, 500M rounds) | 543.4 ms | 543.4 ms | 1.00 |
| Recursion (`fib(35)`) | 68.0 ms | 72.7 ms | 1.07 |
| Sieve over linear memory (16 Mi) | 157–159 ms | 188–194 ms | 1.19–1.23 |

- An epoch check is a load, a compare and a branch at every function entry and loop back edge.
- When a loop body is a long dependency chain (the LCG multiply), the check costs nothing measurable.
- In short loops dominated by memory stores (the sieve's marking loop), it costs about 20%.
- Rust code from spike S8 ran with epoch interruption on at 1.02x native (software SHA-256), so for that code the checks cost at most about 2%.
- These kernels are an upper bound for check-dense code, not a typical app. Paddock has no setting to turn the checks off: preemption and the callback deadline depend on them.

### Concurrent clients against a pool

`pool_load.rs` starts the chaos fixture as an app with a pool of P instances (round-robin) and runs C client tasks that call it back to back for 1 s. Work per call is either none (`echo:x`) or `busy:20000`, about 31 µs of guest compute. Latency is per call, as the client sees it.

| Workers | Work | Pool | Clients | Calls/s | Mean µs | p50 µs | p99 µs |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | empty | 1 | 64 | 453,000 | 141 | 142 | 172 |
| 1 | empty | 16 | 64 | 220,000 | 290 | 81 | 1,019 |
| 1 | busy | 16 | 64 | 27,900 | 2,292 | 1,486 | 7,788 |
| 4 | busy | 4 | 64 | 104,000 | 616 | 512 | 2,460 |
| 4 | busy | 16 | 64 | 89,900 | 710 | 134 | 4,028 |
| 16 | empty | 4 | 64 | 602,000 | 106 | 77 | 415 |
| 16 | empty | 16 | 64 | 578,000 | 110 | 30 | 1,030 |
| 16 | empty | 16 | 256 | 598,000 | 426 | 40 | 3,602 |
| 16 | busy | 1 | 64 | 26,600 | 2,398 | 2,265 | 3,860 |
| 16 | busy | 4 | 64 | 87,300 | 732 | 386 | 3,452 |
| 16 | busy | 16 | 16 | 285,000 | 56 | 47 | 294 |
| 16 | busy | 16 | 64 | 314,000 | 203 | 50 | 2,078 |
| 16 | busy | 16 | 256 | 324,000 | 783 | 51 | 10,716 |
| 16 | busy | 16 | 1 | 26,800 | 37 | 38 | 43 |

The harness prints more combinations than shown here.

- **Throughput scales with workers when the pool has at least as many instances.** With work per call: about 28,000 calls/s on one worker (one core at about 35 µs per call), 90,000–104,000 on 4 workers, and 285,000–324,000 on 16 workers, which is 10–11.6x.
- **A pool smaller than the worker count caps throughput** at about one core per instance: 27,000 calls/s with one instance and about 90,000 with four, whatever the worker count.
- **Empty calls** peak at about 600,000 calls/s on 16 workers. On one worker, a single instance reaches 453,000 calls/s because it drains its mailbox in batches; a pool of 16 there spreads the same work over more task switches and reaches 220,000.
- **Tail latency under oversubscription is uneven.** The mean matches Little's law (clients ÷ throughput: 256 ÷ 324,000/s ≈ 790 µs), but most calls finish near the p50 and a few wait about 10 ms.
  - Cause: an instance with a full mailbox runs callbacks back to back. It gives its worker back only when tokio's cooperative budget runs out (128 mailbox receives) or the 10 ms epoch tick yields it. Meanwhile, clients and instances queued on that worker wait.
  - Tried and not adopted: a `yield_now` after every callback. p99 fell to about 5 ms, but throughput fell too, to 143,000 calls/s with 16 workers, pool 16 and 64 clients (from 314,000), and to 486,000 casts/s in `baseline` (from 787,000).
  - Recorded as roadmap item 0.5, and fixed there by a 2 ms tick (next section).

## 2026-10-07: 2 ms epoch tick (roadmap 0.5)

Same machine. The default epoch tick changed from 10 ms to 2 ms; the sections above ran at 10 ms. `pool_load` takes the tick in µs as its second argument, so both can be rerun.

The tail above follows the tick, not the gaps between messages. With 16 workers, a pool of 16 and 256 clients doing work (two runs each):

| Tick | Calls/s | Mean µs | p50 µs | p99 µs |
| --- | --- | --- | --- | --- |
| 10 ms | 316,700–317,500 | 801–803 | 51–52 | 10,442–11,349 |
| 2 ms | 326,000–331,300 | 771–783 | 539–567 | 3,406–3,943 |
| 1 ms | 318,000–318,500 | 801–803 | 715–732 | 3,063–3,559 |

- At 2 ms the latency is spread evenly: p50 is close to the mean, as queueing predicts, and p99 is a third of what it was.
- The cost falls on a single overloaded instance, which now gives its worker up five times as often: one instance with 64 clients handled 24,800–26,400 calls/s at 10 ms, 21,900–27,400 at 2 ms and 20,700–21,300 at 1 ms. 2 ms keeps most of the fairness of 1 ms at less of this cost.
- `baseline` at 2 ms matches the 2026-10-06 figures within run-to-run variation (1 worker: 2.38 µs empty call p50, 755,000 casts/s, 76.6 µs to instantiate).
- Configurations with fewer workers than busy instances still queue (4 workers, pool 16, 256 clients: mean 2.75 ms); that is waiting for a core, not unfairness.
- Tried and not adopted:
  - A `yield_now` after every callback (previous section).
  - Yielding after 1 ms of back-to-back callbacks, at the 10 ms tick: p99 stayed at 9–11 ms. The tail tracks the tick instead, so the waiting comes from preemption inside callbacks at the tick, not from gaps between messages.
- Epoch-check overhead (`epoch_cost`) is the same at 2 ms: the checks cost the same whatever the tick, and a yield every 2 ms is negligible.

## 2026-10-07: guest log records (roadmap A3)

Same machine. `log_cost.rs` runs `examples/chaos` (built with `paddock-sdk`), whose `log:<n>` command writes n `debug!` records, on one instance with `wasi:logging` granted. Cost per record is the time of `log:100000` minus `log:0`, divided by 100,000; median of 5 runs. No tracing subscriber is installed, so the host's own output formatting is not counted.

```sh
cargo build --release --target wasm32-wasip2 -p example-chaos
cargo run --release -p paddock-host --example log_cost
```

| Record | Before A3 | After A3 |
| --- | --- | --- |
| Below the level (`debug!` at level `info`) | 426–451 ns | 3 ns |
| Emitted (`debug!` at level `debug`) | 607–618 ns | 607–624 ns |

- Before A3 a filtered record was formatted in the guest, copied to the host and dropped there.
- Now the host announces the effective level as a `log-level` host event and the SDK's macros check it before `format!`, so a filtered record is a load and a compare.
- Emitted records cost the same: the guest-side check adds nothing measurable.

## 2026-10-07: admin API and probes (roadmap B1, B5)

Same machine.

```sh
cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
cargo run --release -p paddock --example admin_latency -- 1    # and 4
cargo run --release -p paddock-host --example baseline --features probes -- 1 1000 standard
```

`admin_latency.rs` starts a node with its admin socket, deploys the chaos fixture and times 20,000 `echo:x` calls each way, with the client in the same process as the node. `Client` opens a Unix-socket connection per request, as the CLI and `curl` do. Two runs each:

| Path | Workers | p50 | p99 | Calls/s |
| --- | --- | --- | --- | --- |
| In process (`Node::call`: pool routing, no HTTP) | 1 | 4.3 µs | 5.2–5.3 µs | 272,000–275,000 |
| In process | 4 | 4.6–5.1 µs | 7.6–8.1 µs | 168,000–186,000 |
| Admin API, one client | 1 | 65–66 µs | 121–124 µs | 14,800–15,100 |
| Admin API, one client | 4 | 67–68 µs | 106–108 µs | 14,200–14,400 |
| Admin API, 16 clients | 1 | 728–750 µs | 951–964 µs | 22,000–22,600 |
| Admin API, 16 clients | 4 | 516–533 µs | 796–798 µs | 31,000–32,200 |

- A call through the admin API costs about 60 µs more than one inside the node: connecting, HTTP/1.1 on both sides, and the copies. It suits operators and tests, not a data path.
- 16 clients reach about 2x one client's rate on 4 workers; client and server share the workers here, so a separate client process would see somewhat more.
- The in-process figure is the node's own routing (app lookup, pool pick) on top of the 2.3 µs instance call above.

Probes (`paddock-host`'s `probes` feature, anyprobe USDT) with no tracer attached, `baseline` on 1 worker, two runs each: empty call p50 2.30–2.35 µs without the feature and 2.36–2.40 µs with it, 763,000–770,000 casts/s against 766,000–775,000. The difference is within run-to-run variation.

## 2026-10-09: keyvalue watches (roadmap H4)

Same machine. `watch_cost.rs` runs `paddock-fixture-sdk` instances against a shared `memory` bucket. A writer instance sets one key 2,000 times through `wasi:keyvalue` (median per call) while 0 to 1,000 other instances watch that key, or another key, through `paddock:watch`. Write to notice: the time from starting a write until a call to the watcher returns the change, polling with calls (signals are taken before calls), median of 1,000.

```sh
cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk
cargo run --release -p paddock-host --example watch_cost -- 4    # and 1
```

| Write, watchers | 4 workers (two runs) | 1 worker |
| --- | --- | --- |
| none | 4.8–5.1 µs | 4.8 µs |
| 1 on the key | 5.1–9.7 µs | 9.9 µs |
| 100 on the key | 157–167 µs | 241 µs |
| 1,000 on the key | 3.34–3.36 ms | 0.68 ms |
| 1,000 on another key | 5.2 µs | 5.0 µs |

| Write to notice handled | 4 workers | 1 worker |
| --- | --- | --- |
| Write, then a poll that sees the change | 27.0–27.5 µs | 26.4 µs |
| Two plain calls, for comparison | 9.2–9.9 µs | 7.8 µs |

- Watches on other keys cost a write nothing measurable: exact-key watches are indexed by key, so a write looks up only its own keys (before the index, 1,000 watches on another key made a write 24 µs).
- With watchers on the key, the writer's call time includes the watchers' callbacks, which run on the same workers: each notice wakes an instance and runs `handle-info`, about 3 µs of worker time per watcher on 4 workers. It is not the cost of queueing notices.
- Conflation caps that work when watchers fall behind: on 1 worker the writer outpaces 1,000 watchers, later writes raise queued notices instead of adding callbacks, and a write costs less than with 4 workers.
- A notice is handled about 17–20 µs after the write call starts, beyond two plain calls: the write, the wake-up and one `handle-info` with a JSON decode in the guest.
- Not measured: watches on `disk` buckets (the fsync dominates the write), and resuming a prefix watch over a large bucket (it scans the bucket's keys once).

## Not measured yet

- Epoch-check overhead on compiled Rust apps beyond S8's kernels.
