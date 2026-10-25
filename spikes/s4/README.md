# Spike S4: import inspection and deny stubs

**Question.** Can the host read a component's imports at deploy time, refuse undeclared ones, and link declared-but-ungranted imports to stubs so an app degrades instead of failing to load?

**Answer: yes, with typed deny stubs we build ourselves.** wasmtime's built-in `define_unknown_imports_as_traps` can't be used next to the WASI linker (finding 3).

Run on 2026-10-05 with wasmtime 49.0.2, wasmtime-wasi 49.0.2 (p2), wit-bindgen 0.62, rustc 1.99.

Rerun on 2026-10-25 with wasmtime 50.0.0 (wit-bindgen 0.63): the same results, except finding 3: `define_unknown_imports_as_traps` after the WASI linker now instantiates, and each missing import traps when called. Not checked on 49.0.4 (`docs/BASELINE.md`, Y1).

## How to run

```sh
cd spikes
cargo build -p s4-guest --target wasm32-wasip2 --release
cargo build -p s4-host --release
./target/release/s4-host target/wasm32-wasip2/release/s4_guest.wasm   # TRACE_SOCK=1 to trace socket checks
```

## Setup

- `wit/world.wit` has three custom capabilities that follow the Paddock error convention:
  - `kv`: errors are a variant with a `permission-denied` case
  - `net`: errors are an enum with a `permission-denied` case
  - `audit`: no error channel at all
- The guest also uses WASI through Rust std: files under `/data` and `/priv`, TCP, and environment variables.
- The host runs three deployments: everything granted; nothing granted with typed deny stubs; nothing granted with wasmtime's trap stubs.

## Results

**Deploy-time inspection.** `component.component_type().imports(&engine)` lists all 27 imports: the 3 custom interfaces plus 24 `wasi:*@0.2.12` interfaces that Rust std pulls in. A manifest declaring only `kv` and `net` is refused, naming the missing `paddock:spike4/audit@0.1.0`.

| Test | Everything granted | Nothing granted (typed stubs) |
| --- | --- | --- |
| kv | `set ok, get Some("v")` | `Err(PermissionDenied)` (variant) |
| net | `Ok("200 from ...")` | `Err(permission-denied)` (enum) |
| audit | `audit ok` | Trap: `permission denied: ...#record-event (no error channel, trapping)` |
| read `/data/hello.txt` | contents | `NotFound` (no preopens) |
| write `/data/out.txt` | `Ok`; host sees the file | `NotFound` |
| write `/priv/out.txt` (read-only preopen) | `PermissionDenied` | `NotFound` |
| `/data/../../../../etc/hostname` | `PermissionDenied` (escape blocked) | `NotFound` |
| `/etc/hostname` | `NotFound` (no ambient filesystem) | `NotFound` |
| TCP to loopback listener | `connected` | `PermissionDenied` |
| TCP to 1.1.1.1:443 | `PermissionDenied` (policy: loopback only) | `PermissionDenied` |
| environment | only what the host set (`PORT`) | same |

## Findings

1. **Import inspection is a one-liner.** `component_type().imports()` gives fully versioned names (`wasi:io/poll@0.2.12`). Function signatures are available through `ComponentItem`, so the host can also validate shapes.
2. **Typed deny stubs work generically.** For every function in an ungranted instance, `LinkerInstance::func_new` installs a stub that reads the function's type when called:
   - `result<_, E>` where `E` has a `permission-denied` case (variant or enum) → returns that error
   - `result<_, string>` → returns `"permission-denied"`
   - anything else → traps

   This makes the **error convention part of the `paddock:*` interface rules**: every capability function should return `result<_, error>` with a `permission-denied` case, so denial is a value and not a crash.
3. **`define_unknown_imports_as_traps` is unusable with WASI.** It recurses into already-linked WASI instances and redefines re-exported resources (`pollable` inside `wasi:clocks/monotonic-clock`) as new host resources. Instantiation then fails with `mismatched resource types`. Use per-interface stubs instead.
4. **WASI is restricted by configuration, not stubs.** Rust std imports 24 WASI interfaces in every component, so WASI is always linked and narrowed by `WasiCtx`:
   - Preopens with `FsPerms::ReadOnly` / `ReadWrite`. Path escapes are refused and there's no ambient filesystem.
   - `allow_tcp` / `allow_udp` / `allow_ip_name_lookup`, plus `socket_addr_check`.
   - Environment only as set; nothing is inherited.
5. **Socket policy must be per `SocketAddrUse`.** Rust std binds `0.0.0.0:0` before every `connect`. A destination-only policy ("loopback only") therefore refuses every connection. Allow ephemeral client binds (`TcpBind` on port 0) and check destinations on `TcpConnect`.
6. **wasmtime-wasi 49 API notes:** `preopened_dir(host, guest, FsPerms)` takes a single `FsPerms` (no separate dir/file perms). `SocketAddrUse` lives in `wasmtime_wasi::sockets`.

## Effect on the design

- Deploy check = `component_type().imports()` vs the manifest's `capabilities`; non-`wasi:` imports must be declared.
- Ungranted custom capabilities get typed deny stubs; WASI capabilities are narrowed through `WasiCtx`.
- Interface rule: every `paddock:*` capability function returns `result<T, error>` with a `permission-denied` case.
- The manifest's network scope needs separate bind and connect rules.
