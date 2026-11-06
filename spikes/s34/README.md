# Spike S34: a waker for a native task's rings

**Question.** A native instance's task waiting for its app's frame either yields (`yield_now`, each turn through tokio's scheduler) or watches its rings within the poll (T9), which holds its worker: T12's split found the target's task taking 2.4 to 2.6 µs to notice a reply when it yields, and watching cost 16 instances calling each other about 8%. Can the task instead free its worker and be woken when the frame arrives, within about a microsecond, on stock tokio?

**Answer: no.** Neither way of waking tried here beats yielding:
- **A watcher thread** (one OS thread scanning the waiting slots and waking each task's waker) wakes slowly: 8.8 µs for one pair on the 16-core machine against 1.2 µs yielding, the hand-off to a parked worker S32 measured. Under load it is one more spinning thread competing for cores: on the Pi with work tasks beside the pairs, its round trips fell to a few hundred a second, a p50 of one scheduler time slice (3 ms).
- **A parking worker's hook** (`on_thread_park` watching the waiting slots, after a task found its worker otherwise idle and returned `Pending`) strands tasks: one registered on a worker that then finds other work is never seen, since that worker never parks (24 round trips a second with work tasks beside one pair on the 16-core machine, 4 on the Pi); and when nothing strands it, it wakes a lone pair in 6.8 µs (2.9 on the Pi) against 1.2 (1.4) yielding. It did beat yielding on the Pi with several pairs and no other work, where this run's yielding stalled for a scheduler time slice.
- **Watching** is fastest whenever nothing else needs the worker, and starves what does: 8 work tasks beside 4 to 16 pairs ran about 200 bursts a second instead of a million.
- **Yielding** is the one that holds up everywhere.

So T8's waker is dropped; T12 shipped its watch after a callback starts (clearing the stale flag), taking its 8% for 16 busy callers (decided with the user, 2026-11-05).

## Method

`s34` (one binary) runs, per case, `pairs` pairs of a tokio task and an "app" thread on a runtime of `workers` workers. The task writes a request word and waits for the reply word; the app thread spins with `sched_yield` for the request, works for 500 ns and writes the reply. Beside them, `work` tasks loop on a 1 µs burst and a `yield_now`, standing for the node's other tasks. Per case: round trips per second, the task's p50 and p99 (from writing the request to running again with the reply), the work tasks' bursts per second, and the process's CPU in cores, for 2 s.

How a task waits:
- **yield:** check, else `yield_now`.
- **watch:** check for up to 5 µs within the poll, then `yield_now` (T9).
- **waker:** register the waker and return `Pending`; a watcher thread scans the waiting slots (spinning while any waits, `sched_yield` otherwise) and wakes each task whose reply came.
- **hybrid:** watch while no more tasks wait than there are workers, else the waker.
- **park:** yield, and when the yield came back with no other task run on the worker, register the waker and return `Pending`; the runtime's `on_thread_park` hook watches the waiting slots for up to 20 µs and wakes the tasks whose replies came (all of them at the end, to look again).

`S34_GQI=<n>` sets tokio's `global_queue_interval` (how often a worker looks at the queue where tasks woken from outside the runtime wait; 61 by default).

## Results

4 workers, 2 s a case; each machine idle at the start (load under 1). The park column is from a second run of the whole matrix the same evening; its yield column, beside it, agrees with the first run within run-to-run variation. The 16-core machine (16 CPUs), round trips per second and p50:

| Pairs, work tasks | yield | watch | waker | hybrid | park |
| --- | --- | --- | --- | --- | --- |
| 1, 0 | 536,000, 1.21 µs | 888,000, 1.05 µs | 120,000, 8.80 µs | 874,000, 1.07 µs | 165,000, 6.83 µs |
| 4, 0 | 2,121,000, 1.83 µs | 2,964,000, 1.45 µs | 705,000, 3.88 µs | 3,433,000, 1.18 µs | 1,669,000, 2.08 µs |
| 16, 0 | 2,852,000, 4.34 µs | 2,319,000, 1.48 µs | 1,675,000, 6.79 µs | 2,446,000, 6.31 µs | 3,006,000, 3.87 µs |
| 64, 0 | 4,185,000, 11.0 µs | 821,000, 4.41 µs (p99 1.6 ms) | 606,000, 36.4 µs | 260,000, 42.5 µs | 2,743,000, 11.4 µs |
| 1, 8 | 241,000, 3.60 µs | 875,000, 1.07 µs | 354,000, 2.65 µs | 869,000, 1.08 µs | 24 |
| 4, 8 | 817,000, 4.29 µs | 3,379,000, 1.30 µs | 904,000, 3.78 µs | 3,767,000, 1.08 µs | 229,000, 3.68 µs |
| 16, 8 | 1,895,000, 6.19 µs | 3,273,000, 1.07 µs | 1,028,000, 11.1 µs | 737,000, 18.4 µs | 1,377,000, 5.31 µs |
| 64, 8 | 3,314,000, 11.7 µs | 757,000, 5.00 µs | 1,357,000, 28.5 µs | 656,000, 27.1 µs | 399,000, 8.29 µs |

The work tasks' bursts per second, beside 4 and 16 pairs: about 1.0 to 1.5 million yielding, waking or parking; 81 to 219 watching (hybrid included at 4 pairs, which watches there).

`S34_GQI` at 8, 2 and 1 left the waker's 16 pairs with work at 0.57 to 0.91 million round trips a second, against 1.9 million yielding.

The Raspberry Pi 5 (4 CPUs, so the pairs' app threads, the workers and the watcher compete for 4 cores; every method's p99 reaches milliseconds from 4 pairs on, a scheduler time slice):

| Pairs, work tasks | yield | watch | waker | hybrid | park | yield, park's run |
| --- | --- | --- | --- | --- | --- | --- |
| 1, 0 | 628,000, 1.37 µs | 918,000, 1.00 µs | 317,000, 2.96 µs | 927,000, 0.98 µs | 329,000, 2.93 µs | 653,000, 1.39 µs |
| 4, 0 | 605,000, 2.46 µs | 49,000, p99 3 ms | 524,000, 6.09 µs | 254,000, 2.06 µs | 956,000, 1.41 µs | 1,600, p50 3 ms |
| 16, 0 | 610,000, 6.37 µs | 363,000, 5.07 µs | 929,000, 16.5 µs | 1,324,000, 6.54 µs | 1,139,000, 5.48 µs | 6,300, p50 3 ms |
| 1, 8 | 92,000, 2.81 µs | 439,000, 0.98 µs | 338, 3 ms | 462,000, 1.00 µs | 4 | 113,000, 2.54 µs |
| 16, 8 | 533,000, 7.46 µs | 367,000, 5.13 µs (work 120,000/s) | 21,000, 7.15 µs (p99 6 ms) | 333,000, 7.69 µs | 5,400 | 697,000, 6.82 µs |

Yielding stalled for a scheduler time slice at 4 and 16 pairs without work in park's run, and not in the first: on 4 cores every method's runs vary that way, so the Pi's rows say which methods collapse more than how fast each is.

## Rerunning

```sh
cargo build --release -p s34
target/release/s34 4 2 1,4,16,64 yield,watch,waker,hybrid,park 0   # park's matrix ran all five
target/release/s34 4 2 1,4,16,64 yield,watch,waker,hybrid,park 8
```
