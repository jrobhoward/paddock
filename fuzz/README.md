# Fuzz targets

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets, in their own workspace (libfuzzer needs nightly and sanitizer flags). CI runs each one for five minutes every night (`.github/workflows/fuzz.yml`).

| Target | Input | Checks |
| --- | --- | --- |
| `bundle_open` | arbitrary bytes as a `.rbp` file | opening fails cleanly (unreadable, corrupt or untrusted), never panics |
| `manifest` | arbitrary text as an `appserver` section | parse, validate, and turning an accepted section into an `InstanceSpec` never panic |
| `parse` | arbitrary text | pids, byte sizes, routing, port ranges, CIDRs, capability keys and import names; pids and byte sizes round-trip |
| `routing_key` | arbitrary JSON or CBOR bodies and field names | hash routing's key reader never panics |
| `wire_packet` | arbitrary bytes from a cluster member | as a hello, any agreed version is one both sides speak; as a packet, one that decodes round-trips through `encode` |
| `failover_status` | arbitrary heartbeat payloads from the partner | a status that decodes encodes to 26 bytes that decode to the same status |
| `http_reply` | arbitrary reply bodies from a guest | the HTTP reply envelope decoder never panics; an accepted reply has a valid status code |
| `state_view` | arbitrary snapshot bytes from a guest | `paddock state`'s reader never panics, names a format, and its answer serializes as JSON |
| `modbus_request` | arbitrary Modbus requests and table sizes | a read returns exactly the count asked for and a write changes only addresses the table has |
| `process_ops` | sequences of up to 64 operations on 4 live chaos instances | calls and casts with arbitrary envelopes and pids, links, monitors, exit signals, registration, timers, kills and respawns; afterwards every instance dies when killed and a fresh one answers |

```sh
cargo install cargo-fuzz
# process_ops runs the chaos fixture:
cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
cd fuzz
cargo +nightly fuzz run parse -- -max_total_time=60
```

Crashing inputs land in `artifacts/<target>/`; replay one with `cargo +nightly fuzz run <target> artifacts/<target>/<file>`.
