# Spike S30: what an instance is

**Question.** Bulkhead runs apps as native processes. Should an instance (a pid with a mailbox, one callback at a time) be a process of its own, as today's `native-process` kind runs it, or should one process per app host many instances? What does each cost in start time, memory, calls and crash recovery, and does the frame transport (a pipe today) matter?

**Answer: a process per instance for Phase 5, and a process per app with a thread per instance later, chosen per app, for apps that need density.**
- **A process per instance** isolates every crash and costs:
  - about 0.25 to 0.5 MiB per instance once the kernel's share is counted (1.4 to 2.4 MiB on the Pi's glibc 2.43 unless its malloc arenas are kept off huge pages);
  - 0.7 to 1.2 ms to start bare, 5.7 to 14 ms through today's runner;
  - about 50 µs per call.

  That is enough for the board app and for pools of hundreds of instances.
- **A process per app** starts an instance in 30 to 110 µs and holds one in 5 to 55 KiB, a component's range or below. In exchange, an abort, a segfault or the process's memory limit ends every instance in it.
- **Shared worker threads are not enough while the SDK's API is synchronous.** An instance waiting on a call holds its worker, and a call to an instance on the same worker never returns. So each instance needs a stack of its own: here, a thread.
- **The pipe is not the cost.** A pipe round trip is 11 to 20 µs, and a ring in shared memory woken by an eventfd is no faster. Calls cost the hand-offs between tasks and threads on both sides.

The spike also found Q17: today's native executables became dumpable again after the runner's `execve`, so the node saw a crash 0.1 to 0.3 s late through the system's core handler (fixed, CHANGELOG).

## Method

`s30-app` (`app/`) is one program in three shapes, served over its stdin and stdout with frames that name their instance (a 4-byte length, a kind, the instance, a 64-bit id, the body):
- **A:** one instance per process. A reading thread answers pings and routes replies, and one worker thread runs the callbacks, as `bulkhead_native::app` does.
- **B (`--multi`):** many instances per process on 4 worker threads, each owning the instances whose number picks it. Frames go out through a writer thread that flushes once per batch.
- **B2 (`--multi --per-instance`):** many instances per process, each on a thread of its own (256 KiB stacks).

Each instance holds 4 KiB of state. A call's body is a command: `echo`, `relay` (call another instance through the host and reply its answer), `panic` (shape B and B2 catch it and end that instance alone) or `abort`.

`s30-bench` (`bench/`) is a small host on 4 tokio workers. Each instance has a task that sends one call at a time; each process has a reader task and a writer task; and a call from one instance to another goes through the host, which routes it by instance, as the node does. The `real` phase measures today's native instances through `bulkhead-host`, `bulkhead-runner` and `bulkhead-fixture-native`, to calibrate shape A. The `transport` phase echoes 64-byte frames through an app process over its pipes, over two rings in a memfd woken through eventfds, and over the rings with both sides spinning.

Memory is the PSS of the app processes and of the host, and the drop in `MemAvailable`, which also counts what the kernel keeps per process and per thread (page tables, kernel stacks) and PSS leaves out.

Machines and dates:
- **16-core machine:** AMD Ryzen Threadripper 1950X, Linux 6.8, glibc 2.39, 2026-10-31. A browser tab used most of a core throughout (load 3.4 at the start, 7.2 at the end), so the latency and throughput rows from it are contended. Start and memory varied little between runs.
- **Raspberry Pi 5:** 4 cores, Linux 7.0, glibc 2.43, 2026-10-31, at nice 10, load 0.4 to 2.0.

Component figures, for comparison, are BASELINE.md's (16-core machine, 2026-10-06, 4 workers): an empty call 3.8 µs at p50, instantiate to the first reply 77 µs, 56 KiB resident per idle instance.

## Start

| | 16-core | Pi |
| --- | --- | --- |
| A, a process each, to ready (p50 / p99) | 0.68 / 1.0 ms | 1.2 / 1.6 ms |
| today's runner, to the first answer (p50) | 5.7 to 13.7 ms (three runs) | 6.5 ms (two runs) |
| B, one more in a running process (p50 / p99) | 51 / 104 µs | 32 / 48 µs |
| B, 1,000 at once | 4.6 ms | 5.6 ms |
| B2, one more in a running process (p50 / p99) | 112 / 347 µs | 101 / 426 µs |
| B2, 1,000 at once | 35 ms | 55 ms |

Today's runner restricts itself (Landlock, seccomp) and executes the app, which loads its libraries again. Its start is 5 to 14 times a bare process's; where the difference goes was not measured.

## Memory per instance

| | 16-core: PSS, `MemAvailable` drop | Pi, default | Pi, `GLIBC_TUNABLES=glibc.malloc.hugetlb=0` |
| --- | --- | --- | --- |
| A, 1,000 | 166 KiB, 321 KiB | 1,754 KiB, 1,816 KiB | 151 KiB, 242 KiB |
| A, 5,000 | 165 KiB, 442 KiB | 1,192 KiB, 1,431 KiB | 150 KiB, 376 KiB |
| today's runner, 1,000 | 158 KiB, 466 KiB | 2,187 KiB, 2,407 KiB | not run |
| B, 1,000 | 4.9 KiB, 7.1 KiB | 10.9 KiB, 11.3 KiB | 4.8 KiB, 0 KiB |
| B, 5,000 | 4.3 KiB, 6.5 KiB | 4.7 KiB, 4.9 KiB | 4.3 KiB, 6.3 KiB |
| B2, 1,000 | 17 KiB, 55 KiB | 77 KiB, 79 KiB | 17 KiB, 14 KiB |
| B2, 5,000 | 16 KiB, 50 KiB | 24 KiB, 47 KiB | 16 KiB, 40 KiB |

The host's own share was 0.1 to 17 KiB per instance in every shape.

- **The Pi's glibc gives each malloc arena a 2 MiB huge page.** Any thread but the main one allocates from a new arena, and glibc 2.43 (Ubuntu 26.04) advises huge pages for it; glibc 2.39 here does not. So every app process with a second thread costs about 2 MiB more on the Pi. `GLIBC_TUNABLES=glibc.malloc.hugetlb=0` or `MALLOC_ARENA_MAX=1` takes an idle process from 2,643 to about 590 KiB.
- **A process costs the kernel 90 to 280 KiB beyond its PSS** (the drop in `MemAvailable` less PSS): two thread stacks, page tables and the pipes.
- **A process per app holds an instance in 5 to 11 KiB, and with a thread each in 16 to 55 KiB.** That is a component's 56 KiB or less.
- Today's runner PSS was read before the Q17 fix; a process that is not dumpable hides its `/proc` files, so the bench now reads only `MemAvailable` for it.

## Calls, 100 instances

| | 16-core (contended): p50 / p99 | Pi: p50 / p99 |
| --- | --- | --- |
| host to instance, A | 50 / 86 µs | 49 / 65 µs |
| host to instance, B | 57 / 88 µs | 34 / 46 µs |
| host to instance, B2 | 58 / 92 µs | 38 / 51 µs |
| host to instance, today's runner | 69 / 123 µs | 72 / 91 µs |
| instance to instance through the host, A | 125 / 193 µs | 117 / 151 µs |
| instance to instance, B2 | 141 / 249 µs | 87 / 110 µs |
| instance to instance, today's runner | 137 / 206 µs | 186 / 240 µs |

| 64 calls in flight | 16-core (contended): calls/s, CPU per call | Pi |
| --- | --- | --- |
| A | 363,000, 21 µs | 103,000, 37 µs |
| B | 523,000, 8.0 µs | 446,000, 6.8 µs |
| B2 | 389,000, 15 µs | 250,000, 13.5 µs |
| today's runner | 223,000, 35 µs | 63,000, 61 µs |

- **A call to a process costs about 50 µs and 21 to 37 µs of CPU,** 13 times a component's 3.8 µs call. Today's runner adds 20 µs (the CBOR protocol and its threads).
- **A process per app batches.** Its frames for many instances share each pipe write, so it takes a third to a fifth of a process-per-instance call's CPU.
- **Shape B was not run between instances.** An instance waiting on a call holds its worker thread, so every instance on that worker waits too, and a call to an instance on the same worker never returns. B2 gives each instance a thread and works. User-space fibers would also do it and were not tried.

## The transport

| 64-byte frames echoed | 16-core (contended): round trip p50 / p99, 64 in flight | Pi (load 2.0) |
| --- | --- | --- |
| pipe | 20 / 33 µs; 2.5 million/s at 0.5 µs of CPU | 11 / 14 µs; 2.5 million/s at 0.6 µs |
| ring, eventfd wakes | 13 / 23 µs; 2.8 million/s at 1.0 µs | 11 / 15 µs; 3.7 million/s at 0.8 µs |
| ring, both sides spinning | 1.4 / 5.9 µs; 1.4 million/s at 1.4 µs | 0.33 / 3.8 µs; 5.1 million/s at 0.4 µs |

A round trip is two wake-ups of a sleeping thread; a ring saves the system calls of reading and writing, not the wake-ups, and only spinning removes them (a core held on each side). A call's 50 µs is the frame's trip through the host's instance, writer and reader tasks and the app's reading, worker and writing threads: about six hand-offs, each a wake-up.

## Crashes

| | 16-core | Pi |
| --- | --- | --- |
| A: one process of 100 aborts | seen in 0.24 ms, replaced in 2.2 ms; the other 99 answer | 0.23 ms, 3.5 ms |
| B, 1,000: a panic in one instance | that instance ends in 0.11 ms, replaced in 0.08 ms | 0.15 ms, 0.07 ms |
| B, 1,000: an abort | seen in 0.51 ms; all 1,000 lost, restarted in 5.7 ms | 0.65 ms, 9.0 ms |
| B2, 1,000: an abort | seen in 10.5 ms (1,000 threads to end); restarted in 39 ms | 13.9 ms, 44 ms |
| today's runner: an abort | seen in 120 to 130 ms | 194 to 327 ms |
| today's runner after the Q17 fix | 0.42 ms | not run |

A process per app contains a panic, since the SDK catches it around each callback. It does not contain an abort, a segfault, a stack overflow, an OOM kill or memory one instance corrupts for another.

## Found: native apps were dumpable after the runner's exec (Q17)

`bulkhead-runner` marks itself not dumpable and documented that its restrictions stay in force across the exec of an executable app. Landlock and seccomp do, but `execve` resets the dumpable flag for a readable program. A crashing app therefore went to the system's core handler (`core_pattern` pipes to apport on both machines), which held the dying process: the node saw the exit 120 to 330 ms late. Other processes of the node's user could also trace the app, as far as Yama allows. `bulkhead_native::app::serve` now sets it again before anything else (`tests/native.rs`, `executable____after_the_runner_execs_it____not_dumpable`, which fails without it). An executable that does not use `serve` stays dumpable.

## What it changes

- **Z1 keeps a process per instance for Phase 5's exit.** The board needs a pool of a few instances and a singleton, and every cost above fits. Z1 should:
  - cut the hand-offs per call, which are where the time goes;
  - keep the app's malloc arenas off huge pages (set `GLIBC_TUNABLES` in the environment the runner gives the app, or document it);
  - find where the runner's 5 to 14 ms start goes.
- **A process per app, with a stack per instance, is a later item for apps that need thousands of instances or an instance per connection.** It would be chosen in the manifest, with its isolation caveat. Shared workers would need asynchronous callbacks in the SDK first.
- **A ring in shared memory is not worth it** unless a side can spin.

## How to run

```sh
spikes/s30/run.sh                         # every phase
spikes/s30/run.sh memory calls            # some phases
S30_MAX=1000 spikes/s30/run.sh memory     # cap the instance counts
GLIBC_TUNABLES=glibc.malloc.hugetlb=0 spikes/s30/run.sh memory
```

Phases: `start`, `memory`, `calls`, `crash`, `real`, `transport`. `run.sh` builds `bulkhead-runner` and `bulkhead-fixture-native` in the main workspace for `real`.
