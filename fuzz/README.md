# Fuzz targets

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets, in their own workspace (libfuzzer needs nightly and sanitizer flags). CI runs each one for five minutes every night (`.github/workflows/fuzz.yml`).

| Target | Input | Checks |
| --- | --- | --- |
| `bundle_open` | arbitrary bytes as a `.rbp` file | opening fails cleanly (unreadable, corrupt or untrusted), never panics |
| `manifest` | arbitrary text as an `appserver` section | parse, validate, and turning an accepted section into an `InstanceSpec` never panic |
| `parse` | arbitrary text | pids, byte sizes, routing, port ranges, CIDRs, capability keys and import names; pids and byte sizes round-trip |
| `routing_key` | arbitrary JSON or CBOR bodies and field names | hash routing's key reader never panics |
| `process_ops` | sequences of up to 64 operations on 4 live chaos instances | calls and casts with arbitrary envelopes and pids, links, monitors, exit signals, registration, timers, kills and respawns; afterwards every instance dies when killed and a fresh one answers |

```sh
cargo install cargo-fuzz
# process_ops runs the chaos fixture:
cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
cd fuzz
cargo +nightly fuzz run parse -- -max_total_time=60
```

Crashing inputs land in `artifacts/<target>/`; replay one with `cargo +nightly fuzz run <target> artifacts/<target>/<file>`.
