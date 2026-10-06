# Spike S6: rustbridge bundles as Paddock's deployment format

**Question.** Can Paddock use `rustbridge-bundle` as a library to read `.rbp` bundles carrying a Wasm component and an `appserver` section, and what must change upstream?

**Answer: the library is reusable, but bundle format 1.2 needs seven small rustbridge changes.** A workaround loads today but drops the `appserver` section from the typed manifest. One finding is about security: signature verification trusts a key taken from the bundle itself.

Checked 2026-10-05 against `rustbridge-bundle` 1.0.1 (crates.io, 2026-03-07). The library is about 3,700 lines; its dependencies are zip 8, minisign 0.9, sha2, serde_json and thiserror.

## How to run

```sh
cd spikes
cargo build -p s6-bundle --release
./target/release/s6-bundle target/s5/counter_v1.wasm
```

## Results

| Bundle | Result |
| --- | --- |
| A. `bundle_version: "1.2"`, platform `wasm32-wasip2`, `appserver` section | Refused: `unsupported bundle_version '1.2' (expected '1.0' or '1.1')` |
| B. Same with `bundle_version: "1.1"` | Refused: `unknown platform: wasm32-wasip2` |
| C. Workaround: 1.1, a stub `linux-x86_64` entry, the component as an extra file, `appserver` as an unknown top-level field | Opens. The typed `Manifest` drops `appserver`, but it's readable from raw `manifest.json`; component bytes are readable with `read_file` and the checksum matches |

## What blocks Wasm today (from the source)

1. `Platform` is a closed enum of six native targets, without `#[non_exhaustive]`. `Manifest::validate()` rejects any other platform key. Adding a variant breaks downstream code that matches on the enum exhaustively.
2. `validate()` accepts only bundle versions `1.0` and `1.1`.
3. `Manifest` has no `#[serde(flatten)]` catch-all, so unknown sections (`appserver`) are silently dropped on parse and lost on re-serialization.
4. Library access goes through `get_platform(Platform)` and `extract_library*` (extracts to disk). There's no in-memory "give me the verified bytes" call.
5. **Security:** `verify_manifest_signature()` uses the `public_key` *inside the manifest* unless a key override is passed. A bundle that carries its own key proves integrity, not authorship. Paddock must always call `verify_*_with_key` with operator-trusted keys.

## Proposed rustbridge changes (bundle format 1.2)

| # | Change | Why |
| --- | --- | --- |
| 1 | Add `Platform::Wasm32Wasip2` (`"wasm32-wasip2"`, extension `wasm`, rust target `wasm32-wasip2`); mark `Platform` `#[non_exhaustive]` | Lets the manifest carry components; future targets (`wasm32-wasip3`) don't break users. Semver-breaking once, so ship it in a minor bump with care or as 2.0 |
| 2 | Optional `component: { world }` on a variant | The host picks the linker by world version (see DESIGN.md, WASI migration) |
| 3 | `#[serde(flatten)] extensions: BTreeMap<String, Value>` on `Manifest`, plus a builder `with_extension(name, value)` | `appserver` (and future sections) round-trip instead of disappearing |
| 4 | Accept `bundle_version` `1.2`; 1.2 readers must keep unknown extensions | Forward compatibility |
| 5 | `verify_with_trusted_keys(&[PublicKey])`, and/or make the manifest key advisory only | Authorship comes from the operator's trust store, not the bundle |
| 6 | `read_variant_verified(platform, variant) -> Vec<u8>`: checksum (and signature) verified, in memory | Paddock compiles components from bytes; no temp files |
| 7 | `BundleBuilder::add_component(path, world)` computing the checksum | `rustbridge bundle create` can produce Paddock bundles |

## Effect on the plan

- `paddock-bundle` depends on `rustbridge-bundle` once 1.2 lands. Until then, Phase 1 can use workaround C, or a small fork, behind the same `paddock-bundle` API.
- Signature checks always use operator-trusted keys (DESIGN.md, Security model, step 1).
- These seven changes replace the bundle-format line under "rustbridge work in Phase 1".
