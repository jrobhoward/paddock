# Spike S15: HTTP in the guest versus in the host

Run 2026-10-07, same machine as S14 (Threadripper 1950X, Linux 6.8), wasmtime 49.0.2, 4 server workers; the load generator runs on its own 4-worker runtime. Three runs; ranges below.

```sh
cd spikes
cargo build --release --target wasm32-wasip2 -p s15-proxy -p s15-client -p s15-tcpserver
cargo build --release -p s15-bench
./target/release/s15-bench 4
```

## Ingress (a native hyper client calls the server; body `ok`)

| Server | 1 connection p50 | 1 conn req/s | 32 connections req/s |
|---|---|---|---|
| native hyper | 47–66 µs | 15k–19k | 150k–153k |
| host hyper → Paddock instance call (likely E1 shape) | 64–101 µs | 10k–14k | 114k–115k (about 75% of native) |
| `wasi:http/incoming-handler`, new instance per request (`wasmtime serve` model) | 164–193 µs | 5.1k–5.8k | 21k–23k (about 15%) |
| guest owns the socket (`wasi:sockets`, guest parses HTTP, one instance) | 53–65 µs | 15k–17k | about 7.5k (one single-threaded instance) |

## Egress (the client calls a native hyper server)

| Client | Per request |
|---|---|
| native hyper, one keep-alive connection | 48–57 µs p50 |
| native hyper, new connection per request | 184–195 µs p50 |
| guest, `wasi:http/outgoing-handler` (wstd Client) | 148–206 µs mean |
| guest, `wasi:sockets`, one keep-alive connection, guest parses HTTP | 32–33 µs mean |

## Reading (provisional)

- One-connection latency on localhost is dominated by task wake-ups across threads, not by Wasm; the guest raw-socket client beating native hyper (32 vs 50 µs) reflects hyper's separate connection task, not a faster sandbox.
- Host-terminated HTTP plus an instance call costs about 25% of native throughput: the design E1 should take.
- `wasi:http` incoming with an instance per request costs about 100 µs per request and 85% of throughput, mostly instantiation and the wasi:http resource machinery.
- A guest that owns its socket is fine for one connection but caps at one core (about 7.5k req/s here, with wstd's reactor polling every connection each turn).
- `wasi:http` outgoing opens a new connection per request (wasmtime-wasi-http's default sender), so it costs what a native new-connection request costs; the boundary adds little. Connection pooling in the host sender would matter more than the sandbox.
