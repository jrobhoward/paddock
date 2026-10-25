# Spike S1: callback model on wasmtime async components

**Question.** Can a guest export (`handle-call`) call a host import (`call`) that suspends for a long time without holding a thread, while a per-callback deadline can still kill runaway guest code?

**Answer: yes.** All eight experiments pass with every instance sharing **one** tokio worker thread. The design's callback model works as written, with two constraints listed below.

Run on 2026-10-05 with wasmtime 49.0.2, wasmtime-wasi 49.0.2 (p2 only), wit-bindgen 0.62, rustc 1.99, Linux x86_64.

Rerun on 2026-10-25 with wasmtime 49.0.4 and with 50.0.0 (wit-bindgen 0.63): the same results, except that E3's spinner is killed at 702 to 704 ms on both versions, not about 500 ms (`docs/BASELINE.md`, Y1).

## How to run

```sh
cd spikes
cargo build -p s1-guest --target wasm32-wasip2 --release
cargo build -p s1-host --release
./target/release/s1-host target/wasm32-wasip2/release/s1_guest.wasm
```

Environment knobs: `WORKERS=n` (default 1), `PLAIN_YIELD=1`, `REUSE_AFTER_DROP=1`, and `TRACE_YIELD`, `TRACE_PROBE`, `TRACE_RECV` for tracing.

## What the spike does

- `wit/world.wit`: a stand-in for `paddock:runtime`. The host exports `call`, `sleep` and `log`; the guest exports `handle-call`. Guest WIT is **synchronous**; only the host side is async.
- `guest/`: a Rust component (`wasm32-wasip2`) whose behaviour is chosen by message prefix: `echo`, `sleep`, `spin` (infinite loop), `forward` (calls another instance through the host), `panic`.
- `host/`: one actor task per instance owns a `Store` and drains a bounded mailbox one message at a time. It includes a name registry, async host imports, an epoch ticker (10 ms), a per-callback deadline (500 ms), and restart on trap with an incarnation counter.

## Results (one worker thread)

| # | Experiment | Result |
| --- | --- | --- |
| E1 | a → host `call` → b | `ok(hi)`, under 1 ms |
| E2 | a waits 2 s on b's host `sleep`; 100 probe calls to a third instance meanwhile | 100/100 probes OK, worst latency under 1 ms; a succeeds after 2000 ms even though its deadline is 500 ms |
| E3 | Guest spins forever; 20 probes meanwhile | Probes 20/20, worst 1 ms; spinner trapped (`interrupt`) at about 500 ms and restarted; next call served |
| E4 | a calls a spinner | a receives `err(crashed: ... interrupt)` after 504 ms |
| E5 | a calls b with timeout 300 ms, b takes 1500 ms | `err(timeout)` at 301 ms; b's late reply is dropped; b stays healthy |
| E6 | Guest `panic!` | Trap (`unreachable`); instance restarted; next call served |
| E7 | Host drops the call future while the guest is suspended (hard cap 1 s) | Caller gets `killed`; a fresh instance serves the next call |
| E8 | Call to an unknown name | `err(noproc)` immediately |

## Findings

1. **Async host imports + sync guest ABI is enough.** wasmtime runs each async call on a fiber, so a guest blocked in a host import costs no thread. Guests need no async code and no WASI 0.3. This is the basis for S3's decision.
2. **Deadline accounting.** Epoch checks only happen while wasm runs. The host records time spent inside its own imports (`host_time`) and the epoch callback compares `elapsed - host_time` to the deadline, so waiting on another instance doesn't count as runaway code. Yielded time still counts, so a heavily loaded node will kill slightly early. Phase 1 can accept that or count ticks instead.
3. **Cooperative preemption works.** The epoch callback returns `Yield` each tick, which gives BEAM-like fairness at 10 ms granularity: a spinning guest never starves others.
4. **Constraint: don't host instances on `current_thread` + `LocalSet`.** In that setup the top-level future and tokio timers were starved while a guest yielded every tick (probes stalled for the full 500 ms). On the multi-threaded runtime, even with one worker, both `Yield` and `YieldCustom(tokio::task::yield_now())` are fair. Paddock should use the multi-threaded runtime.
5. **Killing is safe.** Dropping an in-flight call future poisons the instance: the next entry traps with `cannot enter component instance`, with no panic or UB. So "kill" = drop the future and discard the Store. Never reuse a Store after a dropped call.
6. **`wasmtime::Error` is no longer `anyhow::Error`** (as of 49). Host code should use `wasmtime::Result` or convert at the boundary.
7. **`Config::async_support` is deprecated and does nothing**: async is always available. `bindgen!` takes `imports: { default: async | trappable }, exports: { default: async }`.
8. The guest component is about 2.9 MB with `debug = 1`. Size and instantiation cost are S2's job.

## Effect on the design

- Keep the callback world and sync guest WIT.
- `callback_deadline_ms` = guest execution time excluding host-import time; implemented with epoch callback + `Yield`.
- An instance kill is always "drop and rebuild"; supervisors never try to resume a killed Store.
- Host runtime: tokio multi-threaded; one task per instance.
