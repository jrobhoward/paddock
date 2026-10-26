# Spike S22: an app in Python (componentize-py)

**Question.** After S21 (Go), does a Python app built with componentize-py run on a node: deploy, calls, casts, timers, crashes and their limits, snapshot upgrade and rollback? What does it cost next to the Rust counter and S21's Go app?

**Answer: not on an unmodified node; with one host change, yes.** The component has 14 core module instances (CPython, wasi-libc, its other C libraries and componentize-py's runtime), and the host's pooling allocator allows 8 per component (`max_core_instances_per_component`, `crates/paddock-host/src/runtime.rs`). Loading it fails: `The component transitively contains 14 core module instances, which exceeds the configured maximum of 8 in the pooling allocator`. The limit is hard-coded.

With the limit raised to 16 (during the spike a patch to the host; since then node config's `max_core_instances_per_component`), everything the Go app did works, with no change to the app:
- Deploy, calls, casts, a timer through `handle-info`, `register` and `whereis`. A call to its own name ends the instance with `crashed: call to self would deadlock`, as for any app.
- An uncaught exception traps (`crashed: trap: ... unreachable`). Its traceback reaches the log line by line as the guest's stderr.
- A busy loop is killed with `timeout`, and growing past `memory_max` ends with `memory_limit`.
- A snapshot upgrade keeps the count, and so does the rollback.

S21's guest-side problems do not occur here. Memory stayed at 12.4 MiB through 200,000 calls, so there is no leak, and no call paused for a garbage collection. Python's costs are size, start and memory:

| App | Size | Compile | Start p50 | Call p50 | Call p99 | Calls over 1 ms | Memory after 200,000 calls |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Python (componentize-py) | 18,008 KiB | 1,181 ms | 1,638 µs | 34 µs | 55 µs | 7 | 12.4 MiB |
| Go (S21, both fixes) | 2,806 KiB | 110 ms | 71 µs | 21 µs | 29 µs | 1 | 192 KiB |
| Rust counter (`paddock-sdk`) | 244 KiB | 45 ms | 92 µs | 25 µs | 32 µs | 0 | 1,088 KiB |

- **Start:** about 18 times Rust's, on every start, including each restart and each instance of a pool. What the 1.6 ms is spent on (14 instantiations, a 5,684-entry function table, a 12.4 MiB initial memory) was not broken down.
- **Memory:** 12.4 MiB at rest per instance, against about 1 MiB for Rust; it counts against `memory_max` (64 MiB by default), leaving about 50 MiB for the app. On a node with the `dense` memory profile (128 MiB at most per instance), about a tenth as many Python instances as Rust ones fit in the same RAM, where memory is the limit.
- **Upgrade pause:** 9.2 ms for 1.0.0 to 1.0.1 on the node (S21's Go app: 0.6 ms); the rollback 2.0 ms. One sample each.
- **Deploy:** about 1.4 s, most of it compiling (1.2 s in the bench).

Run on 2026-10-25: AMD Ryzen Threadripper 1950X (16 cores), Linux 6.8, componentize-py 0.25.1 (CPython 3.15.2). The machine was not idle: load average 3 to 4, from the spike's own builds just before. One run; only the comparison between rows is meant. Rerun on an idle machine before quoting a number elsewhere.

Other findings:
- **Imports.** The component imports `wasi:filesystem` and all of `wasi:sockets` (network, TCP, UDP, name lookup), and terminal interfaces of `wasi:cli`, though the app uses none of them. `paddock bundle` refuses it until the manifest declares both capabilities; empty declarations (no preopens, no socket rules) are enough. Without a grant, the node links the deny stubs and the app runs; `paddock status` lists both as denied.
- **WASI version.** It imports WASI 0.2.9 interfaces (S21's TinyGo build: 0.2.0); the node links both.
- **An extra export.** componentize-py adds an `exports` interface of its own beside the app world's functions. The bundle check and the node accept it.
- **No WIT work.** componentize-py builds from `wit/runtime.wit` as it is; the bindings are a `WitWorld` class with one method per export, exceptions as `Err` for `result` errors. A build takes about 2 s.
- **Pooling totals.** With `max_core_instances_per_component` at 16, `total_core_instances` (4 per instance slot) also had to grow, or a node holds only a few Python instances at once; the patch sets 16 per slot. Memories (1) and tables (2) are within their limits.

What it changes:
- The host needs a larger `max_core_instances_per_component` before Python apps can run. Done after the spike: node config's `max_core_instances_per_component` (default 8), with the pool's total at that times `max_instances`. Both are counters in wasmtime 50's pooling allocator, reserving no memory.
- DESIGN.md's risk "Guest language support beyond Rust": Python runs once the limit allows it, at about 12 MiB and 1.6 ms per instance.

## How to run

Tools: componentize-py 0.25.1 (`pip install componentize-py==0.25.1`, in a venv), `wasm-tools`, and S21's bench.

```sh
cd spikes/s22/counter
componentize-py -d ../../../wit/runtime.wit -w paddock:runtime/app bindings gen   # optional: to read the bindings
componentize-py -d ../../../wit/runtime.wit -w paddock:runtime/app componentize app -o pycounter.wasm

cd ../../..   # the checkout
(cd spikes && cargo build --release -p s21-bench)
# S21_CORE_INSTANCES sets max_core_instances_per_component; at 8, loading fails
S21_CORE_INSTANCES=16 S21_CALLS=200000 ./spikes/target/release/s21-bench spikes/s22/counter/pycounter.wasm \
    spikes/s21/counter/gocounter-free-nosched.wasm target/wasm32-wasip2/release/example_counter.wasm
```

On a node with `max_core_instances_per_component = 16` in its config: S21's manifest with the names changed to `pycounter`, plus:

```toml
# componentize-py's runtime (CPython on wasi-libc) imports both; no preopens
# and no socket rules, so it reaches neither files nor the network.
[appserver.capabilities."wasi:filesystem"]

[appserver.capabilities."wasi:sockets"]
```

## Method

- **`counter/app.py`** is the app: S21's text protocol (`inc`, `get`) and misbehaving commands (`after <ms>`, `self`, `panic` as an uncaught `RuntimeError`, `spin`, `alloc <mib>` as 1 MiB `bytearray`s), against the generated bindings. The snapshot is the count as 8 bytes.
- **The bench** is S21's (`spikes/s21/bench`): one `paddock_host::Runtime`, default config but for the core instance limit; compile, start (spawn to the first reply, 200 times), a call's round trip (`S21_CALLS` times on one instance), and the instance's memory afterwards.
- **On a node** (`paddock` release build with the limit at 16): deploy; `inc`, `get`, `after 50`, `self`; `panic`, `spin`, `alloc 20` then `alloc 40`; an upgrade to the same component as 1.0.1, a rollback, a cast.
