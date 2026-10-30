# Spike S24: MMU-based interruption from wasmtime PR #12990

**Question.** Epoch checks cost 1.19 to 1.71 times the unchecked time on ordinary app code (BASELINE.md, 2026-10-24 and 2026-10-25), mostly from the register allocation around the check's cold call (B13). wasmtime PR #12990 replaces the check with a load from an interrupt page that is protected to interrupt. How much of the cost does it remove on Paddock's workloads, what does it cost a node with many instances, is preemption still fair, and what has to change in `paddock-host` to use it? (ROADMAP B14.)

**Answer: it removes most of the cost on guest code and keeps preemption at least as fair, with an interrupter of Paddock's own; wasmtime's own costs a node a third of its empty-call throughput at 16 workers.**

- **Guest code.** On `app_cost`'s core module the checks' cost falls from 1.18 to 1.72 times to 1.00 to 1.15 times on the 16-core machine, and from 1.16 to 1.57 times to 1.06 to 1.17 times on the Pi; `map` from 1.71 to about 1.06 and from 1.56 to about 1.07. The Pi's tight loop (B13's 1.5 times) falls to 1.03. Taking interrupts (a signal and a trampoline each, a tick every 2 ms) adds 0.5 to 1% over the loads alone. Interrupts come as often as epoch yields, about one per 2.1 ms of running guest.
- **Code size.** The core module compiles 14.6% larger with epoch checks and 3.4% with MMU checks on x86-64 (3.73, 4.28 and 3.86 MB), 11.0% and 1.8% on aarch64.
- **wasmtime's `TimingWheelInterrupter` does not scale with workers.** Every fiber resume (each callback, and each resume after a host call waits) takes a page from it: a `Box`, a lock on the wheel's slot (one slot for every store at a time) and an `Arc` clone and refcount on the page; release undoes them. At 16 workers, `pool_load`'s empty calls lose about a third (0.64 to 0.66 of epochs at 16 pools); at 4 workers, empty calls lose 3 to 12% on the 16-core machine and 0 to 7% on the Pi. With the wheel never ticked (no `mprotect`, no signals) the loss is the same, so it is the per-resume path.
- **An interrupter with a page per thread matches epochs.** `MmuInterrupter` is a public trait. `PerThreadInterrupter` (in `paddock-host.patch`) gives each thread a page in each of two generations; a resume reads a thread-local and an atomic and returns a zero-sized handle (no allocation, no lock, no shared write); a tick unprotects the other generation, switches to it and protects the old one with one `mprotect`. Every guest running at a tick is interrupted, as with epochs. Empty and busy calls run at 0.99 to 1.04 of epochs at 16 workers (16 pools), and at 0.96 to 1.07 at 4 workers on both machines.
- **Preemption of one busy instance is at least as fair.** `s24_fairness` keeps one instance's mailbox full on a single worker and times calls to a second instance. Under epochs the second waits a whole tick at the median and up to three (6.2 ms) at p99 once the first's callbacks exceed a few µs. Under MMU interruption p99 is 4.1 ms for 31 µs callbacks (tokio's cooperative budget: about 128 receives of 31 µs, since a tick that falls between callbacks interrupts nobody) and 2.1 ms for longer ones; the worst single wait is about the same as with epochs or lower (6.5 against 6.3 ms at most, on the Pi). The constraint "the epoch tick bounds how long a busy instance holds a worker" (CLAUDE.md) holds, with tokio's budget as the bound for callbacks between a few µs and a tick.
- **`paddock-host` needs two changes.** wasmtime calls no epoch callback under MMU interruption, and every interrupt yields. The callback deadline and the lease check (A8) move into `killable()`, after a poll that returned `Pending`: `PollTime` already times the polls, and now also holds the limit and the lease. `terminate`'s overrun maps the deadline exit to `killed`, as the epoch path does through its trap; a real version should let every caller classify the deadline exit as it classifies the trap today. With these changes all 789 `paddock-host` tests pass under MMU interruption with either interrupter, and under epochs on the same wasmtime build (16-core machine; not run on the Pi).

**Yielding once per tick is a policy, and MMU interruption changes it.** Under epochs an instance yields at nearly every tick it runs through: 16 busy instances yield about 38,700 times in 5 s (16 × 500 ticks/s × 5 s is 40,000). Under MMU interruption a tick interrupts only an instance that is in guest code at that moment, so the same instances yield about 26,700 times, and an instance whose callbacks are short (an empty call) never does. Measured with counters in `paddock-host` and `perf` (2026-10-29, 16-core machine, idle), this explains the two differences that S24's first runs left open:
- **One pool instance with 64 clients is 14 to 36% faster under MMU interruption** (empty calls about 260,000 against 350,000 calls/s). The instance's work per call is the same (`perf`: 435 against 449 ns in the instance task itself, the same CPU and instructions per call, system time and context switches per call alike); under epochs it yields about 490 times a second and is idle about a quarter of the time. A yield itself is short (3.8 µs from the `Pending` poll to the next), so the cost is in what the yield does to tokio's scheduling afterwards, not followed further. Resetting the epoch deadline at each callback (`PADDOCK_S24_RESET_DEADLINE=1`) changed nothing because it does not remove the yields.
- **p99 with 256 clients is higher under MMU interruption** (busy calls at 16 workers: 5.2 to 7.4 ms against 2.0 to 5.5 ms with epochs, over four sets of runs; on the Pi at 4 workers 17.7 to 18.8 against 13.1 ms), while p50 is lower (151 to 229 against 347 to 413 µs) and the mean the same (610 to 620 µs). With fewer yields an instance drains more of its mailbox (up to tokio's budget) while the others on its worker wait.
- **A yield between callbacks once per tick restores the epoch behaviour.** With `PADDOCK_S24_TICK_YIELD=1` the instance loop reads a tick counter the ticker thread advances (one atomic load per callback) and yields once when it has moved, waking itself at once as wasmtime's fiber yield does. Then busy calls at 256 clients have p99 3.85 against 4.55 ms with epochs in the same set (p50 361 against 353 µs, 410,000 against 406,000 calls/s), and the single instance falls back to the epoch's rate (288,000 against 277,000 calls/s). `tokio::task::yield_now` is the wrong yield for this: it defers the wake until the worker has drained its queue, and cost 13 to 18% of busy-call throughput.

So Paddock can keep the scheduling it has (a yield per tick: MMU interruption for guest code plus the host-side yield) or trade the tail at high concurrency for the throughput of a hot instance (no host-side yield). Only the first keeps behaviour as it is today.

What it changes: B14 waits for upstream as before. When MMU interruption ships, Paddock uses it with an interrupter of its own (the per-thread design) rather than `TimingWheelInterrupter`, and a host-side yield once per tick between callbacks unless a measurement says otherwise. The timing wheel's per-resume cost, with these numbers, is worth reporting on the PR.

## Results

wasmtime from `erikrose/wasmtime` at `933d170` (main at `50.0.0-dev` on 2026-09-28, plus #12990); rustc 1.99.1. 2026-10-29: AMD Ryzen Threadripper 1950X (16 cores), Linux 6.8, idle (load 0.5 before the runs; the runs' own load during them); Raspberry Pi 5, Linux 7.0, idle (load 0.3 to 0.7 before each set), swap on but unused. Three runs of each; earlier runs on 2026-10-28 under I/O load from another job agreed within 0.03 on every guest-code ratio.

`s24-bench`, one tokio worker, a tick every 2 ms; ratios against no checks, medians of 7, range over the three runs:

| Workload | Epoch idle, 16-core | Epoch | MMU idle | MMU | Epoch idle, Pi | Epoch | MMU idle | MMU |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| lcg 500M | 1.000 | 1.000 | 1.000 | 1.004 to 1.007 | 1.500 | 1.501 | 1.031 to 1.033 | 1.033 to 1.035 |
| fib(35) | 1.070 to 1.079 | 1.072 to 1.081 | 1.032 to 1.039 | 1.044 to 1.052 | 1.335 to 1.346 | 1.337 to 1.347 | 1.140 to 1.148 | 1.140 to 1.149 |
| sieve 16M | 1.187 to 1.195 | 1.220 to 1.241 | 0.982 to 0.985 | 0.993 to 0.996 | 0.743 to 0.846 | 0.860 to 0.877 | 0.854 to 0.866 | 0.864 to 0.874 |
| json-typed | 1.181 to 1.258 | 1.190 to 1.278 | 1.004 to 1.063 | 1.020 to 1.123 | 1.292 to 1.293 | 1.293 to 1.295 | 1.131 to 1.135 | 1.141 to 1.144 |
| json-value | 1.244 to 1.257 | 1.239 to 1.251 | 1.031 to 1.041 | 1.046 to 1.063 | 1.351 to 1.359 | 1.357 to 1.358 | 1.146 to 1.151 | 1.157 to 1.166 |
| regex | 1.279 to 1.293 | 1.274 to 1.293 | 1.127 to 1.142 | 1.135 to 1.145 | 1.159 to 1.170 | 1.162 to 1.167 | 1.083 to 1.098 | 1.092 to 1.101 |
| markdown | 1.199 to 1.212 | 1.194 to 1.204 | 1.098 to 1.105 | 1.111 to 1.132 | 1.198 to 1.199 | 1.200 to 1.202 | 1.079 to 1.081 | 1.090 |
| map | 1.682 to 1.710 | 1.687 to 1.715 | 1.044 to 1.063 | 1.053 to 1.075 | 1.552 to 1.564 | 1.553 to 1.565 | 1.061 to 1.073 | 1.067 to 1.081 |

The Pi's sieve runs faster with checks than without on every checked side; likely code placement, not followed.

`pool_load` (busy is `busy:20000`, about 31 µs on the 16-core machine), medians of three runs; calls/s, and p99 in µs:

| Machine, workers | Work, pools, clients | Epoch | Wheel | Per thread | Per thread / epoch | p99 epoch, wheel, per thread |
| --- | --- | --- | --- | --- | --- | --- |
| 16-core, 16 | empty, 1, 64 | 255,974 | 328,200 | 347,863 | 1.359 | 292, 223, 213 |
| 16-core, 16 | empty, 4, 64 | 1,045,537 | 783,163 | 1,070,215 | 1.024 | 235, 349, 230 |
| 16-core, 16 | empty, 16, 64 | 1,879,779 | 1,224,251 | 1,882,897 | 1.002 | 221, 236, 247 |
| 16-core, 16 | empty, 16, 1 | 270,986 | 232,413 | 276,316 | 1.020 | 7.7, 8.9, 7.8 |
| 16-core, 16 | empty, 16, 16 | 1,777,189 | 1,163,900 | 1,777,867 | 1.000 | 18.6, 39.6, 17.5 |
| 16-core, 16 | empty, 16, 256 | 1,974,428 | 1,261,406 | 1,991,458 | 1.009 | 711, 2,855, 1,772 |
| 16-core, 16 | busy, 1, 64 | 20,193 | 25,043 | 23,064 | 1.142 | 3,958, 3,975, 3,977 |
| 16-core, 16 | busy, 16, 64 | 416,819 | 407,845 | 414,582 | 0.995 | 718, 740, 762 |
| 16-core, 16 | busy, 16, 16 | 389,199 | 374,620 | 384,890 | 0.989 | 88, 113, 108 |
| 16-core, 16 | busy, 16, 256 | 425,972 | 413,785 | 423,428 | 0.994 | 2,034, 7,055, 7,407 |
| 16-core, 4 | empty, 16, 16 | 573,604 | 512,265 | 598,384 | 1.043 | 176, 112, 121 |
| 16-core, 4 | empty, 16, 256 | 607,935 | 532,419 | 583,616 | 0.960 | 2,185, 3,078, 2,506 |
| 16-core, 4 | busy, 16, 64 | 105,767 | 103,899 | 105,191 | 0.995 | 2,811, 3,434, 3,358 |
| Pi, 4 | empty, 1, 64 | 341,024 | 349,685 | 357,896 | 1.049 | 220, 206, 202 |
| Pi, 4 | empty, 16, 16 | 790,165 | 786,747 | 809,625 | 1.025 | 126, 113, 117 |
| Pi, 4 | empty, 16, 256 | 816,789 | 758,339 | 792,896 | 0.971 | 1,665, 2,169, 2,134 |
| Pi, 4 | busy, 16, 64 | 70,836 | 70,443 | 73,925 | 1.044 | 4,118, 5,030, 4,680 |
| Pi, 4 | busy, 16, 256 | 69,483 | 69,242 | 71,458 | 1.028 | 13,143, 17,662, 18,789 |

`s24_fairness`, one tokio worker: the second instance's call latency while the first drains a full mailbox of casts, p99 and worst in µs, range over three runs (wheel and per-thread alike):

| First instance's cast | 16-core, epoch | 16-core, MMU | Pi, epoch | Pi, MMU |
| --- | --- | --- | --- | --- |
| `busy:0` | 19 to 64; 187 to 342 | 13 to 15; 240 to 326 | 50 to 85; 266 to 316 | 22 to 27; 266 to 375 |
| `busy:2000` (about 3 µs) | 301 to 311; 598 to 649 | 13 to 16; 595 to 649 | 728 to 738; 928 to 954 | 392 to 761; 892 to 1,048 |
| `busy:20000` (about 31 µs) | 6,157 to 6,164; 6,161 to 6,169 | 4,103 to 4,120; 4,208 to 4,465 | 6,159 to 6,160; 6,160 to 6,262 | 4,106 to 4,107; 4,162 to 6,458 |
| `busy:200000` | 6,163; 6,165 to 6,173 | 2,061 to 4,096; 4,111 to 4,136 | 6,159; 6,159 to 6,176 | 2,052 to 2,061; 4,106 to 4,137 |
| `busy:2000000` (about 3 ms) | 6,162 to 6,163; 6,167 to 6,168 | 2,055 to 2,064; 2,063 to 4,110 | 6,158 to 6,159; 6,160 to 6,181 | 2,052 to 2,058; 2,063 to 4,228 |

`baseline 4` and `call_split 1` showed no difference beyond run-to-run spread on either machine: an empty call 2.65, 3.25 and 3.37 µs at p50 (epoch, wheel, per thread) with 2.45 to 3.86 across runs on the 16-core machine, 4.46 to 4.65 µs on the Pi; instantiation 88 to 89 µs and 94 to 99 µs; 54 to 60 KiB per idle instance; `call_split` 2.29 to 2.42 and 3.14 to 3.24 µs per empty call.

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
cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
PADDOCK_S24_MMU=0 ./target/release/examples/pool_load 16          # epochs
PADDOCK_S24_MMU=1 ./target/release/examples/pool_load 16          # timing wheel
PADDOCK_S24_MMU=1 PADDOCK_S24_INTERRUPTER=thread ./target/release/examples/pool_load 16
PADDOCK_S24_MMU=1 ./target/release/examples/s24_fairness          # likewise for each
PADDOCK_S24_MMU=1 cargo test -p paddock-host --all-features --no-fail-fast
```

`PADDOCK_S24_NOTICK=1` never ticks the interrupter (no preemption: only for empty calls); `PADDOCK_S24_RESET_DEADLINE=1` sets a fresh epoch deadline at the start of each callback, under epochs; `PADDOCK_S24_TICK_YIELD=1` yields between callbacks once per tick, under MMU interruption. `pool_load` runs one row with `S24_ONLY=<work>,<pools>,<clients>` (`empty,1,64`) for `S24_RUN_S` seconds, and prints the yield counters after each row. `perf` needs `kernel.perf_event_paranoid` at 2 or lower (user space only at 2). On the Pi, the builds ran through `gentle.sh` with `CARGO_BUILD_JOBS=2`, about 35 minutes for the bench and `paddock-host`'s examples.
