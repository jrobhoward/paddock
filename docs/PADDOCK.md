# Bulkhead and Paddock

Bulkhead is a fork of Paddock, made on 2026-10-31 at Paddock's commit 657ae61. Both are clustered application servers with Erlang/OTP-style semantics.
- **Paddock** runs each app instance as a WebAssembly component inside the node.
- **Bulkhead** runs each instance as a native OS process. It keeps components beside native apps until those reach parity, and then drops them (ROADMAP Z8 and Z9).

This page says what the two share, where they differ, and what the differences cost. The figures are BASELINE.md's from 2026-11-01, on a 16-core machine and a Raspberry Pi 5. "Component" figures are Bulkhead's component path, the runtime Paddock runs.

## What is the same

- **The app model:** `gen_server`-style callbacks, one message at a time. Call (with a timeout) and cast, locally or across nodes. Links, monitors, supervisors and exit reasons.
- **Upgrades:** hot upgrades with state carried through `save_snapshot` and `load_snapshot`, reverted if the new version refuses it, and blue-green upgrades.
- **Manifests and grants:** capabilities each manifest declares and an operator grants, signed `.rbp` bundles verified against operator keys, the `bulkhead` (`paddock`) CLI and admin API.
- **The cluster:** QUIC with mutual TLS and pinned keys, an HA pair and a witness with leases, replicated buckets, singletons, releases across the pair.
- **The Rust SDK:** one source builds as a component and, in Bulkhead, as a native program.

Paddock as forked already had a `native-process` kind: rustbridge plugins and executables run as ports over pipes. It could declare logging and nothing else, had no snapshots or passivation, and a call cost about 60 µs on the Pi. Bulkhead grew it into the main kind (ROADMAP Phase 5, the native-costs track).

## Where they differ

| | Paddock: a WebAssembly component | Bulkhead: a native process |
| --- | --- | --- |
| An instance | A wasmtime Store inside the node's process | A process of its own, started through `bulkhead-runner` and kept for the instance's life |
| Languages | Anything that builds a WASI 0.2 component: Rust on `bulkhead-sdk`, Go on TinyGo, and Python and JavaScript demos | Rust on `bulkhead-sdk` or `bulkhead-native`, and rustbridge plugins; another language needs an SDK speaking the protocol (a Go one is planned, Z8j) |
| Where it runs | Any node: Linux or macOS; one bundle runs everywhere | Linux only, x86-64 and aarch64; a bundle carries a program per platform (Z8a) |
| Fence | WebAssembly's sandbox: linear memory with a limit, and only the imports its grant links | Landlock (files), seccomp (no new sockets, no signals or ptrace to others), not dumpable; memory and process limits from a cgroup or `RLIMIT_DATA`; the rest of the kernel's interface is reachable |
| Runaway code | The epoch interrupts a callback past its deadline; nothing runs between callbacks | A callback past its wall-clock deadline is killed; threads may run between callbacks, with no CPU quota |
| Its code's speed | 1.4 to 2.6 times native time on ordinary app code, more for code using SHA or AES instructions | Native |
| Starting one | About 77 µs, and 56 KiB when idle | A few milliseconds, and 0.25 to 0.5 MiB (1.4 to 2.4 MiB on the Pi) |
| Asking the node | A host call, about 0.15 µs | A request over shared-memory rings, each side spinning briefly before it sleeps (T1): 1.3 to 4.3 µs |
| A call within a node | 3 to 5 µs from the node, 5 to 7 µs between instances | 3.7 to 5 µs from the node, 5 to 8 µs between instances |
| A bucket read | A host call, 0.7 to 1.6 µs | A request, 1.4 to 3.1 µs; from a published bucket's snapshot in the app's own memory, 0.16 to 0.39 µs (T4) |
| A crash | A trap ends the instance; its Store is discarded | An abort, segfault or panic ends the process; supervision is the same |
| Upgrade | The new component is swapped in between messages | The new version's process starts beside the old and loads the snapshot (executables); plugins upgrade by restart or blue-green |
| Passivation | The Store is freed and the state kept as a snapshot | The process ends and the state is kept as a snapshot; a message starts a new one |
| Services | All of them: keyvalue and watches, groups, config, secrets, metrics, HTTP routes and egress, SQL, sockets, listeners and connection handlers, volumes, extensions, HTTP handlers | Logging, keyvalue and watches, published buckets, groups, config, secrets, metrics, HTTP routes, volumes, SQL, sockets (UDP relayed by the node), HTTP egress (sent by the node), listeners (each connection passed to an instance of the pool, which holds many); extensions come with Z8, dynamic children with E8 for both kinds; HTTP handlers go with WebAssembly |
| Casts to a busy node | Refused once the link's queue is full: 17% of a component's casts from instances arrived between two node processes | Each cast is a request to the node, which paces the sender: nearly all arrived |
| Casts that must arrive | A cast is not told whether it was queued | `try_cast` is told, and `cast_timeout` waits for room in the target's mailbox on any node: every one that returned `Ok` arrived (BASELINE.md, 2026-11-05) |
| Large messages | No frame limit beyond `message_max_bytes` | A frame between node and app is at most 17 MiB |

## What it costs

From BASELINE.md's summary ("summary: BEAM, Paddock's components and Bulkhead's native apps"), with the native columns from T11 (2026-11-06):

| | 16-core: component | native | BEAM | Pi: component | native | BEAM |
| --- | --- | --- | --- | --- | --- | --- |
| A call between instances within a node, p50 | 4.9 µs | 8.3 µs | 2.0 µs | 7.0 µs | 5.0 µs | 2.3 µs |
| A call between two node processes on one host, p50 | 148 µs | 179 µs | 71 µs (TCP) | 136 µs | 156 µs | 53 µs (TCP) |
| Calls/s between two node processes, 16 callers | 51,000 | 38,000 | 35,000 (TCP), 78,000 (TLS) | 39,000 | 28,000 | 72,000 (TCP), 44,000 (TLS) |
| Bucket or table: 95 reads in 100, 16 instances, ops/s | 0.69 million | 0.60 million (1.11 with `read_concurrency`) | 6.0 million (ETS) | 0.53 million | 0.42 million | 4.7 million (ETS) |
| Reads of read-mostly data, 16 instances, reads/s | 0.74 million | 36.5 million (published) | 16.5 million (`persistent_term`) | 0.57 million | 4.7 million (published) | 8.5 million (`persistent_term`) |

- **Within a node, components are cheaper to talk to:** a host call is a function call, and a native app's is a request to another process. Native apps make up for it with their own code's speed, and with published buckets for data many instances read.
- **Between nodes, the transport dominates and the two are close.**
- **The CPU price of native apps' speed:** they spin while waiting, which costs more CPU per call while many are busy, and throughput on a machine with few cores (`[native] spin_us`, OPERATIONS.md).

## Choosing

- **Paddock's components suit:** many instances that come and go (77 µs to start, 56 KiB each), code the operator trusts least (an interface narrowed to its imports), one bundle for every platform, languages that build components, and chatty calls within a node.
- **Bulkhead's native apps suit:** long-lived instances doing real work at native speed, existing native libraries, threads between callbacks, and read-heavy data shared by many instances.
