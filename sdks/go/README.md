# Bulkhead Go SDK

Apps for Bulkhead in Go, built with TinyGo into WASI 0.2 components. The SDK covers what the Rust SDK (`crates/bulkhead-sdk`) covers for an app: the callbacks, the process functions, envelopes with CBOR and JSON, snapshots in GUEST_FORMATS.md's format, HTTP requests from the ingress, and the services. It passes the conformance suite (`crates/bulkhead-host/tests/conformance.rs`) as the Rust SDK does.

The SDK lives in this repository and is not published. An app's `go.mod` points at it with a `replace`:

```go
require github.com/jrobhoward/bulkhead/sdks/go v0.0.0

replace github.com/jrobhoward/bulkhead/sdks/go => ../path/to/bulkhead/sdks/go
```

## An app

```go
package main

import bulkhead "github.com/jrobhoward/bulkhead/sdks/go"

type Greeter struct{ greeted int }

func (g *Greeter) HandleCall(ctx *bulkhead.Ctx, call bulkhead.CallRef, msg bulkhead.Envelope) bulkhead.Reply {
	g.greeted++
	name, _ := msg.AsText()
	return bulkhead.Now(bulkhead.Text("hello, " + name))
}

func init() {
	bulkhead.Export(bulkhead.Def{
		Init: func(*bulkhead.Ctx, []byte) (bulkhead.App, error) { return &Greeter{}, nil },
	})
}

func main() {}
```

[`examples/board-go`](../../examples/board-go/main.go) is a full app on the SDK: the tutorial's board (HTTP, a shared bucket, a watcher, a hot standby through process groups), run by the same node tests as the Rust board (`crates/bulkhead/tests/board.rs`).

- **Callbacks.** An app's state implements `App` (`HandleCall`), and any of `CastHandler`, `InfoHandler`, `Terminator`, `HealthChecker` and `Snapshotter`. `Def.Init` builds it; `Def.LoadSnapshot` builds it from a snapshot in an upgrade. The host calls one callback at a time.
- **Replies.** `bulkhead.Now(msg)` answers at once; `bulkhead.Later()` answers later with `ctx.Reply(call, msg)`. A `CallRef` is a number, so it can go in a snapshot.
- **Memory.** The SDK copies everything the host passes and frees the original (spike S21: TinyGo's allocator keeps it otherwise), so an app keeps what it is given.
- **Snapshots.** `bulkhead.EncodeSnapshot(version, state)` and `bulkhead.LoadSnapshot(bytes, version, &state)` write and read `[version, state]` as the Rust SDK does, so state can cross between the two languages in an upgrade when both read the same shape. Give state structs `cbor:"name"` tags; fields are written in their order.
- **JSON.** `bulkhead.MarshalJSON`, behind `bulkhead.JSON` and `http.JSON`, writes what serde does: no HTML escaping of `<`, `>` and `&` (encoding/json escapes them by default), so a reply's bytes match the Rust SDK's.
- **Log level.** The SDK handles the `log-level` host event; the `log` package skips records below the level before formatting them.

| Package | What | Capability |
| --- | --- | --- |
| `bulkhead` | Callbacks, `Ctx` (call, cast, reply, links, monitors, timers, names, exit signals), envelopes, snapshots | |
| `http` | Requests from the HTTP ingress and their responses | (routes in the node's config) |
| `kv` | Keyvalue buckets | `wasi:keyvalue` |
| `watch` | Revisions, compare-and-swap, watches and their change notices, and scans in key order (`Scan`, `First`, `Last`, `Next`, `Prev`) | `wasi:keyvalue` |
| `log` | Log records | `wasi:logging` |
| `config` | The app's configuration | `wasi:config` |
| `secrets` | Secrets the operator binds | `bulkhead:secrets` |
| `metrics` | Counters, gauges and histograms | `bulkhead:metrics` |
| `groups` | Process groups and publish | `bulkhead:groups` |
| `sql` | SQL datasources | `bulkhead:sql` |

## Building

```sh
sdks/go/build.sh <app-package-dir> <out.wasm>
```

It runs TinyGo with the SDK's WIT world (`wit/world.wit`) and `-scheduler=none`: TinyGo's default scheduler gave garbage-collection pauses of 1 to 7 ms per call (S21), and an app has no use for goroutines. `TINYGO` names the TinyGo binary if it is not on `PATH`.

[`template/bulkhead.toml`](template/bulkhead.toml) is a manifest to start from. TinyGo's runtime imports `wasi:filesystem` even when the app opens no file, so the manifest declares it, with no preopens; ungranted, the node links deny stubs and the app sees no files.

Then bundle and deploy as for any app: `bulkhead bundle bulkhead.toml app.wasm --key signer.key -o app.rbp`, `bulkhead deploy app.rbp`.

## Toolchain

Tried on Linux x86-64, 2026-10-30:

| Tool | Version | Install |
| --- | --- | --- |
| TinyGo | 0.43.0 | the release tarball |
| Go | 1.27 | what TinyGo 0.43.0 builds with |
| `wit-bindgen-go` | 0.7.0 | `go install go.bytecodealliance.org/cmd/wit-bindgen-go@v0.7.0`; only to regenerate the bindings |

Dependencies, in `go.mod`: `github.com/fxamacker/cbor/v2` 2.10.0 (MIT) and its `github.com/x448/float16` (MIT), and `go.bytecodealliance.org/cm` 0.3.0 (Apache-2.0), the version `wit-bindgen-go` 0.7.0 generates for. Each licence was checked by hand against `deny.toml`'s list on 2026-10-30.

## Layout

| Path | What |
| --- | --- |
| `*.go` | The `bulkhead` package |
| `http/`, `kv/`, `watch/`, `log/`, `config/`, `secrets/`, `metrics/`, `groups/`, `sql/` | The services |
| `internal/abi` | Copying and freeing what the host passes |
| `internal/gen` | Bindings generated by `wit-bindgen-go`, committed |
| `wit/world.wit` | The world apps build against: the app world, the services' interfaces and `wasi:cli`'s imports |
| `wit/deps/` | `wasi:cli` and the WASI 0.2.0 packages it uses, copied from TinyGo 0.43.0's `lib/wasi-cli/wit` (Apache-2.0 WITH LLVM-exception); the others are links into the repository's `wit/` |
| `conformance/` | The SDK's conformance app |
| `template/bulkhead.toml` | A manifest to start from |
| `build.sh`, `generate.sh` | Build an app; regenerate `internal/gen` |

`generate.sh` runs `wit-bindgen-go` twice: version 0.7.0 ignores `--world` once `bulkhead:sql` is among the packages and fails, so `bulkhead:sql` is generated from a world of its own.

## Tests

```sh
cd sdks/go && go vet ./... && go test ./...      # the formats, against GUEST_FORMATS.md's examples
sdks/go/build.sh sdks/go/conformance target/sdks-go/conformance.wasm
BULKHEAD_CONFORMANCE_APP=target/sdks-go/conformance.wasm cargo test -p bulkhead-host --test conformance
```

`go test` runs natively; the packages compile for the host, and the tests touch no host function. Not in CI; the Go SDK is no longer planned and goes with WebAssembly (ROADMAP Z9).
