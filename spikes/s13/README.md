# Spike S13: host-owned listeners, connections as guest streams

**Question.** Can the host own a listener, keep it running across guest crashes and upgrades, and hand each accepted connection to a guest as streams? What does a stream-per-instance guest cost against native code?

**Answer: yes, at native throughput.** With an instance per connection, reading and writing through `wasi:io` streams, the guest matched native hyper from 4 to 64 connections (158,616 against 157,901 requests/s at 32 connections). A new connection costs about 40–65 µs more than native, the price of instantiating. A trapping guest loses only its own connection, and swapping the component under load lost no requests.

Run on 2026-10-07: AMD Ryzen Threadripper 1950X (16 cores), Linux 6.8, wasmtime 49.0.2 (pooling allocator), 4 server workers; the load generator runs on its own 4-worker runtime. Two runs, consistent; the second is shown.

## How to run

```sh
cd spikes
cargo build --release --target wasm32-wasip2 -p s13-guest
cargo build --release --target wasm32-wasip2 -p s13-guest --features v2 --target-dir target/s13v2
cargo build --release -p s13-bench
./target/release/s13-bench 4
```

## Method

- `wit/conn.wit`: a world exporting `serve(input: input-stream, output: output-stream)`, the `wasi:io` stream types.
- `guest/`: serves HTTP/1.1 over the two streams with blocking reads and writes until the client closes; `GET /crash` traps; the `v2` feature answers `ok2` instead of `ok`.
- `bench/`: binds one TCP listener and runs 4 accept tasks on it (the Ranch pattern). Each accepted connection gets a new Store and instance from a pre-instantiated component; the socket's halves become the guest's streams (`AsyncReadStream`, `AsyncWriteStream`). The current component sits behind a lock and can be swapped at any time. Native hyper serves the same responses for comparison.

## Results

Throughput, keep-alive connections, 2 s each:

| Connections | Native hyper | Host listener → guest streams |
|---|---|---|
| 1 | 20.7k req/s (p50 50.6 µs) | 27.8k (33.5 µs) |
| 4 | 83.4k | 72.7k |
| 8 | 103.8k | 89.2k |
| 16 | 142.3k | 133.1k |
| 32 | 157.9k | 158.6k |
| 64 | 183.1k | 176.3k |

A new connection per request, 2,000 sequentially: native p50 155 µs, guest 195 µs (first run: 130 against 202 µs).

Guest crashes: 1,000 requests on new connections, every 100th asking the guest to trap: 990 answered, 10 failed, 10 traps. The listener and the other connections were unaffected.

Upgrade under load: 8 clients making a new connection per request for 2 s, v2 swapped in halfway: 17,630 answered by v1, 12,397 by v2, none failed. The swap held the lock for 0.3 µs; v2 was compiled beforehand, off the request path.

## Reading the results

- **Streams cost about nothing at throughput.** An instance per connection behaves like hyper's task per connection: each is a small independent unit, spread over the workers. The S15 slowdown, where one instance holding 32 connections managed 7.3k requests/s, does not occur, because each instance polls only its own two streams.
- **Single-connection latency** differs by run-to-run noise (S15 saw native vary 45–61 µs); the guest was not slower.
- **Connection setup** costs an instantiation (about 40–65 µs here). Clients that reuse connections pay it once; a protocol with a connection per request pays it every time.
- **Crash isolation** is per connection: a trap ends that instance's Store and closes its socket; nothing else notices.
- **Upgrades** apply to new connections at once; connections already open keep the version they started with until they close, as Erlang keeps old code for running processes. Draining long-lived connections (a timeout, or a host event asking the instance to finish) is a design choice for F2/F3.

## What it changed

- Guest streams are cheap enough: connection handlers go ahead, with an instance per connection as the default policy.
- A connection handler does not fit the `gen_server` callbacks: `serve` runs for the connection's life. Connection handlers need a separate world for connection handlers (with `wasi:io` streams, which `paddock:runtime` excludes for the WASI 0.3 migration; in 0.3 they become `stream<u8>`), its own deadline rules (time blocked in stream reads is host time; a per-request or idle timeout rather than a per-callback deadline), and supervision per connection (a crash closes the connection; no restart).
- Listener ownership in the host works as designed: crashes and upgrades never touched the listener.
- Not attributed: why one instance with many connections slows down (S15). `perf` needs `kernel.perf_event_paranoid` of 2 or less for a normal user; it was 4. Moot for an instance per connection.
