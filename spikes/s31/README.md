# Spike S31: host services for native apps

**Question.** A component reaches each host service through an import, a call into the node of about 0.15 µs (S14). How should a native app, a process of its own, reach them? For each service, does it go over the app's protocol, or does the app use the OS directly with the runner fencing it? What does a request over the protocol cost, can a bucket's data be mapped into app processes instead, and what can Landlock express of the manifest's network rules?

**Answer: state the node owns goes over the protocol, files and clocks are the OS's, and sockets are opened by the node and passed to the app.**
- **A request over the protocol costs about 18 to 19 µs** on both machines, about 120 times a component's import. It suits services called rarely or with large batches: config, secrets, groups, SQL, logging, a watch's setup, a scan's pages. A bucket read per request is about 15 times a component's 1.2 µs.
- **Read-heavy bucket data can be published instead:** a snapshot the node builds into a sealed memfd and app processes map read-only. A read is then 21 to 31 ns on the 16-core machine and 69 ns on the Pi for 100,000 entries, and 100 readers share one copy (165 to 184 KiB each, the process included). This is S29's mapped snapshot without the wasmtime patch it needed for components.
- **Sockets: the manifest's rules name addresses, and Landlock cannot.** On Linux 6.8 (ABI 4) and 7.0 (ABI 8), Landlock's only network rights are binding and connecting TCP by port, with no address, and nothing for UDP. The node can connect or bind as the manifest allows and pass the socket to the app over a Unix socket (`SCM_RIGHTS`): 3 µs over the app connecting itself on the 16-core machine (contended), 20 µs on the Pi. After that the app's reads and writes are its own, at native speed. The runner's seccomp filter keeps refusing new sockets.

## Method

- **`s31-app`** is a native app on `bulkhead_native::app`, run by `bulkhead-host` through `bulkhead-runner`. Within one callback it asks the node for its own pid, or looks up a name, 50,000 times; each request is a frame to the node, answered by the host code a component's `process` import runs, and a frame back.
- **`s31-probe`** is a plain process, started by the bench directly, since today's runner passes an app only its three pipes:
  - with `snapshot`, it maps the bench's memfd read-only, touches every page and times 200,000 random gets;
  - with `fdpass`, it makes 2,000 TCP round trips (connect, a byte each way, close) to a listener of the bench's, first connecting itself, then asking the bench over a Unix socketpair for a connected socket.
- **`s31-bench`** builds the snapshot (`probe/src/snap.rs`: 32-byte records, a 16-byte key and a 16-byte value, and an open-addressed hash index of `u32`s), writes it into a memfd and seals it against writes, shrinking and growing. It runs the phases and reads the Landlock ABI from the kernel and the network rights from the system's `linux/landlock.h`.

Machines and dates:
- **16-core machine:** AMD Ryzen Threadripper 1950X, Linux 6.8, 2026-10-31. A browser tab kept load between 3 and 7, so its timings are contended.
- **Raspberry Pi 5:** Linux 7.0, 2026-10-31, at nice 10 and load 0.8.

## Requests over the protocol

| | 16-core (contended) | Pi |
| --- | --- | --- |
| its own pid, per request | 18.9 µs | 18.0 µs |
| look up a name, per request | 19.1 µs | 17.8 µs |

A request is the app's frame to the node's reader task, the instance's task, the node's writer task, the app's reading thread and back to the callback's thread: four wake-ups, as in S30's calls. S30's finding applies: fewer hand-offs (Z1) make every service cheaper, and the pipe is not the cost.

## A snapshot mapped read-only

| | 16-core (contended) | Pi |
| --- | --- | --- |
| 100,000 entries (4.1 MiB): build, write and seal | 11 to 17 ms, 1.9 to 2.6 ms | 19 ms, 2.9 ms |
| a get, one reader | 21 to 31 ns | 69 ns |
| PSS per reader: 1 / 16 / 100 readers | 4,667 / 428 / 184 KiB | 4,665 / 554 / 165 KiB |
| 1,000,000 entries (38.5 MiB): build, write and seal | 155 to 163 ms, 18 ms | 229 ms, 24 ms |
| a get, one reader | 104 to 106 ns | 121 ns |
| PSS per reader: 1 / 16 / 100 readers | 39,967 / 2,634 / 538 KiB | 39,957 / 2,633 / 539 KiB |

- **One copy serves every reader.** The snapshot's pages are counted once across the processes that map them, so at 100 readers each pays its share (42 or 394 KiB) and its own process.
- **A read is a hash lookup in memory**, as in S27's table in a component's data segment (75 ns on the 16-core machine). With 16 or 100 readers, each reader's timed loop also waited for a core (16 and 100 processes on 16 or 4 cores), so those rows' read times (up to 3.4 µs on the Pi) are not a read's cost and are left out.
- **A new version is a new memfd.** Publishing one costs a build (about 0.1 to 0.25 µs per entry here) and a pass of the new descriptor to each reader, which then maps it and unmaps the old one between callbacks. Not measured: the passing to many readers, and how stale a reader may be.
- **Only for data read far more than written:** every change rebuilds the snapshot, or a delta scheme is needed.

## Sockets the node opens

| TCP: connect, a byte each way, close | 16-core (contended): p50 / p99 | Pi: p50 / p99 |
| --- | --- | --- |
| the app connects | 60.6 / 110.7 µs | 38.8 / 58.0 µs |
| the node connects and passes the socket | 63.9 / 104.1 µs | 58.6 / 82.1 µs |

The node's side here is a thread answering a socketpair directly, not the node's tasks; through an instance's request it would add S30's hand-offs.

## Landlock

| | 16-core | Pi |
| --- | --- | --- |
| kernel, Landlock ABI | 6.8, 4 | 7.0, 8 |
| network rights | `BIND_TCP`, `CONNECT_TCP` (by port) | the same |
| scopes | none | `ABSTRACT_UNIX_SOCKET`, `SIGNAL` |

The manifest's `connect` and `bind` rules give an address range and ports, for TCP and UDP. Landlock can enforce the ports of TCP only. A network namespace with no interfaces (no network at all) plus sockets the node passes would enforce the rules completely; seccomp cannot look at a `connect` call's address, which is behind a pointer.

## Each service

| Service | Path | Why |
| --- | --- | --- |
| Keyvalue and watch | the protocol; mapped snapshots for read-heavy shared data | The node owns the data, its revisions, quotas and replication. Per-request reads cost about 19 µs, so batch them (`get_many`, scan pages of up to 1,000) or map a snapshot. A watch notice should carry the value (S28's lesson, now a frame per change). |
| Groups, config, secrets | the protocol | Rare calls; secrets stay with the node until asked for. |
| Metrics | the protocol, batched by the SDK | A frame per counter increment would cost 19 µs; the SDK should send a callback's updates together. |
| Logging | the protocol, as today | Already a frame from any thread. |
| SQL | the protocol | The node's pool and credentials; a query's milliseconds dwarf the request. |
| HTTP egress | the protocol, or a socket the node connects and passes | The node checks the name and resolves it; with a passed socket the app speaks TLS and HTTP itself. |
| Sockets | sockets the node connects or binds and passes | Landlock cannot express the rules; a passed listener lets the app accept at native speed. Connected UDP only, since a passed unconnected socket could send anywhere. |
| Files | the OS, fenced by Landlock | Path rules are what Landlock does; the runner already gives the app its `priv/` files read-only. |
| Clocks and random | the OS | Nothing to mediate. |

Passing descriptors needs the protocol on a Unix socket (a socketpair) rather than pipes. The cost of that change was not measured.

## What it changes

- **Z2 carries node-owned services over the protocol, batched where a service is called often.**
  - Keyvalue gets mapped snapshots for read-heavy data.
  - Watch notices carry the value.
  - The SDK batches metric updates.
- **Z1's hand-off work is also the services' cost:** each request is S30's call path.
- **The protocol moves to a socketpair,** so the node can pass sockets and snapshot descriptors (Z1 or Z2).
- **Z8's sockets and HTTP egress are sockets the node opens and passes.** A network namespace with no network could make the runner's fence complete.
- **The keyvalue track:** S29 and H10 (a wasmtime patch for mapped snapshots) are not needed for native apps; a mapped snapshot is ordinary memfd sharing here.

## How to run

```sh
cd spikes
cargo build --release -p s31-app -p s31-probe -p s31-bench
(cd .. && cargo build --release -p bulkhead-native --bin bulkhead-runner)
target/release/s31-bench                      # requests snapshot fdpass landlock
target/release/s31-bench snapshot fdpass
```
