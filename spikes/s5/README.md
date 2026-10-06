# Spike S5: snapshot upgrade end to end

**Question.** Can we swap a running instance's component mid-life, carrying state through `save-snapshot`/`load-snapshot`, keep its identity (mailbox, registration) unchanged, revert on failure, and lose no messages?

**Answer: yes.** Three runs, all identical in outcome:
- Upgrades land while traffic is flowing and pause the instance for 0.2–0.35 ms each.
- A broken upgrade reverts with state intact.
- 4,000 of 4,000 concurrent increments are applied exactly once.

Run on 2026-10-05 with wasmtime 49.0.2, wasmtime-wasi 49.0.2 (p2), wit-bindgen 0.62, rustc 1.99, 2 tokio workers.

## How to run

```sh
cd spikes
mkdir -p target/s5
for v in v1 v2 v3; do
  cargo build -p s5-guest --target wasm32-wasip2 --release --features $v
  cp target/wasm32-wasip2/release/s5_guest.wasm target/s5/counter_$v.wasm
done
cargo build -p s5-host --release
./target/release/s5-host target/s5
```

## Setup

- **Guest** (`guest/`): one counter crate built three ways.
  - v1 keeps `count`; its snapshot is `"count"`.
  - v2 adds the last 5 increments; its snapshot is `"count;h1,h2,..."`, and it migrates from v1 snapshots (no history yet).
  - v3 is v2 with a `load-snapshot` that always fails.
- **Host** (`host/`): the instance actor owns the mailbox (the pid) and the current `(Store, App)`. Upgrades are messages in the same mailbox, so they run between callbacks; anything sent meanwhile queues.
  - Steps: `version` + `save-snapshot` on the old instance, then instantiate the new component (pre-compiled) in a fresh Store, then `load-snapshot(old-version, bytes)`, then swap. Any failure keeps the old instance.
- **Load:** 4 senders × 1,000 `inc` calls, paced at 0.5 ms, with upgrades v1 → v2, v2 → v3 (broken) and v2 → v2 at 150 ms intervals.

## Results (run 1; runs 2 and 3 match)

| Step | Outcome |
| --- | --- |
| v1 → v2 at count ≈ 390 | OK, paused 313 µs; next read `v2.0.0 count=396 history=[394, 395, 396]` (count migrated; history starts fresh, as designed) |
| v2 → v3 (broken) | Reverted: `load-snapshot refused: v3 cannot read snapshots from 2.0.0`; still v2, history intact |
| v2 → v2 | OK, paused 286 µs |
| Final | 4000/4000 acknowledged; `count=4000 history=[3996..4000]` |

## Findings

1. **Mailbox-as-identity makes upgrades simple.** Because an upgrade is just another message, "pause delivery" needs no extra mechanism: the actor isn't reading its mailbox while it upgrades. Senders keep the same `Sender` (pid) throughout and never notice the swap.
2. **Revert is free.** The old Store stays untouched until `load-snapshot` succeeds, and `save-snapshot` doesn't mutate state, so revert means "drop the new Store".
3. **The pause is small** when components are pre-compiled: 0.2–0.35 ms for instantiate + snapshot + restore of a tiny state, without the pooling allocator. Large states will cost serialization time; S2 should measure instantiation with pooling.
4. **Version handshake.** Passing the old component's `version()` into `load-snapshot` was enough for v2 to pick the right parser. The manifest's `upgrade.from_versions` can pre-check this before trying.
5. **Not covered here:** links and monitors across an upgrade. They attach to the pid (mailbox), which doesn't change, so they should be unaffected, but Phase 1's M4/M6 tests should confirm it. A guest trap *during* `save-snapshot` should be treated like any failure (revert), and since the old Store is then poisoned, the supervisor restarts it from `init`.

## Effect on the design

- Upgrade = a mailbox message handled between callbacks, as designed. No separate pause flag.
- `save-snapshot` must be side-effect free (document in the SDK; the derive macro guarantees it).
- Pre-compile v2 before sending the upgrade so compile time isn't part of the pause.
- A trap in `save-snapshot` leaves v1 poisoned: report an upgrade failure and let the supervisor restart v1.
