# Spike S32: a channel that spins, then sleeps

**Question.** A native app's request to the node costs 11 to 16 µs and a call between two native instances about 41 µs (BASELINE.md, 2026-11-01, C4). Both sit at the floor of a blocking round trip between processes, two wake-ups of a sleeping thread (S30). Can a shared-memory channel get below that floor if each waiting thread spins briefly before it sleeps, and what does it cost? The questions:
- what spin budget is needed, and with which kind of spin;
- what the CPU costs while busy and while idle;
- how it holds up with more active apps than cores;
- where the node should handle the frames.

**Answer: yes, with a budget of 10 to 20 µs, spinning with `sched_yield`, and the native instances' tasks on the poller's own thread.**
- **Latency at a 20 µs budget, against blocking waits on the same channel** (16-core machine / Raspberry Pi 5):
  - a request: 2.7 / 1.0 µs, against 19 / 7;
  - a bucket get: 3.2 / 1.7 µs, against 19 / 8;
  - a call between instances: 4.9 / 2.2 µs, against 41 / 15.

  Both of T1's targets are met: a request under 3 µs and a call under 10.
- **Spinning costs less CPU per operation than sleeping:** 5.7 against 19 µs per request on the 16-core machine, 2.1 against 8.2 on the Pi. A wake-up costs more than the spin it replaces.
- **Idle is free:** 100 waiting apps and the node use 3 to 6 ms of CPU over 2 s at every budget. The spinning stops when traffic does.
- **Every hand-off to a sleeping thread costs a wake-up, spinning or not.** A frame passed to a task on the node's multi-threaded runtime pays 2 to 3 µs on the Pi and 3 to 13 µs on the 16-core machine, whose tokio workers sleep between frames. A call between instances through that runtime costs 10 to 60 µs, however long the apps spin.
- **What avoids those hand-offs:** a current-thread tokio runtime whose poller task spins with `yield_now`, with the instances' tasks spawned on it. A hand-off becomes a queue push on one thread. That keeps nearly all of what answering inside the poller gains (2.7 against 3.2 µs per request on the 16-core machine, 1.0 against 0.6 on the Pi), so the instances' async code can stay as it is.
- **With more active apps than cores, `sched_yield` holds up and a pause instruction does not:**
  - **With `sched_yield`:** 0.9 to 1.3 million requests a second from 16 or 64 apps on both machines, against 0.4 million for blocking waits.
  - **With a pause instruction:** it collapses on the 16-core machine (0.4 million requests a second with 64 apps), and with the current-thread runtime on the Pi (0.2 to 0.5 million).
- **The budget must outlast a wake-up.** Below that the system is bistable: once one side sleeps, the other side's spin runs out while it waits for the wake, and every round trip pays it. A 5 µs budget was bistable on the 16-core machine (C-states; a wake costs about 20 µs) and 2 µs on the Pi. From 10 µs both machines stayed fast.

## Method

`s32 bench` (one binary) plays the node, and `s32 app ...` (the same binary, started by the bench) plays each app.

**The channel.** Each app has one memfd, inherited at spawn, holding two single-producer, single-consumer frame rings: node to app and app to node. They are S30's rings with:
- **a futex word per ring:** a consumer about to sleep sets it, and a producer that finds it set clears it and wakes the consumer;
- **a reader that trusts nothing the other side wrote:** the positions and the frame length are checked before anything is copied (`Corrupt` ends the app).

**The app side** waits for the node's frame by spinning up to the budget, then sleeping on the futex in the shared mapping. It rings an eventfd when the node's poller sleeps. The kinds of spin:
- **pause:** a spin-loop hint;
- **yield:** `sched_yield` each turn;
- **hybrid:** a pause with a yield every 32nd turn.

**The node side** has one poller thread, which sweeps every app's ring to the node. It spins up to the budget after the last frame, then marks every ring and sleeps in `epoll_wait` on the apps' eventfds. It handles each frame in one of three ways:
- **Direct:** the poller answers the frame itself.
- **Task:** the poller sends the frame through an mpsc to the instance's task on a 4-worker multi-threaded runtime, as today's instance tasks run.
- **Local:** the poller is a task on a current-thread runtime of its own that spins with `yield_now`, and the instances' tasks run on the same runtime. It sleeps by awaiting the epoll fd through the runtime's I/O driver.

**What each app does:**
- a request: the app asks for its own pid;
- a bucket get: a 16 B value from a `HashMap` of 100,000 keys behind an `RwLock`;
- a call between instances: app A sends a call, the node delivers it to app B, B replies, and the node delivers the result to A;
- a call from the node: a task on the multi-threaded runtime calls an app, and the poller completes a oneshot.

**The runs:**
- **Sequential:** 20,000 rounds after 2,000 of warm-up, timed in the app. CPU is the node's and the apps' over the whole run, per round.
- **Concurrent:** 16 and 64 apps, each making 20,000 requests at once.
- **Idle:** 100 apps waiting, with CPU measured over 2 s after the last frame.

Budgets of 0 (sleep at once), 2, 5, 10, 20 and 50 µs.

**Machines:**
- **16-core machine:** Threadripper 1950X, Linux 6.8, 2026-11-01. The final passes started at load 1.7 and 8.5; the spinning runs themselves raise the load average, and the second pass started right after the first. Earlier passes ran at 0.5 to 3.9.
- **Raspberry Pi 5:** Linux 7.0, nice 10 and idle I/O priority, starting at load 0.3 and 6.4, for the same reason.

The results directory has every pass. The tables below are from the final passes, `16core-final-*.txt` and `pi-final-*.txt`.

## Sequential

p50 µs, at a 20 µs budget against 0 (blocking waits); CPU µs per round, node and apps together, in brackets. The apps spin with `sched_yield`:

| | 16-core: blocking | Direct | Task | Local | Pi: blocking | Direct | Task | Local |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| request (own pid) | 19.5 (19.1) | 3.2 (5.9) | 5.5 (18.3) | 2.7 (5.7) | 7.2 (8.2) | 0.61 (1.6) | 3.1 (10.5) | 1.0 (2.1) |
| bucket get | 19.3 (18.9) | 3.0 (6.5) | 5.9 (25.6) | 3.2 (6.7) | 8.1 (9.2) | 1.2 (2.6) | 4.1 (15.8) | 1.7 (3.3) |
| call between instances | 41.0 (39.9) | 4.6 (13.7) | 23.6 (91.7) | 4.9 (14.9) | 14.6 (16.4) | 1.5 (4.9) | 10.0 (41.7) | 2.2 (6.3) |
| call from a task on the multi-threaded runtime | 27.2 | 10.1 | | | 11.7 | 3.7 | | |

"Blocking" is the Direct path at a budget of 0.

- **p99 stays close to p50 at 20 µs:** 3.4 µs for a request on the 16-core machine and 1.6 on the Pi, and 7.0 and 2.7 for a call between instances (Local).
- **With a pause instead of a yield:**
  - **16-core machine:** about the same sequentially (Local: 2.8, 3.0 and 3.8 µs).
  - **Pi:** faster (Local: 0.91, 1.2 and 2.1 µs; Direct: 0.37, 0.85 and 0.78 µs).
- **The Task path stays slow at every budget.** Each frame wakes a tokio worker, and the call between instances crosses two. At 50 µs the workers sometimes still spin from the last frame (15 µs on the 16-core machine), but not reliably.
- **A call from the multi-threaded runtime** pays one cross-thread wake for its reply: 10 µs and 3.7 µs. That is the path a component, HTTP or another node takes into a native instance. Today's equivalent costs 13 to 17 µs from the host (C4).

## Concurrent

Requests per second from 16 and 64 apps each making 20,000, at a 20 µs budget; p50 of the apps' p50s and the worst app's p99 in µs:

| | 16-core: 16 apps | 64 apps | Pi: 16 apps | 64 apps |
| --- | --- | --- | --- | --- |
| blocking (Direct, budget 0) | 0.47M; 33 / 39 | 0.39M; 168 / 185 | 0.42M; 38 / 47 | 0.40M; 157 / 249 |
| Local, yield | 0.94M; 16 / 38 | 1.04M; 58 / 132 | 1.29M; 11 / 50 | 0.98M; 55 / 2,995 |
| Direct, yield | 1.22M; 13 / 17 | 1.34M; 47 / 89 | 1.06M; 14 / 27 | 0.92M; 57 / 135 |
| Local, pause | 0.61M; 16 / 80 | 0.39M; 164 / 353 | 0.51M; 1.9 / 213 | 0.21M; 291 / 1,464 |
| Direct, pause | 0.83M; 13 / 65 | 0.42M; 150 / 192 | 4.09M; 0.59 / 0.9 | 2.36M; 0.98 / 25 |

- **One poller carries about a million requests a second.** On the 16-core machine, its sweep across 16 busy rings takes about 12 µs. Each frame there costs several cache lines crossing the Threadripper's dies, against 4 to 5 million on the Pi with a pause. Giving the apps a lower priority than the poller (nice 5) changed nothing, so it is not the poller losing its core.
- **Yielding while spinning is what keeps oversubscription working on the 16-core machine.** The hybrid spin (a yield every 32nd turn) did no better than a pause.
- **The pause instruction gives the Pi's Direct path its 4 million a second, but collapses wherever it shares a core with the current-thread runtime.**
- **Worst p99s near 3 ms** with 64 apps on 4 or 16 cores are a spinning app losing its core for a scheduler slice.

## What it changes

- **T1's shape:**
  - **The channel:** S32's rings in a memfd, with each waiting side spinning with `sched_yield`, then sleeping (a futex in the app; epoll on the apps' eventfds in the node).
  - **Node config:** `[native] spin_us`, default 20.
  - **Where the instances run:** the native instances' tasks move to a current-thread runtime, with the poller as its own task that spins with `yield_now`.
  - **CLAUDE.md:** this changes its rule against `current_thread` runtimes for native instances only. That rule protects WebAssembly guests, whose code runs inside the task (S1); a native app's code runs in its own process.
- **T2 (answering requests inside the poller) is not worth a separate design:** the current-thread runtime keeps nearly all of its gain. It stays a possible later step, for the Pi's 0.6 against 1.0 µs.
- **Capacity:**
  - **One poller runtime:** about a million requests a second.
  - **Beyond that:** more runtimes, each with its share of the instances, a later step measured when needed.
  - **Calls into a native instance from the multi-threaded runtime** pay one wake each way (10 µs and 3.7 µs).
- **SECURITY.md:** the node reads rings an app can write; the reader's checks are the boundary, and a ring that breaks them ends the app. The poller's spin is bounded by the budget and stops with the traffic.

## How to run

```sh
cd spikes
cargo build --release -p s32
target/release/s32 bench                                   # budgets 0,2,5,20,50 µs; 16 and 64 apps; pause
target/release/s32 bench 0,10,20,50 16,64 yield pause,yield   # the final passes: sequential with yield
target/release/s32 bench 0,10,20,50 none pause                # sequential with a pause, no concurrent cases
S32_APP_NICE=5 target/release/s32 bench 5,20 16,64 pause pause,yield   # apps at a lower priority
```

Arguments: budgets in µs; the concurrent cases' app counts (`none` skips them); how the sequential cases spin (`pause`, `yield`, `hybrid`); and which spins the concurrent cases try.
