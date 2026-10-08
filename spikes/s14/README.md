# Spike S14: storage inside the guest versus host services

**Question.** What does it cost a guest to use a platform service (here, storage) through a host import, compared with native code, and compared with running the database inside the guest? Can a database such as redb run as WebAssembly at all?

**Answer.**
- **redb runs inside a guest** (it builds for `wasm32-wasip2`, in memory or on a file through WASI). In memory it costs 1.0–1.5x native, like other compute.
- **Crossing into the host is cheap:** about 140 ns for an empty host import, and about 1.2 µs more than native for a small read through the keyvalue service, once the host stops handing every read to another thread (below). Copying values across the boundary is about 10 µs per 64 KiB when read and 22 µs per 64 KiB when written, through Paddock's whole path.
- **What dominates is not the sandbox:**
  - fsync: about 1–3 ms per durable write, native or not.
  - The keyvalue service's hop to tokio's blocking pool on every operation: about 20 µs per read today.
  - WASI file I/O for a database inside the guest: unsynced writes 4–6x native.

Run on 2026-10-07: AMD Ryzen Threadripper 1950X, Linux 6.8 x86_64, ext4 on NVMe, rustc 1.99.0, wasmtime 49.0.2 through `paddock-host` (1 tokio worker, 2 ms epoch tick), redb 3.1.3, release builds.

## How to run

```sh
cd spikes
cargo build --release -p s14-guest --target wasm32-wasip2
cargo build --release -p s14-bench
./target/release/s14-bench target/wasm32-wasip2/release/s14_guest.wasm
```

## Method

- `guest/` is a Paddock app (`paddock-sdk` with the `keyvalue` feature, plus redb). Each command runs N operations and times them inside the guest; `bench/` divides by N.
- One put or one get is one transaction, as in the host's keyvalue service. Puts write key `i`, gets read it back.
- **Native redb:** the same loop in the bench process. **Guest redb:** the loop inside the guest; `mem` is redb's in-memory backend in linear memory, `file` a redb file under `/data` (a preopened host directory) through WASI.
- **Host kv:** the guest calls the keyvalue service through `paddock_sdk::kv`. `mem` buckets are a map in the host; `disk` buckets are native redb in the host (one shared file, default durability, an fsync per commit).
- **fsync** columns use redb's default durability; **nosync** uses `Durability::None`.
- Two runs, consistent to a few percent; the second is shown.

## Results

Host import round trip from the guest, `process.self` (no work, no data): **142–144 ns**.

| Value | Op | Native redb mem | Guest redb mem | Guest → host kv mem | Native redb file nosync | Guest redb file nosync | Native redb file fsync | Guest redb file fsync | Guest → host kv disk (fsync) |
|---|---|---|---|---|---|---|---|---|---|
| 16 B | put | 27.5 µs | 42.2 µs | 1.49 µs | 39.9 µs | 0.21 ms | 0.99 ms | 2.93 ms | 0.99 ms |
| 16 B | get | 1.63 µs | 2.40 µs | 1.14 µs | 1.35 µs | 2.59 µs | 1.08 µs | 2.26 µs | 24.6 µs |
| 1 KiB | put | 40.6 µs | 61.6 µs | 1.84 µs | 48.3 µs | 0.30 ms | 1.47 ms | 2.91 ms | 1.48 ms |
| 1 KiB | get | 2.90 µs | 3.00 µs | 1.36 µs | 2.02 µs | 11.9 µs | 2.42 µs | 2.35 µs | 29.8 µs |
| 64 KiB | put | 0.31 ms | 0.40 ms | 24.0 µs | 0.15 ms | 0.48 ms | 3.03 ms | 3.38 ms | 3.10 ms |
| 64 KiB | get | 21.0 µs | 11.2 µs | 11.1 µs | 52.3 µs | 2.79 µs | 41.1 µs | 2.74 µs | 0.12 ms |

Experiment: the keyvalue service runs every `disk` operation on tokio's blocking pool. With read-only operations run inline on the worker instead (a local change, measured once and reverted), host kv disk gets took **2.33 µs** (16 B), **3.92 µs** (1 KiB) and **50.6 µs** (64 KiB), against native redb's 1.11, 2.47 and 41.5 µs in the same run.

## Reading the results

- **A host call costs about 140 ns** with nothing to do. A small read through the whole keyvalue path (SDK wrapper, canonical ABI, service, resource table, redb, value copied back into the guest) costs about 1.2 µs more than the same read natively, once the blocking-pool hop is gone. Against any real I/O this is noise.
- **Copies.** A 64 KiB value read from a host `mem` bucket costs about 10 µs more than a 16-byte one, and a 64 KiB write about 22 µs more. That covers every copy on the path: out of the host map, into guest memory through `cabi_realloc`, and the guest's own buffer, plus the host's allocation on writes. S8 measured the canonical ABI copy alone at about 28 µs per MiB (about 1.8 µs per 64 KiB), so most of this is allocations and the extra copies around it, not the boundary itself.
- **redb in guest memory** runs at 1.5x native for puts (42 against 28 µs, 62 against 41 µs) and 1.0–1.45x for small gets, in line with S8's compute ratios. Large gets were faster in the guest than natively (11 against 21 µs at 64 KiB); allocator behavior is the likely reason (glibc against the guest's allocator in already-touched linear memory), not investigated.
- **redb on a file inside the guest** pays for WASI:
  - Unsynced writes cost 4–6x native (0.21 against 0.04 ms), because each of redb's file writes is a WASI call that wasmtime's WASI host runs on its blocking pool.
  - Synced writes cost 2–3x native (2.9 against 1.0 ms at 16 B). WASI may issue more or heavier syncs (not investigated).
  - Reads are served from redb's page cache in guest memory, so they are as fast as in-memory reads (2.2–2.8 µs). That beat the native file database's large reads (41 µs at 64 KiB, unexplained).
- **The host's `disk` buckets** match native redb for durable writes (0.99 against 0.99 ms; fsync dominates). Their reads are 10–20x native today (22–30 µs against 1–2.5 µs), and the experiment shows why: the hop to tokio's blocking pool, not the sandbox. Running reads inline brings them to 1.2–1.5 µs above native.

## What it changed

- The design's choice holds: durable storage is a host service. A database inside the guest works, but on files it pays WASI's per-call cost for every write, and its data dies with the instance unless it is on a preopened volume. In guest memory it suits caches and indexes rebuilt on restart.
- **Follow-up: keyvalue reads without the blocking-pool hop** (ROADMAP A6). redb reads usually hit its page cache, so running them on the worker saves about 20 µs per read. A cache miss then blocks the worker on disk for that read. Options: inline reads only, or a dedicated storage thread with a cheaper handoff. Writes stay off the worker: they fsync.
- For guests, the rule of thumb: a host call is about 0.15 µs plus about 1 µs of service overhead for small values, plus about 10–20 µs per 64 KiB moved through the keyvalue path. Batch calls (`get_many`, `set_many`) amortize the fixed part.
