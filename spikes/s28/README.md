# Spike S28: a mirror in the guest, kept by watches

**Question.** When does a copy of a bucket inside each reader instance, kept current by `bulkhead:watch` notices, beat reading the shared bucket? What do the copies cost in memory, warm-up, staleness and CPU, for the readers and for the writer? And how does the owner-shard shape compare: the data in a few instances' own memory, read through calls?

**Answer: a mirror reads 5 to 7 times faster than the bucket and scales with workers, but every change costs each mirror about 9 µs of CPU, and each mirror costs the writer 0.6 to 0.9 µs per write.** A mirror pays when its instance reads the data about ten times more often than anyone writes it, and N copies fit in memory. Below that ratio the bucket is cheaper. Owner shards lose to the bucket on every read measure. Build a mirror by watching first and scanning second: scan-then-resume never converged while anything in the bucket was deleted. The spike also found Q16: on a `read_concurrency` bucket, `get` and `scan` could return a value with a later write's revision, which left a mirror wrong for good (fixed, CHANGELOG).

## Method

`s28-guest` (`guest/src/lib.rs`) holds the prototype, `Mirror`:
- **Building it:** `watch_prefix` from now, then `scan` every page into a map. A change during the scan arrives as a notice afterwards, so nothing is missed and nothing can be too old.
- **Applying a notice:** `handle_info` turns the notice into a `Change`. A notice whose revision the mirror already holds is skipped. Otherwise the mirror `get`s the key and stores the value with its revision, or removes the key if it is gone.
- **The other mode, for comparison:** scan first, then `resume` from the first page's revision, scanning again on `too-old`.
- **Maps:** std's `HashMap` (SipHash), a `HashMap` with FxHash, or a `BTreeMap`.

The same guest also reads through `wasi:keyvalue` (`get`), calls owner instances (`o:<key>` to one of 4 owners picked by key hash; each owner holds its share in a `HashMap`, the pool's hash routing done by the caller), and writes stamped values (`write`: the first 8 bytes are the wall-clock time, so a reader computes lag when it applies the change).

`s28-bench` (`bench/src/main.rs`): 4 tokio workers, a memory bucket of 100,000 keys `k000000` ... with 16 B values (and one of 1 KiB values), `read_concurrency` on unless `S28_RC=0`, freed Store slots giving their pages back (`keep_resident` 0) so each memory row is its own.

Machines:
- **16-core:** AMD Ryzen Threadripper 1950X, Linux 6.8. Run on 2026-10-31 at load 0.26, before the Q16 fix, which changes no timing. The Fx rows come from a later run beside other jobs (load about 6), after a fix to the spike's FxHash (its low bits were weak, so the first run's Fx rows were wrong).
- **Raspberry Pi 5:** 4 cores, Linux 7.0. Run on 2026-10-31 at nice 10, load 0.5 at the start, after the fix.

Each machine ran `run.sh` once.

## Reads

One instance, 200,000 random keys per run, median of 5 runs, timed in the guest:

| ns per read | 16-core | Pi |
| --- | --- | --- |
| mirror, `HashMap` (SipHash) | 181 | 302 |
| mirror, `HashMap` (FxHash) | 157 (contended) | 265 |
| mirror, `BTreeMap` | 660 | 1,079 |
| bucket `get` (`read_concurrency`) | 1,182 | 1,559 |
| bucket `get` (plain memory bucket) | 1,433 | 1,549 |
| owner call, 4 owners | 6,397 | 7,938 |
| dependent reads along a chain: mirror / bucket | 163 / 983 | 220 / 1,575 |

Reads per second from 1, 4 and 16 instances at once:

| | 16-core | Pi |
| --- | --- | --- |
| mirror | 2.64M / 10.9M / 9.83M | 1.76M / 6.08M / 5.93M |
| bucket, `read_concurrency` | 0.73M / 2.39M / 2.92M | 0.56M / 1.80M / 1.82M |
| bucket, plain | 0.83M / 0.70M / 0.68M | 0.54M / 0.58M / 0.57M |
| owner calls, 4 owners | 0.17M / 0.29M / 0.35M | 0.13M / 0.38M / 0.44M |

- **A mirror read is a hash lookup in the instance's own memory:** 6.5 times faster than the bucket on the 16-core machine, 5.2 times on the Pi, and 3.3 to 4.6 times the bucket's throughput at 4 and 16 instances (both bounded by 4 workers).
- **The hasher matters little:** FxHash saves 12 to 13% over SipHash. Most of a read is a cache miss in a 10 MiB table, as for S27's index (75 ns on the 16-core machine for a table in the data segment).
- **A `BTreeMap` costs 3.6 times a hash map** (string comparisons down the tree); keep one only for ordered or prefix reads.
- **Owner calls are the slowest at every count:** a call to another instance costs 6.4 to 7.9 µs, and 16 readers on 4 owners reach 0.35 to 0.44 million reads per second. Owners suit state that one instance must own (its writes serialized with its logic), not a read path.

## Memory and warm-up

| | 16-core | Pi |
| --- | --- | --- |
| PSS per mirror, 100,000 x 16 B, `HashMap`, 1 / 16 / 100 instances | 10.4 / 10.5 / 10.5 MiB | 10.4 / 10.4 / 10.4 MiB |
| the same, `BTreeMap` | 11.3 MiB | 11.3 MiB |
| PSS per mirror, 100,000 x 1 KiB | 106 MiB | 106 MiB |
| build a mirror: 1,000 / 100,000 x 16 B | 1.2 / 170 ms | 0.9 / 228 ms |
| build: 100,000 x 16 B with `BTreeMap`; 100,000 x 1 KiB | 213 / 244 ms | 276 / 334 ms |
| build by scan-then-resume, no writer | 198 ms | 266 ms |

- **Every mirror is a full copy, charged to its instance:** about 109 bytes per entry for 16 B values and 1,110 for 1 KiB values, against the bucket's 174 and 1,194 bytes (BASELINE.md, 2026-10-30), which it keeps once for every reader. 100 mirrors of 100,000 small entries take 1 GiB. The instance's `memory_max` is charged the linear memory (11 and 106 MiB here).
- **Building a mirror costs 1.7 µs per entry on the 16-core machine and 2.3 µs on the Pi:** a scan page's copy across the sandbox plus the map insert. An instance pays it on every start and restart. Watches survive passivation and a notice wakes the instance, so a passivating mirror wakes for every change and must either carry the mirror in its snapshot (10 MiB here, per wake) or rebuild it. Neither was measured; a mirror suits instances that stay live.
- The Pi's first PSS row reported the instances' start as -68 MiB (the bench's fill instance had just exited); the mirror's own delta is unaffected.

## Readers beside a writer

One writer overwrites random keys of the 100,000 at a set rate for 5 s, paced every millisecond by the bench. Every reader mirrors all 100,000 keys:
- **Lag** is the time from the write to its application in a mirror.
- **Drain** runs from the writer's last write to the last notice applied.
- **Cores** is the process's CPU use. The "0 readers" rows give the writer alone.
- **Differ** is the number of entries in which a mirror and the bucket disagree at the end.

| rate, readers | 16-core: writes/s, notices, lag p50 / p99, drain, cores | Pi: writes/s, notices, lag p50 / p99, drain, cores |
| --- | --- | --- |
| 1,000/s, 0 | 1,000, -, -, -, 0.07 | 1,000, -, -, -, 0.07 |
| 1,000/s, 1 | 1,000, 100%, 30 / 51 µs, 14 µs, 0.09 | 1,000, 100%, 39 / 61 µs, 16 µs, 0.10 |
| 1,000/s, 4 | 1,000, 100%, 47 / 74 µs, 27 µs, 0.19 | 1,000, 100%, 48 / 86 µs, 24 µs, 0.20 |
| 1,000/s, 16 | 1,000, 100%, 92 / 187 µs, 59 µs, 0.43 | 1,000, 100%, 81 / 140 µs, 0 µs, 0.42 |
| 100,000/s, 0 | 99,973, -, -, -, 0.46 | 99,997, -, -, -, 0.48 |
| 100,000/s, 1 | 99,988, 100%, 0.86 / 8.6 ms, 1.3 ms, 1.35 | 99,983, 100%, 0.88 / 16.8 ms, 0.5 ms, 1.43 |
| 100,000/s, 4 | 87,336, 100%, 0.91 / 6.6 ms, 2.0 ms, 3.84 | 97,765, 71%, 366 / 773 ms, 460 ms, 3.76 |
| 100,000/s, 16 | 32,814, 85%, 584 / 1,245 ms, 977 ms, 3.84 | 34,836, 70%, 1,353 / 2,676 ms, 2,282 ms, 3.80 |

- **A notice costs a mirror about 9 µs of CPU:** (1.35 - 0.46) cores for 100,000 notices a second on the 16-core machine, 9.5 µs on the Pi. That is 7 to 9 times what the mirror saves on one read. System time stayed under 0.1 cores, so it is user-space work. A profile (16-core, `perf record`, one reader at 100,000 writes a second) put about 14% of all samples in the reader's `get` host call, 11% in building the notice's JSON (`Notice::take`, serde_json), and 7% in the writer's `Watch::note` and `notify`. Most of the rest is the callback machinery and the guest's JSON decode.
- **At low rates a mirror trails by tens of microseconds.**
- **When notices exceed the workers, both sides give.** 16 mirrors at 100,000 writes a second ask for 1.6 million notices a second, against about 0.44 million that 4 workers can apply. The fair scheduler slows the writer to about a third of its rate. Conflation drops 15 to 30% of the notices, since a key rewritten while its notice waits is sent once. Lag still reaches seconds, but a mirror's queue stays bounded by its keys.
- **Every mirror matched the bucket at the end, except two rows on the 16-core machine before the Q16 fix** (one entry each). That was a key whose value the mirror had read with a newer write's revision (below).

## A writer's cost

The same writer overwrites 100,000 random keys as fast as it can, timed in the guest. "Held" mirrors are kept in a call that is never answered until the writes end, so they run nothing: the writer pays only for queueing their notices.

| mirrors | 16-core: ns per write, mirrors running / held | Pi |
| --- | --- | --- |
| 0 | 2,458 | 3,841 |
| 1 | 4,694 / 3,283 | 5,363 / 4,568 |
| 16 | 24,168 / 11,783 | 29,177 / 16,916 |
| 100 | 271,652 / 75,045 | 357,432 / 96,624 |

- **Each mirror adds 0.6 to 0.9 µs to every write** (held rows), linear in the number of mirrors. `notify` tests every prefix watch on the bucket against the key, and each match takes the watch's pending map lock, allocates the key and signals the instance. With 100 held mirrors a write costs 25 to 31 times what it costs alone.
- **With the mirrors running, the writer's own callback is also delayed** by readers holding the 4 workers. That is the slowdown in the lag table seen from the writer: 100 mirrors held it to about 3,700 writes a second (16-core).

## Building a mirror while keys are deleted

A mirror of the 100,000 keys `k...` was built while another instance inserted and deleted a key `d...`, outside the mirror's prefix, 1,000 times a second:

| | 16-core | Pi |
| --- | --- | --- |
| scan, then `resume` from the scan's revision | failed after 50 rescans, 11 s: `too-old` | failed after 50 rescans, 12 s |
| watch, then scan | 202 ms | 129 ms |

`resume` refuses any revision older than the bucket's latest delete, anywhere in the bucket. A scan of 100,000 entries takes longer than a millisecond, so it never caught up. Watching first needs no replay.

## Found: a value read with another write's revision (Q16)

The lag rows first ended with a mirror that disagreed with the bucket on one key: same revision on both sides, different values. On a `read_concurrency` bucket (S26), `bulkhead:watch`'s `get` read the value and then, in a second lookup, the revision. `scan` did the same for each entry. A write between the two lookups returned the old value with the new revision. The mirror then skipped the notice for that revision as already applied, and kept the old value for good. A compare-and-swap after such a `get` would succeed against a write it never saw.

The fix (`storage.rs`): `Txn::entry` and `Txn::range_revisions`, which the live bucket serves from one load of each entry's value-and-revision cell. A threaded test (`live_bucket____entries_and_scans_beside_a_writer____each_value_with_its_own_revision`) found 9,370 torn reads without the fix and none with it. `read_concurrency` was unreleased, so the bug never shipped.

## What it changes

- **H8 (the SDKs' mirror) follows this prototype:**
  - build by watching first and then scanning, never by resuming;
  - a hash map for point reads, and a `BTreeMap` only when the app asks for ordered reads;
  - WRITING_APPS guidance on when a mirror pays: reads per instance well above ten times the writes to the mirrored keys, and N copies within memory.
- **The notice path is the cost to cut, before or with H8 (recorded in H8):**
  - a notice that carries the value and its revision would save the `get`;
  - delivering a watch's pending changes as one event per callback, in a binary body, would amortize the callback and the JSON;
  - the writer's cost per mirror would fall with an index of prefix watches.

  None of these is measured here.
- **Owner shards are not a read path** (H12's guidance).
- **Data that changes once per release** belongs in the component (S27), not in a mirror.

## How to run

```sh
spikes/s28/run.sh                     # every phase, then the reads with read_concurrency off
cd spikes && target/release/s28-bench target/wasm32-wasip2/release/s28_guest.wasm read lag
S28_LAG=100000:1:10 perf record -g target/release/s28-bench target/wasm32-wasip2/release/s28_guest.wasm lag
```

Phases: `warm`, `memory`, `read`, `lag`, `writer`, `resume`. `S28_RC=0` turns `read_concurrency` off. `S28_LAG=<rate>:<readers>:<seconds>` runs one lag row.
