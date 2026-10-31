# Spike S27: reference data inside the component

**Question.** Can read-only data that changes once per release (a price list, a routing table, a dictionary) ship inside the app's component, read in place by every instance with no host call and no copy per instance? What does it cost in memory with many instances, in compile, load and start time, in an upgrade to a release with new data, and against `memory_max`?

**Answer: yes, on Paddock as it is.** The data segment is mapped copy-on-write from one image per loaded component, and every instance that only reads it shares the same physical pages: 100 instances of a 268 MiB table add 295 MiB to the process, not 26 GiB. Reads are plain memory reads; with a hash index built at compile time, a lookup in 100,000 entries takes 75 ns (49 along a chain) on the 16-core machine and 142 ns (58) on the Raspberry Pi 5, against `persistent_term`'s 238 (137) and 438 (195); 160 and 253 ns in 1.5 million. And 16 instances on 4 workers read 20 to 40 million entries a second (8.6 to 18 million on the Pi). Starting an instance does not cost more with more data (about 0.1 to 0.8 ms), and an upgrade to new data pauses an instance about 1 ms. Three things to know, one of them a bug found here and fixed (below):

- Each instance's `memory_max` is charged the whole table, shared or not.
- A release's first load on a node holds its data twice in memory that cannot be reclaimed (a compiled copy and a memfd) until the node restarts; loaded from the compile cache it is one copy of reclaimable page cache.
- An upgraded instance's task kept the old version alive until it exited, so the old release's data stayed mapped, and a passivated instance woke on the old version: the bug, fixed since (Q15).

## Method

`s27-guest`: a Rust component on `paddock-sdk` whose `build.rs` writes a table into `OUT_DIR`, embedded with `include_bytes!` as a `static`: a 16-byte header, sorted records (a 16-byte key `k0000000` ..., a 160-byte value whose first 4 bytes name another record), then a hash index (a power of two of `u32` record numbers, at least twice the records, linear probing). Not rkyv: a flat layout written by hand isolates what the platform does with the pages; an archive format is a choice for H9. Calls, each timed inside the guest with the keys made before the clock starts: `get` (random keys, binary search, the value's first 16 bytes copied out, as the ETS and `persistent_term` runs read 16 B), `chain` (each value names the next key), `hget` and `hchain` (the same through the hash index), `touch` (one byte of every 4 KiB page), `count` (state for the upgrade).

`build.sh` builds 100,000, 400,000 and 1,500,000 records (18, 71 and 268 MiB of table; components of 18, 71 and 268 MiB, the table being nearly all of each) and the 400,000-record table again with different values (version 2), without debug info. `s27-bench` runs them on one `Runtime` with the default configuration (standard memory profile, `keep_resident` 16 MiB, PAGEMAP_SCAN where the kernel has it) and 4 tokio workers, loading through `Runtime::load_cached` as a node does. Memory is the process's `/proc/self/smaps_rollup` against before the load: PSS counts a physical page once however many instances map it (RSS once per mapping, so it is shown only to make that point), split into anonymous, file-backed and shmem (memfd) pages. `run.sh` runs every case.

16-core machine (AMD Ryzen Threadripper 1950X, 16 threads, Linux 6.8, 62 GiB; load 0.75 at the start), 2026-10-31. Raspberry Pi 5 (4 cores, Linux 7.0, 7.7 GiB, swap on but unused; load 0.78 at the start, at nice 10), 2026-10-31, with the guests built on the 16-core machine.

## Memory

| Table | Instances | 16-core: PSS added (file pages) | RSS | Pi: PSS added |
| --- | --- | --- | --- | --- |
| 18 MiB | 1 / 16 / 100 | 40 / 41 / 45 MiB (25) | 40 MiB / 308 MiB / 1.8 GiB | 32 / 32 / 34 MiB |
| 71 MiB | 1 / 16 / 100 | 93 / 94 / 98 MiB (78) | 94 MiB / 1.1 GiB / 7.0 GiB | 88 / 88 / 90 MiB |
| 268 MiB | 1 / 16 / 100 | 291 / 291 / 295 MiB (275) | 291 MiB / 4.2 GiB / 26 GiB | 283 / 283 / 287 MiB |

Each instance has read every page of the table. A process that loads the component from an existing cache, never having compiled it, adds 20, 73 and 270 MiB with one instance, all of it file pages: the table once, plus code and the instance. The other 15 MiB (Pi: 9 to 11) of anonymous memory in the rows above is what compiling left with the allocator.

- **Unwritten image pages stay shared.** 99 more instances add 4 MiB at every size (Pi: 2 to 4): their page tables, stacks and the few heap pages each wrote. The image is the `.cwasm` file in the compile cache, mapped privately, so the shared pages are page cache: clean, and the kernel can reclaim them under pressure and read them back.
- **Slot reuse keeps the mapping.** A freed slot keeps the image mapped for the next instance of the same component (wasmtime's slot affinity, and PAGEMAP_SCAN resets only the pages the instance wrote), so on the 16-core machine 100 instances started again on the same slots read every page in 66 µs, 224 µs and 0.64 ms, against 1.1, 6 and 18.6 ms for the first instance, which faults the pages in. On the Pi the first instance is faster (0.46, 1.7 and 7.7 ms) and a reused slot no faster with the cache file's image (0.39, 1.5 and 6.6 ms), though it is with a memfd image (0.49 against 2.9 ms at 71 MiB); not attributed.
- **Without PAGEMAP_SCAN** (Linux before 6.7), wasmtime resets a freed slot's first `keep_resident` bytes by copying the image over them, which makes up to 16 MiB of the table private to each slot that has run an instance. Read from wasmtime 50's `cow.rs`, not measured: both machines have the call.
- **The heap is separate.** The 50 to 70 MiB of anonymous memory that appears after the read-speed runs is the instances' heaps (the keys each `get` call builds), kept zeroed in free slots up to `keep_resident`. `release_idle_memory` gives back nothing on Linux 6.8 (ROADMAP, "Pool memory"); on the Pi (Linux 7.0) it released 35 to 50 MiB, and the free slots' mappings of the old version's image with it (below).

## Load, start and charge

| Table | Compile and write the cache | Load from the cache | Start, p50 | Read every page, first instance | `memory_bytes` |
| --- | --- | --- | --- | --- | --- |
| 18 MiB | 130 ms | 0.49 ms | 0.14 to 0.41 ms | 1.1 ms | 18 MiB |
| 71 MiB | 385 ms | 0.55 ms | 0.11 to 0.36 ms | 6.1 ms | 72 MiB |
| 268 MiB | 1.32 s | 0.59 ms | 0.13 to 0.45 ms | 18.6 ms | 268 MiB |

16-core machine. On the Pi: compile and write the cache 165 ms, 488 ms and 1.71 s (153 ms without the cache at 71 MiB); load from the cache 0.73 to 0.96 ms, 4 to 5 ms in a process that never compiled; start p50 0.10 to 0.83 ms; read every page, first instance, 0.46, 1.7 and 7.7 ms; the same `memory_bytes`.

- **Compiling costs little; writing the cache most of it.** Without the cache the 71 MiB component compiles in 138 ms; with it, 385 ms, the rest writing and syncing the file. Just after compiling, the process holds about twice the data in anonymous memory (about 145 MiB at 71 MiB), half of it the compiled component.
- **Loading from the cache maps the file** (0.4 to 0.7 ms at every size), and starting an instance maps the image into its slot: neither copies the data.
- **`memory_max` is charged the whole table.** The table is part of the instance's linear memory, so `memory_bytes` (and `ps`) shows it for every instance and the instance's `memory_max` must cover it plus stack and heap. The standard profile's limit (4 GiB) leaves room; the dense profile's 128 MiB does not for tables near that size.

## Reads

| Table | `get`, binary search | `chain`, binary search | `hget`, hash index | `hchain`, hash index | 1 / 4 / 16 instances, `hget`, reads/s |
| --- | --- | --- | --- | --- | --- |
| 18 MiB (100,000 records) | 325 ns | 226 ns | 75 ns | 49 ns | 11.7M / 32.1M / 39.6M |
| 71 MiB (400,000) | 533 ns | 347 ns | 135 ns | 68 ns | 6.7M / 20.1M / 23.4M |
| 268 MiB (1,500,000) | 737 ns | 606 ns | 160 ns | 81 ns | 5.8M / 17.4M / 20.1M |

Median of 5 runs of 200,000 reads in one instance; the throughput is 4 workers. 16-core machine. On the Pi:

| Table | `get` | `chain` | `hget` | `hchain` | 1 / 4 / 16 instances, `hget`, reads/s |
| --- | --- | --- | --- | --- | --- |
| 18 MiB | 487 ns | 321 ns | 142 ns | 58 ns | 6.2M / 18.7M / 18.3M |
| 71 MiB | 698 ns | 657 ns | 215 ns | 113 ns | 4.1M / 12.5M / 11.8M |
| 268 MiB | 985 ns | 978 ns | 253 ns | 196 ns | 3.5M / 8.8M / 8.6M |

On the same machines (BASELINE.md, 2026-10-30, "a cheaper bucket operation (S25)"), with 100,000 keys and 16 B values: `persistent_term` 238 ns a read and 137 ns along a chain on the 16-core machine, 438 and 195 ns on the Pi, and 16.4 and 8.4 million reads a second from 16 processes on 4 schedulers; ETS `ordered_set` 401 and 727 ns along a chain; a memory bucket about 1 and 1.5 µs a read, 961 ns and 1.5 µs along a chain.

- **The layout decides the speed.** A binary search over 176-byte records misses the cache at every step and is slower than `persistent_term`'s hash map; a hash index built at compile time is two cache misses, about 3 times faster than `persistent_term` at 100,000 entries on both machines. Nothing in the platform is in the way of either.
- **Readers scale with workers,** with nothing shared but read-only pages: 2.5 to 3.4 times from 1 to 4 instances on 4 workers; 16 instances on the Pi's 4 cores add nothing.

## Upgrade to a release with new data

100 instances of the 71 MiB table, each upgraded in turn to the version 2 build (another 71 MiB of different data) through `save-snapshot` and `load-snapshot`; the counter in the snapshot arrived intact, and every instance read version 2's table afterwards.

| | Pause, first / p50 / max | PSS after | Of which |
| --- | --- | --- | --- |
| Version 2 read back from the cache (a node after a restart) | 1.6 / 1.05 / 4.1 ms | +224 MiB | both tables as file pages (150) |
| Version 2 as compiled (a node's first deploy of it) | 44.8 / 1.08 / 44.8 ms | +295 MiB | version 1 as file pages (78), version 2 twice: compiled code and data (anonymous, 71) and a memfd image (71) |

16-core machine. On the Pi: pauses 0.95 / 0.80 / 1.57 ms from the cache and 51 / 0.68 / 51 ms as compiled, with the same memory; there `release_idle_memory` then unmapped version 1 from the free slots (PSS +214 to +95 MiB, from the cache), which it does not on Linux 6.8.

- **A release's first load holds its data twice, unreclaimably.** On a cache miss `compiled::component` keeps the component it compiled, whose code image holds the data, and wasmtime builds a memfd image from it on the first instantiation (the 45 ms first pause, and the first start's 45 ms without the cache). Reading back the file just written instead (`deserialize_file`, under a millisecond) would make every load the file-backed case. Not changed here (ROADMAP H9).
- **The old version stayed until each instance exited.** Dropping version 1's `LoadedApp` freed nothing: each instance's task kept the `LoadedApp` it started with for its whole life (`InstanceTask::run`). Once the instances exited, wasmtime unmapped version 1 from every slot (`purge_module` when the module is dropped), and dropping version 2 freed its pages too. The same field was the bug below; with its fix, dropping version 1's `LoadedApp` after the upgrade frees its 71 MiB at once (PSS +224 to +153 MiB) while the instances run version 2.

## Found: a passivated instance woke on the version it started with

`InstanceTask::lifecycle` instantiated from the `app` it was started with when an instance woke from passivation, and `upgrade` replaced the Store, the spec and the version but not that `app`. So an instance that was upgraded and then passivated woke on the old component with the new version's snapshot: checked with the counter fixture (upgraded from 1.0.0 to 2.0.0, a `get` answered `2.0.0 count=2 history=` live and `1.0.0 count=2` after a passivation), while its version still read 2.0.0. A node's release upgrades instances the same way (`apps.rs`). Fixed on 2026-10-31 as ROADMAP Q15: the task holds its current `LoadedApp`, which a successful upgrade replaces; tests in `paddock-host`'s `tests/passivation.rs` and `tests/upgrade.rs`.

## What it changes

- H9 can proceed: data in the component needs nothing new from the host. What H9 should carry: a build-time helper that writes an indexed table (or an archive format with a hash index), an SDK accessor, guidance that `memory_max` counts the data, and reading the cache file back after compiling.
- Q15, the wake-after-upgrade bug, fixed (CHANGELOG).

## How to run

```sh
cd spikes
s27/build.sh                  # the guests into target/s27/, and s27-bench
s27/run.sh                    # every case; on an idle machine
target/release/s27-bench target/s27/guest-400000-v1.wasm [target/s27/guest-400000-v2.wasm]
```

`S27_MEMFD=1` loads without the compile cache; `S27_REUSE=1` keeps the cache of the previous run, so the first load reads it; `S27_V2_FIRST=1` upgrades to version 2 as compiled. The cache goes in `target/s27-cache` (`S27_CACHE`).
