# Spike S23: an app in JavaScript (jco componentize)

**Question.** After S21 (Go) and S22 (Python), does a JavaScript app built with `jco componentize` (componentize-js, StarlingMonkey on SpiderMonkey) run on a node: deploy, calls, casts, timers, crashes and their limits, snapshot upgrade and rollback? What does it cost next to the Rust counter and S21's Go app?

**Answer: yes, on an unmodified node, once the app is given about 80 MiB of memory.** The component has 4 core module instances, within the pooling limit S22 hit. Everything S21's Go app did works with no change to the host or the app:
- Deploy, calls, casts, a timer through `handle-info`, `register` and `whereis`. A call to its own name ends the instance with `crashed: call to self would deadlock`, as for any app.
- An uncaught exception traps (`crashed: trap: ... unreachable`). Its stack trace reaches the log line by line as the guest's stderr.
- A busy loop is killed with `timeout`, and growing past `memory_max` ends with `memory_limit`.
- A snapshot upgrade keeps the count, and so does the rollback.

The one problem is memory under the default limit:
- **The heap settles near 75 MiB, above the default `memory_max` (64 MiB).** Memory grows by about 0.8 KiB per call (13 MiB after 2,000 calls, 52 MiB after 50,000) until SpiderMonkey collects. With `memory_max` at 64 MiB the instance ended with `memory_limit` before 200,000 calls, without a collection having made room. With 80, 96, 128 or 512 MiB, memory stayed between 75 and 79 MiB through 400,000 to 1,000,000 calls, so it does not leak. A JS app sets `limits.memory_max` to about 96 MiB. That fits the `dense` memory profile's 128 MiB ceiling. Not tried: SpiderMonkey's GC settings, which jco does not expose.

Its other costs are size, start, and calls about 6 times Rust's:

| App | Size | Compile | Start p50 | Call p50 | Call p99 | Calls over 1 ms | Memory after 200,000 calls |
| --- | --- | --- | --- | --- | --- | --- | --- |
| JavaScript (jco 1.39.0) | 11,869 KiB | 1,262 ms | 668 µs | 90 µs | 166 µs | 111 | 75 MiB |
| JavaScript, `--disable http` | 11,868 KiB | 1,236 ms | 651 µs | 91 µs | 168 µs | 110 | 75 MiB |
| JavaScript, `--aot` (weval; one run, 2 minutes earlier) | 15,352 KiB | 1,675 ms | 679 µs | 91 µs | 168 µs | 126 | 75 MiB |
| Go (S21, both fixes) | 2,806 KiB | 110 ms | 108 µs | 18 µs | 32 µs | 0 | 192 KiB |
| Rust counter (`paddock-sdk`) | 244 KiB | 47 ms | 75 µs | 14 µs | 22 µs | 0 | 1,088 KiB |

- **Calls:** about 90 µs at p50 whatever the build; ahead-of-time compilation (`--aot`) changes nothing for a handler this small, so the cost is outside the app's own code. Where it goes (the bindings converting envelopes, `TextEncoder` and `TextDecoder`, entering the engine) was not broken down. About one call in 1,800 took over 1 ms, presumably a collection; the longest was not recorded.
- **Start:** about 9 times Rust's, on every start.
- **Memory:** 9.1 MiB at rest per instance, and up to about 79 MiB under load.
- **Upgrade pause:** 6.0 ms for 1.0.0 to 1.0.1 on the node (S21's Go app: 0.6 ms; S22's Python: 9.2 ms); the rollback 1.5 ms. One sample each.
- **Deploy:** about 1.8 s, most of it compiling.

Run on 2026-10-25: AMD Ryzen Threadripper 1950X (16 cores), Linux 6.8, Node 24.22.1, jco 1.39.0, componentize-js 0.23.0. The machine was not idle: load average about 2, from a TLC run. One run, with `memory_max` at 128 MiB; only the comparison between rows is meant. Rust and Go ran faster here than in S21's and S22's tables (S22: Rust 25 µs at p50) under the same load, so numbers vary by about that much between runs. Rerun on an idle machine before quoting a number elsewhere.

Other findings:
- **Imports.** The component imports `wasi:http/outgoing-handler` and `wasi:http/types` (for `fetch`), and `wasi:filesystem`, though the app uses none of them. `paddock bundle` refuses it until the manifest declares `wasi:filesystem` and an HTTP capability; empty declarations (no preopens, no allow list) are enough. Without a grant, the node links the deny stubs and the app runs; `paddock status` lists both as denied. `--disable http` drops `outgoing-handler` but keeps `wasi:http/types`, which still needs an HTTP key declared, so it saves nothing.
- **WASI version.** It imports WASI 0.2.12 and `wasi:http` 0.2.10 interfaces; the node links them.
- **No WIT work.** `jco componentize` builds from `wit/runtime.wit` as it is. The app exports one function per callback (`handleCall`, ...); a `result` error is a thrown string, variants are `{ tag, val }`, `u64` values are `BigInt`. A build takes about 5 s.

What it changes:
- MANIFEST.md or WRITING_APPS.md, when a JS SDK or guide exists: a JS app needs `limits.memory_max` of about 96 MiB.
- DESIGN.md's risk "Guest language support beyond Rust": JS runs on an unmodified node, with more memory than the default.

## How to run

Tools: Node 24 with `npm install @bytecodealliance/jco@1.39.0 @bytecodealliance/componentize-js@0.23.0`, `wasm-tools`, and S21's bench.

```sh
cd spikes/s23/counter
jco guest-types ../../../wit/runtime.wit --world-name paddock:runtime/app -o types   # optional: to read the types
jco componentize app.js --wit ../../../wit/runtime.wit --world-name paddock:runtime/app -o jscounter.wasm
# variants: add `-d http`, or `--aot`

cd ../..    # spikes/
cargo build --release -p s21-bench
S21_MEMORY_MAX_MIB=128 S21_CALLS=200000 ./target/release/s21-bench s23/counter/jscounter.wasm \
    s21/counter/gocounter-free-nosched.wasm ../target/wasm32-wasip2/release/example_counter.wasm
```

On a node: S21's manifest with the names changed to `jscounter`, plus:

```toml
# SpiderMonkey's heap settles near 75 MiB before it collects (S23).
[appserver.limits]
memory_max = "96MiB"

# componentize-js imports both (fetch, and wasi-libc's files); no allow list
# and no preopens, so it reaches neither the network nor files.
[appserver.capabilities."wasi:filesystem"]

[appserver.capabilities."wasi:http/outgoing-handler"]
```

## Method

- **`counter/app.js`** is the app: S21's text protocol (`inc`, `get`) and misbehaving commands (`after <ms>`, `self`, `panic` as an uncaught `Error`, `spin`, `alloc <mib>` as 1 MiB `Uint8Array`s), importing `paddock:runtime/process@0.1.0`. The count is a `BigInt`; the snapshot is it as 8 bytes.
- **The bench** is S21's (`spikes/s21/bench`): one `paddock_host::Runtime` with the default config; compile, start (spawn to the first reply, 200 times), a call's round trip (`S21_CALLS` times on one instance), and the instance's memory afterwards. `S21_MEMORY_MAX_MIB` sets the instances' `memory_max`.
- **On a node** (an unmodified `paddock` release build): deploy; `inc`, `get`, `after 50`, `self`; `panic`, `spin`, `alloc 10` then `alloc 100`; an upgrade to the same component as 1.0.1, a rollback, a cast.
