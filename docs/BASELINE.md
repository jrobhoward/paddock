# Baseline numbers

Phase 1 reference figures for the host runtime. They are recorded, not gated: CI does not fail on them. Rerun after changes to the call path, the instance loop or the engine configuration, and add a dated section rather than overwriting this one.

## How to run

`scripts/bench.sh` runs every benchmark below, and the newer ones, one at a time on an idle machine, and writes a dated section to `target/bench/` (`--list` names them). The commands below run one by hand.

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

Same machine, but probably not idle: a TLA+ model check using about 8 of the 16 cores was likely running at the time (load average about 10 an hour later). Treat these figures as provisional; the 2026-10-15 section below reruns them on an idle machine. `watch_cost.rs` runs `paddock-fixture-sdk` instances against a shared `memory` bucket. A writer instance sets one key 2,000 times through `wasi:keyvalue` (median per call) while 0 to 1,000 other instances watch that key, or another key, through `paddock:watch`. Write to notice: the time from starting a write until a call to the watcher returns the change, polling with calls (signals are taken before calls), median of 1,000.

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

## Modbus point throughput (roadmap F6)

`modbus_load.rs` runs in-process fake Modbus TCP devices whose registers all change on every read (the worst case for report by exception), polls them through the `paddock-modbus` extension over loopback, and delivers each scan to `examples/scada/points`, a hash-routed pool that writes every update into a shared `memory` bucket in one batch. It reports updates stored per second against polls asked, and points per second.

```sh
cargo build --release --target wasm32-wasip2 -p example-scada-points
cargo run --release -p paddock-modbus --example modbus_load -- 16 100 10 4 4
cargo run --release -p paddock-modbus --example modbus_load -- 64 125 1 4 4
cargo run --release -p paddock-modbus --example modbus_load -- 64 125 1 8 8
```

Arguments: devices, registers per device, poll interval in ms, pool instances, tokio worker threads. Measured on 2026-10-15 (the section below). The fake devices share the node's cores, so the figures will understate what a node polling real devices sustains.

## 2026-10-15: benchmark suite (scripts/bench.sh)

AMD Ryzen Threadripper 1950X 16-Core Processor, 16 threads, 63 GiB, Linux 6.8.0-146-generic; rustc 1.99.0; commit 49f6abd with local changes; load average 0.31 at the start.

### baseline: call, cast, instantiate, idle memory (1 and 4 workers, dense)

`target/release/examples/baseline 1 1000 standard` (1.50 s wall, 1.22 s user, 0.08 s system, 81912 KiB peak RSS):

```text
tokio workers: 1, memory profile: Standard
component: 116 KiB, compiled in 146 ms
call, empty call: p50 2.55 µs, p99 3.26 µs, p99.9 5.78 µs, max 30.02 µs; 384062 calls/s sequential
call, 1 KiB echo: p50 4.60 µs, p99 5.39 µs, p99.9 9.15 µs, max 71.48 µs; 214038 calls/s sequential
cast: 668510 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 1451 times)
instantiate to first reply (1000 instances): p50 83.19 µs, p99 247.64 µs, mean 85.29 µs
idle memory per instance: 58 KiB resident, 0 KiB virtual (process: 80 MiB resident, 8129 GiB virtual)
```

`target/release/examples/baseline 4 1000 standard` (2.01 s wall, 2.24 s user, 0.83 s system, 87092 KiB peak RSS):

```text
tokio workers: 4, memory profile: Standard
component: 116 KiB, compiled in 147 ms
call, empty call: p50 4.49 µs, p99 9.42 µs, p99.9 13.26 µs, max 121.78 µs; 219457 calls/s sequential
call, 1 KiB echo: p50 6.74 µs, p99 10.98 µs, p99.9 13.44 µs, max 67.43 µs; 159531 calls/s sequential
cast: 467893 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 279817 times)
instantiate to first reply (1000 instances): p50 85.56 µs, p99 236.76 µs, mean 88.99 µs
idle memory per instance: 63 KiB resident, 0 KiB virtual (process: 85 MiB resident, 8129 GiB virtual)
```

`target/release/examples/baseline 1 5000 dense` (2.07 s wall, 1.49 s user, 0.37 s system, 343244 KiB peak RSS):

```text
tokio workers: 1, memory profile: Dense
component: 116 KiB, compiled in 173 ms
call, empty call: p50 2.65 µs, p99 3.41 µs, p99.9 6.97 µs, max 20.18 µs; 369507 calls/s sequential
call, 1 KiB echo: p50 4.68 µs, p99 5.50 µs, p99.9 9.60 µs, max 64.05 µs; 210769 calls/s sequential
cast: 657997 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 1471 times)
instantiate to first reply (5000 instances): p50 81.92 µs, p99 224.33 µs, mean 85.28 µs
idle memory per instance: 63 KiB resident, 39 KiB virtual (process: 335 MiB resident, 1265 GiB virtual)
```

### pool_load: concurrent clients against a pool (1, 4, 16 workers)

`target/release/examples/pool_load 1` (12.38 s wall, 12.23 s user, 0.05 s system, 37172 KiB peak RSS):

```text
tokio workers: 1; epoch tick 2000 µs; 1 s per row
work        pool  clients  calls/s   mean µs  p50 µs  p99 µs  p99.9 µs
empty          1       64    416463    153.0   154.6   194.0     244.0
empty          4       64    334239    190.4   180.8   543.2     572.3
empty         16       64    210521    303.7   238.8  1042.1    1180.2
empty         16        1    211022      4.7     4.6     5.6      10.2
empty         16       16    205237     77.9    28.2   292.8     300.6
empty         16      256    221927   1152.3  1161.5  3210.2    3370.5
busy:20000     1       64     29612   2157.5  2058.0  4075.3    4077.8
busy:20000     4       64     29274   2182.5  2061.2  4081.3    4086.6
busy:20000    16       64     27588   2317.1  2718.9  6006.0    6703.7
busy:20000    16        1     27456     36.4    36.0    40.6      65.8
busy:20000    16       16     27399    583.5   174.6  2053.1    2159.0
busy:20000    16      256     27845   9177.1  10178.4  18885.9   23044.1
```

`target/release/examples/pool_load 4` (12.43 s wall, 37.30 s user, 1.98 s system, 47612 KiB peak RSS):

```text
tokio workers: 4; epoch tick 2000 µs; 1 s per row
work        pool  clients  calls/s   mean µs  p50 µs  p99 µs  p99.9 µs
empty          1       64    331619    192.3   191.0   280.8     302.5
empty          4       64    364601    174.7    89.9   712.8     900.3
empty         16       64    231939    275.0    66.9  1418.4    1875.1
empty         16        1    142375      7.0     6.8    12.6      17.3
empty         16       16    223369     71.5    20.4   442.2     575.4
empty         16      256    232583   1096.6   756.0  4540.0    5754.1
busy:20000     1       64     25057   2550.9  2331.9  3837.4    3903.7
busy:20000     4       64    101552    629.5   607.9  1999.3    2679.3
busy:20000    16       64     88789    719.7   318.3  3302.2    4434.8
busy:20000    16        1     24628     40.5    38.8    66.1      72.6
busy:20000    16       16     85802    186.3    52.7  1049.1    1344.7
busy:20000    16      256     90822   2810.9  2123.9  10074.2   12482.3
```

`target/release/examples/pool_load 16` (12.54 s wall, 93.00 s user, 22.69 s system, 118740 KiB peak RSS):

```text
tokio workers: 16; epoch tick 2000 µs; 1 s per row
work        pool  clients  calls/s   mean µs  p50 µs  p99 µs  p99.9 µs
empty          1       64    244746    260.6   263.3   309.5     348.0
empty          4       64    540697    117.7    88.3   442.8     711.0
empty         16       64    524881    121.4    34.2   952.5    1582.7
empty         16        1    150064      6.6     6.7    12.4      16.8
empty         16       16    485846     32.7    27.6   146.5     434.6
empty         16      256    536966    474.5   115.2  2871.2    3942.3
busy:20000     1       64     22933   2787.3  2273.2  3915.1    3950.4
busy:20000     4       64     99008    645.0   461.5  2598.5    3478.5
busy:20000    16       64    315575    202.1    53.4  1402.4    2945.9
busy:20000    16        1     24716     40.4    38.4    66.1      73.5
busy:20000    16       16    294001     54.3    49.4   144.0     844.0
busy:20000    16      256    322772    790.4   629.0  2818.8    3676.4
```

### epoch: epoch-check overhead

`target/release/examples/epoch_cost` (12.65 s wall, 12.54 s user, 0.13 s system, 28336 KiB peak RSS):

```text
median of 7 runs; epoch tick 2 ms, yield per tick
tight loop, 500M rounds: epoch off 543.3 ms, epoch on 543.4 ms, ratio 1.000
recursion, fib(35): epoch off 68.0 ms, epoch on 72.8 ms, ratio 1.071
sieve, 16M: epoch off 157.7 ms, epoch on 194.0 ms, ratio 1.230
```

### log: guest log records

`target/release/examples/log_cost` (0.46 s wall, 0.46 s user, 0.00 s system, 21476 KiB peak RSS):

```text
filtered (debug record, level info): 3 ns per record (median of 5 runs of 100000)
emitted (debug record, level debug): 519 ns per record (median of 5 runs of 100000)
```

### admin: admin API latency (1 and 4 workers)

`target/release/examples/admin_latency 1` (2.60 s wall, 1.51 s user, 1.03 s system, 29100 KiB peak RSS):

```text
tokio workers: 1; 20000 calls each
in process (Node::call): p50 4.7 µs, p99 5.7 µs, p99.9 11.1 µs; 256705 calls/s
admin API, 1 client: p50 67.5 µs, p99 122.1 µs, p99.9 179.2 µs; 14475 calls/s
admin API, 16 clients: p50 767.4 µs, p99 1018.8 µs, p99.9 1278.7 µs; 21394 calls/s
```

`target/release/examples/admin_latency 4` (2.41 s wall, 2.61 s user, 2.24 s system, 30908 KiB peak RSS):

```text
tokio workers: 4; 20000 calls each
in process (Node::call): p50 4.7 µs, p99 7.8 µs, p99.9 12.6 µs; 204600 calls/s
admin API, 1 client: p50 69.6 µs, p99 120.5 µs, p99.9 162.3 µs; 13688 calls/s
admin API, 16 clients: p50 536.9 µs, p99 807.8 µs, p99.9 917.8 µs; 30819 calls/s
```

### watch: keyvalue writes with watchers, memory and disk buckets

`target/release/examples/watch_cost 4` (7.20 s wall, 26.21 s user, 0.31 s system, 218912 KiB peak RSS):

```text
4 worker threads, a Memory bucket; median of 2000 writes each
write,    0 watchers on the key: 9.1 µs
write,    1 watchers on the key: 10.1 µs
write,  100 watchers on the key: 240.2 µs
write, 1000 watchers on the key: 3107.3 µs
write, 1000 watchers on another key: 7.9 µs
write to notice handled: 28.2 µs (two plain calls: 10.5 µs; median of 1000)
```

`target/release/examples/watch_cost 4 disk` (17.15 s wall, 31.42 s user, 0.94 s system, 225820 KiB peak RSS):

```text
4 worker threads, a Disk bucket; median of 2000 writes each
write,    0 watchers on the key: 818.5 µs
write,    1 watchers on the key: 822.0 µs
write,  100 watchers on the key: 1055.4 µs
write, 1000 watchers on the key: 3779.0 µs
write, 1000 watchers on another key: 806.9 µs
write to notice handled: 854.4 µs (two plain calls: 33.6 µs; median of 1000)
```

### publish: a publish to 1 to 1,000 group members

`target/release/examples/publish_cost 4` (1.01 s wall, 2.05 s user, 0.16 s system, 154472 KiB peak RSS):

```text
4 worker threads; 200 publishes per size
    1 members: publish call 8.8 µs (median), 85227 deliveries/s handled
   10 members: publish call 24.4 µs (median), 363205 deliveries/s handled
  100 members: publish call 173.7 µs (median), 551917 deliveries/s handled
 1000 members: publish call 1681.2 µs (median), 555593 deliveries/s handled
```

### ingress: HTTP ingress throughput

`target/release/examples/ingress_throughput 4 4` (20.53 s wall, 41.96 s user, 32.91 s system, 31836 KiB peak RSS):

```text
ingress_throughput: 4 server workers, a pool of 4, 4 client workers
1 connection: 12371 requests/s, p50 84 us, p99 114 us
8 connections: 70267 requests/s
32 connections: 82076 requests/s
128 connections: 87593 requests/s
```

### cluster: calls, casts and payloads between two nodes over QUIC

`target/release/examples/cluster_throughput 4` (44.73 s wall, 120.88 s user, 26.57 s system, 65256 KiB peak RSS):

```text
cluster_throughput: 4 tokio workers for both nodes, QUIC over loopback, 16 instances per node
one node: sequential calls 282869/s, p50 3.5 us, p99 6.1 us
two nodes: sequential calls 9222/s, p50 105.8 us, p99 137.3 us
16 callers: one node 451414 calls/s, two nodes 61972 calls/s
64 callers: one node 468893 calls/s, two nodes 55631 calls/s
one node: casts 990579 handled/s (400000 of 400000 handled, 0 refused when sent)
two nodes: casts 181630 handled/s (295761 of 400000 handled, 104239 refused when sent)
1 KiB bodies, 16 callers: one node 390661 calls/s (382 MiB/s each way), two nodes 50443 calls/s (49 MiB/s each way)
64 KiB bodies, 16 callers: one node 26592 calls/s (1662 MiB/s each way), two nodes 4433 calls/s (277 MiB/s each way)
```

### passivation: wake cost, instances per Store slot, random calls

`target/release/examples/passivation_cost 4 1000 100000` (16.99 s wall, 28.60 s user, 22.81 s system, 2558520 KiB peak RSS):

```text
passivation_cost: 4 workers, 1000 Store slots, 100000 instances, passivated after 20 ms idle
runtime started: RSS 20 MiB, address space 8259014 MiB
100000 instances started and passivated in 4.6 s; Stores held: 0
all passivated: RSS 2460 MiB, address space 8261514 MiB
per passivated instance: 25.0 KiB RSS, 25.6 KiB address space
call that wakes its instance: p50 64 us, p90 111 us, p99 253 us, max 331 us (2000 calls)
call to a live instance: p50 6 us, p90 9 us, p99 18 us, max 28 us (2000 calls)
random calls, 64 callers: 29433 calls/s
random call: p50 1803 us, p90 2772 us, p99 16304 us, max 45440 us (147166 calls)
after random calls: RSS 2487 MiB, address space 8261514 MiB
counts kept: 151166 of 151166 increments
```

`target/release/examples/passivation_cost 4 100 2000` (5.75 s wall, 12.23 s user, 7.73 s system, 83508 KiB peak RSS):

```text
passivation_cost: 4 workers, 100 Store slots, 2000 instances, passivated after 20 ms idle
runtime started: RSS 19 MiB, address space 826248 MiB
2000 instances started and passivated in 0.2 s; Stores held: 0
all passivated: RSS 67 MiB, address space 826249 MiB
per passivated instance: 24.5 KiB RSS, 0.5 KiB address space
call that wakes its instance: p50 101 us, p90 113 us, p99 145 us, max 240 us (2000 calls)
call to a live instance: p50 10 us, p90 11 us, p99 13 us, max 25 us (2000 calls)
random calls, 64 callers: 30871 calls/s
random call: p50 2099 us, p90 2363 us, p99 3267 us, max 14588 us (154355 calls)
after random calls: RSS 80 MiB, address space 826249 MiB
counts kept: 158355 of 158355 increments
```

`target/release/examples/passivation_cost 4 1000 1000 off` (5.43 s wall, 19.11 s user, 1.37 s system, 119628 KiB peak RSS):

```text
passivation_cost: 4 workers, 1000 Store slots, 1000 instances, no passivation
runtime started: RSS 19 MiB, address space 8259014 MiB
1000 instances started in 0.1 s; Stores held: 1000
all idle live: RSS 87 MiB, address space 8259015 MiB
per idle live instance: 69.0 KiB RSS, 1.0 KiB address space
call to a live instance: p50 3 us, p90 3 us, p99 5 us, max 8 us (1000 calls)
random calls, 64 callers: 381689 calls/s
random call: p50 111 us, p90 403 us, p99 899 us, max 1946 us (1908446 calls)
after random calls: RSS 91 MiB, address space 8259015 MiB
counts kept: 1910446 of 1910446 increments
```

### modbus: Modbus points through the scada pool

`target/release/examples/modbus_load 16 100 10 4 4` (12.30 s wall, 12.09 s user, 0.90 s system, 26360 KiB peak RSS):

```text
16 devices x 100 registers every 10 ms, pool of 4, 4 workers: 1601 updates/s stored (1600 polls/s asked), 160095 points/s
```

`target/release/examples/modbus_load 64 125 1 4 4` (12.38 s wall, 37.00 s user, 2.38 s system, 37636 KiB peak RSS):

```text
64 devices x 125 registers every 1 ms, pool of 4, 4 workers: 6203 updates/s stored (64000 polls/s asked), 775367 points/s
```

`target/release/examples/modbus_load 64 125 1 8 8` (12.49 s wall, 41.66 s user, 2.48 s system, 45052 KiB peak RSS):

```text
64 devices x 125 registers every 1 ms, pool of 8, 8 workers: 4759 updates/s stored (64000 polls/s asked), 594880 points/s
```

### s14: spike S14: storage in the guest and host services

`spikes/target/release/s14-bench spikes/target/wasm32-wasip2/release/s14_guest.wasm` (28.89 s wall, 20.45 s user, 3.89 s system, 1044876 KiB peak RSS):

```text
host import round trip (process.self, from the guest): 247 ns
| value | op | native redb mem | guest redb mem | guest -> host kv mem | native redb file nosync | guest redb file nosync | native redb file fsync | guest redb file fsync | guest -> host kv disk (fsync) |
|---|---|---|---|---|---|---|---|---|---|
| 16 B | put | 27.75 µs | 42.46 µs | 1.95 µs | 40.05 µs | 0.20 ms | 0.89 ms | 2.93 ms | 1.05 ms |
| 16 B | get | 1.64 µs | 2.44 µs | 1.47 µs | 1.36 µs | 2.57 µs | 1.08 µs | 2.33 µs | 3.91 µs |
| 1024 B | put | 40.78 µs | 61.64 µs | 2.41 µs | 48.24 µs | 0.27 ms | 1.49 ms | 2.88 ms | 1.57 ms |
| 1024 B | get | 3.63 µs | 3.06 µs | 1.63 µs | 2.41 µs | 9.91 µs | 3.04 µs | 2.42 µs | 6.74 µs |
| 65536 B | put | 0.31 ms | 0.40 ms | 27.21 µs | 0.15 ms | 0.46 ms | 2.96 ms | 3.35 ms | 3.16 ms |
| 65536 B | get | 22.36 µs | 11.57 µs | 11.70 µs | 53.15 µs | 2.73 µs | 46.58 µs | 2.71 µs | 69.79 µs |
```

### s15: spike S15: HTTP in the guest and the host, and the connection sweep

`spikes/target/release/s15-bench 4` (15.54 s wall, 21.58 s user, 20.42 s system, 85884 KiB peak RSS):

```text
server tokio workers: 4; load generator: 4 workers, separate threads
## Ingress (a native client calls a server)
| Server | 1 connection: p50 | p99 | req/s | 32 connections: p50 | p99 | req/s |
|---|---|---|---|---|---|---|
| native hyper | 67.8 µs | 76.5 µs | 14607 | 185.7 µs | 482.4 µs | 155227 |
| host hyper -> Paddock instance call | 100.1 µs | 147.9 µs | 9680 | 272.1 µs | 633.1 µs | 110548 |
| wasi:http incoming-handler (instance per request) | 162.3 µs | 216.5 µs | 5943 | 1435.6 µs | 2657.5 µs | 22404 |
| guest-owned socket (wasi:sockets, guest parses HTTP) | 66.4 µs | 99.1 µs | 14883 | 402.3 µs | 456.1 µs | 7487 |
## Egress (a client calls a native hyper server)
| Client | per request (p50 \| p99 \| req/s, or mean) |
|---|---|
| native hyper, one keep-alive connection | 51.9 µs | 61.0 µs | 19486 |
| native hyper, new connection per request | 196.5 µs | 233.5 µs | 5046 |
| guest, wasi:http outgoing-handler (wstd Client) | mean 172.3 µs |
| guest, wasi:sockets, one keep-alive connection, guest parses HTTP | mean 32.5 µs |
```

`spikes/target/release/s15-bench 4 sweep` (31.99 s wall, 39.11 s user, 45.70 s system, 74752 KiB peak RSS):

```text
| Server | connections | p50 | p99 | req/s |
|---|---|---|---|---|
| native hyper | 1 | 52.4 µs | 61.7 µs | 19638 |
| native hyper | 2 | 26.9 µs | 55.4 µs | 60822 |
| native hyper | 4 | 43.2 µs | 99.1 µs | 84474 |
| native hyper | 8 | 70.5 µs | 152.5 µs | 105363 |
| native hyper | 16 | 104.9 µs | 277.6 µs | 135112 |
| native hyper | 32 | 187.2 µs | 490.6 µs | 157659 |
| guest-owned socket | 1 | 37.0 µs | 49.8 µs | 26445 |
| guest-owned socket | 2 | 49.0 µs | 69.0 µs | 40191 |
| guest-owned socket | 4 | 74.5 µs | 157.0 µs | 41777 |
| guest-owned socket | 8 | 171.6 µs | 353.3 µs | 40747 |
| guest-owned socket | 16 | 277.2 µs | 459.9 µs | 25269 |
| guest-owned socket | 32 | 408.0 µs | 460.1 µs | 7374 |
| 8 guest-owned sockets, spread | 8 | 103.1 µs | 241.9 µs | 74052 |
| 8 guest-owned sockets, spread | 16 | 145.7 µs | 426.6 µs | 97381 |
| 8 guest-owned sockets, spread | 32 | 284.6 µs | 715.9 µs | 106422 |
```

### Reading the 2026-10-15 numbers

- **Between nodes.** A call to an instance on the other node, over QUIC on loopback with both nodes in one process, takes about 106 µs at p50 against 3.5 µs on one node: about 9,200 sequential calls per second. Concurrent callers reach about 62,000 calls per second, about 13% of one node's rate, and 64 KiB bodies move 277 MiB/s each way. Where the 100 µs goes (the QUIC stream per frame, encryption, wake-ups across tasks) is not attributed. In the cast burst, 104,239 of 400,000 casts were refused at once by the per-peer send queue's frame cap (65,536 frames, D8), and every cast accepted was handled. The figure is between two processes' worth of work in one process; between machines, the network adds its own latency.
- **HTTP ingress.** 12,400 requests per second on one keep-alive connection (p50 84 µs), and 82,000 to 88,000 with 32 to 128 connections, against a pool of 4. S15's host-hyper shape measured 110,500 at 32 connections in the same suite; the clients differ (S15's is hyper, this one a plain HTTP/1.1 loop) and so does the app, and the gap is not explained yet.
- **Groups.** A publish call costs about 1.7 µs per member on its node, since the host casts to every member before it returns; deliveries level off at about 550,000 per second on 4 workers.
- **`disk` buckets.** A write costs about 810 µs, the redb commit with its fsync; watchers add what they add on a `memory` bucket (about 3 µs each on the written key). From a write to a watcher having handled its notice: 854 µs.
- **Passivation (S11).** A call that wakes its instance takes 64 to 101 µs at p50; 100,000 instances held 25 KiB each on 1,000 Store slots; calls spread at random, nearly all waking their instance, ran at 29,000 to 31,000 per second. `spikes/s11/README.md` has the reading.
- **Modbus.** 160,000 points per second at the paced load (16 devices every 10 ms, every poll stored). At 64 devices polled every millisecond, the node stored 6,200 updates per second (775,000 points) with a pool of 4 and 4 workers, but fewer, 4,800 (595,000 points), with 8 and 8; the fake devices share the cores, and why more workers stored less is not explained yet.
- **Not measured:** the cost of the per-callback metric counters, which needs a build without them.

## 2026-10-15: host time by call hook, and HTTP egress (roadmap E2)

Same machine as the suite above (Threadripper 1950X, 16 threads); commit bfae14b with E2's changes; the load average was 0.5 to 3 from builds between runs, so read differences under about 10% as noise.

`baseline` before and after the Store's call hook (wasmtime `call-hook`, which times every host import as host time), three runs each:

| Workers | Empty call p50, before | after | Casts handled per second, before | after |
| --- | --- | --- | --- | --- |
| 1 | 2.59, 2.60, 2.73 µs | 2.60, 2.62, 2.73 µs | 615,000 to 692,000 | 654,000 to 659,000 |
| 4 | 3.47, 3.49, 4.56 µs | 3.53, 4.55, 4.65 µs | 472,000 to 521,000 | 431,000 to 477,000 |

On 1 worker the hook is within run-to-run variation; on 4 workers both sets vary more than they differ.

`egress_cost`: a guest's `GET` through `wasi:http`, the node's filter and its connection pool, to a local hyper server answering `hi`, driven by calls (each adds about 3 µs); a native hyper client on a kept-alive connection for comparison. Two runs each at 4 workers, one at 1:

| Client | Workers | Mean per request, sequential | Requests per second, 16 instances at once |
| --- | --- | --- | --- |
| native hyper, kept-alive | 4 | 29 to 34 µs | |
| guest, pooled | 1 | 86 µs | 15,700 |
| guest, pooled | 4 | 140 to 164 µs | 34,100 to 35,500 |
| guest, a new connection per request (`pool_max_idle_per_host = 0`) | 4 | 181 to 206 µs | 15,300 to 15,500 |

- The pool doubles throughput under load; one request at a time it saves less, because a TCP connection on loopback is cheap (no TLS here; over a network, each new connection also costs round trips and, for HTTPS, a handshake).
- What is left is not the connection: on 1 worker a request takes 86 µs, on 4 it takes 140 to 164 µs. wasmtime-wasi-http spawns a task per request and hyper's pooled connection runs on a task of its own, so a request crosses tasks several times, and on several workers each crossing is a wake-up on another thread. Not attributed further.
- Spike S15 measured 172 µs mean through wasmtime's own sender (a new connection per request) on the same machine.

## Not measured yet

- Epoch-check overhead on compiled Rust apps beyond S8's kernels.
- The per-callback metric counters.
