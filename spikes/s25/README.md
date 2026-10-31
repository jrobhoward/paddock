# Spike S25: where a bucket operation's microsecond goes

**Question.** A memory-bucket operation from a guest cost about 1 µs before it touched any data (BASELINE.md, 2026-10-30, "keyvalue buckets against ETS"), against 50 to 270 ns for ETS and about 250 ns for a bare host import (S14). Where does the rest go, and how far can the current path come down without a new interface?

**Answer: about 40% of a read was Paddock's own overhead, now removed; what is left is mostly the guest, the canonical ABI and the ordered map.** Three changes, kept (ROADMAP H6), took a read of a missing key from 817 to 493 ns and a read of a 16 B value from 997 to 640 ns on the 16-core machine, one worker; an overwrite of 16 B from 1,332 to 992 ns. No interface changed and no guest needs rebuilding.

## Method

`kv_cost` with `KV_ONLY=<op>:<bucket>:<keys>:<size>:<seconds>` runs one guest loop (`paddock-fixture-kvbench`) over and over in one process: `perf record -F 999 --call-graph dwarf` on the 16-core machine (AMD Ryzen Threadripper 1950X, Linux 6.8, idle), then each change measured by `measure.sh`'s median of three 8-second runs per loop, one tokio worker, the loops a missing key (`miss`), a 16 B value (`get`), 1,000 and 100,000 keys, an overwrite of 16 B and of 64 KiB (`set`), and `get_many` of 100 keys.

## Where the time went

A read of a missing key, before (817 ns), by inclusive share of samples:

| Share | Where |
| --- | --- |
| about 24% | The guest: the loop, the SDK's `Bucket::get`, the guest side of the canonical ABI |
| 9 to 11% | wasmtime's async host-function machinery and the fiber switch around it |
| about 8% | Lifting the key into a host `String` |
| about 7% | Cloning the bucket handle and building the storage key: four `String` copies a read |
| about 7% | The node-wide map of memory buckets: SipHash over the `(space, name)` key, compares, and its one lock |
| about 12% | The `BTreeMap` lookup, mostly `memcmp` of keys: the ordered map itself |
| about 3% | Bookkeeping in `apply` on a read |
| 3 to 5% | wasmtime's `validate_scope_exit` and resource borrows |
| about 30% (overlapping) | `malloc`, `free`, `memmove`, `memcpy` across the above |

A read of a value adds lowering it into the guest (about 9%, one `cabi_realloc` call into the guest per value). `get_many` of 100 spends about 40% lowering its result: `wasi:keyvalue`'s `get-many` returns `list<option<tuple<string, list<u8>>>>`, so every key comes back with its value and each is its own allocation in the guest. That is the interface's shape, not Paddock's; a batch read returning one buffer would avoid it, at the cost of a Paddock interface of its own.

## The changes, one at a time

Median ns per operation (the three runs in brackets in `measure.sh`'s output); each row adds to the one above.

| Change | miss, 1,000 | get 16 B, 1,000 | get 16 B, 100,000 | set 16 B | set 64 KiB | get_many 100, per key |
| --- | --- | --- | --- | --- | --- | --- |
| Before | 817 | 997 | 1,402 | 1,332 | 10,613 | 474 |
| E1: the bucket handle borrowed, not cloned; E4: an overwrite or delete reads the old value's length, not the value | 748 | 898 | 1,276 | 1,098 | 8,784 | 474 |
| E2: memory buckets found by namespace and name without building a key, each behind its own lock | 612 | 800 | 1,220 | 1,061 | 8,761 | 481 |
| E3: read imports bound synchronously (`get`, `exists`, `list-keys`, `get-many`, and `paddock:watch`'s `get`, `revision`, `scan`) | 493 | 640 | 1,019 | 992 | 8,666 | 476 |

- **E1 and E4** (`ServiceCtx::run`, `Txn::value_len`): the clone of the handle was two `String`s a call; an overwrite copied the old value only to learn its length, which at 64 KiB was a fifth of the operation.
- **E2** (`storage::Memory`): `RwLock<HashMap<space, HashMap<name, Arc<Mutex<MemBucket>>>>>`, looked up with `&str`. Besides the copies, it gives every memory bucket its own lock where they all shared one; a single hot bucket is still one lock (ROADMAP H7, S26).
- **E3**: reads never wait (a disk read runs from redb's page cache on the worker, as before; a replicated bucket's read is its local copy), so they need no future. Writes stay `async`: one binding serves disk buckets (written on the blocking pool) and replicated ones (written through replication). A sync host function runs inside the guest's poll, so its time counts against the callback deadline as host work that does not wait always has.

## What is left

A read of a missing key after the changes (493 ns): the guest about 30%; the `BTreeMap` lookup about 18% (inherent to an ordered bucket); lifting the key about 13%; wasmtime's resource handling about 10%; the scope check about 5%; SipHash in the bucket map about 4%. A faster hasher would save at most the last.

The whole harness, before and after, on both machines and with 4 workers, is in BASELINE.md, 2026-10-30, "a cheaper bucket operation (S25)": a read 28 to 51% cheaper on the 16-core machine and 18 to 38% on the Raspberry Pi 5, writes 13 to 41% and 9 to 17%. Against ETS `ordered_set` a read from 1,000 keys went from 5.0 to 2.6 times (16-core) and from 3.8 to 2.7 (Pi); from 100,000 keys from 2.2 to 1.5 and from 1.7 to 1.3.

## What it changes

- ROADMAP H6: done but for `watch::next` as a cursor; the harness additions (batches, dependent reads, reads per call, readers beside a writer, `persistent_term`) land with it.
- ROADMAP S26 and H7: the per-bucket lock is in; readers of one bucket still share its lock.
- Not pursued: a faster hasher (about 4%); a Paddock batch read returning one buffer (the cost is `wasi:keyvalue`'s result shape; only if an app reads many keys per call).

## Files

`measure.sh` runs the six loops three times each with `KV_ONLY`; the `perf` recordings were made with the commands above and are not kept.
