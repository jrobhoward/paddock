# Spike S10: can `info` grow without a new world version?

**Question.** If the host's `paddock:runtime` adds a case to `variant info`, does a guest built against the original `@0.1.0` still load?

**Answer: no.** wasmtime type-checks the guest's exports structurally against the host bindings, and a variant with one more case is a different type:

```
instantiate_pre: ok
AppPre::new (export types checked): type-checking export func `handle-info`:
  expected variant of 4 cases, found 3 cases
```

Run on 2026-10-07 with wasmtime 49.0.2 against `paddock-fixture-echo` built from `wit/runtime.wit` at `@0.1.0`. The host's copy of the WIT (`wit-extra/`) adds `node-down(u32)` to `info`.

## How to run

```sh
cargo build -p paddock-fixture-echo --target wasm32-wasip2 --release --target-dir target/fixtures   # from the repo root
cd spikes
cargo build -p s10-host --release
./target/release/s10-host ../target/fixtures/wasm32-wasip2/release/paddock_fixture_echo.wasm
```

## Findings

1. **Imports link, exports do not.** `instantiate_pre` succeeds: echo imports only `types`, whose interface type does not appear in its imports. The failure comes when the host builds typed access to `handle-info`. A guest that never imported `types` would fail the same way, because `info` is in the export's signature.
2. **Every type in an export signature is frozen for a world version.** That covers `info`, `exit-reason` (in `terminate` and inside `info`), `envelope`, `call-ref` and `call-result`. Types used only by imports were not tested; wasmtime checks host functions against the component's import types too, so the same rule is expected.
3. **A reserved case avoids it for events.** If `info` carries a generic case from `@0.1.0` on, such as `host-event(tuple<string, envelope>)`, a later host can deliver new kinds of event (role changes, key changes, node up and down) to `@0.1.0` guests as a name plus an envelope. A guest ignores names it does not know. This was not run; it follows from finding 2, since the type does not change.
4. **`exit-reason` already has an escape hatch:** `crashed(string)` carries any new abnormal reason as text.

## Effect on the design

- Before M8 freezes `paddock-sdk` against `@0.1.0`: add the reserved `host-event` case to `info` (action A1 in [`docs/REVIEW_HAPPLIANCE_OTP.md`](../../docs/REVIEW_HAPPLIANCE_OTP.md)).
- Anything else that changes an export-signature type needs `@0.2.0` and the host's per-version linker, as the WASI migration plan already describes.
