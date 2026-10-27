# ping-pong in other languages

The `pong` half of [`examples/ping-pong`](../src/lib.rs) written in Go, Python and JavaScript, each against `paddock:runtime/app` ([`wit/runtime.wit`](../../../wit/runtime.wit)) through its language's own Component Model tooling. Paddock ships an SDK for Rust only. These show that an app in another language runs on an unmodified node, and what each language needs to do so.

Each `pong` does what the Rust one does: `init` registers the name `pong`, and `handle-call` answers a CBOR `Ping { n }` (content type `application/cbor`, schema `ping-pong.ping.v1`) with `Pong { n: n + 1 }`. The Rust `ping` app calls it by name and monitors it. Since a message is an envelope of bytes, nothing in `ping` depends on the language `pong` is written in.

`demo.sh` runs one node with the Rust `ping` and a `pong` that starts in Rust and is upgraded in place to Go, Python and then JavaScript. After each upgrade `ping` calls the new `pong`, and its monitor sees the old instance go:

```text
== pong upgraded to python (1.2.0, 17908 KiB)
  running: pong 1.2.0
  $ paddock call ping ping:41
  pong 42
  $ paddock call ping ping:4294967295
  pong 4294967296
...
== what ping's monitor saw
  $ paddock call ping downs
  3
```

## Files

| Path | What |
| --- | --- |
| `go/main.go` | The callbacks, against the bindings `wit-bindgen-go` generates |
| `go/free.go` | Frees what the host passes to each export (below) |
| `go/cbor.go`, in `python/app.py` and `js/app.js` | The one message shape, `{"n": <unsigned>}`, encoded and decoded by hand to keep the examples free of dependencies. A larger app would use a CBOR library. |
| `go/world.wit` | The world TinyGo builds against: the app world plus `wasi:cli`'s imports |
| `python/app.py` | The callbacks, as methods of componentize-py's `WitWorld` class |
| `js/app.js` | The callbacks, as exported functions |
| `*/pong.toml` | Each version's manifest |
| `build.sh` | Builds each language whose toolchain is found, into `target/ping-pong-languages/` |
| `demo.sh` | The demo above |

## Toolchains

Tried with these versions on Linux x86-64, 2026-10-26:

| Language | Tools | Install |
| --- | --- | --- |
| Go | TinyGo 0.43.0, Go 1.27, `wit-bindgen-go` 0.7.0 | TinyGo's release tarball; `go install go.bytecodealliance.org/cmd/wit-bindgen-go@v0.7.0` |
| Python | componentize-py 0.25.1 | `pip install componentize-py==0.25.1`, in a venv |
| JavaScript | Node 24, jco 1.39.0, componentize-js 0.23.0 | `npm install @bytecodealliance/jco@1.39.0 @bytecodealliance/componentize-js@0.23.0` |

`build.sh` finds each tool on `PATH`, or through `TINYGO`, `WIT_BINDGEN_GO`, `COMPONENTIZE_PY` and `JCO`, and skips a language whose tools are missing. `demo.sh` also needs the Rust toolchain and the `wasm32-wasip2` target, as the rest of the repository does.

```sh
examples/ping-pong/languages/build.sh            # every language found
examples/ping-pong/languages/build.sh python     # one
examples/ping-pong/languages/demo.sh
```

These builds are not in CI, and `crates/paddock/tests/examples.rs` does not run them; the demo is the check. A toolchain release can change a component's imports or core module count.

## What each language needs

Found by spikes S21 to S23 ([`spikes/s21`](../../../spikes/s21/README.md), [`s22`](../../../spikes/s22/README.md), [`s23`](../../../spikes/s23/README.md)), each kept here:

| | Go (TinyGo) | Python (componentize-py) | JavaScript (jco) |
| --- | --- | --- | --- |
| WIT | A world of its own that includes `wasi:cli/imports` beside the app world, restated rather than included (`go/world.wit`) | `wit/runtime.wit` as it is | `wit/runtime.wit` as it is |
| Build flags | `-scheduler=none`: the default scheduler (asyncify) gives garbage-collection pauses of 1 to 7 ms | none | none |
| Guest code | Free every string and list the host passes to an export (`go/free.go`): TinyGo's `cabi_realloc` keeps each block live until freed, and the generated glue never frees them, so without it every message leaks until `memory_limit` | none | none |
| Manifest | Declare `wasi:filesystem` (TinyGo's runtime imports it) | Declare `wasi:filesystem` and `wasi:sockets` (CPython's runtime imports them) | Declare `wasi:filesystem` and `wasi:http/outgoing-handler`; `limits.memory_max` about 96 MiB, as SpiderMonkey's heap settles near 75 MiB before it collects |
| Node config | none | `max_core_instances_per_component` of 14 or more (the demo sets 16): the component has 14 core module instances, against a default of 8 | none |
| Component size | 744 KiB | 17.9 MiB | 11.9 MiB |

The capabilities each runtime imports are declared with no preopens, socket rules or allow list, and the demo's node grants none of them. The node links deny stubs, so each `pong` reaches no file and no network, and `paddock status pong` lists them as denied. An upgrade that adds a capability needs `paddock upgrade --approve`.

S21 to S23 measured each language's cost against the Rust counter (start, call, memory per instance) on a counter app; their tables apply here. The Go `pong` held 128 KiB through 5,000 calls from `ping` (2026-10-26), so `go/free.go` frees what it must.
