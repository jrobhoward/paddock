# Vendored WASI proposal WIT

Paddock implements these draft interfaces in its own host. They are copied
here, not tracked upstream, so the guest ABI changes only when Paddock chooses
(spike S7).

| Directory | Package | Source |
| --- | --- | --- |
| `wasi-keyvalue/` | `wasi:keyvalue@0.2.0-draft` | `wasmtime-wasi-keyvalue` 49.0.2, `wit/deps/keyvalue` |
| `wasi-config/` | `wasi:config@0.2.0-rc.1` | `wasmtime-wasi-config` 49.0.2, `wit/deps/config` |
| `wasi-logging/` | `wasi:logging@0.1.0-draft` | WebAssembly/wasi-logging proposal, `wit/logging.wit` |
| `wasi-io/` | `wasi:io@0.2.12` | `wasmtime-wasi` 49.0.2, `src/p2/wit/deps/io.wit`; the stream types of `wit/connection` (wasmtime's own WASI host implements them) |

The files are unmodified copies; the WASI proposals and wasmtime are
Apache-2.0 WITH LLVM-exception. `wit/vendor/` is not a `deps/` directory, so
parsing `wit/` loads only `paddock:runtime`; host and guest bindings name each
vendored package explicitly.
