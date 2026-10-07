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

## Not measured yet

- The cost of epoch interruption itself, against a raw wasmtime store without it (suggested by S8).
- Calls through a node's admin API (HTTP on a Unix socket), which add hyper and JSON on top of the figures above.
- Many concurrent clients against a pool of instances.
