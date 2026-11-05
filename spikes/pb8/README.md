# Spike PB8: a direct channel between two native apps

**Question** (ROADMAP PB8, the messaging half of paddock-b's PB8). After T7, a call between two native instances on one node costs about 8 µs on the 16-core machine: two requests and two hand-offs between tasks in the node. How much of that would a channel between the two app processes save, and how much is the price of holding mailboxes, ordering, links, monitors, timeouts and sender identity in the node? Three designs carry the same frames:
- **a hub:** each app's two rings to the hub (S32's), and the hub forwarding each frame to the other app's ring;
- **a ring pair** between the two apps, in a memfd the hub creates, hands to both and can revoke;
- **a Unix socketpair** (`SOCK_SEQPACKET`) the hub hands out and revokes with `shutdown(2)` on its own descriptor, as a control (a socket cost a call 40% more than a pipe, Z2).

**Answer: most of a call's cost is the node's machinery, not the hop through it.** A hub that only forwards frames costs 1.6 to 3.3 µs per call; the node costs 7 to 8. A direct ring halves the hub again, but its revocation depends on the apps' cooperation.
- **A call, one pair** (p50, 16-core machine / Raspberry Pi 5): the node 8.1 / 7.1 µs, the hub 3.3 / 1.6, a ring pair 1.6 / 0.67, a socketpair 3.0 / 3.3.
- **The node's own share** (the node less the hub): about 4.8 µs on the 16-core machine and 5.5 on the Pi, against 1.6 and 0.9 for the hop itself (the hub less a ring pair). The node's share is its instance tasks, mailboxes and the hand-offs between them on the tokio runtime (S32 found 2 to 13 µs per hand-off to a sleeping worker), not the frames or the channel.
- **Throughput, 16 pairs:** calls at 315,000 a second through the node, 1.37 million through the hub, 2.83 million over ring pairs and 1.74 million over socketpairs (16-core machine); casts at 0.75, 8.3, 24 and 7.6 million. On the Pi's 4 cores (32 app processes): calls at 131,000, 206,000, 616,000 and 277,000.
- **CPU per call, one pair:** 9.7 µs through the hub, 3.4 over a ring pair, 7.5 over a socketpair (16-core machine); 5.0, 1.7 and 7.2 on the Pi. A ring pair's spinning sides wake no other thread.
- **Revocation:** both stop a busy pair in 0.17 to 0.32 ms, process exit included. A socketpair's is enforced by the kernel: after `shutdown(2)` neither end can send. A ring pair's is not: the hub sets a word each app checks, and an app that ignores it keeps reading and writing the memory it has mapped. The hub can stop such an app only by killing it, or by never handing two apps the same memory.
- **A socketpair is no faster than the hub** for a call, and slower for casts, on both machines: each frame is a system call each way. It is the design whose revocation the kernel enforces, at the hub's cost.

What a direct channel would give up, beyond enforced revocation: the node's view of each message (ordering against other senders and against exit signals, `pending_calls_max` and mailbox limits, monitors and links firing at the right point, sender identity for ACLs, traces and probes), and a single place that sleeps (an app with many peers would watch many rings, or need one doorbell per app shared by every peer; this spike's apps have one peer each).

## Method

`pb8 bench [ROUNDS]` (one binary) plays the node, and `pb8 app ...` (the same binary, started by the bench) plays each app. Apps come in pairs: a caller and its target.
- **The frames:** a kind byte, a sequence number and a 16 B body (25 bytes): calls, replies and casts.
- **Waiting:** every waiting thread spins with `sched_yield` for up to 20 µs (Bulkhead's `spin_us` default), then sleeps: on a futex in the ring it reads, on the hub's epoll of the apps' eventfds (the hub's poller marks every ring before it sleeps), or in `poll` on the socket.
- **The hub:** four poller threads (the node's four tokio workers), each serving a quarter of the pairs and sweeping their rings; both apps of a pair on the same thread, so a frame is forwarded within one thread. No tasks and no mailboxes: the floor of any design through the node.
- **The ring pair:** a memfd of two rings and a revoked word, created by the bench and inherited by both apps (as the node would pass it on T3's descriptor socket). Each app checks the word on every send and every turn of its wait.
- **The socketpair:** non-blocking `SOCK_SEQPACKET`, one frame per message; a full socket waits in `poll`.
- **The node:** `cluster_throughput` with `CLUSTER_NATIVE=1` at T7's commit, one node, 4 workers, `spin_us` 20: its sequential calls from an instance, 16 instances each calling its own target, and casts from instances, three runs right after the spike's.

**The runs**, each design in turn, three rounds, medians:
- **Calls, one pair:** 50,000 calls after 2,000 of warm-up, each timed by the caller.
- **Calls, 16 pairs:** 20,000 calls from each caller at once; calls per second over the span from the first caller's start to the last caller's end, and the median of the callers' p50s and p99s.
- **Casts:** 400,000 from one caller, or 100,000 from each of 16, then a call that returns the target's count, which must match.
- **CPU per operation:** every app's whole life (getrusage) and the hub's threads, over the timed operations and the warm-up together.
- **Revocation** (`pb8 revoke`): a pair calling without end, revoked after 300 ms; the time until both have exited, and their exit codes (4, revoked), three times per design.

**Machines** (2026-11-04):
- **16-core machine:** Threadripper 1950X, Linux 6.8, load 0.6 at the start; the spike's runs take seconds and leave it near idle, the node's raise it to 5 to 9.
- **Raspberry Pi 5:** Linux 7.0, nice 10 and idle I/O priority, load 0.2 at the start, 3 to 11 during the node's runs.

The results directory has every run: `16core-bench.txt`, `16core-revoke.txt` and `16core-node-r*.txt`, and the same for the Pi.

## Results

Calls, one pair (p50 and p99 µs; CPU µs per call in brackets):

| | 16-core: node | Hub | Ring pair | Socketpair | Pi: node | Hub | Ring pair | Socketpair |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| p50 | 8.1 | 3.3 (9.7) | 1.6 (3.4) | 3.0 (7.5) | 7.1 | 1.6 (5.0) | 0.67 (1.7) | 3.3 (7.2) |
| p99 | 14.3 | 4.1 | 2.4 | 7.6 | 12.1 | 2.1 | 1.4 | 4.2 |

Sixteen pairs at once (operations per second; CPU µs per operation in brackets):

| | 16-core: node | Hub | Ring pair | Socketpair | Pi: node | Hub | Ring pair | Socketpair |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| calls | 315,000 | 1.37M (9.8) | 2.83M (3.9) | 1.74M (6.2) | 131,000 | 206,000 (19) | 616,000 (5.8) | 277,000 (13) |
| call p50, each caller | | 9.7 µs | 3.5 µs | 5.5 µs | | 69 µs | 23 µs | 52 µs |
| casts | 750,000 | 8.3M (1.3) | 24.4M (0.43) | 7.6M (1.5) | 210,000 | 3.0M (1.6) | 3.9M (1.0) | 2.3M (1.7) |

Casts, one pair: 1.7M a second through the hub, 4.0M over a ring pair, 0.81M over a socketpair on the 16-core machine; 8.2M, 11.7M and 1.2M on the Pi.

- **The Pi's hub is faster than the 16-core machine's for one pair** (1.6 against 3.3 µs), as in S32: no C-state tail on the Pi, and the Threadripper's wakes and cross-die cache lines cost more.
- **With 16 pairs on the Pi's 4 cores** every design is oversubscribed (32 app processes and the hub's threads); the ring pair still does three times the hub's calls.
- **The node's casts from instances** (750,000 and 210,000 a second) trail even the hub's by 10 to 14 times: each cast is a request to the node, a mailbox send and a callback in the target, each a hand-off between tokio tasks.

## Not measured

- An app with many peers: one ring pair per peer, and how it sleeps on all of them.
- Bodies larger than 16 B.
- A hub built on the node's own code (its tasks, mailboxes and tokio) without the node's other work; the node itself stands in for it.
- The FreeBSD VM.

## What it changed

- **The decision that calls between instances stay mediated by the node** (ROADMAP, 2026-11-01) stands: a direct ring pair saves 0.9 to 1.6 µs a call over a hub, against 5 to 5.5 µs the node's own machinery costs, and gives up enforced revocation and the node's view of each message.
- **A new roadmap item:** a call between native instances through the node with fewer hand-offs between tasks, aiming at the hub's cost (T12).
