# Spike S11: passivation

**Question.** Can an idle instance be passivated (snapshot, drop the Store, rebuild on the next message) without changing what other instances see? What does a wake cost, how many instances fit in a node's address space with and without it, and when must an instance stay live?

**Answer: yes, as a host feature, off by default.** Unlike the other spikes, the prototype is in `paddock-host` itself (decided 2026-10-15), because what other instances see depends on the real instance loop:
- `InstanceSpec::passivate_after` opts an instance in.
- `RuntimeConfig::max_processes` lets a node hold more instances than Store slots.

Neither is in manifests or node config yet (ROADMAP, follow-up below).
- **Same pid and state.** A passivated counter keeps its pid, incarnation, registered name, links and timers. A call, a cast, a timer firing and a trapped exit signal each wake it, and it carries on from its count. No increment was lost in any run: 140,847 of 140,847, spread at random over 100,000 instances on 1,000 Store slots.
- **Wake cost.** About 64 to 101 µs at p50 for a call that wakes its instance (instantiate plus `load-snapshot`), against 6 to 10 µs for a call to a live instance. S5's swaps took 0.2 to 0.35 ms, but those included `save-snapshot` and a version change.
- **Address space, not RAM, was the limit, and passivation lifts it.** The pooling allocator reserves address space for `max_instances` Stores when the runtime starts: about 8 GiB each with the standard memory profile, so about 15,000 per node (S2). A passivated instance holds no Store, so the node holds `max_processes` instances on `max_instances` slots: 100,000 on 1,000 here. RAM is then the limit: about 25 KiB per passivated instance against 69 KiB per idle live one.
- **Eviction under pressure is needed.** With only idle timers, a woken instance keeps its slot for `passivate_after` (20 ms here), which caps calls that wake instances at slots ÷ idle time: 4,908 calls per second with 100 slots, the 5,000 that predicts. An instance that needs a slot now asks one idle instance to passivate at once, and the instance idle longest goes first. With that, about 29,000 to 31,000 calls per second, nearly each waking its instance.

Measured on 2026-10-15 with wasmtime 49.0.3 and rustc 1.99, on an AMD Threadripper 1950X (16 threads, 63 GiB, Linux 6.8), idle (`scripts/bench.sh passivation`; `docs/BASELINE.md`, 2026-10-15). The idle-timer-only row was taken earlier the same day under load (TLC, load average 12 to 15), before eviction under pressure was added.

## How to run

```sh
# The behaviour (cast, call, timer, exit signal, deferred call, snapshot,
# stop, more instances than slots, eviction under pressure):
cargo test -p paddock-host --test passivation

# The costs:
cargo build --release --target wasm32-wasip2 -p paddock-fixture-counter
cargo run --release -p paddock-host --example passivation_cost -- 4 1000 100000
cargo run --release -p paddock-host --example passivation_cost -- 4 100 2000
cargo run --release -p paddock-host --example passivation_cost -- 4 1000 1000 off
```

Arguments: tokio workers, Store slots (`max_instances`), instances; `off` runs without passivation. The guest is `paddock-fixture-counter` v1, whose snapshot is its count in decimal. Instances are passivated after 20 ms idle. The program prints each phase:
- memory with every instance passivated;
- a call to each of 2,000 passivated instances, then a second call while it is live;
- 64 callers calling `inc` on instances picked at random for 5 s;
- a check that every increment is in some instance's count.

## How it works

- **Passivating.** After `passivate_after` with nothing to do, the instance runs `save-snapshot` (within its callback deadline). It turns the Store back into its data (`Store::into_data`) and drops it, which returns the Store to the pooling allocator.
  - **Kept host-side:** the data (WASI context, timers, waiting callers, the services' state) and the snapshot bytes. The pid, mailbox, links, monitors and registered names live outside the Store anyway.
  - **Released:** the instance's slot. Its state is `Passive` and its memory reads 0.
  - **Something arrived during `save-snapshot`:** the instance stays live, and the snapshot is discarded.
- **Waking.** The first message, signal or operation wakes the instance.
  - **Steps:** it takes a slot, builds a Store on the kept data, instantiates the pre-compiled component, and runs `load-snapshot(own version, bytes)` within `init_timeout`. Then it handles what woke it.
  - **Host events:** sent again, as after an upgrade, since the new guest's statics are fresh.
  - **A failed wake:** ends the instance (`crashed`), and its state is lost.
- **Slots.** A semaphore holds one permit per Store the pooling allocator has room for.
  - **Who holds one:** an instance while it has a Store, and a connection handler for its connection. A handler finding none ends the connection, as a full allocator did before.
  - **Waiting:** an instance that finds none asks an idle, passivable instance to passivate now, again every 10 ms, until a permit frees. tokio's `Notify` wakes waiters in the order they began waiting, so the instance idle longest goes first.
  - **Without passivation:** instances never wait, since the process table holds no more instances than slots.
- **Call references** are now unique for the instance's whole life, not per Store. A guest woken from a snapshot may still hold the reference of a call whose caller gave up, and could otherwise answer a new caller with it.

## When an instance stays live

1. **Its app does not opt in.** `load-snapshot` must read the app's own snapshots. The SDK's default refuses every snapshot, so an app that has not written one would lose its state at the first wake. Opting in is the app's decision: a manifest field in the follow-up, not a node-wide setting.
2. **A caller waits on a deferred reply** (`noreply`, answered later with `process.reply`). Callers that gave up do not count.
3. **The guest holds a host resource**, such as an open bucket or a stream. A new guest could not name it. The SDK's `Bucket::open` per use leaves none; one kept in the app's state keeps the instance live. Connection handlers own their streams and are never passivated.
4. **A pending outbound call needs no rule.** Calls are synchronous inside a callback, and an instance is passivated only between callbacks.

What wakes a passivated instance, and what does not:
- **Wakes it:** any mailbox message, a timer, a trapped exit or `down` notice, a host event (a new log level, a failover role, a watch notice), an upgrade, and a graceful stop (to run `terminate`).
- **Answered without waking:** a `snapshot` operation (`paddock state`), from the kept bytes.
- **Ends it without waking:** a kill, or an untrapped exit signal.
- **Health checks:** each is a call, so an interval shorter than `passivate_after` keeps the instance live.

## Results

4 tokio workers; passivated after 20 ms idle.

| Run | Slots | Instances | RSS each | Wake call p50 / p99 | Live call p50 / p99 | Random calls per second (p50) |
| --- | --- | --- | --- | --- | --- | --- |
| No passivation | 1,000 | 1,000 | 69 KiB | none | 3 / 5 µs | 381,689 (111 µs) |
| Idle timer only (under load) | 100 | 2,000 | 26 KiB | 67 / 2,204 µs | 6 / 226 µs | 4,908 (14.2 ms) |
| Eviction under pressure | 100 | 2,000 | 24.5 KiB | 101 / 145 µs | 10 / 13 µs | 30,871 (2.1 ms) |
| Eviction under pressure | 1,000 | 100,000 | 25.0 KiB | 64 / 253 µs | 6 / 18 µs | 29,433 (1.8 ms) |

- **Address space.** With the standard profile it was about 8,259,000 MiB (about 7.9 TiB) for 1,000 slots, and about 806 GiB for 100, whatever the number of instances. Each passivated instance added about 25 KiB.
- **Random calls under passivation:** nearly every one wakes its instance and passivates another. The rate is bounded by the four workers running `save-snapshot`, instantiation and `load-snapshot`, not by slots.
- **What the 25 KiB holds:** not measured. Candidates are the instance task's future, its channels, the WASI context and the services' state.

## Follow-up (ROADMAP)

- **Expose it.** The manifest opts an app in (`instances.passivate_after_ms`), and node config sets `max_processes`. Metrics: instances passivated, wakes, wake time. `ps` already shows `Passive`.
- **Starvation.** If every slot is held by instances that cannot passivate, an instance that needs one waits until one frees. That includes one starting its `init`. A node with `max_processes` above `max_instances` needs enough passivating instances.
- **Upgrades.** An upgrade's second Store takes no slot, so with every slot taken it can fail to instantiate, and it reverts.
- **Untested:** an SDK app (needs `Snapshot`), watches and groups across a passivation. Watches and group membership are kept host-side by pid, so they should carry over.
- **Shared mechanism.** S17, moving an instance to another node, is this path with the snapshot and kept state sent over the cluster.
