# Spike S26: reads of shared buckets without a lock

**Question.** After S25, one bucket served 0.5 to 0.7 million operations per second however many instances read it: every operation took the bucket's lock (and, on the way, the node-wide map's lock and the bucket's reference count). What structure lets readers scale with cores, and what do writers and single reads pay for it?

**Answer: a concurrent B+-tree with lock-free reads (`scc::TreeIndex`), each entry's value swapped in place, triples reads under many readers on both machines; it costs single reads 0 to 22%, writes 13 to 54% and a hot key's writes 19 to 51%.** That is the trade ETS's `read_concurrency` makes, and the measurements argue for the same answer ETS gives: an option per bucket, off by default.

## The structures, outside Paddock

`s26-bench` (this directory): one map of 100,000 keys (`k000000` ...) with 16 B values, read by 1 to N threads (a read copies the value out, as the keyvalue host does), with no writer, with one thread overwriting random keys as fast as it can, or with one inserting a new key and deleting it again; reads and writes per second, and reads that found no value (an overwrite must never hide a key). 2 s per row. 16-core machine (AMD Ryzen Threadripper 1950X, 16 threads, Linux 6.8; load 0.94 at the start), Raspberry Pi 5 (4 cores, Linux 7.0; at nice 10, 0.5 to 2 from the builds just before).

| Structure (all MIT or Apache-2.0) | Reads/s, 1 / 4 / 16 readers, no writer | With an overwriting writer: reads/s, writes/s, 4 / 16 readers | With inserts and deletes: reads/s, writes/s, 4 / 16 readers | Misses |
| --- | --- | --- | --- | --- |
| `Mutex<BTreeMap>` (S25) | 1.81M / 0.92M / 0.94M | 0.68M, 171k / 0.83M, 50k | 0.72M, 219k / 0.84M, 65k | none |
| `RwLock<BTreeMap>` | 1.74M / 5.62M / 8.33M | 0.52M, 228k / 4.37M, 39k | 0.63M, 426k / 5.22M, 31k | none |
| `ShardedLock<BTreeMap>` (crossbeam: a reader count per thread, as `read_concurrency`) | 1.79M / 7.07M / 24.0M | 1.33M, 75k / 8.47M, 21k | 1.45M, 98k / 8.29M, 23k | none |
| `SkipMap` (crossbeam, lock-free) | 0.97M / 3.46M / 12.8M | 2.56M, 323k / 9.08M, 224k | 2.87M, 1.28M / 9.85M, 791k | **20 and 82**: an overwrite removes and re-adds the key |
| Copy-on-write `ArcSwap<BTreeMap>` | 1.79M / 5.57M / 26.3M | 6.15M, 31 / 18.7M, 14 (a write clones 100,000 entries) | not run | none |
| `SkipMap`, values swapped in place | 0.88M / 3.18M / 11.6M | 3.09M, 300k / 10.8M, 203k | 2.44M, 639k / 8.18M, 552k | none |
| `TreeIndex` (scc), values swapped in place | 1.17M / 4.41M / 16.9M | 4.35M, 341k / 15.3M, 240k | 4.48M, 793k / 15.9M, 461k | none |
| `ShardedLock<BTreeMap>`, values swapped in place (overwrites under the read lock) | 1.49M / 6.00M / 20.5M | 5.75M, 359k / 19.1M, 251k | 0.76M, 106k / 10.9M, **12k** | none |

On the Pi (1 / 4 readers): `Mutex` 1.12M / 0.76M; `TreeIndex` swapped 0.96M / 3.03M, writes 376k beside 4 readers overwriting and 859k inserting; `ShardedLock` swapped 1.02M / 3.01M, but 65k writes beside 4 readers inserting.

- **Every lock-based map starves its writer** once readers keep it busy: a writer needs every reader out, and readers keep coming back. `ShardedLock` is the extreme (21k writes/s beside 16 readers), because its writer takes every shard.
- **A lock-free map must not remove a key to replace it.** `SkipMap`'s insert of an existing key removes the old entry and adds a new one, and readers fell between the two. Holding each value in a cell swapped in place (`ArcSwap`, or scc's epoch-reclaimed `AtomicShared`) fixed it for every structure.
- **`ShardedLock` with swapped values is fastest when writers only overwrite** (overwrites take a read lock), but inserts and deletes still take every shard, and the writer starves again.
- **`TreeIndex` with swapped values keeps both sides moving** in every mix: reads scale almost linearly, writers keep 0.24 to 0.86 million per second, inserts and deletes included. Its single read is about two-thirds of a `BTreeMap`'s here (native, cache misses on 100,000 keys).
- `TreeIndex` and its crates (`sdd`, `saa`) are Apache-2.0; `im`, `imbl` and `rpds`, the persistent maps that would make copy-on-write writes cheap, are MPL-2.0, which `deny.toml` excludes.

## Inside Paddock

`LiveBucket` (`crates/paddock-host/src/services/keyvalue/storage.rs`): the entries in a `TreeIndex<String, AtomicShared<(Vec<u8>, u64)>>`, read with no lock, each value and its revision swapped whole; writers serialize on a mutex per bucket, which keeps the revision and the size bookkeeping in step (the transaction logic, `apply`, is unchanged). The node's map of memory buckets became an `ArcSwap` swapped whole when a bucket is first used, so finding a bucket writes no shared memory. Tested against the `BTreeMap` model with the scan property test and a threaded test that no read misses a key while a writer overwrites, inserts and deletes beside it.

`scripts/bench.sh kv` with the live bucket against the run with S25's `Mutex<BTreeMap>` the same day (BASELINE.md, "a cheaper bucket operation (S25)"); 4 workers; one suite run each; the 16-core machine at load 1.15 at the start, the Pi at 0.46.

| | 16-core: S25 | live bucket | Pi: S25 | live bucket |
| --- | --- | --- | --- | --- |
| get, 1,000 / 100,000 keys, ns | 627 / 1,015 | 706 / 1,237 | 866 / 1,568 | 867 / 1,543 |
| missing key, 1,000 / 100,000 keys, ns | 493 / 621 | 446 / 513 | 639 / 820 | 578 / 673 |
| overwrite, 1,000 / 100,000 keys, ns | 984 / 1,520 | 1,164 / 1,908 | 1,300 / 2,153 | 1,495 / 2,743 |
| increment, 1,000 keys, ns | 857 | 1,123 | 1,118 | 1,496 |
| 100 entries from a key, 100,000 keys, ns per entry | 581 | 662 | 808 | 978 |
| 95 reads in 100, 1 / 4 / 16 instances, ops/s | 0.62M / 0.68M / 0.67M | 0.70M / 2.07M / 2.47M | 0.51M / 0.56M / 0.54M | 0.52M / 1.52M / 1.69M |
| 50 reads in 100, 16 instances | 0.52M | 0.60M | 0.43M | 0.57M |
| every write to one key, 16 instances | 0.71M | 0.50M | 1.40M | 0.69M |
| 16 readers beside a writer: reads/s, writes/s | 0.63M, 0.20M | 2.26M, 0.24M | 0.50M, 0.15M | 1.57M, 0.31M |

- **Reads scale 3 to 3.7 times** with 4 and 16 instances on the 16-core machine, 2.7 to 3.1 times on the Pi, and a writer beside them is faster too: it no longer queues behind readers for the bucket's lock.
- **A single read costs 13 to 22% more on the 16-core machine and the same on the Pi; a missing key 10 to 18% less** (the tree finds the gap faster).
- **Writes cost 13 to 40% more on the 16-core machine and 15 to 54% on the Pi,** and one hot key's writes halve on the Pi: a write allocates a new cell and retires the old through the tree's epochs, where the `BTreeMap` wrote in place under its lock.
- **Scans cost up to 31% more per entry** (6 to 31% in seven of the eight scan rows; one 1% less).
- **Semantics change.** Readers no longer exclude writers, so a reader can see part of a `set-many` (which `wasi:keyvalue` does not promise to be atomic), and a scan's page holds each entry as of its own revision rather than one revision for the page (ETS's `select` and `foldl` behave the same). A page's `revision` stays the bucket's revision when the page began, so a watch resumed from it still misses nothing; it may repeat a change the page already showed.

## What it changes

- `read_concurrency` on memory buckets, in the manifest and in node config, off by default (decided with the user, 2026-10-30): a bucket is created as a `MemBucket` behind its lock or as a `LiveBucket` when first used. The node's bucket map stays an `ArcSwap` for both.
- Writers stay serialized per bucket. Concurrent writers (ETS's `write_concurrency`) would need the revision and the quota check without one lock; not attempted (ROADMAP H7).
- `bench.sh kv-rc` runs the harness with every bucket `read_concurrency`.

## How to run

```sh
cd spikes && cargo build --release -p s26-bench
target/release/s26-bench [max readers] [seconds per row]
S26_ONLY=treeswap,mutex target/release/s26-bench 16 2
```
