# Spike S24: MMU-based interruption from wasmtime PR #12990

**Question.** Epoch checks cost 1.19 to 1.71 times the unchecked time on ordinary app code (BASELINE.md, 2026-10-24 and 2026-10-25), mostly from the register allocation around the check's cold call (B13). wasmtime PR #12990 replaces the check with a load from an interrupt page that is protected to interrupt. How much of the cost does it remove on Paddock's workloads, what does it cost a node with many instances, and what has to change in `paddock-host` to use it? (ROADMAP B14.)

**Answer, so far: it removes most of the cost on guest code, and wasmtime's own interrupter does not scale; one written for Paddock does.** Results are from the 16-core machine only; the Pi was unreachable on 2026-10-28.

- **Guest code.** On `app_cost`'s core module the checks' cost falls from 1.20 to 1.71 times to 1.04 to 1.15 times; `map` from 1.71 to about 1.07. On the WAT kernels, `sieve` from 1.23 to 1.00 and `fib` from 1.07 to 1.04. Taking interrupts (a signal and a trampoline per interrupt, a tick every 2 ms) adds 0.5 to 1% over the loads alone. Interrupts come at the same rate as epoch yields, about one per 2.1 ms of running guest.
- **Code size.** The core module compiles to 3.73 MB without checks, 4.28 MB with epoch checks (+14.6%) and 3.86 MB with MMU checks (+3.4%).
- **wasmtime's `TimingWheelInterrupter` costs a node a third of its empty-call throughput at 16 workers.** Every fiber resume (each callback, and each resume after a host call waits) takes a page from the interrupter: a `Box`, a lock on the wheel's slot (one slot for every store at a time) and an `Arc` clone and refcount on the page; release undoes them. With 16 workers, `pool_load`'s empty calls at 16 pools fall from 1.72 to 1.09 million calls/s and p99 at 16 clients rises from 25 to 85 µs; at 4 workers the loss is 6 to 10%. With the wheel never ticked (no `mprotect`, no signals) the loss is the same, so it is the per-resume path, not the protection faults.
- **An interrupter with a page per thread recovers it.** `MmuInterrupter` is a public trait. `PerThreadInterrupter` (in `paddock-host.patch`) gives each thread a page in each of two generations; a resume reads a thread-local and an atomic and returns a zero-sized handle (no allocation, no lock, no shared write); a tick unprotects the other generation, switches to it and protects the old one with one `mprotect`. Every running guest is interrupted at the next tick, as with epochs. At 16 workers, empty calls at 16 pools reach 0.94 to 0.97 of epoch throughput (1.67 against 1.72 million calls/s at 16 clients, p99 25.0 against 24.6 µs). The remaining 3 to 6% under heavy concurrency is not explained.
- **`paddock-host` needs two changes.** wasmtime calls no epoch callback under MMU interruption, and every interrupt yields. The callback deadline and the lease check (A8) move into `killable()`, after a poll that returned `Pending`: `PollTime` already times the polls, and now also holds the limit and the lease. `terminate`'s overrun maps the deadline exit to `killed`, as the epoch path does through its trap; a real version should let every caller classify the deadline exit as it classifies the trap today. With these changes all 789 `paddock-host` tests pass under MMU interruption with either interrupter, and under epochs on the same wasmtime build.

Not explained, not followed further: with one pool instance and 64 clients, MMU interruption is 25 to 43% faster than epochs (empty calls 266,000 to 358,000 calls/s, busy calls 18,300 to 26,200), also with the wheel never ticked; more than `call_split`'s whole callback time (about 1 µs) per call. Resetting the epoch deadline at the start of each callback (`PADDOCK_S24_RESET_DEADLINE=1`) did not change it. `perf` was not available (`perf_event_paranoid` is 4).

Open before Paddock switches:
- **Fairness.** Under epochs, an instance draining a full mailbox yields at the next tick even when each callback is short, since the deadline is set only when it yields (CLAUDE.md, "The epoch tick bounds how long a busy instance holds a worker"). Under MMU interruption each callback is a fresh resume with a fresh page, so short callbacks are never interrupted and the instance may hold its worker until tokio's cooperative budget runs out. Not measured; it may be the single-pool speedup above. Check with S1's spinning-guest and oversubscription runs (`pool_load` p99 for the other pools).
- **The Pi.** The same runs on aarch64, where the tight-loop cost was 1.5 times (B13).
- **Idle machine.** Every run here had I/O load from another job (load average 4 to 13, mostly I/O wait; CPU 87 to 93% idle). The ratios held within 0.03 across three runs, but rerun before quoting a number in BASELINE.md.
- **Upstream.** The per-resume cost of `TimingWheelInterrupter` is worth reporting on the PR, with the per-thread design as data. The flags PR (#14929) is in draft; the rest of #12990 is being split up.

## Results

AMD Ryzen Threadripper 1950X (16 cores), Linux 6.8, 2026-10-28; wasmtime from `erikrose/wasmtime` at `933d170` (main at `50.0.0-dev` on 2026-09-28 plus #12990); rustc 1.99.0. Contended by I/O as above.

`s24-bench`, one tokio worker, a tick every 2 ms; ratios against `off`, medians of 7, three runs (ranges):

| Workload | Epoch idle | Epoch | MMU idle | MMU |
| --- | --- | --- | --- | --- |
| lcg 500M | 1.000 | 1.000 | 1.000 | 1.005 to 1.006 |
| fib(35) | 1.074 to 1.076 | 1.071 to 1.072 | 1.031 to 1.034 | 1.038 to 1.042 |
| sieve 16M | 1.189 to 1.197 | 1.226 to 1.230 | 0.982 to 0.986 | 0.991 to 0.998 |
| json-typed | 1.232 to 1.259 | 1.236 to 1.284 | 1.051 to 1.068 | 1.061 to 1.075 |
| json-value | 1.241 to 1.246 | 1.247 to 1.279 | 1.041 to 1.077 | 1.042 to 1.050 |
| regex | 1.268 to 1.281 | 1.273 to 1.283 | 1.121 to 1.143 | 1.130 to 1.146 |
| markdown | 1.191 to 1.241 | 1.193 to 1.223 | 1.093 to 1.102 | 1.100 to 1.123 |
| map | 1.708 to 1.714 | 1.710 to 1.714 | 1.060 to 1.067 | 1.068 to 1.108 |

`pool_load 16` (16 workers), medians of three runs, calls/s and p99 µs:

| Work, pools, clients | Epoch | Wheel | Per thread | Per thread / epoch | p99 epoch, wheel, per thread |
| --- | --- | --- | --- | --- | --- |
| empty, 1, 64 | 266,407 | 335,707 | 358,742 | 1.347 | 295, 273, 259 |
| empty, 4, 64 | 1,018,928 | 825,168 | 1,041,655 | 1.022 | 248, 314, 259 |
| empty, 16, 64 | 1,819,534 | 1,190,289 | 1,712,282 | 0.941 | 251, 458, 269 |
| empty, 16, 1 | 209,519 | 257,503 | 268,758 | 1.283 | 9.9, 7.9, 8.0 |
| empty, 16, 16 | 1,720,069 | 1,086,296 | 1,674,726 | 0.974 | 24.6, 84.8, 25.0 |
| empty, 16, 256 | 1,868,490 | 1,244,673 | 1,758,141 | 0.941 | 1,012, 2,779, 1,956 |
| busy, 1, 64 | 18,289 | 24,393 | 26,184 | 1.432 | 3,963, 3,980, 3,915 |
| busy, 16, 64 | 394,056 | 375,234 | 389,914 | 0.989 | 1,490, 1,587, 1,603 |
| busy, 16, 16 | 352,428 | 329,726 | 341,429 | 0.969 | 165, 228, 197 |
| busy, 16, 256 | 420,537 | 407,442 | 410,148 | 0.975 | 3,713, 5,937, 6,747 |

With the timing wheel, `baseline 4` (empty call p50 2.65 against 3.20 µs, noisy; instantiation 88 against 89 µs; 59 to 60 KiB per idle instance either way) and `call_split 1` (2.25 to 2.36 against 2.26 to 2.28 µs per empty call) showed no difference beyond run-to-run spread.

## How to run

The guest-code comparison, against `app_cost`'s core module (built in the main workspace):

```sh
cargo rustc --release -p paddock-workload --features exports \
    --crate-type cdylib --target wasm32-unknown-unknown --target-dir target/fixtures
cd spikes/s24/bench     # a workspace of its own, wasmtime from the PR branch
cargo run --release [-- kernels|app]
```

The host comparison: `paddock-host` against the branch, in a worktree, never on main (the branch lacks wasmtime 50.0.x's security fixes):

```sh
git worktree add --detach ../paddock-s24 HEAD
cd ../paddock-s24 && git apply <this checkout>/spikes/s24/paddock-host.patch
cargo build --release -p paddock-host --examples
PADDOCK_S24_MMU=0 ./target/release/examples/pool_load 16          # epochs
PADDOCK_S24_MMU=1 ./target/release/examples/pool_load 16          # timing wheel
PADDOCK_S24_MMU=1 PADDOCK_S24_INTERRUPTER=thread ./target/release/examples/pool_load 16
PADDOCK_S24_MMU=1 cargo test -p paddock-host --all-features --no-fail-fast
```

`PADDOCK_S24_NOTICK=1` never ticks the interrupter (no preemption: only for empty calls); `PADDOCK_S24_RESET_DEADLINE=1` sets a fresh epoch deadline at the start of each callback, under epochs.
