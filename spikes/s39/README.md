# S39: sends from an app's own threads

Spike S39 of `docs/ROADMAP.md` (Phase 9, ahead of DA1 in Phase 7). Only an app's callback thread may ask the node today (WRITING_APPS.md); other threads may log. An event processor that reads Kafka, ActiveMQ or a websocket on threads of its own needs `cast`, `try_cast`, `cast_timeout`, bucket reads and writes and metrics from those threads, and a driver app (DA1, S37) needs calls in flight from any of its threads, each answered by id. S39 measures the ways to let a thread other than the callback thread reach the node, against a callback's own request, and picks one for AP1 and DA1.

Run 2026-11-12, release builds, two rounds, on two machines idle at the start (the rest is the runs' own load):

- **16-core machine:** Threadripper 1950X, 16 threads, Linux 6.8, no VM running; load 0.9 at the start.
- **Raspberry Pi 5:** Cortex-A76, 4 cores, Linux 7.0, nice 10 and idle I/O priority; load 0.4 at the start.

**Answer: a channel per sending thread (m1).** The node makes a channel for each thread that asks for one, up to a limit, and passes it as it passes a snapshot; the thread writes and reads it alone, as the callback thread does its own.
- **It costs what a callback's request costs.** One thread's get on the Pi: 0.8 µs and 2.0 µs of CPU, the same as from the callback thread; on the 16-core machine 0.9 to 2.1 µs against 0.6 to 0.9 (placement, below). Every mechanism with one shared ring paid 2 to 4 times the CPU (3.7 to 8.2 µs on the 16-core machine, 4.9 to 6.2 on the Pi) for the hand-off of each answer between two of the app's threads.
- **It scales with threads.** 8 threads: 1.4 to 2.2 million gets a second on the two machines, p99 5 to 7 µs, against 0.6 to 1.3 million and p99 10 to 37 µs on a shared ring; casts 6 to 14 million a second with p99 0.2 µs, against 0.9 to 3.9 million with p99 31 to 46 µs. Senders on a shared ring queue on its writer's lock, and their p99 is a futex wake-up.
- **A busy callback thread does not hold it up.** With 1 ms callbacks back to back, a get from another thread took 0.6 to 5.2 µs p50. A queue drained between callbacks (m3) took 1 to 2 ms: each request waits for the callback that is running, and often for the next one too. m3 is out.
- **No new code shared between threads in the app.** m2's reader role, handed from thread to thread with slots and parking, matches m1 when the callback thread is busy, but it is subtle (a lost wake-up stalls every sender) and falls behind as senders are added.
- **A door per thread** is a descriptor an event loop can wait on (epoll, kqueue) beside its sockets, which a driver (DA1) needs.

What it costs instead: memory (two 68 KiB rings per channel at today's ring size, so 1.1 MiB for 8 senders), a limit per instance, and more rings for the node to watch (no cost showed with 9 rings and one poller; the node's instance tasks are another matter, below).

Where m1 did not win: on the Pi with the callback thread busy, the run has more spinning threads than cores (the node's poller, the busy callback thread, the other app's callback thread, the senders), and there m1 and m2 tie on gets and m2 is ahead on calls (one thread: 1.9 to 5.4 µs against 5.7). How many threads spin per core decides that case, and the real node's spin and idle hint (T1b) change it; not measured here.

## The mechanisms

All five use `bulkhead-native`'s channel as the node does: its rings, its spin-then-sleep wait (`Spin::block_until`, `sched_yield` turns, 20 µs), its doors and its bell. Only how an app's threads share it differs.

- **m0, the callback thread (baseline).** One thread asks and waits on the app's one channel, as a callback does today.
- **m1, a ring per thread.** Each sending thread has a channel of its own, made by the node and passed to the app, so it writes and reads alone. The callback thread keeps channel 0.
- **m2, one shared ring, read by whichever thread waits.** Senders write under a lock with request ids. A waiting thread that finds no reader takes the reader role: it reads every frame, puts each answer in its thread's slot (waking that thread if it sleeps) and queues callbacks for the callback thread, until its own answer comes; then it hands the role to a sleeping waiter. The callback thread waiting for its next callback is such a waiter, so between callbacks it usually holds the role.
- **m2r, one shared ring, a reader thread.** As m2, but a thread of the app's own always reads, so every answer and every callback crosses a thread.
- **m3, a queue drained by the callback thread.** Senders queue frames and wake the callback thread (through a copy of its door's write end); it writes them, reads the answers and fills the slots. It drains the queue between callbacks only.

A waiting sender in m2, m2r and m3 spins with `sched_yield` on its slot for the same budget, then parks.

## Method

`s39 bench` (one binary) plays the node: one poller thread sweeps every ring to the node and answers each frame itself (S32's direct path), spins up to 20 µs after the last frame, then marks every ring and sleeps on epoll over the apps' bells. It starts `s39 app` (the same binary) as the subject app on the mechanism under test and, for calls, a second app whose callback thread answers them.

Frames are a 4-byte length and `kind | id | rest`, tiny and fixed, so the comparison is of the mechanisms and not of encoding. The operations, from each of 1, 2, 4 and 8 sending threads at once:
- **get:** a bucket get, a 16-byte value from 100,000 keys, answered by id;
- **cast:** a frame to the node with no answer, counted by the node; a get after the last one waits until all have arrived;
- **call:** a call to the other app through the node, answered by its callback thread.

The callback thread either waits for callbacks that do not come (idle), or runs 1 ms callbacks back to back (busy): the node sends the next as soon as the last ends.

Each thread times every operation; p50 and p99 are over all threads' operations, and throughput is all operations over the span from the first thread's start to the last thread's end. CPU per operation is the node's process, the subject app's (less what its busy callbacks burned) and the other app's (from `/proc`, 10 ms ticks), over the measured part. 20,000 operations a thread after 2,000 of warm-up when idle; 2,000 after 200 when busy. Two rounds.

What the harness does not model: the node's instance task. Today it answers an app's requests only within a callback, one at a time, in order (`instance_native.rs`); here the poller answers every ring at once. What each mechanism asks of the node is under "What the node must change".

Run (from this directory):

```bash
cargo build --release
scripts/run.sh results/<machine>-<date>.txt
```

## Results

`results/x86-2026-11-12.txt` and `results/pi5-2026-11-12.txt`, summarised by `scripts/tables.py` (which prints every thread count): each cell is the range over two rounds. CPU per operation is the node's, the subject app's (less its busy callbacks' own) and the other app's. p50 and p99 are each operation's own time, over all threads.

On the 16-core machine, rows with one sending thread are bimodal: rerun, m1 and m2 each gave either about 0.9 or about 2.5 µs per get, whichever the mechanism, as the scheduler placed the threads (two dies of 8 cores). Differences under 2 µs there are placement, not mechanism.

### A bucket get, callback thread idle

| mechanism | threads | 16-core: thousand a second | p50 µs | p99 µs | CPU µs | Pi: thousand a second | p50 µs | p99 µs | CPU µs |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| m0 (callback thread) | 1 | 1,071–1,406 | 0.6–0.9 | 0.9–1.0 | 1.4–1.9 | 986–1,000 | 0.8 | 1.4–1.5 | 2.0 |
| m1 (ring per thread) | 1 | 435–1,025 | 0.9–2.1 | 0.9–3.0 | 2.0–4.6 | 978–983 | 0.8 | 1.4–1.5 | 2.0 |
| m1 | 8 | 1,410–1,497 | 5.1–5.3 | 6.6–6.7 | 5.5–6.0 | 1,879–2,152 | 3.6 | 5.0–7.2 | 1.7–2.0 |
| m2 (shared, leader) | 1 | 750–817 | 1.1–1.4 | 1.5–1.9 | 3.7–4.0 | 603–609 | 1.4 | 2.1–2.2 | 4.9–5.0 |
| m2 | 8 | 887–939 | 6.1–6.3 | 34.4–37.0 | 9.6–10.0 | 993–1,198 | 6.4–7.3 | 11.8–14.2 | 3.3–3.9 |
| m2r (shared, reader thread) | 1 | 574–661 | 1.3 | 3.3–4.6 | 4.5 | 596–602 | 1.6–1.8 | 2.0–2.1 | 5.0 |
| m2r | 8 | 926–1,045 | 5.7–6.5 | 31.0–34.8 | 8.5–9.6 | 1,101–1,165 | 6.2–6.5 | 11.2–12.3 | 3.3–3.6 |
| m3 (queue) | 1 | 364–732 | 1.3–2.6 | 1.8–3.7 | 4.1–8.2 | 487–493 | 1.8–2.1 | 2.7 | 6.1–6.2 |
| m3 | 8 | 629–644 | 11.3–11.7 | 25.2–26.0 | 15.2–15.6 | 1,059–1,287 | 5.7–7.5 | 9.9–12.1 | 3.1–3.8 |

### A bucket get, callback thread busy with 1 ms callbacks

| mechanism | threads | 16-core: thousand a second | p50 µs | p99 µs | Pi: thousand a second | p50 µs | p99 µs |
| --- | --- | --- | --- | --- | --- | --- | --- |
| m1 | 1 | 449–1,286 | 0.6–2.2 | 3.4–6.6 | 663–867 | 0.9–1.3 | 3.9–4.7 |
| m1 | 8 | 950–1,192 | 4.5–5.2 | 12.0–12.8 | 1,179–1,237 | 4.9–5.2 | 13.4–14.3 |
| m2 | 1 | 341–1,324 | 0.6–2.9 | 3.5–6.9 | 656–890 | 0.9–1.4 | 3.6–4.9 |
| m2 | 8 | 820–834 | 6.5–6.7 | 38.0–39.5 | 1,000–1,088 | 6.8–7.3 | 10.7–13.9 |
| m2r | 8 | 829–1,045 | 6.0–6.1 | 26.3–40.6 | 676–902 | 7.5–8.6 | 14.6–15.8 |
| m3 | 1 | 1 | 1,019–2,011 | 2,038–2,054 | 1 | 2,012 | 2,014 |
| m3 | 8 | 4 | 2,020 | 2,044–2,046 | 6–7 | 1,017 | 2,026 |

### A cast

Thousand a second and p99 µs:

| mechanism | threads | 16-core, idle | 16-core, busy | Pi, idle | Pi, busy |
| --- | --- | --- | --- | --- | --- |
| m0 | 1 | 6,866–7,485; 0.2 | | 3,534–4,392; 0.3 | |
| m1 | 1 | 4,528–7,497; 0.2–0.3 | 1,792–1,826; 1.1 | 4,397–5,521; 0.2–0.3 | 3,455–3,512; 0.3 |
| m1 | 8 | 11,664–12,309; 0.1–0.2 | 13,292–15,441; 0.6–0.7 | 6,399–14,172; 0.2 | 9,374–10,315; 0.2–0.3 |
| m2 | 1 | 4,224–4,345; 0.2–0.3 | 1,377–1,424; 1.1 | 4,158–4,180; 0.3 | 3,261–3,274; 0.3 |
| m2 | 8 | 889; 39.3–39.7 | 844–949; 39.0–40.2 | 3,725–3,945; 36.6–38.8 | 3,066–3,147; 27.6–51.7 |
| m2r | 8 | 892–974; 36.9–39.9 | 821–923; 39.7–43.6 | 3,796–4,006; 36.4–43.7 | 3,316–4,481; 0.6–36.0 |
| m3 | 1 | 1,139–3,715; 0.9–4.0 | 838–843; 0.4 | 1,774–1,787; 1.4 | 849–859; 0.3 |
| m3 | 8 | 1,188–1,193; 31.2–31.6 | 989–1,057; 32.2–33.6 | 2,493–2,538; 38.4–45.9 | 1,847–1,967; 33.1–38.9 |

A cast on m3 is a push onto the app's own queue; it reaches the node only when the callback thread drains it, so its low p99 when busy is not delivery.

### A call to another app

The other app's one callback thread answers every call, so calls level off below a million a second whatever the mechanism. p50 and p99 µs, and CPU µs per call when idle:

| mechanism | threads | 16-core, idle | 16-core, busy | Pi, idle | Pi, busy |
| --- | --- | --- | --- | --- | --- |
| m0 | 1 | 1.4–1.7; 2.0–2.5; CPU 4.5–5.2 | | 1.7–1.8; 2.6; CPU 6.0 | |
| m1 | 1 | 1.4; 2.0–5.8; CPU 4.0–5.1 | 1.9–5.6; 6.4–9.4 | 1.8; 2.7; CPU 6.0 | 5.7; 15.6–16.0 |
| m1 | 8 | 8.1–8.6; 10.5–12.5 | 7.5–9.9; 15.4–16.8 | 8.4–8.5; 11.9–13.0 | 17.8–17.9; 27.6–33.1 |
| m2 | 1 | 1.9; 3.8–5.8; CPU 8.0–8.2 | 1.8–2.4; 7.2–8.3 | 2.7; 5.8–5.9; CPU 11.1–11.7 | 1.9–5.4; 8.0–14.2 |
| m2 | 8 | 8.8–10.7; 28.4–31.5 | 6.8–10.3; 22.9–30.1 | 9.4–16.6; 15.1–22.6 | 10.9–19.4; 17.6–26.6 |
| m2r | 1 | 1.6–1.8; 2.3–5.6; CPU 7.1–8.0 | 4.0–5.4; 9.5–10.7 | 2.7; 3.2–5.8; CPU 10.5–11.8 | 5.4; 13.8–14.7 |
| m3 | 1 | 2.2–4.6; 7.6–8.2; CPU 10.4–19.9 | 1,023–1,999; 2,046–2,051 | 3.3; 5.9–6.6; CPU 13.3–13.5 | 2,012; 2,014–2,015 |

## What the node must change

The harness answers every ring from one poller. The node does not: an instance's task answers its app's requests only within a callback, one at a time, and a request between callbacks breaks the protocol (`instance_native.rs`). Whatever the mechanism, AP1 changes that:

- **Requests at any time.** A sender's frames arrive between callbacks and during them.
- **Not behind the callback.** A sender's request must not wait for the callback that is running, nor for a slow request the callback made (an SQL query, a call): served by the instance's task, it would put m3's 1 to 2 ms waits back, in the node. A task per sender channel, holding the instance's identity and handles to the services, not its `InstanceState`, is the shape to try first.
- **Replies to a sender.** A call from a sender is stamped with the instance's app name (messaging ACLs apply as to the instance), but its reply goes to that sender's channel, not to the instance's mailbox.
- **Calls in flight (DA1).** A sender may have many requests outstanding, each answered by id on its own ring; the sender's task does not await one before reading the next. Up to a limit per channel.
- **Channels as a resource.** Made on request (from a callback or from an existing sender), counted against a limit in node config and manifest, closed when the instance ends, and counted by the misbehaviour policy (PS5) as the instance's own frames are.
- **The cost of watching more rings** in the node's instance tasks, idle and busy, is AP1's to measure (`native_cost`, `cluster_throughput`), since the harness's single poller does not show it.

## What it did not measure

- Many apps each with many senders, against the node's own tasks rather than the harness's poller.
- A lock-free multi-producer ring (writers reserving space with an atomic add) in place of m2's lock: it would remove the writer's lock but not the hand-off of each answer, and the ring's layout would change for every app.
- FreeBSD: nothing here is Linux-only but epoll in the harness's poller; the descriptors m1 passes already pass on FreeBSD (FB1).
