# Spike S33: waiting on the ring's cache line

**Question.** A native app waiting for the node's frame spins with `sched_yield`, a system call per turn (ROADMAP T10). Could the waiting side instead sleep in the CPU until the other side writes the ring's tail word, with no system call and a wake within nanoseconds of the write? Both machines have an instruction for it in user space: AMD's `MONITORX`/`MWAITX` on the 16-core machine (with a timeout), and a load-exclusive followed by `WFE` on the Raspberry Pi 5 (no timeout; Linux's event stream ends a wait nothing else ends). S32 saw a pause collapse with more busy threads than cores; does a line wait collapse the same way, and does a wait on the line that gives the core up between slices avoid it?

**Answer: no. Alone between two threads it is faster only on the Pi, and in the node it made no request cheaper on either machine; on the 16-core machine it cost CPU per call. The app's spin stays `sched_yield` (T10 tried and not kept, below).**
- **Waking alone, a line wait is no faster on the 16-core machine and much faster on the Pi.** A round trip between two threads: 1.36 µs with `MWAITX` against 1.0 to 1.1 with `sched_yield` or a pause (16-core); 0.20 µs with `WFE` against 0.56 with `sched_yield` and 0.17 with a pause (Pi).
- **A pure line wait collapses with more threads than cores, as a pause does:** 1.0 million round trips a second with 64 pairs on 16 cores against 5 million yielding, and 0.05 to 0.14 million with 16 pairs on the Pi's 4 against 2 million, at 5 to 40 times the CPU per round trip.
- **On the 16-core machine, a line wait of up to 2 µs before each yield (`lineyield`) beats yielding with more threads than cores:** 1.2 to 2.4 times the round trips a second at 10 to 60% less CPU each, from a quarter or less of the context switches; with 32 and 64 pairs the worst p99 rises from 15 to 34 µs to 0.9 to 2.9 ms. A 1 µs slice did as well or a little better, and 5 µs worse with 64 pairs.
- **On the Pi, `lineyield` is plain yielding:** the first `WFE` returns at once on an event left from before, and then the thread yields. A `WFE` nothing ends lasts 76 µs (the event stream: 4,096 ticks of the 54 MHz timer), whatever slice is asked for.
- **In the node (a temporary switch in `bulkhead-native`'s channel, since removed), the app waiting on the line for up to 2 µs before each yield:**
  - **16-core machine:** CPU per call 9 to 24% more with 16 and 64 calls in flight and casts between instances 12% fewer. Three rounds put a request from a callback at 3.6 µs against 4.4 and a bucket get at 2.2 against 3.0, but eight rounds (below) showed both bimodal in every build (a bucket get 2.2 or about 3.0 µs, whichever build ran): no gain.
  - **Pi:** no change beyond the spread between rounds (a request 2.9 µs either way), and one round's p99 between instances of 68 µs against 13.
  - **The node's watch (T9) on the line instead of pausing** (16-core machine only; on the Pi a `WFE` could hold a worker 76 µs past the watch's 5 µs) changed nothing beyond the spread but casts to one instance, 14% fewer.

## Method

`s33` (one binary, threads in one process). Each pair is a client and a server thread ping-ponging over two words laid out as a ring's are: the position one side writes and the other side's "I sleep" flag, each on a 128-byte line of its own. Each side waits for the other's word to change by spinning up to a budget of 20 µs (`[native] spin_us`'s default) with one method, then sets its flag and sleeps on a futex on that flag, as `bulkhead-native`'s channel does:
- **yield:** `sched_yield` each turn (the channel today);
- **pause:** a spin-loop hint each turn (S32's pause);
- **line:** `MWAITX` with its timer at the budget's end, or `WFE` after a load-exclusive of the word (`src/wait.rs`), until the budget runs out;
- **lineyield:** a line wait of up to a slice (2 µs; 1 and 5 µs in separate runs), then `sched_yield`, and again.

The first version slept on a futex on the word itself, and a writer late from the round before could clear the flag just before the waiter slept, losing the next wake: a pair hung on the Pi (4 pairs, pause). Every run below is from the fixed version.

Each case runs for 2 s: round trips a second over all pairs, the median of the pairs' p50 round trip and the worst p99, CPU per round trip (the process's user and system time), context switches per round trip, and the share of waits that slept. A last run adds a counting thread per core beside 1 and 4 pairs, for what a held core costs other work; those runs were noisy on both machines (yielding pairs waited a 3 ms time slice behind the counters), and nothing below rests on them.

**The node trial.** A temporary `BULKHEAD_S33` switch, passed through the node and `bulkhead-runner` to the app: `app`, the app's spin for the node's frame waits on the ring to the app's tail line for up to 2 µs before each yield (waits for room unchanged); `node`, T9's watch inside a callback waits on the ring to the node's tail line up to the end of its slice instead of pausing (x86-64 only); `both`. `cluster_throughput` natively on one node and `native_cost`, 4 workers, `spin_us` 20, three rounds, the builds in a rotating order, each round starting at load under 1.

**Machines,** 2026-11-06:
- **16-core machine:** Threadripper 1950X (`mwaitx` in its CPU flags), Linux 6.8; each round starting at load 0.9 to 1.0 (the spinning runs raise the load average).
- **Raspberry Pi 5:** Cortex-A76 (`evtstrm` in its features), Linux 7.0, at nice 10 and idle I/O priority, each round starting at load 0.85 to 0.99.

## The microbenchmark

Ranges over three rounds; budget 20 µs, `lineyield`'s slice 2 µs.

16-core machine:

| Pairs (threads) | | yield | pause | line | lineyield |
| --- | --- | --- | --- | --- | --- |
| 1 (2) | p50 µs | 1.00 to 1.08 | 0.94 to 1.08 | 1.36 to 1.37 | 1.36 |
| 8 (16) | round trips/s | 6.3 to 7.6M | 8.3 to 17.8M | 6.3 to 8.0M | 7.7 to 12.2M |
| 16 (32) | round trips/s | 4.1 to 5.3M | 7.1 to 8.3M | 1.6 to 2.1M | 5.5 to 10.3M |
| 32 (64) | round trips/s | 4.3 to 4.4M | 3.1 to 7.2M | 1.4 to 1.9M | 9.8 to 10.7M |
| 64 (128) | round trips/s | 4.9 to 5.0M | 1.3 to 3.3M | 1.0M | 5.7 to 7.5M |
| 64 (128) | CPU µs per round trip | 2.9 to 3.0 | 4.7 to 12.4 | 14.5 to 16.1 | 2.0 to 2.7 |
| 64 (128) | worst p99 µs | 28 to 34 | 274 to 386 | 325 to 421 | 969 to 2,937 |

Raspberry Pi 5:

| Pairs (threads) | | yield | pause | line | lineyield |
| --- | --- | --- | --- | --- | --- |
| 1 (2) | p50 µs | 0.56 | 0.17 | 0.20 | 0.50 |
| 2 (4) | round trips/s | 3.1 to 3.2M | 11.3M | 9.1 to 9.8M | 3.7M |
| 4 (8) | round trips/s | 2.1 to 2.2M | 0.3 to 3.5M | 0.8 to 4.4M | 1.9 to 2.2M |
| 16 (32) | round trips/s | 2.0 to 2.1M | 0.7 to 1.7M | 0.05 to 0.14M | 2.0 to 2.1M |
| 16 (32) | CPU µs per round trip | 1.9 | 2.4 to 5.8 | 28 to 77 | 1.9 |

- **`MWAITX`'s wake costs about 0.15 µs a side more than a spinning load sees the write** on the 16-core machine; on the Pi a round trip waiting with `WFE` is within 0.03 µs of one with a pause.
- **A line wait holds its core as a pause does,** so with more threads than cores the thread the waiter needs may not run until the waiter's wait ends (`MWAITX`'s timer, or `WFE`'s 76 µs).
- **`lineyield`'s gain on the 16-core machine is fewer context switches:** 0.2 to 0.6 involuntary switches per round trip against about 2 yielding, since the other side usually writes within the slice.

## The node trial

Medians of three rounds:

| | 16-core: off | app | node | both | Pi: off | app |
| --- | --- | --- | --- | --- | --- | --- |
| A request from a callback (`native_cost`) | 4.4 µs | 3.6 | 4.3 | 4.2 | 2.9 | 2.9 |
| A bucket get from a callback | 3.0 µs | 2.2 | 3.0 | 2.8 | 1.4 | 1.4 |
| 16 calls in flight, calls/s (CPU per call) | 255,000 (30.0 µs) | 257,000 (32.6) | 251,000 (30.0) | 260,000 (32.7) | 116,000 (29.5) | 110,000 (30.3) |
| 64 calls in flight, calls/s (CPU per call) | 313,000 (29.5 µs) | 333,000 (36.7) | 321,000 (29.3) | 337,000 (37.5) | 117,000 (30.5) | 117,000 (30.7) |
| Casts to one instance, handled/s | 274,000 | 242,000 | 236,000 | 256,000 | 443,000 | 434,000 |
| Sequential calls between instances, p50 (p99) | 8.2 µs (11.6) | 8.0 (12.0) | 8.5 (13.9) | 8.1 (12.7) | 5.4 (13.7) | 5.4 (13.7) |
| 16 instances calling each other, calls/s | 314,000 | 331,000 | 315,000 | 323,000 | 118,000 | 118,000 |
| Casts from instances, handled/s | 742,000 | 651,000 | 747,000 | 685,000 | 210,000 | 201,000 |

`results/trial-*/summary.txt` has each round.

- **CPU per call rises with many calls in flight,** where apps wait between callbacks and a line wait holds a core a yield would have given up.
- **The request rows above are not a gain** (eight rounds, below). On the Pi too the request does not move: the app's wake is not where its time goes.

## T10: the line wait inside a callback only, tried and not kept

T10 built the line wait into `bulkhead-native`'s channel for the app's wait for an answer inside a callback only, on CPUs with `MWAITX` (`sched_yield` between callbacks and elsewhere), and was reverted. The app builds were swapped (the node's side did not change), 16-core machine, 2026-11-06, each round starting at load under 1.

Eight rounds of `native_cost`, the builds in a rotating order: before T10, T10, and the line wait in every wait for the node's frame ("all", as the node trial's `app`). Medians, ranges in brackets:

| | Before | T10 | All |
| --- | --- | --- | --- |
| A request from a callback | 4.35 µs [3.5 to 4.5] | 4.05 [3.6 to 4.4] | 4.25 [3.6 to 4.4] |
| A bucket get from a callback | 2.80 µs [2.2 to 3.0] | 2.65 [2.2 to 2.9] | 2.80 [2.2 to 2.9] |
| 16 calls in flight, calls/s (CPU per call) | 253,000 (29.6 µs) | 257,000 (29.8) | 259,000 (32.8) |
| 64 calls in flight, calls/s (CPU per call) | 314,000 (29.6 µs) | 319,000 (29.4) | 333,000 (37.0) |

Three rounds of `cluster_throughput` and `native_cost`, before against T10, showed the same: no row beyond the spread between rounds (`results/t10-16core/summary.txt`).

- **No build makes a request cheaper:** each swings between the same two levels from round to round.
- **The CPU cost comes from the waits between callbacks:** "all" costs a quarter more CPU per call with 64 calls in flight; T10, without them, costs nothing and gains nothing.

## What it changes

- **The app's spin stays `sched_yield` on both machines** (T10 not kept). A pause between yields (S32's hybrid) is not worth trying: a pause is no faster than yielding on the 16-core machine alone, and on the Pi a faster wake did not reach the request.
- **The node's watch stays a pause loop.**
- **Not measured:** Intel's `UMONITOR`/`UMWAIT` (neither machine has WAITPKG), and arm64's `WFET` with a timeout (FEAT_WFxT; not on the Cortex-A76).

## How to run

```sh
cargo build --release -p s33
target/release/s33 idle     # how long one line wait lasts with nothing written
target/release/s33 [secs] [budget_us] [slice_us] [pairs,...] [methods,...] [hogs]
target/release/s33 2 20 2 1,4,8,16,32,64 yield,pause,line,lineyield
```

`results/` has every run: `t10-16core/` and `t10-native-cost-16core/` from T10's trial (`base`, `t10`, `all`), `16core-*` and `pi-*` from `s33` (`main-N` three rounds of every method, `sliceN-N` `lineyield` at 1 and 5 µs, `hogs` with a counting thread per core), `trial-16core/` and `trial-pi/` from the node trial (`ct-*` from `cluster_throughput`, `nc-*` from `native_cost`), summarized by `summarize.py <dir> <builds>`. The node trial's switch is not in the tree; its shape is above.
