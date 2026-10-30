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
- Rust code from spike S8 ran with epoch interruption on at 1.02x native (software SHA-256), so for that code the checks cost at most about 2%. On ordinary app code they cost 1.19 to 1.71 times (2026-10-25, "ordinary app code in Wasm").
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
  - Fixed by a 2 ms tick (next section).

## 2026-10-07: 2 ms epoch tick

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

## 2026-10-07: guest log records

Same machine. `log_cost.rs` runs `examples/chaos` (built with `paddock-sdk`), whose `log:<n>` command writes n `debug!` records, on one instance with `wasi:logging` granted. Cost per record is the time of `log:100000` minus `log:0`, divided by 100,000; median of 5 runs. No tracing subscriber is installed, so the host's own output formatting is not counted.

```sh
cargo build --release --target wasm32-wasip2 -p example-chaos
cargo run --release -p paddock-host --example log_cost
```

| Record | Before | After |
| --- | --- | --- |
| Below the level (`debug!` at level `info`) | 426–451 ns | 3 ns |
| Emitted (`debug!` at level `debug`) | 607–618 ns | 607–624 ns |

- Before this change a filtered record was formatted in the guest, copied to the host and dropped there.
- Now the host announces the effective level as a `log-level` host event and the SDK's macros check it before `format!`, so a filtered record is a load and a compare.
- Emitted records cost the same: the guest-side check adds nothing measurable.

## 2026-10-07: admin API and probes

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

## 2026-10-09: keyvalue watches

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

## Modbus point throughput

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

- **Between nodes.** A call to an instance on the other node, over QUIC on loopback with both nodes in one process, takes about 106 µs at p50 against 3.5 µs on one node: about 9,200 sequential calls per second. Concurrent callers reach about 62,000 calls per second, about 13% of one node's rate, and 64 KiB bodies move 277 MiB/s each way. Where the 100 µs goes (the QUIC stream per frame, encryption, wake-ups across tasks) is not attributed. In the cast burst, 104,239 of 400,000 casts were refused at once by the per-peer send queue's frame cap (65,536 frames), and every cast accepted was handled. The figure is between two processes' worth of work in one process; between machines, the network adds its own latency.
- **HTTP ingress.** 12,400 requests per second on one keep-alive connection (p50 84 µs), and 82,000 to 88,000 with 32 to 128 connections, against a pool of 4. S15's host-hyper shape measured 110,500 at 32 connections in the same suite; the clients differ (S15's is hyper, this one a plain HTTP/1.1 loop) and so does the app, and the gap is not explained yet.
- **Groups.** A publish call costs about 1.7 µs per member on its node, since the host casts to every member before it returns; deliveries level off at about 550,000 per second on 4 workers.
- **`disk` buckets.** A write costs about 810 µs, the redb commit with its fsync; watchers add what they add on a `memory` bucket (about 3 µs each on the written key). From a write to a watcher having handled its notice: 854 µs.
- **Passivation (S11).** A call that wakes its instance takes 64 to 101 µs at p50; 100,000 instances held 25 KiB each on 1,000 Store slots; calls spread at random, nearly all waking their instance, ran at 29,000 to 31,000 per second. `spikes/s11/README.md` has the reading.
- **Modbus.** 160,000 points per second at the paced load (16 devices every 10 ms, every poll stored). At 64 devices polled every millisecond, the node stored 6,200 updates per second (775,000 points) with a pool of 4 and 4 workers, but fewer, 4,800 (595,000 points), with 8 and 8; the fake devices share the cores, and why more workers stored less is not explained yet.
- **Not measured:** the cost of the per-callback metric counters, which needs a build without them.

## 2026-10-15: host time by call hook, and HTTP egress

Same machine as the suite above (Threadripper 1950X, 16 threads); commit bfae14b with HTTP egress; the load average was 0.5 to 3 from builds between runs, so read differences under about 10% as noise.

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

## 2026-10-16: batches between apps

Same machine (Threadripper 1950X, 16 threads); load average 0.97 at the start (`scripts/bench.sh batch`). `batch_cost`: `source` instances of the SDK fixture each send 200,000 sample points (about 25 bytes each in CBOR) to a `sink` instance, one cast per point or in batches through `paddock_sdk::batch`; the clock runs from the first send until the sinks have counted every point. Nothing was dropped (the sinks' mailboxes held every message).

| Workers | Pairs | One cast per point | Batch of 1 | 10 | 100 | 1,000 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | 310,000 points/s | 273,000 | 724,000 | 940,000 | 967,000 |
| 1 | 4 | 309,000 | 272,000 | 724,000 | 946,000 | 972,000 |
| 4 | 1 | 343,000 | 355,000 | 1,133,000 | 1,548,000 | 1,708,000 |
| 4 | 4 | 926,000 | 824,000 | 2,587,000 | 3,728,000 | 3,906,000 |

- Batches of 100 move 3 to 4.5 times the points of one cast each. From 100 to 1,000 items the gain is under 10%. At 1,000 a point costs about 1 µs on 1 worker, nearly all of it in the guests (encoding, decoding, counting): the messages' share, about 1.3 µs per batch, is under 1% of it. The split between encoding and decoding was not measured.
- A batch of one costs about 10% more than a plain cast (an array around the item, and a schema id).
- Four pairs on one worker move what one pair does: the worker is the limit. On 4 workers four pairs move about 2.3 times one pair.

## 2026-10-16: HTTP handlers

Same machine (Threadripper 1950X, 16 threads); load average 0.89 at the start (`scripts/bench.sh ingress`). `ingress_throughput` on 4 server workers and 4 client workers, keep-alive HTTP/1.1 on loopback: requests as calls to a pool of 4 SDK-fixture instances (`/plain`), against the `paddock-fixture-http` handler with a new instance per request (`/hello`, a streamed `hello`).

| Shape | 1 connection | p50 | p99 | 8 connections | 32 | 128 |
| --- | --- | --- | --- | --- | --- | --- |
| Request as a call to a pool of 4 | 13,300 requests/s | 66 µs | 111 µs | 68,500 | 78,500 | 86,300 |
| HTTP handler, an instance per request | 3,100 | 182 µs | 233 µs | 21,400 | 24,700 | 25,000 |

- A handler serves about 30% of the pool's requests per second with 32 to 128 connections, and adds about 115 µs to one request: S15 measured the same shape (21,000 to 23,000 at 32 connections, 164 to 193 µs p50) with `wasmtime serve`'s model. The split between instantiation and the `wasi:http` resources was not measured.
- Peak resident memory was lower for the handler run (28.9 MiB against 38.2 MiB): the pool keeps 4 instances alive, a handler only those serving a request.

## 2026-10-16: what the first suite left unexplained

Same machine (Threadripper 1950X, 16 threads). Benchmarks ran back to back, so the load average (1.5 to 6) is mostly their own; each comparison alternates its two sides in one session. `perf` profiles were taken with `kernel.perf_event_paranoid = 2` and `line-tables-only` debug info.

**A call between nodes (106 µs).** The floor is QUIC and tokio, not Paddock. `quic_floor` runs bare quinn over loopback (TLS 1.3 on ring, one stream, 64-byte frames, one in flight):

| Runtime | Round trip p50 |
| --- | --- |
| 4 workers | 52 to 98 µs (it flips between runs) |
| 2 workers | 96 µs |
| 1 worker (multi-thread) | 61 µs |
| current thread | 31 µs |

Paddock's two-node call measured 102 to 126 µs in the same session: about 25 to 50 µs over the floor, the four extra task hand-offs per call (the data writer task and the instance or waiting caller, each way). Writing a frame's length and body separately costs nothing (98.5 against 98.2 µs). Most of the floor is wakes crossing threads on an idle runtime; under load the 16- and 64-caller figures are what matter.

**HTTP ingress against S15 (82k against 110k).** Not the client: under the same client, `ingress_throughput ... s15` (S15's bare hyper service calling chaos instances) served 134k to 136k requests/s at 32 connections, the ingress 88k with a guest doing no CBOR (`chaos` mode) and 78k with the SDK fixture. The profile showed waste on every request, now removed: the app's pool copied its supervisor's whole child list to pick one instance (every message to an app paid it, not only HTTP), the app and handler were looked up twice, headers were copied into strings and the envelope built as a CBOR value tree, and a GET armed a body timer. After (envelope bytes unchanged, checked by a test):

| Mode | 1 connection p50 | 32 connections | 128 connections |
| --- | --- | --- | --- |
| `chaos` (host cost only) | 85 → 71 µs | 88k → 99k | 94k → 110k |
| default (SDK guest) | 86 → 79 µs | 78k → 85k | 84k → 97k |
| `s15` (for reference) | 53 to 60 µs | 134k to 136k | 154k |

What is left is spread thin: hyper's header-read timer (S15 has none), counters, route matching. The guest's CBOR costs about 12% at 32 connections.

**`modbus_load` with 8 workers.** The pool size, not the workers: with 64 devices polled every millisecond (saturated, 64,000 polls/s asked):

| Pool | Workers | Updates stored/s |
| --- | --- | --- |
| 4 | 4 | 5,820, 5,878 |
| 4 | 8 | 7,018, 6,579 |
| 8 | 4 | 5,679 |
| 8 | 8 | 4,595 |

Neither run uses its cores (about 3.3 busy either way), and lock waits are under 0.1% of samples. Every instance writes its 125-key batch into the one shared `memory` bucket, whose `Txn::get`, key comparisons and inserts lead the profile; more writers on more cores likely move its 8,000 keys between caches (not confirmed with hardware counters). The host also decoded each update's whole CBOR body to read its `device` key for hash routing; it now walks the map for the key, falling back to a full decode only for encodings it leaves alone (tags, unusual simple values). Alternated in one session, 64 devices, a pool of 4, 8 workers: 7,176 and 6,518 updates/s stored before, 8,454 and 8,784 after, with 294 and 311 s of user CPU before against 269 and 264 s after.

**Per-callback metric counters.** Nothing measurable: `baseline` (4 workers) and `pool_load 16` with the counters and with them compiled out agree within run-to-run noise (empty call p50 3.5 to 4.6 µs either way; 369k to 384k calls/s at 64 clients either way).

**Boxing the guest callback futures.** Not worth it: a passivated instance stays at 19.3 KiB resident either way (an idle task never touches the futures' space), and call latency does not change.

## 2026-10-18: replicated buckets

`replication_cost` (`scripts/bench.sh replication`): an HA pair and a witness as three runtimes in one process over QUIC on loopback, 4 tokio workers for all three, each pair node's keyvalue database on the root filesystem (ext4 on a Samsung 970 EVO Plus NVMe), the SDK fixture writing 2,000 sequential writes, or as many as 16 or 64 writers manage in 5 s. 16-core machine, idle. Two builds alternated three times each: applying committed entries synced (one synced redb commit per apply) and unsynced (as shipped: the entries are already synced in both nodes' logs; see `replication/log.rs`).

| Write | Synced apply | Unsynced apply |
| --- | --- | --- |
| Disk bucket (node scope), 1 writer | 375 to 965/s, p50 0.9 to 2.5 ms | 950 to 970/s, p50 0.9 ms |
| Replicated, 1 writer on the primary | 208 to 241/s, p50 3.6 ms, p99 11 to 14 ms | 361 to 365/s, p50 2.3 ms, p99 6.9 to 8.4 ms |
| Replicated, 1 writer on the standby (forwarded) | 228 to 231/s, p50 3.8 ms | 338 to 343/s, p50 2.6 ms |
| Disk bucket, 16 or 64 writers | about 1,000/s | about 1,000/s |
| Replicated, 16 writers on the primary | 1,371 to 1,476/s | 1,513 to 1,594/s |
| Replicated, 64 writers on the primary | 3,799 to 4,159/s | 3,970 to 4,288/s |
| Alone (standby gone), 1 writer | 278 to 500/s | 274 to 690/s |
| Alone, 16 writers | 2,896 to 3,005/s | 1,472 to 3,789/s |

- A replicated write costs a synced append on the primary, the frame to the standby, a synced append there and the acknowledgement; with the apply unsynced, 2.3 ms at p50, about 2.5 times a disk bucket write at its faster mode. A forwarded write adds about 0.2 ms.
- Writers arriving together share an append on each node (group commit), so 64 writers reach about four times a disk bucket's rate, which takes one synced commit per write.
- The disk bucket's single-writer figure and the alone figures jump between two modes from run to run with no change in the code (0.9 or 2.5 ms for the same write), which looks like the drive's fsync latency; take ranges, not single runs.
- Not measured: between machines (the network adds its round trip), on the Pi, or with large values.

## 2026-10-18: Raspberry Pi 5

Raspberry Pi 5 Model B Rev 1.1 (Cortex-A76), 4 cores, 8 GiB, Ubuntu 26.04, Linux 7.0.0-1020-raspi, 4 KiB pages; rustc 1.99.1; commit 7d7095d; `scripts/bench.sh` with every benchmark but `s14` and `s15` (the spikes workspace was not built on the Pi), load average 0.02 at the start. Home directory, and so `target/` and the disk buckets, on ext4 on an NVMe drive (Realtek RTS5765DL, DRAM-less). An 8 GB swap partition was on and stayed unused (0 B) throughout. Drive temperature peaked at 49 °C, with no throttling and no errors. The full output is below, after the summary.

Against the 16-core machine (the 2026-10-15 suite unless marked; the Pi's run includes the 2026-10-16 ingress changes, the 16-core suite does not):

| Measure | Pi 5 | 16-core |
| --- | --- | --- |
| Empty call, 1 worker, p50 | 3.74 µs (260k/s) | 2.55 µs (384k/s) |
| Instantiate to first reply, 1 worker, p50 | 79.8 µs | 83.2 µs |
| Idle live instance, standard profile | 104 KiB resident | 58 KiB |
| Idle live instance, dense profile (5,000) | 110 KiB resident | 63 KiB |
| Passivated instance (100,000 on 1,000 Store slots) | 19.2 KiB resident | 19.3 KiB (2026-10-16) |
| `pool_load`, 4 workers, pool of 16, 64 clients, empty / busy:20000 | 671k / 68k calls/s | 232k / 89k calls/s |
| Epoch checks: tight loop, fib(35), sieve (on / off) | 1.50, 1.35, 1.01 | 1.00, 1.07, 1.23 |
| Debug log record emitted | 814 ns | 519 ns |
| Admin API, 1 client, 1 worker, p50 | 87.6 µs | 67.5 µs |
| HTTP ingress, 32 connections | 62.8k requests/s | 85k (2026-10-16) |
| HTTP handler, 32 connections | 23.9k requests/s | about 25k (2026-10-16) |
| Call between two nodes, sequential, p50 | 84.6 µs | 105.8 µs |
| Replicated write, one writer on the primary | 548/s, p50 1.76 ms | 361 to 365/s, p50 2.3 ms (2026-10-18) |

- A call costs about 1.5 times the 16-core machine's on one worker; instantiation costs the same.
- Epoch checks cost more on the Pi in loop-heavy code: a tight loop runs at 1.5 times its unchecked time, against no measurable cost on the 16-core machine. The sieve, which is memory-bound, pays almost nothing.
- An idle live instance takes about 110 KiB resident on the Pi against 58 to 63 KiB, with the same page size and transparent huge pages set to `madvise` on both. The cause has not been looked into. (2026-10-25, B13: shared image pages counted once per instance; 57 KiB proportionally.) A passivated instance costs the same on both.
- The replicated write and the disk bucket (2,105 writes/s, p50 441 µs, against about 960/s) run faster than on the 16-core machine. They are bound by the drive's fsync, and the drives differ; they do not compare the CPUs.
- Modbus on the Pi: 64 devices polled every millisecond store 7,767 updates/s (971k points/s) with a pool of 4 on 4 workers, 6,317 with a pool of 8 on 8 workers (more workers than cores).

Instances per GB, dense profile (`baseline 1 N dense`; the 20,000 and 40,000 runs started at load average 1.86, just after the suite, so their latencies are provisional; the memory figures do not depend on it):

| Instances | Instantiate p50 / p99 | Resident per instance | Process resident | Virtual |
| --- | --- | --- | --- | --- |
| 5,000 | 81.1 / 139 µs | 110 KiB | 563 MiB | 1,265 GiB |
| 20,000 | 81.2 / 139 µs | 111 KiB | 2,204 MiB | 5,056 GiB |
| 40,000 | 83.0 / 165 µs | 111 KiB | 4,392 MiB | 10,111 GiB |

- About 9,400 idle live instances per GiB of RAM, steady from 5,000 to 40,000; the Pi held 40,000 in 4.4 GiB with no swapping. Passivated instances cost 19.2 KiB each, about 54,000 per GiB.
- The dense profile reserved about 256 MiB of address space per slot, 10 TiB for 40,000, without trouble.
- Not measured on the Pi: S14 and S15, more than 40,000 instances, and a guest with a larger heap than the chaos fixture.


### Pi 5: baseline: call, cast, instantiate, idle memory (1 and 4 workers, dense)

`target/release/examples/baseline 1 1000 standard` (1.92 s wall, 1.63 s user, 0.08 s system, 129108 KiB peak RSS):

```text
tokio workers: 1, memory profile: Standard
component: 118 KiB, compiled in 167 ms
call, empty call: p50 3.74 µs, p99 4.87 µs, p99.9 8.87 µs, max 39.76 µs; 260180 calls/s sequential
call, 1 KiB echo: p50 5.68 µs, p99 7.17 µs, p99.9 11.98 µs, max 101.69 µs; 173011 calls/s sequential
cast: 456539 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 1543 times)
instantiate to first reply (1000 instances): p50 79.76 µs, p99 193.33 µs, mean 83.89 µs
idle memory per instance: 104 KiB resident, 0 KiB virtual (process: 126 MiB resident, 8129 GiB virtual)
```

`target/release/examples/baseline 4 1000 standard` (2.16 s wall, 2.44 s user, 0.57 s system, 130968 KiB peak RSS):

```text
tokio workers: 4, memory profile: Standard
component: 118 KiB, compiled in 171 ms
call, empty call: p50 5.24 µs, p99 7.13 µs, p99.9 12.31 µs, max 92.76 µs; 204940 calls/s sequential
call, 1 KiB echo: p50 7.20 µs, p99 9.18 µs, p99.9 15.94 µs, max 117.70 µs; 147079 calls/s sequential
cast: 422226 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 453505 times)
instantiate to first reply (1000 instances): p50 86.37 µs, p99 226.26 µs, mean 91.81 µs
idle memory per instance: 101 KiB resident, 0 KiB virtual (process: 128 MiB resident, 8129 GiB virtual)
```

`target/release/examples/baseline 1 5000 dense` (2.66 s wall, 2.11 s user, 0.34 s system, 577580 KiB peak RSS):

```text
tokio workers: 1, memory profile: Dense
component: 118 KiB, compiled in 217 ms
call, empty call: p50 3.94 µs, p99 6.26 µs, p99.9 9.31 µs, max 56.52 µs; 245372 calls/s sequential
call, 1 KiB echo: p50 6.55 µs, p99 8.30 µs, p99.9 12.98 µs, max 96.35 µs; 149359 calls/s sequential
cast: 420466 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 1443 times)
instantiate to first reply (5000 instances): p50 81.07 µs, p99 139.42 µs, mean 84.35 µs
idle memory per instance: 110 KiB resident, 26 KiB virtual (process: 563 MiB resident, 1265 GiB virtual)
```

### Pi 5: pool_load: concurrent clients against a pool (1, 4, 16 workers)

`target/release/examples/pool_load 1` (12.45 s wall, 12.28 s user, 0.05 s system, 38880 KiB peak RSS):

```text
tokio workers: 1; epoch tick 2000 µs; 1 s per row
work        pool  clients  calls/s   mean µs  p50 µs  p99 µs  p99.9 µs
empty          1       64    293653    217.0   218.0   295.0     377.4
empty          4       64    252027    253.1   219.3   722.3     759.4
empty         16       64    232419    274.9   164.8   938.4    1057.8
empty         16        1    226725      4.3     4.2     6.1      11.9
empty         16       16    228721     69.8    40.5   260.1     272.6
empty         16      256    246634   1035.0  1007.1  3120.4    3326.5
busy:20000     1       64     18221   3506.1  4099.8  4106.3    6070.0
busy:20000     4       64     18021   3544.0  4142.7  6201.9    7908.2
busy:20000    16       64     17815   3589.8  4255.7  8785.2    9815.1
busy:20000    16        1     17537     56.9    55.9    64.7      70.9
busy:20000    16       16     17528    911.8   158.7  2444.0    3397.6
busy:20000    16      256     17959  14200.4  15171.5  26541.3   31187.4
```

`target/release/examples/pool_load 4` (12.62 s wall, 37.16 s user, 1.00 s system, 70944 KiB peak RSS):

```text
tokio workers: 4; epoch tick 2000 µs; 1 s per row
work        pool  clients  calls/s   mean µs  p50 µs  p99 µs  p99.9 µs
empty          1       64    290255    219.7   218.1   289.2     400.3
empty          4       64    785214     80.7    65.0   289.2     383.6
empty         16       64    671479     94.5    14.7   560.4     748.0
empty         16        1    185933      5.3     5.3     8.7      16.0
empty         16       16    658622     24.1     6.1   158.6     212.3
empty         16      256    696692    364.8   166.9  1882.3    2680.5
busy:20000     1       64     16496   3871.7  3765.3  4467.6    5583.0
busy:20000     4       64     69295    921.2   873.8  2668.4    3907.3
busy:20000    16       64     68387    932.9   500.1  4135.8    5659.0
busy:20000    16        1     16937     59.0    58.4    72.0      89.9
busy:20000    16       16     68328    234.0    62.2  1345.7    1739.2
busy:20000    16      256     68848   3704.1  3212.1  12397.2   16015.5
```

`target/release/examples/pool_load 16` (12.59 s wall, 34.59 s user, 0.94 s system, 120652 KiB peak RSS):

```text
tokio workers: 16; epoch tick 2000 µs; 1 s per row
work        pool  clients  calls/s   mean µs  p50 µs  p99 µs  p99.9 µs
empty          1       64    291209    219.0   217.7   265.4     380.5
empty          4       64    635352     99.9    41.9   591.6    1399.7
empty         16       64    586311    108.4     6.3  1338.0    2078.8
empty         16        1    171473      5.7     5.9    10.5      24.1
empty         16       16    557683     28.4     6.1   238.1     748.9
empty         16      256    668001    379.7    20.8  3661.7    5170.0
busy:20000     1       64     16288   3922.3  3940.5  4495.3    5213.0
busy:20000     4       64     64809    985.1   721.8  4302.3    6202.7
busy:20000    16       64     68423    932.6   106.1  6295.2    8478.9
busy:20000    16        1     17327     57.6    57.4    69.2      79.1
busy:20000    16       16     63617    251.2    56.9  2731.1    3747.4
busy:20000    16      256     70445   3615.2  2901.7  14415.6   18843.7
```

### Pi 5: epoch: epoch-check overhead

`target/release/examples/epoch_cost` (31.85 s wall, 31.73 s user, 0.15 s system, 30004 KiB peak RSS):

```text
median of 7 runs; epoch tick 2 ms, yield per tick
tight loop, 500M rounds: epoch off 835.2 ms, epoch on 1252.9 ms, ratio 1.500
recursion, fib(35): epoch off 86.3 ms, epoch on 116.6 ms, ratio 1.351
sieve, 16M: epoch off 830.9 ms, epoch on 839.3 ms, ratio 1.010
```

### Pi 5: log: guest log records

`target/release/examples/log_cost` (0.66 s wall, 0.64 s user, 0.01 s system, 23480 KiB peak RSS):

```text
filtered (debug record, level info): 7 ns per record (median of 5 runs of 100000)
emitted (debug record, level debug): 814 ns per record (median of 5 runs of 100000)
```

### Pi 5: admin: admin API latency (1 and 4 workers)

`target/release/examples/admin_latency 1` (3.34 s wall, 1.92 s user, 1.37 s system, 31416 KiB peak RSS):

```text
tokio workers: 1; 20000 calls each
in process (Node::call): p50 4.2 µs, p99 6.8 µs, p99.9 10.3 µs; 229645 calls/s
admin API, 1 client: p50 87.6 µs, p99 166.6 µs, p99.9 177.9 µs; 11220 calls/s
admin API, 16 clients: p50 1011.7 µs, p99 1319.8 µs, p99.9 1366.9 µs; 15911 calls/s
```

`target/release/examples/admin_latency 4` (2.84 s wall, 2.79 s user, 2.42 s system, 36508 KiB peak RSS):

```text
tokio workers: 4; 20000 calls each
in process (Node::call): p50 6.1 µs, p99 11.1 µs, p99.9 19.5 µs; 166426 calls/s
admin API, 1 client: p50 97.6 µs, p99 134.9 µs, p99.9 180.9 µs; 9914 calls/s
admin API, 16 clients: p50 375.6 µs, p99 709.2 µs, p99.9 1279.0 µs; 41326 calls/s
```

### Pi 5: watch: keyvalue writes with watchers, memory and disk buckets

`target/release/examples/watch_cost 4` (13.50 s wall, 47.05 s user, 2.67 s system, 228636 KiB peak RSS):

```text
4 worker threads, a Memory bucket; median of 2000 writes each
write,    0 watchers on the key: 9.5 µs
write,    1 watchers on the key: 11.8 µs
write,  100 watchers on the key: 239.1 µs
write, 1000 watchers on the key: 6071.2 µs
write, 1000 watchers on another key: 8.9 µs
write to notice handled: 40.9 µs (two plain calls: 13.1 µs; median of 1000)
```

`target/release/examples/watch_cost 4 disk` (16.51 s wall, 55.86 s user, 3.50 s system, 238716 KiB peak RSS):

```text
4 worker threads, a Disk bucket; median of 2000 writes each
write,    0 watchers on the key: 66.6 µs
write,    1 watchers on the key: 75.5 µs
write,  100 watchers on the key: 348.1 µs
write, 1000 watchers on the key: 6984.8 µs
write, 1000 watchers on another key: 83.9 µs
write to notice handled: 144.1 µs (two plain calls: 36.3 µs; median of 1000)
```

### Pi 5: publish: a publish to 1 to 1,000 group members

`target/release/examples/publish_cost 4` (1.77 s wall, 4.21 s user, 0.24 s system, 178844 KiB peak RSS):

```text
4 worker threads; 200 publishes per size
    1 members: publish call 13.3 µs (median), 67677 deliveries/s handled
   10 members: publish call 17.7 µs (median), 393101 deliveries/s handled
  100 members: publish call 140.0 µs (median), 653911 deliveries/s handled
 1000 members: publish call 4343.6 µs (median), 227059 deliveries/s handled
```

### Pi 5: ingress: HTTP ingress throughput: requests as calls to a pool, the ingress with a guest doing no CBOR, S15's bare shape, and an HTTP handler's instance per request

`target/release/examples/ingress_throughput 4 4` (20.74 s wall, 29.66 s user, 26.37 s system, 52920 KiB peak RSS):

```text
ingress_throughput: 4 server workers, a pool of 4, 4 client workers
1 connection: 18096 requests/s, p50 54 us, p99 68 us
8 connections: 53536 requests/s
32 connections: 62794 requests/s
128 connections: 68848 requests/s
```

`target/release/examples/ingress_throughput 4 4 chaos` (20.25 s wall, 26.89 s user, 28.19 s system, 42560 KiB peak RSS):

```text
ingress_throughput: 4 server workers, a pool of 4, 4 client workers
1 connection: 19349 requests/s, p50 51 us, p99 63 us
8 connections: 55971 requests/s
32 connections: 65889 requests/s
128 connections: 70435 requests/s
```

`target/release/examples/ingress_throughput 4 4 s15` (20.24 s wall, 20.87 s user, 32.02 s system, 41676 KiB peak RSS):

```text
ingress_throughput: 4 server workers, S15's shape (bare hyper, 4 chaos instances), 4 client workers
1 connection: 21260 requests/s, p50 46 us, p99 68 us
8 connections: 62773 requests/s
32 connections: 80978 requests/s
128 connections: 87337 requests/s
```

`target/release/examples/ingress_throughput 4 4 handler` (20.56 s wall, 33.22 s user, 21.94 s system, 43304 KiB peak RSS):

```text
ingress_throughput: 4 server workers, an HTTP handler (an instance per request), 4 client workers
1 connection: 3195 requests/s, p50 135 us, p99 222 us
8 connections: 17583 requests/s
32 connections: 23902 requests/s
128 connections: 24129 requests/s
```

### Pi 5: batch: points per second between apps, one cast each or batched

`target/release/examples/batch_cost 1` (14.58 s wall, 14.31 s user, 0.23 s system, 194952 KiB peak RSS):

```text
tokio workers: 1; 200000 points per source
sources  sinks  batch  points/s     messages/s  time
1        1      none   220907       220907      905.4 ms
1        1      1      192330       192330      1039.9 ms
1        1      10     621496       62150       321.8 ms
1        1      100    808886       8089        247.3 ms
1        1      1000   837955       838         238.7 ms
4        4      none   218349       218349      3663.9 ms
4        4      1      190950       190950      4189.6 ms
4        4      10     623676       62368       1282.7 ms
4        4      100    808673       8087        989.3 ms
4        4      1000   836653       837         956.2 ms
```

`target/release/examples/batch_cost 4` (5.39 s wall, 14.70 s user, 0.21 s system, 196196 KiB peak RSS):

```text
tokio workers: 4; 200000 points per source
sources  sinks  batch  points/s     messages/s  time
1        1      none   306013       306013      653.6 ms
1        1      1      307045       307045      651.4 ms
1        1      10     1163476      116348      171.9 ms
1        1      100    1397000      13970       143.2 ms
1        1      1000   1396405      1396        143.2 ms
4        4      none   850813       850813      940.3 ms
4        4      1      705676       705676      1133.7 ms
4        4      10     2456368      245637      325.7 ms
4        4      100    3234432      32344       247.3 ms
4        4      1000   3310779      3311        241.6 ms
```

### Pi 5: egress: outgoing HTTP from a guest through the pool

`target/release/examples/egress_cost 1` (1.65 s wall, 1.41 s user, 0.23 s system, 36020 KiB peak RSS):

```text
tokio workers: 1, server 127.0.0.1:32799, egress pool: true
native hyper, pooled, sequential: p50 23.5 µs, p99 32.8 µs, mean 24.1 µs; 41429 requests/s
guest wasi:http, sequential: p50 94.0 µs, p99 130.9 µs, mean 95.9 µs; 10423 requests/s
guest wasi:http, 16 instances at once: p50 1410.2 µs, p99 2805.0 µs, mean 1445.6 µs; 10969 requests/s
```

`target/release/examples/egress_cost 4` (1.25 s wall, 1.60 s user, 0.33 s system, 41912 KiB peak RSS):

```text
tokio workers: 4, server 127.0.0.1:37619, egress pool: true
native hyper, pooled, sequential: p50 23.8 µs, p99 36.4 µs, mean 24.8 µs; 40262 requests/s
guest wasi:http, sequential: p50 125.0 µs, p99 148.3 µs, mean 125.8 µs; 7945 requests/s
guest wasi:http, 16 instances at once: p50 406.5 µs, p99 657.9 µs, mean 410.3 µs; 38523 requests/s
```

`target/release/examples/egress_cost 4 nopool` (1.64 s wall, 2.12 s user, 1.20 s system, 42132 KiB peak RSS):

```text
tokio workers: 4, server 127.0.0.1:37773, egress pool: false
native hyper, pooled, sequential: p50 23.6 µs, p99 29.7 µs, mean 24.0 µs; 41472 requests/s
guest wasi:http, sequential: p50 185.2 µs, p99 242.9 µs, mean 187.5 µs; 5330 requests/s
guest wasi:http, 16 instances at once: p50 995.6 µs, p99 1519.5 µs, mean 1003.8 µs; 15842 requests/s
```

### Pi 5: cluster: calls, casts and payloads between two nodes over QUIC, and bare quinn's floor

`target/release/examples/cluster_throughput 4` (44.02 s wall, 134.11 s user, 17.19 s system, 76548 KiB peak RSS):

```text
cluster_throughput: 4 tokio workers for both nodes, QUIC over loopback, 16 instances per node
one node: sequential calls 203204/s, p50 5.3 us, p99 6.6 us
two nodes: sequential calls 11583/s, p50 84.6 us, p99 100.8 us
16 callers: one node 737223 calls/s, two nodes 56095 calls/s
64 callers: one node 797981 calls/s, two nodes 82879 calls/s
one node: casts 1011099 handled/s (400000 of 400000 handled, 0 refused when sent)
two nodes: casts 268402 handled/s (348761 of 400000 handled, 51239 refused when sent)
1 KiB bodies, 16 callers: one node 532639 calls/s (520 MiB/s each way), two nodes 42443 calls/s (41 MiB/s each way)
64 KiB bodies, 16 callers: one node 13659 calls/s (854 MiB/s each way), two nodes 1708 calls/s (107 MiB/s each way)
```

`target/release/examples/quic_floor 4` (15.29 s wall, 9.87 s user, 8.10 s system, 22028 KiB peak RSS):

```text
quic_floor: bare quinn ping-pong over loopback, 64-byte frames, one in flight
4 workers, two writes: 19698 round trips/s, p50 49.8 us, p99 58.1 us
4 workers, one write: 19576 round trips/s, p50 50.0 us, p99 59.1 us
current thread, one write: 32691 round trips/s, p50 30.1 us, p99 39.9 us
```

### Pi 5: replication: writes to a replicated (cluster) bucket on a pair and witness over QUIC, against a disk bucket

`target/release/examples/replication_cost 4` (41.85 s wall, 22.29 s user, 8.86 s system, 124160 KiB peak RSS):

```text
replication_cost: 4 tokio workers for the three nodes, QUIC over loopback, redb on target/.tmpxpDIak
disk bucket, one writer: 2105 writes/s, p50 441 us, p99 828 us
replicated, one writer on the primary: 548 writes/s, p50 1764 us, p99 2529 us
replicated, one writer on the standby: 485 writes/s, p50 2013 us, p99 2716 us
disk bucket, 16 writers: 2011 writes/s
replicated, 16 writers on the primary: 2643 writes/s
disk bucket, 64 writers: 1967 writes/s
replicated, 64 writers on the primary: 5647 writes/s
replicated, alone, one writer: 1123 writes/s, p50 883 us, p99 971 us
replicated, alone, 16 writers: 5188 writes/s
```

### Pi 5: passivation: wake cost, instances per Store slot, random calls

`target/release/examples/passivation_cost 4 1000 100000` (18.75 s wall, 39.49 s user, 16.58 s system, 2039596 KiB peak RSS):

```text
passivation_cost: 4 workers, 1000 Store slots, 100000 instances, passivated after 20 ms idle
runtime started: RSS 27 MiB, address space 8259015 MiB
100000 instances started and passivated in 5.1 s; Stores held: 0
all passivated: RSS 1897 MiB, address space 8260746 MiB
per passivated instance: 19.2 KiB RSS, 17.7 KiB address space
call that wakes its instance: p50 58 us, p90 83 us, p99 128 us, max 381 us (2000 calls)
call to a live instance: p50 5 us, p90 8 us, p99 16 us, max 45 us (2000 calls)
random calls, 64 callers: 24099 calls/s
random call: p50 1225 us, p90 8105 us, p99 15168 us, max 52187 us (120495 calls)
after random calls: RSS 1931 MiB, address space 8260810 MiB
counts kept: 124495 of 124495 increments
```

`target/release/examples/passivation_cost 4 100 2000` (5.73 s wall, 13.12 s user, 6.45 s system, 78144 KiB peak RSS):

```text
passivation_cost: 4 workers, 100 Store slots, 2000 instances, passivated after 20 ms idle
runtime started: RSS 27 MiB, address space 826247 MiB
2000 instances started and passivated in 0.1 s; Stores held: 0
all passivated: RSS 60 MiB, address space 826248 MiB
per passivated instance: 16.9 KiB RSS, 0.5 KiB address space
call that wakes its instance: p50 76 us, p90 87 us, p99 116 us, max 162 us (2000 calls)
call to a live instance: p50 7 us, p90 7 us, p99 10 us, max 27 us (2000 calls)
random calls, 64 callers: 36734 calls/s
random call: p50 1737 us, p90 2024 us, p99 3024 us, max 13508 us (183672 calls)
after random calls: RSS 72 MiB, address space 826248 MiB
counts kept: 187672 of 187672 increments
```

`target/release/examples/passivation_cost 4 1000 1000 off` (5.35 s wall, 19.91 s user, 0.36 s system, 155472 KiB peak RSS):

```text
passivation_cost: 4 workers, 1000 Store slots, 1000 instances, no passivation
runtime started: RSS 27 MiB, address space 8259014 MiB
1000 instances started in 0.1 s; Stores held: 1000
all idle live: RSS 129 MiB, address space 8259015 MiB
per idle live instance: 104.7 KiB RSS, 1.0 KiB address space
call to a live instance: p50 4 us, p90 4 us, p99 6 us, max 17 us (1000 calls)
random calls, 64 callers: 248563 calls/s
random call: p50 176 us, p90 600 us, p99 1357 us, max 8682 us (1242813 calls)
after random calls: RSS 137 MiB, address space 8259015 MiB
counts kept: 1244813 of 1244813 increments
```

### Pi 5: modbus: Modbus points through the scada pool

`target/release/examples/modbus_load 16 100 10 4 4` (12.34 s wall, 8.57 s user, 0.52 s system, 31168 KiB peak RSS):

```text
16 devices x 100 registers every 10 ms, pool of 4, 4 workers: 1600 updates/s stored (1600 polls/s asked), 159997 points/s
```

`target/release/examples/modbus_load 64 125 1 4 4` (12.39 s wall, 38.27 s user, 2.52 s system, 36800 KiB peak RSS):

```text
64 devices x 125 registers every 1 ms, pool of 4, 4 workers: 7767 updates/s stored (64000 polls/s asked), 970875 points/s
```

`target/release/examples/modbus_load 64 125 1 8 8` (12.79 s wall, 33.82 s user, 2.17 s system, 49768 KiB peak RSS):

```text
64 devices x 125 registers every 1 ms, pool of 8, 8 workers: 6317 updates/s stored (64000 polls/s asked), 789609 points/s
```

## 2026-10-24: what running in Wasm costs

AMD Ryzen Threadripper 1950X (16 threads, 63 GiB), Linux 6.8.0-146-generic, rustc 1.99.0, wasmtime 49, commit fac7857 with the new benchmarks; Erlang/OTP 28 (erts 16.2) for the reference; `scripts/bench.sh baseline timer restart heap compile beam`, load average 0.73 at the start. The full output is below, after the reading.

New harnesses: `timer_cost`, `restart_cost`, `heap_cost` and `compile_cost` (`paddock-host` examples), and `scripts/beam_baseline.escript`, which runs the shapes of `baseline` and `timer_cost` as gen_servers. The chaos fixture gained the commands they use (`timed`, `lateness`, `clock`, `ballast`).

### Against Erlang/OTP

| Measure | Paddock, 1 worker | Paddock, 4 workers | BEAM, 1 scheduler | BEAM, 4 schedulers |
| --- | --- | --- | --- | --- |
| Empty call p50 | 2.69 µs | 4.53 µs | 1.97 µs | 2.00 µs |
| 1 KiB echo call p50 | 4.78 µs | 6.49 µs | 2.08 µs | 2.07 µs |
| Casts handled per second | 636,000 | 441,000 | 4,257,000 | 2,060,000 |
| Start to first reply p50 | 85 µs | 88 µs | 5.1 µs | 5.2 µs |
| Idle memory per instance or process | 60 KiB (19 to 25 KiB passivated) | 62 KiB | 2.7 KiB | 2.6 KiB |
| Timer lateness p50 / p99, idle (`send-after`, `send_after`) | | 1.1 / 2.0 ms | 0.41 / 1.0 ms | 0.41 / 1.05 ms |
| Timer lateness p50 / p99, loaded | | 1.9 to 2.1 / 4.9 to 5.0 ms | 1.0 to 1.1 / 1.7 ms | 0.65 to 0.76 / 1.3 ms |

The loaded rows run the same load on both: 64 clients calling 16 instances or gen_servers back to back, each call about 31 µs of integer work; Paddock's timer figures are from 4 workers.

- **Calls** cost about 1.4 times a gen_server call on one thread, and 2.3 times on four, where Paddock's reply wakes a task on another worker and BEAM's does not leave the scheduler. A 1 KiB body adds about 2 µs in Paddock (copied into and out of guest memory) and nothing measurable in BEAM, which passes binaries over 64 bytes by reference.
- **Casts** handled per second are a sixth to a fifth of BEAM's. Part of that is the guest boundary per message; part is the bounded mailbox (the 4-worker sender found it full 411,443 times; BEAM's mailbox is unbounded).
- **Starting an instance** costs about 17 times a gen_server start, and an idle instance holds about 22 times a process's memory: a Store, its linear memory's first pages, a WASI context and the host-side state, against a small heap and stack.
- **Timers** are handled about 0.7 ms later than BEAM's at p50 when idle, and about 1 ms later under load. The difference is the host's timer, not Wasm: a native tokio sleep on the same runtime is 1.2 ms late at p50, and `send-after` is a tokio sleep. tokio's timer has a 1 ms grain and rounds a deadline up; why it is more than 1 ms late was not looked into. Under load, the instance's wait for a worker adds up to an epoch tick (2 ms) at p99.

### Timers and clocks

- **Idle, 4 workers:** a native tokio sleep is 1.18 to 1.21 ms late at p50 and 1.26 to 1.35 ms at p99, whatever the delay (1, 10 or 50 ms). `send-after`, handled in the guest's `handle-info`, is 1.09 to 1.12 ms late at p50 and about 2 ms at p99. A guest's `wasi:clocks` sleep, timed by the caller (so including one call), is 1.54 to 1.58 ms late at p50 and 1.8 to 1.9 ms at p99.
- **Loaded:** native sleeps reach 3.2 to 4.5 ms at p99; `send-after` 4.9 to 5.0 ms (max 7.2 ms); guest sleeps 4.6 to 5.4 ms. The lateness does not grow with the delay.
- **A monotonic clock read** costs 169 ns in a guest against 25 ns natively: it is a host call (`wasi:clocks`), where the native read is a vDSO call.
- **Not measured:** wall-clock (`wasi:clocks/wall-clock`) reads. The Pi's figures are in the next section.

### Restarts and upgrades

| Measure, 1 worker | p50 | p99 |
| --- | --- | --- |
| Kill to the restarted instance's first reply | 66.5 µs | 73.8 µs |
| Crash (trap) to the restarted instance's first reply | 115.8 µs | 127.7 µs |
| Snapshot upgrade, instance paused, no state | 47.6 µs | 86.4 µs |
| Same, 64 KiB of state | 104 µs | 120 µs |
| Same, 1 MiB | 920 µs | 1.37 ms |
| Same, 16 MiB | 21.0 ms | 25.7 ms |

- A restart costs about an instantiation; a trap adds about 50 µs over a kill, not attributed.
- An upgrade pauses the instance for about 1.3 ms per MiB of state past the first 64 KiB: `save-snapshot` (3.6 ms at 16 MiB; the guest copies its state into a new vector), the copy out of the old Store, and `load-snapshot` in the new one (a copy in, then the guest's own copy). The share of each was not measured.
- On 4 workers the restart and small-upgrade figures are up to 70% higher (a kill 82 µs, an upgrade with no state 81 µs), the large-state ones the same.

### Heap size

| Heap (16 instances, medians, 1 worker) | Growing it (`alloc`) | Native, fresh pages | Resident each, live | After exit | Call that wakes it, passivated with that much state |
| --- | --- | --- | --- | --- | --- |
| 0 | 10 µs | 0 | 0.10 MiB | 0.06 MiB | 35 µs |
| 1 MiB | 556 µs | 541 µs | 1.03 MiB | 0 | 1.21 ms |
| 16 MiB | 5.7 ms | 6.2 ms | 16.03 MiB | 0 | 14.9 ms |
| 64 MiB | 23.4 ms | 22.5 ms | 64.03 MiB | 0 | 59.7 ms |

- Growing linear memory costs what a native allocation of fresh pages costs: page faults dominate both.
- An instance's memory is returned when it exits: the pooling allocator decommits its slot, and a new instance on the slot starts in 47 to 78 µs whatever the last one held. While it lives, an instance keeps the most it ever grew to (linear memory does not shrink).
- A passivated instance holds its snapshot in host memory, the size of its state, and waking it costs about 0.9 ms per MiB of state (`load-snapshot`, as in an upgrade). Passivation suits instances with small state; one with tens of MiB saves no memory and pays tens of ms per wake.

### Compiling

| Component | Size | Compile | Load from the compiled-code cache | Cache file |
| --- | --- | --- | --- | --- |
| echo fixture | 17 KiB | 21.8 ms | 0.32 ms | 60 KiB |
| chaos fixture | 150 KiB | 180 ms | 0.49 ms | 453 KiB |
| scada `control` example | 488 KiB | 516 ms | 0.53 ms | 1,295 KiB |
| SDK fixture | 543 KiB | 570 ms | 0.73 ms | 1,453 KiB |
| SDK fixture, debug build | 5,486 KiB | 1,057 ms | 0.96 ms | 3,244 KiB |

- Release components compile at about 1 ms per KiB, on one thread: `paddock-host` does not enable wasmtime's `parallel-compilation` feature. A deploy pays this once; a node restart reads the cache in under a millisecond.
- The debug build is ten times the size but compiles in twice the time; size alone does not predict compile time.
- A cache directory that group or others can write is refused and the component compiled again, as designed (`paddock_host::compiled`). The harness found it: a directory made under umask 002 is group-writable.

### What these leave open

- Wake and upgrade cost per MiB of state (about 1 ms per MiB, roughly 1 GB/s), which looks slow for copies; not attributed between the guest's copies, the canonical ABI and page faults.
- Compile time with `parallel-compilation` on.
- Timer lateness with a finer timer than tokio's 1 ms grain.

### 2026-10-24 run: baseline: call, cast, instantiate, idle memory (1 and 4 workers, dense)

`target/release/examples/baseline 1 1000 standard` (1.60 s wall, 1.30 s user, 0.09 s system, 87840 KiB peak RSS):

```text
tokio workers: 1, memory profile: Standard
component: 150 KiB, compiled in 184 ms
call, empty call: p50 2.69 µs, p99 3.44 µs, p99.9 6.82 µs, max 22.47 µs; 362575 calls/s sequential
call, 1 KiB echo: p50 4.78 µs, p99 5.64 µs, p99.9 9.46 µs, max 745.33 µs; 205844 calls/s sequential
cast: 636465 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 1520 times)
instantiate to first reply (1000 instances): p50 85.05 µs, p99 212.56 µs, mean 85.90 µs
idle memory per instance: 60 KiB resident, 0 KiB virtual (process: 86 MiB resident, 8129 GiB virtual)
```

`target/release/examples/baseline 4 1000 standard` (2.12 s wall, 2.41 s user, 0.80 s system, 89112 KiB peak RSS):

```text
tokio workers: 4, memory profile: Standard
component: 150 KiB, compiled in 184 ms
call, empty call: p50 4.53 µs, p99 7.67 µs, p99.9 13.25 µs, max 114.61 µs; 199676 calls/s sequential
call, 1 KiB echo: p50 6.49 µs, p99 10.82 µs, p99.9 13.91 µs, max 218.55 µs; 159193 calls/s sequential
cast: 440529 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 411443 times)
instantiate to first reply (1000 instances): p50 88.31 µs, p99 184.74 µs, mean 88.73 µs
idle memory per instance: 62 KiB resident, 0 KiB virtual (process: 87 MiB resident, 8129 GiB virtual)
```

`target/release/examples/baseline 1 5000 dense` (2.17 s wall, 1.59 s user, 0.37 s system, 365732 KiB peak RSS):

```text
tokio workers: 1, memory profile: Dense
component: 150 KiB, compiled in 214 ms
call, empty call: p50 2.73 µs, p99 3.47 µs, p99.9 6.63 µs, max 18.14 µs; 359437 calls/s sequential
call, 1 KiB echo: p50 4.80 µs, p99 5.59 µs, p99.9 8.82 µs, max 24.12 µs; 205712 calls/s sequential
cast: 632052 casts/s handled (200000 casts, mailbox 1024, sender saw a full mailbox 1529 times)
instantiate to first reply (5000 instances): p50 85.52 µs, p99 214.14 µs, mean 89.02 µs
idle memory per instance: 67 KiB resident, 26 KiB virtual (process: 357 MiB resident, 1265 GiB virtual)
```

### 2026-10-24 run: timer: send-after and guest sleep lateness against a native tokio sleep, idle and loaded; clock reads

`target/release/examples/timer_cost 4` (10.81 s wall, 27.90 s user, 0.19 s system, 30788 KiB peak RSS):

```text
tokio workers: 4; 100 timers at once, 10 rounds per delay; lateness in µs
monotonic clock read: native 25.3 ns, guest 169.2 ns (one run of 1000000 reads)
                                    delay      p50      p99      max
idle, native tokio sleep              1 ms     1176     1261     1268
idle, guest send-after                1 ms     1085     1985     2044
idle, guest wasi:clocks sleep         1 ms     1544     1814     1876
idle, native tokio sleep             10 ms     1202     1265     1280
idle, guest send-after               10 ms     1108     2037     2073
idle, guest wasi:clocks sleep        10 ms     1579     1868     1912
idle, native tokio sleep             50 ms     1214     1350     1359
idle, guest send-after               50 ms     1119     1986     2063
idle, guest wasi:clocks sleep        50 ms     1583     1831     1879
loaded, native tokio sleep            1 ms     1475     3519     3520
loaded, guest send-after              1 ms     2108     4957     6269
loaded, guest wasi:clocks sleep       1 ms     2362     4595     4649
loaded, native tokio sleep           10 ms      976     3248     3250
loaded, guest send-after             10 ms     1861     4934     6653
loaded, guest wasi:clocks sleep      10 ms     3409     5440     5485
loaded, native tokio sleep           50 ms     2260     4516     4520
loaded, guest send-after             50 ms     2133     5014     7196
loaded, guest wasi:clocks sleep      50 ms     2961     5434     5457
```

### 2026-10-24 run: restart: crash and kill to the restarted instance's first reply; upgrades by state size

`target/release/examples/restart_cost 1` (1.61 s wall, 0.84 s user, 0.77 s system, 106232 KiB peak RSS):

```text
tokio workers: 1; 500 restarts each, 50 upgrades per size
a crash (trap in handle-cast) to the restarted instance's first reply: p50 115.8 µs, p99 127.7 µs, max 213.5 µs
a kill to the restarted instance's first reply: p50 66.5 µs, p99 73.8 µs, max 78.5 µs
     0 KiB state: snapshot taken: p50 2.7 µs, p99 5.2 µs, max 5.2 µs
     0 KiB state: upgrade, instance paused: p50 47.6 µs, p99 86.4 µs, max 86.4 µs
     0 KiB state: upgrade to first reply: p50 59.1 µs, p99 99.8 µs, max 99.8 µs
    64 KiB state: snapshot taken: p50 7.4 µs, p99 60.4 µs, max 60.4 µs
    64 KiB state: upgrade, instance paused: p50 104.3 µs, p99 120.3 µs, max 120.3 µs
    64 KiB state: upgrade to first reply: p50 117.9 µs, p99 135.6 µs, max 135.6 µs
  1024 KiB state: snapshot taken: p50 95.4 µs, p99 920.5 µs, max 920.5 µs
  1024 KiB state: upgrade, instance paused: p50 919.8 µs, p99 1366.7 µs, max 1366.7 µs
  1024 KiB state: upgrade to first reply: p50 938.3 µs, p99 1396.1 µs, max 1396.1 µs
 16384 KiB state: snapshot taken: p50 3589.9 µs, p99 15686.3 µs, max 15686.3 µs
 16384 KiB state: upgrade, instance paused: p50 20974.8 µs, p99 25654.4 µs, max 25654.4 µs
 16384 KiB state: upgrade to first reply: p50 21022.6 µs, p99 25703.1 µs, max 25703.1 µs
```

`target/release/examples/restart_cost 4` (1.63 s wall, 0.86 s user, 0.80 s system, 106744 KiB peak RSS):

```text
tokio workers: 4; 500 restarts each, 50 upgrades per size
a crash (trap in handle-cast) to the restarted instance's first reply: p50 126.7 µs, p99 243.3 µs, max 421.7 µs
a kill to the restarted instance's first reply: p50 82.4 µs, p99 141.2 µs, max 159.3 µs
     0 KiB state: snapshot taken: p50 7.7 µs, p99 11.8 µs, max 11.8 µs
     0 KiB state: upgrade, instance paused: p50 80.8 µs, p99 130.1 µs, max 130.1 µs
     0 KiB state: upgrade to first reply: p50 101.9 µs, p99 160.4 µs, max 160.4 µs
    64 KiB state: snapshot taken: p50 10.1 µs, p99 93.5 µs, max 93.5 µs
    64 KiB state: upgrade, instance paused: p50 103.0 µs, p99 212.7 µs, max 212.7 µs
    64 KiB state: upgrade to first reply: p50 117.6 µs, p99 238.3 µs, max 238.3 µs
  1024 KiB state: snapshot taken: p50 97.6 µs, p99 828.9 µs, max 828.9 µs
  1024 KiB state: upgrade, instance paused: p50 868.2 µs, p99 1242.4 µs, max 1242.4 µs
  1024 KiB state: upgrade to first reply: p50 888.0 µs, p99 1268.3 µs, max 1268.3 µs
 16384 KiB state: snapshot taken: p50 3612.4 µs, p99 15938.7 µs, max 15938.7 µs
 16384 KiB state: upgrade, instance paused: p50 20887.0 µs, p99 25830.1 µs, max 25830.1 µs
 16384 KiB state: upgrade to first reply: p50 20939.5 µs, p99 25882.0 µs, max 25882.0 µs
```

### 2026-10-24 run: heap: heap growth, memory held live and after exit, slot reuse, passivated state by size

`target/release/examples/heap_cost 1` (7.85 s wall, 1.30 s user, 4.07 s system, 2514592 KiB peak RSS):

```text
tokio workers: 1; 16 instances per size; medians
  0 MiB heap: alloc call 10 µs (native 0 µs); 0.10 MiB resident each live, 0.06 MiB after exit; instantiate on a reused slot 78.2 µs
  1 MiB heap: alloc call 556 µs (native 541 µs); 1.03 MiB resident each live, 0.00 MiB after exit; instantiate on a reused slot 77.5 µs
 16 MiB heap: alloc call 5733 µs (native 6159 µs); 16.03 MiB resident each live, 0.00 MiB after exit; instantiate on a reused slot 46.7 µs
 64 MiB heap: alloc call 23411 µs (native 22499 µs); 64.03 MiB resident each live, 0.00 MiB after exit; instantiate on a reused slot 46.7 µs
  0 MiB state, passivated: snapshot 0 KiB held by the host; call that wakes it 35 µs
  1 MiB state, passivated: snapshot 1024 KiB held by the host; call that wakes it 1212 µs
 16 MiB state, passivated: snapshot 16384 KiB held by the host; call that wakes it 14940 µs
 64 MiB state, passivated: snapshot 65536 KiB held by the host; call that wakes it 59687 µs
```

### 2026-10-24 run: compile: compile time by component size, and loads from the compiled-code cache

`target/release/examples/compile_cost` (23.77 s wall, 23.50 s user, 0.15 s system, 59380 KiB peak RSS):

```text
median of 5 loads each, one thread
component                                     size KiB  compile ms cache write ms   cached ms  cache KiB
release/paddock_fixture_echo.wasm                   17        21.8          24.6        0.32         60
release/paddock_fixture_chaos.wasm                 150       179.9         185.4        0.49        453
release/paddock_fixture_sdk.wasm                   543       570.3         584.5        0.73       1453
release/example_scada_control.wasm                 488       516.2         530.1        0.53       1295
debug/paddock_fixture_sdk.wasm                    5486      1057.2        1073.5        0.96       3244
```

### 2026-10-24 run: beam: Erlang/OTP reference: gen_server call, cast, spawn, process memory, send_after lateness (1 and 4 schedulers)

`ERL_FLAGS='+S 1:1' escript scripts/beam_baseline.escript` (11.58 s wall, 9.17 s user, 0.13 s system, 124784 KiB peak RSS):

```text
erts 16.2, OTP 28, schedulers: 1
call, empty call: p50 1.97 us, p99 4.06 us, p99.9 6.74 us, max 2578.63 us; 423593 calls/s sequential
call, 1 KiB echo: p50 2.08 us, p99 4.17 us, p99.9 273.00 us, max 874.81 us; 361996 calls/s sequential
cast: 4256707 casts/s handled (200000 casts, unbounded mailbox)
start to first reply (1000 gen_servers): p50 5.07 us, p99 10.53 us
idle memory per process: 2.7 KiB (erlang:memory(processes) delta), 2.6 KiB (process_info)
idle, send_after 1 ms: lateness p50 409 us, p99 1005 us, max 1019 us
idle, send_after 10 ms: lateness p50 432 us, p99 996 us, max 1024 us
idle, send_after 50 ms: lateness p50 408 us, p99 977 us, max 1015 us
load: 64 clients, 16 gen_servers, busy 298 rounds per call (about 31 us)
loaded, send_after 1 ms: lateness p50 1033 us, p99 1702 us, max 1976 us
loaded, send_after 10 ms: lateness p50 1054 us, p99 1665 us, max 1811 us
loaded, send_after 50 ms: lateness p50 1112 us, p99 1652 us, max 1734 us
```

`ERL_FLAGS='+S 4:4' escript scripts/beam_baseline.escript` (7.29 s wall, 16.62 s user, 0.23 s system, 120908 KiB peak RSS):

```text
erts 16.2, OTP 28, schedulers: 4
call, empty call: p50 2.00 us, p99 3.98 us, p99.9 7.45 us, max 2392.34 us; 427179 calls/s sequential
call, 1 KiB echo: p50 2.07 us, p99 4.10 us, p99.9 272.27 us, max 899.15 us; 367700 calls/s sequential
cast: 2060291 casts/s handled (200000 casts, unbounded mailbox)
start to first reply (1000 gen_servers): p50 5.24 us, p99 12.86 us
idle memory per process: 2.6 KiB (erlang:memory(processes) delta), 2.6 KiB (process_info)
idle, send_after 1 ms: lateness p50 408 us, p99 1054 us, max 1067 us
idle, send_after 10 ms: lateness p50 481 us, p99 1059 us, max 1086 us
idle, send_after 50 ms: lateness p50 732 us, p99 1080 us, max 1090 us
load: 64 clients, 16 gen_servers, busy 293 rounds per call (about 31 us)
loaded, send_after 1 ms: lateness p50 652 us, p99 1273 us, max 1583 us
loaded, send_after 10 ms: lateness p50 742 us, p99 1308 us, max 1467 us
loaded, send_after 50 ms: lateness p50 757 us, p99 1339 us, max 1446 us
```

## 2026-10-24: what running in Wasm costs, on the Pi

Raspberry Pi 5 Model B Rev 1.1 (Cortex-A76), 4 cores, 8 GiB, Ubuntu 26.04, Linux 7.0.0-1020-raspi; rustc 1.99.1; commit 1a7fd5c; Erlang/OTP 27 (erts 15.2.7.4, Ubuntu's `erlang-base`) for the reference. `scripts/bench.sh timer restart heap compile`, load average 0.25 at the start, then `scripts/bench.sh beam` once OTP was installed, load average 0.26. Both ran at nice 10 on an otherwise idle machine. The 8 GB swap partition was on and stayed unused (0 B). Drive temperature peaked at 45 °C and the CPU at 58 °C, with no throttling and no errors. The full output is below, after the reading. `baseline` was not run again: the Paddock figures in the BEAM table are from the 2026-10-18 Pi run (commit 7d7095d).

### Against Erlang/OTP, on the Pi

| Measure | Paddock, 1 worker | Paddock, 4 workers | BEAM, 1 scheduler | BEAM, 4 schedulers |
| --- | --- | --- | --- | --- |
| Empty call p50 | 3.74 µs | 5.24 µs | 2.44 µs | 2.44 µs |
| 1 KiB echo call p50 | 5.68 µs | 7.20 µs | 2.63 µs | 2.56 µs |
| Casts handled per second | 457,000 | 422,000 | 3,429,000 | 3,518,000 |
| Start to first reply p50 | 80 µs | 86 µs | 7.5 µs | 7.2 µs |
| Idle memory per instance or process | 104 KiB | 101 KiB | 2.7 KiB | 2.7 KiB |
| Timer lateness p50 / p99, idle | | 0.98 to 1.16 / 2.0 to 2.1 ms | 0.42 to 0.46 / 1.02 ms | 0.48 to 0.65 / 1.05 ms |
| Timer lateness p50 / p99, loaded | | 2.6 to 2.9 / 6.2 to 7.2 ms | 0.94 to 1.6 / 2.1 to 2.4 ms | 1.0 / 1.6 to 1.8 ms |

- **Calls** cost 1.5 times a gen_server call on one thread and 2.1 times on four, as on the 16-core machine (1.4 and 2.3).
- **Casts** handled per second are an eighth of BEAM's, against a sixth to a fifth on the 16-core machine. BEAM's cast rate on 4 schedulers holds up on the Pi (3.5M/s), where on the 16-core machine it halved.
- **Starting an instance** costs 11 times a gen_server start (17 on the 16-core machine; BEAM's start is slower on the Pi, Paddock's is not). An idle instance holds 39 times a process's memory, against 22: the Pi's unexplained 104 to 110 KiB per instance.
- **Timers:** idle, the same as on the 16-core machine; loaded, `send-after` reaches 6.2 to 7.2 ms at p99 against 4.9 to 5.0 ms, on four cores fully taken by the load.

### Timers, clocks, restarts, heap and compiling, Pi against the 16-core machine

| Measure | Pi 5 | 16-core (2026-10-24) |
| --- | --- | --- |
| Monotonic clock read, guest / native | 240 / 36.7 ns | 169 / 25 ns |
| Native tokio sleep lateness p50, idle | 1.12 to 1.34 ms | 1.18 to 1.21 ms |
| `send-after` lateness p50 / p99, idle | 0.98 to 1.16 / 2.0 to 2.1 ms | 1.09 to 1.12 / about 2 ms |
| `send-after` lateness p50 / p99, loaded | 2.6 to 2.9 / 6.2 to 7.2 ms | 1.9 to 2.1 / 4.9 to 5.0 ms |
| Native tokio sleep lateness p99, loaded | 2.7 to 3.5 ms | 3.2 to 4.5 ms |
| Kill to the restarted instance's first reply, 1 worker, p50 | 79.2 µs | 66.5 µs |
| Crash (trap) to first reply, 1 worker, p50 | 150 µs | 116 µs |
| Upgrade pause, no state / 64 KiB / 1 MiB / 16 MiB, p50 | 57 µs / 136 µs / 1.94 ms / 28.6 ms | 48 µs / 104 µs / 0.92 ms / 21.0 ms |
| Snapshot taken, 16 MiB of state, p50 | 7.3 ms | 3.6 ms |
| Growing the heap by 1 / 16 / 64 MiB (native) | 0.43 / 6.7 / 24.7 ms (0.48 / 5.2 / 20.3) | 0.56 / 5.7 / 23.4 ms (0.54 / 6.2 / 22.5) |
| Idle instance resident, after exit | 0.12 MiB, 0 to 0.05 MiB | 0.10 MiB, 0 to 0.06 MiB |
| Instantiate on a reused slot | 92 to 99 µs | 47 to 78 µs |
| Wake with 1 / 16 / 64 MiB of state | 1.39 / 15.7 / 65.6 ms | 1.21 / 14.9 / 59.7 ms |
| Compile: echo, chaos, SDK, SDK debug | 26 / 201 / 647 / 1,157 ms | 22 / 180 / 570 / 1,057 ms |
| Load from the compiled-code cache | 0.46 to 1.64 ms | 0.32 to 0.96 ms |
| Cache file, echo / chaos / SDK | 264 / 613 / 1,429 KiB | 60 / 453 / 1,453 KiB |

- **Timers** are as late on the Pi as on the 16-core machine when idle: the tokio timer's grain, not the CPU, sets them. Under load they are later at p99, by the instance's wait for a worker on four busy cores.
- **A guest's clock read** costs 6.5 times a native one, the same ratio as on the 16-core machine.
- **Restarts** cost about 1.2 to 1.3 times the 16-core machine's.
- **Upgrades** with 1 MiB of state pause the instance twice as long as on the 16-core machine, and 1.4 times as long at 16 MiB; taking the snapshot alone costs twice as long. Waking a passivated instance costs about 1 ms per MiB, close to the 16-core machine's 0.9. An upgrade copies out of the old Store as well as into the new one, where a wake only copies in; why the Pi pays more for the upgrade's share was not looked into.
- **The heap** behaves as on the 16-core machine: growing it costs about a native allocation (1.2 to 1.3 times at 16 and 64 MiB), and the memory is returned on exit. An idle instance is 0.12 MiB resident, the same excess as the 2026-10-18 run found.
- **Compiling** takes 1.1 to 1.2 times as long as on the 16-core machine (one thread each). The cache files for small components are larger on aarch64 (264 KiB for the 17 KiB echo fixture, against 60 KiB); not looked into.

### Pi 5, 2026-10-24 run: timer: send-after and guest sleep lateness against a native tokio sleep, idle and loaded; clock reads

`target/release/examples/timer_cost 4` (11.94 s wall, 31.91 s user, 0.11 s system, 39680 KiB peak RSS):

```text
tokio workers: 4; 100 timers at once, 10 rounds per delay; lateness in µs
monotonic clock read: native 36.7 ns, guest 240.0 ns (one run of 1000000 reads)
                                    delay      p50      p99      max
idle, native tokio sleep              1 ms     1117     1163     1167
idle, guest send-after                1 ms      984     1968     2039
idle, guest wasi:clocks sleep         1 ms     1511     1790     1818
idle, native tokio sleep             10 ms     1130     1180     1184
idle, guest send-after               10 ms     1116     1977     2115
idle, guest wasi:clocks sleep        10 ms     1583     1904     1981
idle, native tokio sleep             50 ms     1342     1448     1452
idle, guest send-after               50 ms     1156     2108     2184
idle, guest wasi:clocks sleep        50 ms     1679     2047     2094
loaded, native tokio sleep            1 ms     1837     3542     3724
loaded, guest send-after              1 ms     2810     7196     8449
loaded, guest wasi:clocks sleep       1 ms     4424     6453     8163
loaded, native tokio sleep           10 ms     1435     2704     2970
loaded, guest send-after             10 ms     2630     6177     7325
loaded, guest wasi:clocks sleep      10 ms     3724     5506     5998
loaded, native tokio sleep           50 ms     1837     3481     4011
loaded, guest send-after             50 ms     2887     7074     8484
loaded, guest wasi:clocks sleep      50 ms     2983     5216     5245
```

### Pi 5, 2026-10-24 run: restart: crash and kill to the restarted instance's first reply; upgrades by state size

`target/release/examples/restart_cost 1` (2.27 s wall, 1.33 s user, 0.93 s system, 107928 KiB peak RSS):

```text
tokio workers: 1; 500 restarts each, 50 upgrades per size
a crash (trap in handle-cast) to the restarted instance's first reply: p50 149.6 µs, p99 230.0 µs, max 485.7 µs
a kill to the restarted instance's first reply: p50 79.2 µs, p99 98.0 µs, max 110.5 µs
     0 KiB state: snapshot taken: p50 3.2 µs, p99 9.9 µs, max 9.9 µs
     0 KiB state: upgrade, instance paused: p50 56.7 µs, p99 126.2 µs, max 126.2 µs
     0 KiB state: upgrade to first reply: p50 70.0 µs, p99 144.2 µs, max 144.2 µs
    64 KiB state: snapshot taken: p50 9.9 µs, p99 50.1 µs, max 50.1 µs
    64 KiB state: upgrade, instance paused: p50 135.8 µs, p99 162.8 µs, max 162.8 µs
    64 KiB state: upgrade to first reply: p50 151.3 µs, p99 180.9 µs, max 180.9 µs
  1024 KiB state: snapshot taken: p50 288.9 µs, p99 985.7 µs, max 985.7 µs
  1024 KiB state: upgrade, instance paused: p50 1940.2 µs, p99 1997.7 µs, max 1997.7 µs
  1024 KiB state: upgrade to first reply: p50 1997.6 µs, p99 2059.5 µs, max 2059.5 µs
 16384 KiB state: snapshot taken: p50 7251.1 µs, p99 16179.7 µs, max 16179.7 µs
 16384 KiB state: upgrade, instance paused: p50 28581.4 µs, p99 33003.6 µs, max 33003.6 µs
 16384 KiB state: upgrade to first reply: p50 28640.9 µs, p99 33101.8 µs, max 33101.8 µs
```

`target/release/examples/restart_cost 4` (2.32 s wall, 1.38 s user, 0.97 s system, 141272 KiB peak RSS):

```text
tokio workers: 4; 500 restarts each, 50 upgrades per size
a crash (trap in handle-cast) to the restarted instance's first reply: p50 203.1 µs, p99 304.6 µs, max 503.4 µs
a kill to the restarted instance's first reply: p50 122.6 µs, p99 138.6 µs, max 141.7 µs
     0 KiB state: snapshot taken: p50 5.6 µs, p99 17.8 µs, max 17.8 µs
     0 KiB state: upgrade, instance paused: p50 69.0 µs, p99 138.2 µs, max 138.2 µs
     0 KiB state: upgrade to first reply: p50 86.8 µs, p99 160.9 µs, max 160.9 µs
    64 KiB state: snapshot taken: p50 12.3 µs, p99 42.6 µs, max 42.6 µs
    64 KiB state: upgrade, instance paused: p50 137.5 µs, p99 189.9 µs, max 189.9 µs
    64 KiB state: upgrade to first reply: p50 153.2 µs, p99 210.6 µs, max 210.6 µs
  1024 KiB state: snapshot taken: p50 296.4 µs, p99 754.0 µs, max 754.0 µs
  1024 KiB state: upgrade, instance paused: p50 1941.6 µs, p99 2681.8 µs, max 2681.8 µs
  1024 KiB state: upgrade to first reply: p50 1993.3 µs, p99 2742.8 µs, max 2742.8 µs
 16384 KiB state: snapshot taken: p50 6896.8 µs, p99 20001.2 µs, max 20001.2 µs
 16384 KiB state: upgrade, instance paused: p50 27439.4 µs, p99 41020.4 µs, max 41020.4 µs
 16384 KiB state: upgrade to first reply: p50 27511.1 µs, p99 41138.9 µs, max 41138.9 µs
```

### Pi 5, 2026-10-24 run: heap: heap growth, memory held live and after exit, slot reuse, passivated state by size

`target/release/examples/heap_cost 1` (8.75 s wall, 2.38 s user, 3.89 s system, 2516844 KiB peak RSS):

```text
tokio workers: 1; 16 instances per size; medians
  0 MiB heap: alloc call 20 µs (native 0 µs); 0.12 MiB resident each live, 0.05 MiB after exit; instantiate on a reused slot 92.9 µs
  1 MiB heap: alloc call 430 µs (native 481 µs); 1.03 MiB resident each live, 0.00 MiB after exit; instantiate on a reused slot 98.7 µs
 16 MiB heap: alloc call 6720 µs (native 5176 µs); 16.03 MiB resident each live, 0.00 MiB after exit; instantiate on a reused slot 96.0 µs
 64 MiB heap: alloc call 24705 µs (native 20285 µs); 64.03 MiB resident each live, 0.00 MiB after exit; instantiate on a reused slot 92.1 µs
  0 MiB state, passivated: snapshot 0 KiB held by the host; call that wakes it 69 µs
  1 MiB state, passivated: snapshot 1024 KiB held by the host; call that wakes it 1387 µs
 16 MiB state, passivated: snapshot 16384 KiB held by the host; call that wakes it 15686 µs
 64 MiB state, passivated: snapshot 65536 KiB held by the host; call that wakes it 65609 µs
```

### Pi 5, 2026-10-24 run: compile: compile time by component size, and loads from the compiled-code cache

`target/release/examples/compile_cost` (27.19 s wall, 26.38 s user, 0.14 s system, 63476 KiB peak RSS):

```text
median of 5 loads each, one thread
component                                     size KiB  compile ms cache write ms   cached ms  cache KiB
release/paddock_fixture_echo.wasm                   17        26.1          29.6        0.46        264
release/paddock_fixture_chaos.wasm                 150       201.4         236.9        1.02        613
release/paddock_fixture_sdk.wasm                   543       646.9         686.3        1.12       1429
release/example_scada_control.wasm                 487       594.8         633.6        0.96       1311
debug/paddock_fixture_sdk.wasm                    5417      1156.9        1201.6        1.64       3260
```
### Pi 5, 2026-10-24 run: beam: Erlang/OTP reference: gen_server call, cast, spawn, process memory, send_after lateness (1 and 4 schedulers)

`ERL_FLAGS='+S 1:1' escript scripts/beam_baseline.escript` (15.15 s wall, 12.73 s user, 0.12 s system, 135560 KiB peak RSS):

```text
erts 15.2.7.4, OTP 27, schedulers: 1
call, empty call: p50 2.44 us, p99 4.89 us, p99.9 12.26 us, max 1636.77 us; 344524 calls/s sequential
call, 1 KiB echo: p50 2.63 us, p99 4.87 us, p99.9 302.35 us, max 1627.57 us; 288805 calls/s sequential
cast: 3428733 casts/s handled (200000 casts, unbounded mailbox)
start to first reply (1000 gen_servers): p50 7.50 us, p99 24.00 us
idle memory per process: 2.7 KiB (erlang:memory(processes) delta), 2.6 KiB (process_info)
idle, send_after 1 ms: lateness p50 456 us, p99 1020 us, max 1033 us
idle, send_after 10 ms: lateness p50 436 us, p99 1019 us, max 1039 us
idle, send_after 50 ms: lateness p50 417 us, p99 1018 us, max 1033 us
load: 64 clients, 16 gen_servers, busy 314 rounds per call (about 31 us)
loaded, send_after 1 ms: lateness p50 943 us, p99 2355 us, max 2441 us
loaded, send_after 10 ms: lateness p50 1576 us, p99 2225 us, max 2245 us
loaded, send_after 50 ms: lateness p50 1598 us, p99 2147 us, max 2395 us
```

`ERL_FLAGS='+S 4:4' escript scripts/beam_baseline.escript` (11.06 s wall, 30.99 s user, 0.28 s system, 150384 KiB peak RSS):

```text
erts 15.2.7.4, OTP 27, schedulers: 4
call, empty call: p50 2.44 us, p99 4.80 us, p99.9 12.33 us, max 1876.18 us; 348333 calls/s sequential
call, 1 KiB echo: p50 2.56 us, p99 4.76 us, p99.9 295.24 us, max 1562.92 us; 294929 calls/s sequential
cast: 3517550 casts/s handled (200000 casts, unbounded mailbox)
start to first reply (1000 gen_servers): p50 7.24 us, p99 14.76 us
idle memory per process: 2.7 KiB (erlang:memory(processes) delta), 2.6 KiB (process_info)
idle, send_after 1 ms: lateness p50 481 us, p99 1049 us, max 1093 us
idle, send_after 10 ms: lateness p50 621 us, p99 1076 us, max 1109 us
idle, send_after 50 ms: lateness p50 645 us, p99 1051 us, max 1078 us
load: 64 clients, 16 gen_servers, busy 353 rounds per call (about 31 us)
loaded, send_after 1 ms: lateness p50 1037 us, p99 1755 us, max 1893 us
loaded, send_after 10 ms: lateness p50 1012 us, p99 1588 us, max 2398 us
loaded, send_after 50 ms: lateness p50 1038 us, p99 1628 us, max 3531 us
```

## 2026-10-24: no timer per call (B12)

Raspberry Pi 5 (as above), idle (load average under 0.3 before each set); commit 1a7fd5c plus `call_split` and three builds of `InstanceHandle::send_call`: `tokio::time::timeout` around every call (before), the change (yield once, then `timeout_at` the same deadline), and no timeout at all (the ceiling, not shippable). Each set alternates the builds; `call_split N paddock`, 200,000 sequential empty calls, five rounds; `pool_load 4`, three rounds. Logs on the Pi under `~/pi-runs/b12-timer-2026-10-24/`.

| Empty call, sequential | Before | Yield, then arm | No timer |
| --- | --- | --- | --- |
| 1 worker | 3.55 to 3.67 µs | 3.25 to 3.35 µs | 3.28 to 3.40 µs |
| 4 workers | 4.33 to 4.63 µs | 4.01 to 4.19 µs | 3.97 to 4.20 µs |
| 4 workers, on-CPU summed over threads | 5.56 to 5.89 µs | 5.17 to 5.36 µs | 5.14 to 5.37 µs |

| `pool_load 4`, empty calls | Before, calls/s | After, calls/s | Before, p99.9 | After, p99.9 |
| --- | --- | --- | --- | --- |
| Pool 1, 64 clients | 263k to 298k | 310k to 326k | 344 to 396 µs | 325 to 393 µs |
| Pool 4, 64 clients | 800k to 813k | 841k to 865k | 357 to 367 µs | 374 to 395 µs |
| Pool 16, 64 clients | 662k to 670k | 726k to 761k | 1.35 to 1.39 ms | 0.69 to 0.72 ms |
| Pool 16, 256 clients | 708k to 717k | 739k to 766k | 2.41 to 2.54 ms | 2.45 to 3.46 ms |

- The change takes about 8% off an empty call, on 1 and on 4 workers: what removing the timer altogether takes. A reply that arrives before the caller runs again (nearly every one here) never touches the timer wheel.
- Under concurrency, empty calls per second rise 5 to 12%; calls doing guest work (`busy:20000`) are unchanged within noise, their time being the guest's.
- p99.9 at 256 clients was higher after in two of three rounds (3.46 and 3.18 ms against at most 2.54 ms), and `busy:20000` with 16 clients likewise (2.50 and 3.06 ms against at most 1.77 ms); p99 did not move. Not the change: the next section's runs show those two rows moving between the two levels from run to run of the same build.
- Timeouts still fire on time: the deadline is taken before the yield. Tests: a call to an instance that never replies, on one thread, and a call to an instance busy in a callback, which times out within 100 to 500 ms of a 100 ms timeout (`crates/paddock-host/tests/instance.rs`).

## 2026-10-24: a cheaper killable() (B12)

Raspberry Pi 5, idle (load average under 0.3 before each set); the previous section's tree plus three builds of `killable()`: a `watch::wait_for` future against the control channel per callback, selected against the callback (before; item 1's code), the change (a kill flag and an `AtomicWaker` on the instance, set and woken by the first kill; `killable()` registers the waker, checks the flag and polls the callback), and no kill check at all (the ceiling, not shippable). Same method as above. Logs on the Pi under `~/pi-runs/b12-kill-2026-10-24/`.

| Empty call, sequential | Before | After | No kill check |
| --- | --- | --- | --- |
| 1 worker | 3.33 to 3.41 µs | 3.04 to 3.18 µs | 3.03 to 3.19 µs |
| 4 workers | 3.99 to 4.13 µs | 3.85 to 4.04 µs | 3.79 to 3.92 µs |

| `pool_load 4`, empty calls | Before, calls/s | After, calls/s |
| --- | --- | --- |
| Pool 1, 64 clients | 309k to 321k | 333k to 340k |
| Pool 4, 64 clients | 850k to 870k | 885k to 895k |
| Pool 16, 64 clients | 717k to 738k | 764k to 782k |
| Pool 16, 256 clients | 768k to 776k | 769k to 788k |

- About 8% off an empty call on 1 worker, all of what dropping the check takes; 2 to 3% on 4 workers. Calls doing guest work are unchanged.
- In the first three rounds of `pool_load`, p99.9 was higher after in every round for `busy:20000` on a pool of 16 with 16 clients (2.83 to 3.12 ms against 1.66 to 1.78 ms) and for empty calls with 256 clients (3.29 to 3.54 ms against 2.29 to 2.61 ms). Six more rounds, alternated: the same before and after (busy 1.65 to 1.79 ms against 1.71 to 1.77 ms; empty 2.34 to 2.48 ms against 2.28 to 2.48 ms). Those rows move between two levels from run to run of one build; neither change moves them. Why they move was not looked into.
- A kill still interrupts a guest waiting in a host call: a new test kills an instance blocked in a 10 s `wasi:clocks` sleep and one spinning in guest code, and expects `killed` within 100 ms; with the wake removed, the first case fails (the caller saw `timeout` after 10 s). R11 and A8 (docs/tla/VERIFICATION.md) are unchanged: `crates/paddock/tests/failover.rs`, the simulation tests and a 30-minute `scripts/soak.sh --only failover` (link faults, SIGKILL and SIGSTOP) passed.

## 2026-10-24: the call hook off the hot path (B12)

Raspberry Pi 5, idle (load average under 0.3 before each set); the previous section's tree (item 2) plus builds of the callback clock: the Store call hook timing every host call (before), the task's polls of the callback timed in a future of their own (`polls`), the same timing inside `killable`'s poll (`inline`, the change), and no hook and no timing (the ceiling: the deadline then counts host time, not shippable). Same method as above; six rounds; logs on the Pi under `~/pi-runs/b12-hook-2026-10-24/`.

| Empty call, sequential, six rounds | Before | Polls | Inline | No hook, no timing |
| --- | --- | --- | --- | --- |
| 1 worker | 3.05 to 3.27 µs (median 3.18) | 3.12 to 3.16 (3.13) | 3.11 to 3.14 (3.13) | 2.93 to 3.11 (3.04) |
| 4 workers | 3.96 to 4.15 µs (median 4.01) | 3.79 to 4.04 (3.90) | 3.86 to 4.07 (3.93) | 3.70 to 3.83 (3.77) |

- About 1.5% off an empty call on 1 worker and 2 to 3% on 4. Removing the hook with no timing at all takes 4.5 to 6%: the timing's two clock reads per poll (37 ns each on the Pi, `timer_cost`) and its atomics cost the rest. Timing inside `killable` rather than in a future of its own made no difference.
- `pool_load 4` (three rounds, `polls` against before): within run-to-run noise in every row.
- What the deadline counts changed (MANIFEST.md `callback_deadline_ms`): time waiting for a worker after an epoch yield no longer counts, so a callback on a loaded node is not killed for others' work; a host function's work before it waits (a memory-bucket transaction, a read from redb's cache) now counts. The existing tests hold: a guest sleeping 800 ms past a 300 ms deadline is not killed, a spinning one is killed at it.

## 2026-10-24: a slot for the current call's reply (B12)

Raspberry Pi 5, idle; the previous section's tree (items 2 and 3) against the change: the caller of the call being handled kept in a slot in the Store's data, moved to the pending-reply map only when `handle-call` returns `noreply`, where every call was inserted into and removed from the map (SipHash, and a `retain` over it). `call_split` six rounds, `pool_load 4` three, alternated; both builds carry `call_split`'s counting allocator, idle unless asked. Logs on the Pi under `~/pi-runs/b12-slot-2026-10-24/`.

| Measure | Before | After |
| --- | --- | --- |
| `call_split`, one instance, 1 worker | 2.99 to 3.14 µs | 3.08 to 3.17 µs |
| `call_split`, one instance, 4 workers | 3.72 to 3.91 µs | 3.73 to 3.89 µs |
| `pool_load`, pool 1, 64 clients | 332k to 340k calls/s | 355k to 363k |
| `pool_load`, pool 4, 64 clients | 898k to 911k | 924k to 940k |
| `pool_load`, pool 16, 1 client | 200k to 212k | 221k to 228k |
| `pool_load`, pool 16, 64 clients | 769k to 793k | 816k to 818k |
| `pool_load`, pool 16, 256 clients | 807k to 826k | 833k to 852k |

- Every `pool_load` row is better after, in every round (3 to 8%; p99 a little lower too). Sequential calls to one instance show no gain, the 1-worker range a little higher: its map stays in cache, and a difference of that size is within what a change of code layout does between builds. Not looked into further.
- An empty call makes 6 heap allocations of 195 bytes in all, against 2 for the bare tokio round trip (`call_split 1 allocs`, 16-core machine): the caller's envelope (content type and body), the reply channel, and the reply's content type and body lifted out of guest memory. The map made none (its capacity is kept). Fewer would take a change to `Envelope`, which owns its content type.
- B12 in all, comparing the first section's "before" with this one's "after" (different sessions on the same idle Pi, so read the figures loosely): an empty call 3.55 to 3.67 µs to 3.08 to 3.17 µs on 1 worker (about 14%) and 4.33 to 4.63 µs to 3.73 to 3.89 µs on 4 (about 14%); `pool_load 4` empty calls 800k to 813k to 924k to 940k on a pool of 4 and 662k to 670k to 816k to 818k on a pool of 16. A gen_server call on the Pi is 2.44 µs (OTP 27): 1.5 times before, 1.3 times after on 1 worker.

## 2026-10-24: compiling on every core, slots kept resident, timer lateness (B10)

The three leads from B9. Each A/B ran alternated rounds on an idle machine (the 16-core machine, then the Raspberry Pi 5); Pi logs under `~/pi-runs/b10-parcomp-2026-10-24/` and `~/pi-runs/b10-resident-2026-10-24/`.

### Parallel compilation

wasmtime's `parallel-compilation` feature compiles a component's functions on rayon's pool, one thread per core. `compile_cost`, two rounds each, medians of 5 loads:

| Component | 16-core before | 16-core after | Pi before | Pi after |
| --- | --- | --- | --- | --- |
| echo, 17 KiB | 21.8 to 22.0 ms | 13.9 to 15.2 ms | 25.7 to 26.0 ms | 12.5 to 12.9 ms |
| chaos, 150 KiB | 179.6 to 180.3 ms | 25.9 to 28.0 ms | 199.7 to 201.1 ms | 69.8 to 70.5 ms |
| SDK fixture, 543 KiB | 570.8 to 573.3 ms | 62.1 to 62.9 ms | 641.1 to 646.6 ms | 222.9 to 228.1 ms |
| scada `control`, 488 KiB | 517.5 to 517.7 ms | 52.4 to 54.4 ms | 587.9 to 588.1 ms | 203.8 to 209.6 ms |
| SDK fixture debug, 5.4 MiB | 1055.5 to 1058.3 ms | 120.1 to 124.4 ms | 1151.9 to 1152.9 ms | 383.4 to 385.6 ms |

- The whole run takes about 25% more CPU (16-core machine: 23.5 s to 28.6 s user; Pi: 26.1 to 26.2 s to 32.5 to 32.8 s) and 15 to 43 MB more peak memory. Cache files and cached loads are unchanged, except the echo fixture's cached load on the 16-core machine, 0.32 to 0.54 ms.
- A deploy now uses every core for a shorter time. Nothing was measured of a compile next to a loaded node.

### Waking, upgrading and restarting: page faults

A scratch harness woke one instance holding `ballast:<mib>` over and over, with page faults and user and system time from `/proc/self/stat` (16-core machine): every wake took 512 minor faults per MiB of state, two per 4 KiB page, and about three quarters of its time was system time (16 MiB: 15.3 ms, of which 11.5 to 12 ms system). The state is touched twice in fresh memory: the canonical ABI's copy of the snapshot into guest memory, then the fixture's own copy out of it (an SDK app's decode does the same). A freed pool slot returns its pages to the kernel (`madvise`), so the next instance faults each one in again. Faulting in fresh memory costs the same natively (`heap_cost`'s native column, about 1.5 µs per page here), so the copies are not what costs; neither can go from the host side.

wasmtime's `linear_memory_keep_resident` keeps a freed slot's pages, zeroed, up to a size, and `pagemap_scan` (Linux 6.7's `PAGEMAP_SCAN`) limits that to the pages the guest wrote. Without the scan, a free slot holds the guest's whole accessible memory up to the size (1.13 MiB after an idle chaos instance against 0.08 MiB), and restarts are about 15 µs slower. The change: both, at 16 MiB per slot for the standard profile and none for dense (`MemoryProfile::keep_resident`, node config `keep_resident`). `heap_cost 1`, `restart_cost 1` and `baseline 1 5000`, two rounds each:

| Measure | 16-core before | 16-core after | Pi before | Pi after |
| --- | --- | --- | --- | --- |
| Wake, 1 MiB state | 1152 to 1192 µs | 411 to 418 µs | 1320 to 1565 µs | 654 to 676 µs |
| Wake, 16 MiB state | 14.5 to 15.4 ms | 9.8 to 10.1 ms | 17.5 to 20.6 ms | 12.3 to 13.5 ms |
| Upgrade with 1 MiB, to first reply | 882 to 884 µs | 389 to 412 µs | 2307 to 2636 µs | 1452 to 1571 µs |
| Upgrade with 16 MiB, to first reply | 20.9 ms | 16.5 to 16.6 ms | 28.3 to 29.7 ms | 25.0 to 26.7 ms |
| Kill to the restarted instance's first reply, p50 | 64.4 µs | 56.3 to 56.9 µs | 73.3 to 74.3 µs | 62.9 to 64.6 µs |
| Crash to the restarted instance's first reply, p50 | 113.4 to 114.3 µs | 105.8 to 106.3 µs | 136.3 to 137.0 µs | 125.5 to 127.3 µs |
| Instantiate on a reused slot, 0 MiB heap | 74.3 to 76.4 µs | 61.6 to 62.2 µs | 89.7 to 93.8 µs | 63.5 to 66.9 µs |
| Resident per free slot after a 0 / 1 / 16 MiB heap | 0.06 / 0 / 0 MiB | 0.08 / 1.00 / 14.98 MiB | 0.05 / 0 / 0 MiB | 0.12 / 0.96 / 14.98 MiB |
| `baseline`, empty call p50 | 2.29 to 2.40 µs | 2.25 to 2.30 µs | 3.09 to 3.20 µs | 3.00 to 3.08 µs |

- State larger than half the kept size still faults for the rest: a 16 MiB wake needs about 32 MiB, and a 64 MiB heap is past 16 MiB (0 MiB held after exit).
- A crash's restart p99 moves between about 115 and 210 µs from run to run on the 16-core machine, before and after alike (three more rounds of each).
- `instantiate to first reply` in `baseline` starts 5,000 instances on fresh slots, so it does not change.

### Timer lateness

A tokio sleep rounds its deadline up to the timer's next 1 ms tick (`deadline_to_tick` adds 999,999 ns), and the worker then parks in `epoll_wait`, whose timeout mio rounds up to whole milliseconds again: 1 to 2 ms late, 1.2 ms at p50 (B9's `timer_cost`). `send-after` and guest sleeps are tokio sleeps. A thread's `park_timeout` of 1 or 10 ms, which a timer thread of Paddock's own would use, is 54 to 61 µs late at p50 and under 100 µs at max on both machines (a scratch program, 200 sleeps each). Not built: MANIFEST.md states the lateness to expect, and the timer thread is parked in ROADMAP.md.

## 2026-10-25: resident memory over two hours (B11)

`scripts/soak.sh --hours 2.2 --only chaos --chaos-minutes 120` on the Raspberry Pi 5 (2026-10-24 23:37 to 2026-10-25 01:37, idle apart from the run; swap on and unused). One node runs the chaos example under random commands (traps, timeouts, spins, `alloc:<mib>`, log bursts): 243,511 calls in 2 hours, 198,683 answered with an error. The node's resident memory, sampled every 10 s (720 samples), by half hour:

| Minutes | Lowest | Highest |
|---|---|---|
| 0 to 30 | 65.0 MiB | 77.4 MiB |
| 30 to 60 | 66.4 MiB | 80.4 MiB |
| 60 to 90 | 66.4 MiB | 79.4 MiB |
| 90 to 120 | 66.4 MiB | 80.4 MiB |

The floor rises from 65.0 MiB at 10 s to 66.4 MiB by minute 30 and stays within 8 KiB of that for the last 90 minutes: no creep. The highs are instances holding memory from `alloc` commands (39 samples above 68.4 MiB, the last at minute 119); each returns to the floor when its instance exits. The summary line's growth, +1.2 MiB (65.1 to 66.4 MiB, from the first sample after a minute to the last), is the first half hour's warm-up. The drive watcher paused nothing: NVMe at most 44 C, CPU at most 54 C.

## 2026-10-25: the QUIC floor between two machines (B11)

`quic_floor 4 connect HOST:4433` against `quic_floor 4 serve 0.0.0.0:4433` on the other machine: the Raspberry Pi 5 and the 16-core machine on the same wired gigabit Ethernet LAN, both idle (load under 1). Bare quinn, TLS 1.3, 64-byte frames, one in flight; `udp` is a bare UDP echo of the same 64 bytes through tokio. `ping` (ICMP, 50 packets) averages 0.162 to 0.163 ms either way. Two runs each way, p50 (p99) in µs:

| Client to server | Two writes | One write | Client on current thread | UDP |
|---|---|---|---|---|
| 16-core to Pi | 238.8 to 239.7 (304 to 305) | 230.5 to 241.5 (300 to 307) | 197.4 to 199.5 (241 to 246) | 144.0 to 148.7 (198 to 199) |
| Pi to 16-core | 221.2 to 223.8 (267 to 271) | 222.9 to 223.5 (270) | 200.2 to 201.1 (248 to 249) | 138.1 to 140.6 (184) |
| Pi, loopback | 54.8 (61.3) | 54.7 (61.1) | 30.2 (39.0) | 16.4 (19.7) |

- **The network.** A UDP round trip through tokio on both ends takes about 140 µs, close to `ping`'s 162 µs average: that is the wire, two kernels' network stacks and the switch. No packet was lost.
- **QUIC on top.** QUIC adds 80 to 97 µs to that with 4 workers and 50 to 60 µs with the client on one thread: encryption, quinn's own tasks and, with workers, wakes across threads on both machines. On the Pi's loopback the same costs come to about 38 µs (one machine doing both ends' work without the wire). The Pi as the server is 7 to 18 µs slower at p50 than as the client.
- **What it means for a call between nodes.** On loopback a call between two nodes took about 106 µs against a 50 µs QUIC floor (2026-10-07, 16-core machine). Across this LAN the floor alone is 220 to 240 µs, so a call between the Pi and the 16-core machine is expected near 300 µs; `cluster_throughput` between the two machines will measure it.
- The 16-core machine's own loopback figure moved between 53 and 97 µs at p50 with 4 workers across runs this session (30.6 to 32.3 µs on one thread), so it is not tabled.

## 2026-10-25: calls between nodes on two machines (B11)

`cluster_throughput 4 connect` (node 1, the caller) on one machine against `cluster_throughput 4 serve` (node 2 and its 16 chaos-fixture instances) on the other, keys from `cluster_throughput keys` copied to both: the Raspberry Pi 5 and the 16-core machine on the wired gigabit LAN of the section before, each idle but for the runs (vmstat 99% idle between runs). Each direction run three times (the first Pi-to-16-core run stopped after its casts, below). "One node" is node 1 calling its own 16 instances in the same run.

| Node 1 to node 2 | Sequential call p50 (p99) | 16 callers | 64 callers | Casts handled/s (refused of 400,000) | 1 KiB, 16 callers | 64 KiB, 16 callers |
|---|---|---|---|---|---|---|
| 16-core to Pi | 271 to 284 µs (317 to 325) | 38,300 to 39,100/s | 44,700 to 46,000/s | 236,000 to 247,000 (78,000 to 133,000) | 27,300/s, 27 MiB/s | 1,625 to 1,734/s, 102 to 108 MiB/s |
| Pi to 16-core | 281 to 283 µs (320 to 322) | 40,400 to 42,900/s | 37,400 to 38,800/s | 256,000 to 270,000 (147,000 to 157,000) | 28,300 to 29,100/s, 28 MiB/s | 1,723 to 1,771/s, 108 to 111 MiB/s |
| Loopback, 16-core | 107.5 µs (138) | 57,100/s | 49,700/s | 163,000 (115,000) | 48,400/s, 47 MiB/s | 4,359/s, 272 MiB/s |

The loopback row is this session's run of the unchanged one-process mode (load 2.8, from the builds before it), in line with 2026-10-07's 106 µs.

- **Latency.** A call to the other machine takes 271 to 284 µs at p50, 30 to 61 µs over the QUIC floor between the same machines (220 to 240 µs, the section before). Over loopback Paddock adds about 57 µs to its 50 µs floor, so what Paddock adds per call between nodes is about the same with a network under it; the network adds about 170 µs.
- **Concurrency.** 16 callers reach 11 to 12 times the sequential rate, and 64 callers no more: 37,000 to 46,000 calls/s, 67 to 75% of loopback's rate with 16 callers and 75 to 93% with 64. Latency alone would allow more (16 in flight at 280 µs is 57,000/s), so something along the one connection limits it; not attributed.
- **Bulk.** 64 KiB bodies move 102 to 111 MiB/s each way at once, about 900 Mbit/s: the gigabit link is the limit.
- **Casts.** The per-peer send queue's frame cap (65,536 frames) refuses 19 to 39% of a 400,000-cast burst, against 29% on loopback; every cast accepted is handled. The Pi as the sender handles about 10% more per second than the 16-core machine does.
- **One node on each machine.** The same run's one-node figures: sequential calls 3.4 to 6.3 µs at p50 on the 16-core machine and 3.8 to 4.9 µs on the Pi; 16 callers 663,000 to 698,000 calls/s here and 875,000 to 887,000 on the Pi.
- **Two fixes to the benchmark on the way.** The first Pi-to-16-core run failed reading the casts' counts: the call that reads them came back `Overloaded` while the queue still held casts, so `count` now retries that. A run started straight after another failed its first call with `Noconnection` while node 2 still held the earlier node 1, so node 1 now retries its first calls for up to 30 s.

## 2026-10-25: replicated writes between two machines (B11)

`replication_cost 4 primary` (node 1 and the witness, node 3) on one machine against `replication_cost 4 standby` (node 2) on the other, keys from `replication_cost keys` copied to both: the Raspberry Pi 5 and the 16-core machine on the wired gigabit LAN of the sections before, each idle but for the runs. Both nodes' databases are on tmpfs (`/dev/shm`), where a sync costs nothing, so the difference from loopback is the network's. The loopback rows run the same two processes on one machine, also on tmpfs. Two runs each way, writes/s and p50 (p99) in µs:

| | Primary on the 16-core machine, standby on the Pi | Primary on the Pi, standby on the 16-core machine | Loopback, 16-core | Loopback, Pi |
|---|---|---|---|---|
| Disk bucket, one writer (no network) | 7,193 to 7,606/s, 132 to 152 (189 to 192) | 9,770 to 10,101/s, 90 to 97 (142) | 6,977/s, 153 (189) | 9,943/s, 96 (133) |
| Replicated, one writer on the primary | 1,124 to 1,141/s, 873 to 893 (982 to 1,003) | 1,142 to 1,146/s, 872 to 875 (968 to 990) | 1,450/s, 688 (803) | 1,356/s, 737 (828) |
| Replicated, one writer on the standby | 731 to 745/s, 1,339 to 1,358 (1,483 to 1,663) | 755 to 760/s, 1,306 to 1,315 (1,544 to 1,557) | 921/s, 1,080 (1,245) | 918/s, 1,074 (1,519) |
| Replicated, 16 writers | 6,445 to 6,452/s | 6,710 to 6,729/s | 7,180/s | 6,420/s |
| Replicated, 64 writers | 9,551/s | 9,639 to 9,746/s | 10,252/s | 9,902/s |
| Replicated, alone, one writer | 2,593 to 2,631/s, 380 to 384 (440 to 445) | 2,948 to 3,023/s, 329 to 337 (370 to 378) | 2,537/s, 393 (472) | 3,013/s, 329 (371) |

- **One writer.** A replicated write takes 872 to 893 µs at p50 across the LAN against 688 to 737 µs on loopback: 135 to 205 µs more, about one round trip between the machines less a loopback one (280 against 107 µs for a call, the section before). It takes the same time whichever machine is the primary. A write on the standby, forwarded to the primary, adds 230 to 280 µs.
- **Concurrent writers.** Writes arriving together share a log append, so the network hardly shows: 16 writers reach 6,445 to 6,729/s and 64 writers 9,551 to 9,746/s, within 10% of loopback's either way.
- **Alone.** With the standby gone the primary writes at its loopback rate, the network out of the path.
- **On disk.** Not compared: a synced write on the 16-core machine takes 0.9 to 2.5 ms (its disk bucket, 2026-10-18 and this session's loopback runs: 413 to 855/s; a 4 KiB `fsync` there, 2.5 ms at p50 today), several times what the network adds. Its root filesystem was 99% full (25 GB free of 1.8 TB) during these runs.
- **The handover.** The two processes hand over through the replicated bucket: the primary writes `bench-go` when its runs are done; the standby then runs its writer, writes `bench-done` and shuts its transport down, and the primary runs alone.

## 2026-10-25: replicated writes between two machines on disk (B11)

The section before's runs with both nodes' databases on each machine's NVMe instead of tmpfs: on the 16-core machine the root filesystem (ext4 on the Samsung 970 EVO Plus, 81% full), on the Pi `/home` (ext4 on an Inland TN320, 8% full). A 4 KiB write with `O_DSYNC` (`dd`, 500 writes) takes 2.5 ms on the 16-core machine and 0.59 ms on the Pi, so here the Pi has the faster disk. Same build, keys and processes as the section before; both machines idle (load under 1.5); the Pi's swap on and unused (40 KiB), its drive watched by its temperature and error monitor throughout (peak 49 °C, no errors). Two runs each way, three on the 16-core machine's loopback, one on the Pi's; writes/s and p50 (p99) in µs:

| | Primary on the 16-core machine, standby on the Pi | Primary on the Pi, standby on the 16-core machine | Loopback, 16-core | Loopback, Pi |
|---|---|---|---|---|
| Disk bucket, one writer (no network) | 383 to 963/s, 922 to 2,485 (4,021 to 5,982) | 2,082 to 2,115/s, 437 to 444 (825 to 829) | 979 to 1,037/s, 861 to 911 (2,760 to 4,036) | 2,068/s, 446 (848) |
| Replicated, one writer on the primary | 341 to 406/s, 2,234 to 2,268 (5,356 to 6,075) | 399 to 401/s, 2,255 to 2,284 (5,367 to 5,407) | 361 to 362/s, 2,386 to 2,395 (6,844 to 7,446) | 528/s, 1,834 (2,608) |
| Replicated, one writer on the standby | 350 to 351/s, 2,785 to 2,803 (5,889 to 5,915) | 351/s, 2,741 to 2,750 (5,854 to 5,864) | 145 to 204/s, 5,857 to 7,154 (8,855 to 11,580) | 443/s, 2,234 (2,781) |
| Replicated, 16 writers | 2,738 to 2,742/s | 2,274 to 2,526/s | 1,549 to 1,657/s | 2,610/s |
| Replicated, 64 writers | 6,383 to 6,386/s | 5,965 to 6,280/s | 4,785 to 4,937/s | 5,640/s |
| Replicated, alone, one writer | 694 to 695/s, 1,254 to 1,261 (4,345 to 4,372) | 1,100 to 1,102/s, 874 to 876 (1,311 to 1,314) | 266 to 597/s, 1,264 to 3,387 (4,634 to 6,809) | 1,097/s, 879 (1,311) |
| Replicated, alone, 16 writers | 3,085 to 3,184/s | 5,258 to 5,280/s | 1,306 to 3,149/s | 5,188/s |

- **One writer.** A replicated write takes 2.23 to 2.28 ms at p50 across the LAN whichever machine is the primary, against 0.87 to 0.89 ms on tmpfs: the 16-core machine's synced write is in the path either way, as the primary's or as the standby's. The network's 135 to 205 µs (the section before) is now under a tenth of a write. With the Pi as the primary a write takes 0.42 to 0.45 ms more than on the Pi's loopback (1.83 ms), the standby's slower disk.
- **Concurrent writers.** 64 writers reach 5,965 to 6,386/s across the LAN, more than on either machine's loopback (4,785 to 4,937 and 5,640/s), and 16 writers likewise on the 16-core machine: on loopback both nodes sync to one drive, across the LAN each to its own.
- **Writes on the standby.** Forwarded writes take 2.74 to 2.80 ms across the LAN, 0.5 ms over a write on the primary, but 5.9 to 7.2 ms on the 16-core machine's loopback. Not attributed; the two nodes share one drive there, and the Pi's loopback, with the faster drive, shows 0.4 ms.
- **Alone.** The primary writes at its own disk's pace: the Pi at 874 to 879 µs on the LAN and on loopback alike, the 16-core machine at 1.25 to 1.26 ms on the LAN and 1.26 to 3.39 ms on its loopback, where the standby's process was just shutting down on the same drive.
- **The 16-core machine's disk varies.** Its disk bucket ran at 383/s (p50 2.5 ms) in one run and 963 to 1,037/s in the other four, matching 2026-10-18's 0.9 to 2.5 ms per synced write. A pair with a slow-syncing node runs at that node's pace whichever side is primary, network or not.

## 2026-10-25: wasmtime 49 to 50 (Y1)

`baseline` (1 and 4 workers, standard; 1 worker, dense) and `pool_load` (1, 4 and 16 workers), wasmtime 49.0.4 (commit 11539d1, a worktree) against wasmtime 50.0.0 (the same tree with Y1's dependency update), alternated, three rounds each, on the 16-core machine, idle (load under 1 before the runs; `pool_load` raises it to about 5 itself).

| | wasmtime 49 | wasmtime 50 |
|---|---|---|
| Empty call, 1 worker, p50 | 2.29 to 2.40 µs | 2.30 to 2.35 µs |
| 1 KiB echo, 1 worker, p50 | 4.35 to 4.43 µs | 4.38 to 4.43 µs |
| Empty call, 4 workers, p50 | 3.24 to 3.94 µs | 3.24 to 3.97 µs |
| Casts handled, 1 worker | 696,748 to 727,499/s | 708,683 to 724,429/s |
| Instantiate to first reply, 1 worker, p50 | 84.3 to 85.4 µs | 83.4 to 84.6 µs |
| Idle memory per instance (standard, dense) | 64 to 68 KiB | 64 to 68 KiB |
| `pool_load 4`, empty, pool of 16, 64 clients | 579,253 to 597,429 calls/s | 570,142 to 587,769 calls/s |
| `pool_load 4`, `busy:20000`, pool of 16, 64 clients | 105,711 to 106,434 calls/s | 105,646 to 105,976 calls/s |

- **No change beyond noise.** Every row's ranges overlap but two: on 1 worker, `pool_load` with empty calls runs 1 to 2% fewer calls/s on 50 with a pool of 4 (399,151 to 406,484 against 408,709 to 409,805) and with a pool of 16 and one client (359,645 to 369,051 against 370,682 to 377,159). The 16-worker rows vary by up to 40% from run to run on either version.
- **Spikes.** S1's eight experiments and its poisoned-Store check (`REUSE_AFTER_DROP`) pass on 50 as on 49; E3's spinner is killed at 702 to 704 ms on both versions (its README says about 500 ms). S4's results match its README, except that `define_unknown_imports_as_traps` after the WASI linker now instantiates (the missing imports trap when called): wasmtime 50 preserves resource aliases when stubbing unknown imports (its release notes, #14396).

## 2026-10-25: redb 3 to 4 (Y2)

`replication_cost` on the 16-core machine's loopback with both nodes' databases on its NVMe, as in "replicated writes between two machines on disk" above: that section's three redb 3.1.3 runs against three on redb 4.4.0 the same afternoon, same disk and keys, idle (load under 1). Writes/s and p50 in µs:

| | redb 3 | redb 4 |
|---|---|---|
| Disk bucket, one writer | 979 to 1,037/s, 861 to 911 | 667 to 964/s, 944 to 1,120 |
| Disk bucket, 64 writers | 1,004 to 1,016/s | 934 to 954/s |
| Replicated, one writer on the primary | 361 to 362/s, 2,386 to 2,395 | 270 to 371/s, 2,354 to 3,769 |
| Replicated, one writer on the standby | 145 to 204/s, 5,857 to 7,154 | 258 to 347/s, 2,740 to 2,816 |
| Replicated, 16 writers | 1,549 to 1,657/s | 1,732 to 1,770/s |
| Replicated, 64 writers | 4,785 to 4,937/s | 4,623 to 4,732/s |
| Replicated, alone, one writer | 266 to 597/s, 1,264 to 3,387 | 439 to 730/s, 1,222 to 1,560 |
| Replicated, alone, 16 writers | 1,306 to 3,149/s | 3,661 to 3,776/s |

- **Writes on the standby.** A forwarded write takes 2.74 to 2.82 ms on redb 4, the same as between the two machines, where redb 3 took 5.9 to 7.2 ms on this loopback. Not attributed further; redb 4.2's notes list faster `Durability::None` commits (the replicated log's) and freed pages reused one transaction sooner.
- **Alone** is steady on redb 4 (1.22 to 1.56 ms, 3,661 to 3,776/s with 16 writers) where redb 3's runs spread by more than two times.
- **The rest** is within this disk's variation: one writer on the primary at 2.35 ms in two runs and 3.77 ms in one; synced disk-bucket writes 5 to 7% fewer per second on redb 4, less than the spread between runs this morning (383 to 1,037/s in that section).

## 2026-10-25: ordinary app code in Wasm (B11)

`app_cost.rs` (`scripts/bench.sh app`) runs `paddock-workload`'s five workloads of ordinary app code over generated inputs: `json-typed` (400 orders, 172 KB, parsed into structs with serde derive, totals recomputed, serialized back), `json-value` (the same document through `serde_json::Value`), `regex` (a capturing pattern over a 282 KB access log), `markdown` (123 KB rendered to HTML with `pulldown-cmark`) and `map` (10,000 keys counted in a `BTreeMap<String, u64>`, looked up and ranked). Each runs the same number of times, chosen so the native run takes about 50 ms, on four sides: native in the benchmark's process; in Paddock, as one call to `paddock-fixture-workload` on a default `Runtime`; and the same crate built as a core module with no imports (`wasm32-unknown-unknown`) on two wasmtime engines that differ only in epoch interruption, the epoch side ticked every 2 ms with a yield on each tick, as `epoch_cost` does. Every side's checksum matches the native one. Median of 7 samples per side, alternated; three runs on each machine, idle (load under 1). Commit dcb11d4 with the benchmark added; wasmtime 50.0.0, rustc 1.99.0; native code built for generic x86-64 and aarch64.

Ratios, the range over the three runs:

| Workload | In Paddock / native, 16-core | Pi | Epoch off / native, 16-core | Pi | Epoch on / off, 16-core | Pi |
| --- | --- | --- | --- | --- | --- | --- |
| json-typed | 1.98 to 2.14 | 1.76 | 1.53 to 1.65 | 1.35 | 1.24 to 1.26 | 1.30 |
| json-value | 1.71 to 1.72 | 1.42 to 1.43 | 1.33 to 1.34 | 1.05 to 1.06 | 1.26 to 1.29 | 1.36 to 1.37 |
| regex | 1.94 to 1.98 | 1.60 to 1.61 | 1.49 to 1.52 | 1.37 to 1.38 | 1.27 to 1.29 | 1.17 to 1.19 |
| markdown | 2.50 to 2.53 | 2.28 to 2.30 | 2.05 to 2.09 | 1.89 to 1.92 | 1.19 to 1.20 | 1.20 |
| map | 2.54 to 2.55 | 2.18 to 2.20 | 1.44 to 1.47 | 1.46 to 1.47 | 1.68 to 1.71 | 1.56 to 1.57 |

- **App code runs at 1.4 to 2.6 times native in Paddock**, closer to native on the Pi (1.4 to 2.3) than on the 16-core machine (1.7 to 2.6). Native times were 33 to 49 ms per sample.
- **Epoch checks cost 1.19 to 1.71 times the unchecked time** on this code, on both machines, against at most 2% on S8's Rust kernels and 0 to 23% on `epoch_cost`'s WAT kernels on the 16-core machine (up to 1.5 times on the Pi's tight loop). With the ticker stopped, so the deadline is never reached and the guest never yields, the ratios are the same (16-core, one run): the cost is the checks in the compiled code, not the yields. For `map`, and for `json-value` on the Pi, the checks cost more than the rest of the gap to native. Which code pays is not attributed yet (ROADMAP B13).
- **The guest is the core module with checks on.** Paddock's call takes 0.95 to 1.03 times the core module's time with epochs on in every run (one round trip, about 2 µs, against 45 to 116 ms of work), so the app world, the `wasm32-wasip2` build and the pooling allocator add nothing measurable to compute.
- The 2026-10-07 note on S8 above ("for that code the checks cost at most about 2%") holds for those kernels only.

`target/release/examples/app_cost`, first run on each machine (16-core: 12.48 s wall, 17.37 s user, 151 MiB peak RSS; Pi: 12.15 s wall, 17.31 s user, 102 MiB):

```text
16-core
| json-typed | 44 | 42.20 ms | 90.39 ms | 2.14x | 69.75 ms | 87.78 ms | 1.259 |
| json-value | 21 | 46.50 ms | 79.94 ms | 1.72x | 62.20 ms | 80.07 ms | 1.287 |
| regex | 19 | 37.98 ms | 75.04 ms | 1.98x | 56.70 ms | 72.58 ms | 1.280 |
| markdown | 48 | 46.07 ms | 115.68 ms | 2.51x | 95.72 ms | 114.44 ms | 1.196 |
| map | 8 | 43.48 ms | 110.70 ms | 2.55x | 63.12 ms | 107.95 ms | 1.710 |
Pi
| json-typed | 28 | 38.87 ms | 68.36 ms | 1.76x | 52.37 ms | 68.21 ms | 1.302 |
| json-value | 11 | 36.14 ms | 51.47 ms | 1.42x | 38.06 ms | 51.71 ms | 1.359 |
| regex | 17 | 45.19 ms | 72.81 ms | 1.61x | 62.41 ms | 73.98 ms | 1.185 |
| markdown | 34 | 42.03 ms | 96.87 ms | 2.30x | 80.59 ms | 96.42 ms | 1.196 |
| map | 6 | 42.37 ms | 93.14 ms | 2.20x | 62.41 ms | 97.49 ms | 1.562 |
```

Columns: workload, runs per sample, native, in Paddock, ratio, core module epoch off, epoch on, epoch ratio.

## 2026-10-25: where epoch checks cost, and G2's open questions (B13)

The core module of "ordinary app code in Wasm" above, compiled by wasmtime 50.0.0 with epoch interruption off and on (default `Config` otherwise, as `paddock-host` builds it), profiled with `perf` (wasmtime's perf map) and disassembled; a scratch driver calls `workload_run` without ever advancing the epoch, so only the checks run. The 16-core machine for timings unless named; the Pi for the idle-memory runs.

**Epoch checks on app code.** A check is a load of the epoch counter, a compare and a branch to a cold block that calls `wasmtime_builtin_new_epoch`, at every function entry and loop header. The call costs more than the check: its clobbers shape register allocation for the whole function, even though it is never taken.

- A function that was a leaf gains a stack-limit check and saves callee-saved registers: 152 of the module's 1,936 functions gain a stack check, callee-saved register saves rise by 25%, and the compiled code by 24% (2.04 to 2.54 MB).
- In tight loops, values move between caller- and callee-saved registers, or to the stack, on every iteration. The guest's `memcmp` (compiler-builtins' byte loop; wasi-libc's in the app is the same loop) keeps both pointers on the stack with checks on: three more loads and two stores per byte compared, against a 12-instruction loop held in registers without them.
- `map` (a `BTreeMap<String, u64>`) spends about 72% of its added time in `memcmp`: 0.90 s of 3.15 s with checks off, 2.51 s of 5.39 s with them on. A `memcmp` comparing 8 bytes per step (built into the workload for the test) takes `map` from 108 to 72 ms with checks on and 63.5 to 49.7 ms off (epoch ratio 1.70 to 1.45); the other four workloads change by under 5% either way.
- In `json-typed` no function stands out: the added time spreads over serde's functions in proportion to their share.

**Tight loops on the Pi (1.5 times).** The same pattern, `epoch_cost`'s LCG loop compiled for aarch64: 12 instructions per iteration without checks, 21 with them (the epoch load, compare and branch, and six register moves that shuttle the accumulator, counter and cached deadline between registers). On x86-64 the loop goes from 7 to 17 instructions and shows no cost on the 16-core machine; likely because Zen 1 eliminates register moves at rename and issues wider, so the moves fit under the 4-cycle multiply-add chain, where the Cortex-A76 cannot hide them (not measured with counters).

**Compiled-code files on aarch64.** wasmtime aligns `.text` and `.rodata.wasm` to 64 KiB on aarch64 (4 KiB on x86-64): the echo fixture's file is 271,240 bytes for aarch64 against 62,336 for x86-64, cross-compiled on the 16-core machine, with 32 KiB of x86-64 code becoming a 64 KiB section and 4 KiB of read-only data another 64 KiB. A fixed padding of about 200 KB per file, on disk; pages of padding are never touched, so they cost no memory.

**Idle memory on the Pi.** The Pi's 109 KiB per idle instance is resident memory counting the component's memory image once per instance: by mapping, 64 KiB per instance of `memfd:wasm-memory-image` on the Pi against 12 KiB here, with the malloc heap (36 to 37 KiB) and the pool's share (4 KiB) the same on both. Measured proportionally (PSS, the image's pages divided among the instances that map them), an idle instance costs 57 KiB on the Pi (standard and dense profiles) and 56 KiB here. `baseline` now prints both figures. The larger window is likely the same 64 KiB alignment on aarch64, with the kernel's fault-around mapping it whole on the first read (not checked further).

## 2026-10-25: giving free slots' pages back

A freed Store slot keeps up to `keep_resident` (16 MiB on the standard profile) of the pages its instance wrote, so the next instance on it takes no page faults for them (B10). Since 2026-10-25 a node gives them back once no Store has been taken for `release_idle_after_s` (300 by default), through wasmtime 50's `Engine::release_idle_pool_memory`. `heap_cost`'s passivation step now wakes its 16 passivated instances twice: warm, as before, then after they passivate again and the free slots' pages are released (`Runtime::release_idle_memory`). Raspberry Pi 5 (Linux 7.0), idle (load under 0.6), three runs; medians of 16 wakes:

| State | Released | Wake, warm slot | Wake, after release |
| --- | --- | --- | --- |
| 0 MiB | 0 | 49 to 50 µs | 51 to 52 µs |
| 1 MiB | 32 MiB | 0.60 to 0.73 ms | 1.30 to 1.31 ms |
| 16 MiB | 256 MiB | 12.4 to 12.8 ms | 16.5 to 19.2 ms |
| 64 MiB | 208 to 224 MiB | 61.6 to 73.1 ms | 67.6 to 85.4 ms |

- **What a release costs a wake:** about twice the warm wake at 1 MiB and 30 to 50% more at 16 MiB, the page faults B10 removed. At 64 MiB the slot kept only 16 MiB of the state's pages anyway, and the difference is within the run-to-run spread.
- **Hence the default of five minutes:** a node whose passivated instances wake every minute or two keeps its warm slots; one that had a burst and went quiet gets its memory back within about five minutes.
- **Linux before 6.13 releases nothing when more than one slot is warm.** wasmtime 50 decommits several slots with one `process_madvise` call on `PIDFD_SELF`, which Linux has from 6.13; the 16-core machine's 6.8 answers EBADF, which wasmtime does not treat as "unsupported" (it falls back to `madvise` only for ENOSYS, EINVAL and EPERM), so the release returns 0 and the slots are left as they were (seen with `strace`, 2026-10-25). One warm slot takes the `madvise` path and is released. Paddock then tries once per quiet period rather than at every check.

## 2026-10-29: inline atomics on the Pi

A quarter of `pool_load`'s samples on the Pi fell in the `__aarch64_cas*` and `__aarch64_ldadd*` helpers (S24's profiles): Rust on aarch64 Linux calls these for every atomic operation, so that one binary runs on ARMv8.0 CPUs, which lack the LSE atomic instructions. `paddock-host`'s examples built three ways: default; `-C target-feature=+lse` (atomics inline); `-C target-cpu=cortex-a76` (also the Pi's instruction scheduling). The flags went in `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS`. `pool_load` holds 13,804 helper calls by default against 13,708 inline LSE instructions with either flag; the 165 calls left are in the precompiled standard library. Raspberry Pi 5, Linux 7.0, idle (load 0.6 before the runs); three runs of each, alternating:

| Measure | Default | `+lse` | `cortex-a76` |
| --- | --- | --- | --- |
| `call_split 1`: empty call | 3.20 to 3.27 µs | 2.96 to 3.09 µs | 3.04 to 3.12 µs |
| `call_split 1`: bare tokio round trip | 0.46 to 0.47 µs | 0.43 to 0.44 µs | 0.43 to 0.45 µs |
| `baseline 4`: sequential empty calls/s (median) | 227,632 | 247,081 | 243,488 |
| `baseline 4`: instantiate p50 (median) | 95.8 µs | 88.8 µs | 92.3 µs |
| `pool_load 4`: empty calls/s, against default | | 1.013 to 1.092 | 1.006 to 1.076 |
| `pool_load 4`: busy calls/s, against default | | 0.996 to 1.015 | 0.990 to 1.009 |

- **Inline atomics save 3 to 9% on message paths, and nothing on guest work.** Most of the helpers' share of the profile was the atomic operations themselves (cache lines shared between workers), not the calls.
- **`cortex-a76` adds nothing beyond `+lse`.**
- **Not the default:** a binary with LSE inline faults on ARMv8.0 CPUs (Raspberry Pi 4, Cortex-A72). OPERATIONS.md says how to build for a CPU that has them.

## Not measured yet

- What limits 16 concurrent callers between machines to 67 to 75% of loopback's rate.
- Resident memory over more than two hours, and in the failover, replication and scada scenarios, whose nodes the faults kill every minute or two.
