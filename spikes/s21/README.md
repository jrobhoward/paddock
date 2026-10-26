# Spike S21: an app in Go (TinyGo)

**Question.** DESIGN.md says Paddock hosts apps in any Component Model language, and only Rust had been tried. Does a Go app built with TinyGo and `wit-bindgen-go` run on an unmodified node: deploy, calls, casts, timers, crashes and their limits, snapshot upgrade and rollback? What does it cost next to the Rust counter?

**Answer: yes, with two fixes on the guest side and none on the host.** A TinyGo component implementing the `app` world bundles, deploys and runs on an unmodified node. Calls, casts, timers through `handle-info`, `register` and `whereis`, a supervisor restart, a snapshot upgrade and a rollback all behave as they do for a Rust app. A panic traps (`crashed: trap`) and its message reaches the log as the guest's stdout; a busy loop is killed with `timeout`; growing past `memory_max` ends with `memory_limit`.

As TinyGo builds it by default, the app is not fit to run:
- **Every message leaks.** TinyGo's `cabi_realloc` allocates with `allocManual`, which keeps a block live until it is freed. The host lowers every export argument (the envelope's content type, schema and body) through it, and the export glue `wit-bindgen-go` generates never frees them. Memory grew to 24 MiB after 200,000 calls, and the instance ended with `memory_limit` (64 MiB) before 1,000,000. The app has to free each envelope once it has copied what it keeps (`release.go`, through `//go:linkname` to `runtime.freeManual`). Results of imports (a `call`'s reply) are allocated the same way. Not checked: whether TinyGo or `wit-bindgen-go` know about this.
- **The asyncify scheduler costs pauses and start time.** TinyGo's `wasip2` target defaults to `-scheduler=asyncify`. With it, a call's p99 was 1.3 to 7 ms; garbage collections paused calls for 1 to 7 ms, every few calls with the leak fixed (a fifth of all calls over 1 ms) and less often with it. Start took 3 times as long as Rust's. `-gc=conservative` changed nothing. `-scheduler=none` removed all of it. An app is driven one callback at a time and has no use for goroutines; without a scheduler, starting one does not build.

With both fixes the Go app costs about what the Rust counter costs, except in size:

| Build | Size | Compile | Start p50 | Call p50 | Call p99 | Calls over 1 ms | Memory after 200,000 calls |
| --- | --- | --- | --- | --- | --- | --- | --- |
| TinyGo default (asyncify, leaking) | 3,975 KiB | 306 ms | 249 µs | 34 µs | 4.1 ms | 2,957 | 24 MiB |
| freeing envelopes | 3,978 KiB | 253 ms | 230 µs | 40 µs | 1.4 ms | 40,982 | 768 KiB |
| `-scheduler=none` | 2,804 KiB | 107 ms | 95 µs | 25 µs | 31 µs | 6 | 24 MiB |
| both | 2,806 KiB | 103 ms | 73 µs | 25 µs | 31 µs | 0 | 192 KiB |
| Rust counter (`paddock-sdk`) | 244 KiB | 39 ms | 85 µs | 24 µs | 30 µs | 1 | 1,088 KiB |

Run on 2026-10-25: AMD Ryzen Threadripper 1950X (16 cores), Linux 6.8. The machine was not idle: load average 2 to 3, from a TLC run and the spike's own builds. Only the comparison between rows is meant; rerun on an idle machine before quoting a number elsewhere. One run of each; the earlier runs of the default build at 20,000 calls agreed (p99 about 7 ms every time).

Other findings:
- **Imports.** TinyGo's runtime imports `wasi:cli` (environment, stdio), `wasi:clocks`, `wasi:random` and `wasi:filesystem` (types and preopens), even in an app that never touches a file. `paddock bundle` refuses the component until the manifest declares `wasi:filesystem`. An empty declaration (no preopens) is enough. Without a grant, the node links the deny stubs and the app runs; `paddock status` lists `wasi:filesystem` as denied.
- **Pooling limits.** The component has three core modules, as the Rust counter does, within `max_core_instances_per_component` (8). `wasm-tools print` counts 15 core instances, most of them export bundles that the pooling allocator does not count.
- **WIT.** TinyGo needs a world that includes `wasi:cli/imports` beside the app world (`wit/world.wit`). `wit-bindgen-go` 0.7.0 re-emits an included world's `use types.{...}` without its package and fails, so the world restates the app's exports with fully qualified `use`. The `go.bytecodealliance.org/cm` module must be pinned to v0.3.0 (what `wit-bindgen-go` 0.7.0 requires): newer tags resolve to the parent module, which lacks the package.
- **Calls to itself** end the instance with `crashed: call to self would deadlock`, as for any app.

What it changes:
- DESIGN.md's risk "Guest language support beyond Rust": Go runs, with the two fixes. A Go SDK would hide them: free each envelope after decoding it, build with `-scheduler=none`, and ship the WIT world with `wasi:cli` included.
- ROADMAP's "More guest SDKs": TinyGo is the first language beyond Rust shown to work; JS and Python are not tried.

## How to run

Tools: Go 1.27, TinyGo 0.43.0 (release tarball), `wit-bindgen-go` 0.7.0 (`go install go.bytecodealliance.org/cmd/wit-bindgen-go@v0.7.0`), `wasm-tools`.

```sh
cd spikes/s21/counter
wit-bindgen-go generate --world s21:go/app --out gen ../wit
# the fixed build; leave out -tags s21free or -scheduler=none for the others
tinygo build -target=wasip2 --wit-package ../wit --wit-world app -opt=2 \
    -tags s21free -scheduler=none -o gocounter-free-nosched.wasm .

cd ../..    # spikes/
cargo build --release -p s21-bench
S21_CALLS=200000 ./target/release/s21-bench s21/counter/*.wasm \
    ../target/wasm32-wasip2/release/example_counter.wasm
```

A TinyGo build takes about a minute. `S21_MEMORY_MAX_MIB` sets the instances' `memory_max` (default 64); the pauses did not change between 16 and 256 MiB.

On a node: `gocounter.toml` below, bundled with `paddock bundle gocounter.toml <component> --key signer.key -o go.rbp`, deployed, then `paddock call gocounter <command>`.

```toml
[app]
name = "gocounter"
version = "1.0.0"
license = "MIT OR Apache-2.0"

[appserver]
registered_names = ["gocounter"]

[appserver.lifecycle]
start_args = { register = "gocounter" }

[appserver.upgrade]
mode = "snapshot"
snapshot_version = 1

# TinyGo's runtime imports wasi:filesystem; no preopens, so it sees no files.
[appserver.capabilities."wasi:filesystem"]
```

## Method

- **`counter/`** is the app: `main.go` against the raw bindings, the counter example's text protocol (`inc`, `get`) and commands that misbehave: `after <ms>` (a timer that increments through `handle-info`), `self` (a call to its own registered name), `panic`, `spin`, `alloc <mib>`. The snapshot is the count as 8 bytes. `release.go` (build tag `s21free`) frees each envelope; `release_off.go` does nothing.
- **`wit/`**: the world TinyGo builds against. `deps/` holds the WASI 0.2.0 WIT shipped with TinyGo 0.43.0 (`lib/wasi-cli/wit`), and a link to Paddock's `wit/runtime.wit`.
- **`bench/`** loads each component on one `paddock_host::Runtime` (default config, 2 tokio workers) and measures, per component: compile (`Runtime::load`); start, from spawn to the reply of a first `get`, 200 times; a call's round trip (`inc`), `S21_CALLS` times on one instance; and the instance's memory afterwards.
- **On a node** (`paddock` release build): deploy; `inc`, `get`, `self`, `after 50`; `panic`, `spin`, `alloc 32` then `alloc 40` (and `alloc 70`); an upgrade to the same component as 1.0.1 and back by rollback, then to the fixed build as 1.1.0, rechecking the misbehaving commands on it.
