# S37: HTTP ingress as a driver app

Spike S37 of `docs/ROADMAP.md` (Phase 7). It asks what HTTP ingress costs when it runs as a driver app, a process of its own, instead of in the node, and decides three things:

- the driver's profile;
- the frames a driver needs beyond an app's;
- the target for a request through the node, against which DA1's driver is measured.

Run 2026-11-11, release builds, on two machines, each idle at the start (load under 0.6; the rest is the runs' own load):

- **16-core machine:** Threadripper 1950X, 16 threads, Linux 6.8, both VMs shut down.
- **Raspberry Pi 5:** Cortex-A76, 4 cores, Linux 7.0.

Results: `results/x86-2026-11-11.txt`, `results/pi5-2026-11-11.txt`, and the split below in `results/*-split-2026-11-11.txt`.

## What was built

Three programs, built from this directory alone against the main workspace's crates:

- **`s37-app`**, on `bulkhead-sdk`: answers an HTTP request envelope (`bulkhead_sdk::http`) with its body echoed, or `ok` for none. Each instance registers the first free name `echo-<n>`.
- **`s37-driver`**, a connection handler on `bulkhead-sdk` (the node's listeners, `connection::Handler`). The node accepts each connection on the driver's listener and passes it, reading none of it, and tells the driver when it has bytes (`io-ready`). The driver:
  - parses HTTP/1.1 itself (`httparse`);
  - terminates TLS itself on its `https` listener (rustls, a P-256 certificate made at the first connection);
  - makes each request a call through the node to an instance of the app, by name, in turn: the same envelope the node's ingress sends;
  - answers `/local` itself, without a call, to split the cost.
- **`s37-bench`**, in two roles:
  - `node`: a runtime with the echo app (a pool of 4) behind the node's own ingress (HTTP and HTTPS, `bulkhead_host::http`), and the driver (a pool of 4) on two listeners, calling the same pool;
  - `load`: keep-alive connections, one request at a time each, for 10 s after a second's warm-up. It reports requests a second, p50 and p99, and the CPU the node and its children (every instance) spent per request. The client runs on the same machine, as before (BASELINE.md, 2026-10-16), with 4 workers, as the node has.

The same app, envelope and pool are behind both paths. Only what lies between the client and the app changes:

- **in the node:** hyper and rustls in the node, then a call to the app;
- **through the driver:** the node's listener and a readiness notice, the driver's parse and TLS, a call from the driver to the node, then the node's call to the app.

## Results

Requests a second and µs of CPU per request, two rounds each (the rows alternate the paths), with p50 in µs.

16-core machine:

| | in the node | through the driver | driver / node |
| --- | --- | --- | --- |
| GET, 1 connection | 10,009, 10,789; p50 88–99; CPU 105–108 | 12,995, 13,702; p50 68–72; CPU 173–182 | 1.3× the requests, 1.7× the CPU |
| GET, 32 | 79,368, 79,910; p50 314–324; CPU 67 | 40,312, 40,126; p50 792–794; CPU 182 | 0.50×, 2.7× |
| GET, 256 | 89,748, 90,687; CPU 58–59 | 39,550, 38,675; CPU 183–185 | 0.43×, 3.1× |
| HTTPS GET, 1 | 9,064, 9,014; p50 103–104; CPU 115–116 | 12,699, 12,307; p50 75–77; CPU 184–189 | 1.4×, 1.6× |
| HTTPS GET, 32 | 73,648, 73,514; CPU 71 | 38,969, 38,475; CPU 188–189 | 0.52×, 2.7× |
| HTTPS GET, 256 | 81,012, 82,382; CPU 64–65 | 38,407, 37,997; CPU 185–187 | 0.47×, 2.9× |
| POST 1 KiB, 1 | 8,689, 8,395; CPU 136–137 | 12,743, 12,312; CPU 181–183 | 1.5×, 1.3× |
| POST 1 KiB, 32 | 68,928, 68,257; CPU 80 | 35,908, 35,819; CPU 194–195 | 0.52×, 2.4× |
| POST 64 KiB, 1 | 2,938, 3,492; CPU 315–389 | 2,626, 3,308; CPU 509–608 | 0.92×, 1.6× |
| POST 64 KiB, 32 | 19,599, 21,045; CPU 283–324 | 8,446, 9,325; CPU 595–673 | 0.44×, 2.1× |
| POST 1 MiB, 1 | 134, 158; CPU 7,235–8,496 | 129, 143; CPU 9,035–10,047 | 0.93×, 1.2× |
| POST 1 MiB, 32 | 651, 621; CPU 9,017–9,508 | 327, 346; CPU 13,529–14,670 | 0.53×, 1.5× |

Raspberry Pi 5:

| | in the node | through the driver | driver / node |
| --- | --- | --- | --- |
| GET, 1 connection | 15,333, 15,427; p50 64; CPU 79–80 | 13,643, 15,739; p50 58–73; CPU 148–171 | 0.95×, 2.0× |
| GET, 32 | 51,958, 53,767; p50 342–414; CPU 52–54 | 20,673, 20,696; p50 1,335–1,365; CPU 149 | 0.39×, 2.8× |
| GET, 256 | 47,729, 49,696; CPU 58–62 | 20,251, 19,721; CPU 152–157 | 0.41×, 2.6× |
| HTTPS GET, 1 | 13,316, 13,833; p50 71–74; CPU 84–86 | 11,625, 11,898; p50 80–82; CPU 189–192 | 0.87×, 2.2× |
| HTTPS GET, 32 | 45,796, 45,978; CPU 59 | 18,204, 18,396; CPU 165–167 | 0.40×, 2.8× |
| HTTPS GET, 256 | 42,627, 42,835; CPU 66 | 16,989, 17,251; CPU 176–179 | 0.40×, 2.7× |
| POST 1 KiB, 1 | 13,122, 13,232; CPU 101–102 | 11,050, 11,366; CPU 202–203 | 0.85×, 2.0× |
| POST 1 KiB, 32 | 39,682, 39,602; CPU 71 | 16,366, 16,186; CPU 187–189 | 0.41×, 2.7× |
| POST 64 KiB, 1 | 2,196, 2,221; CPU 505–511 | 1,502, 1,491; CPU 1,024–1,030 | 0.68×, 2.0× |
| POST 64 KiB, 32 | 1,964, 2,001; CPU 1,470–1,506 | 1,166, 1,159; CPU 2,377–2,394 | 0.59×, 1.6× |
| POST 1 MiB, 1 | 118, 125; CPU 9,025–9,408 | 73, 78; CPU 15,995–16,977 | 0.62×, 1.8× |
| POST 1 MiB, 32 | 103, 101; CPU 31,864–32,688 | 61, 71; CPU 42,735–52,405 | 0.65×, 1.5× |

### Where the driver's cost goes

At 32 connections, a GET through each path, and the driver answering `/local` itself with no call; then the driver's pool doubled. One round each (`scripts/split.sh`).

| | 16-core: req/s, CPU µs | Pi: req/s, CPU µs |
| --- | --- | --- |
| in the node, a call | 78,668, 67 | 53,016, 53 |
| the driver, answering itself | 83,981, 64 | 39,178, 81 |
| the driver, a call | 41,164, 177 | 20,338, 151 |
| 8 drivers, a call (32 connections) | 65,823, 155 | 24,098, 137 |
| 8 drivers, a call (256 connections) | 69,342, 150 | 22,800, 146 |

## Findings

1. **One connection costs the same or less through the driver.** On the 16-core machine a request takes less time through the driver (p50 68 to 80 µs against 88 to 119); on the Pi about the same (58 to 83 against 64 to 74). The extra hop is hidden behind hyper's own cost in the node.
2. **Under load the driver costs 2.4 to 3.1 times the CPU per request** and serves 39 to 52% of the node's requests a second with 4 drivers. Two costs, about equal:
   - **Getting the bytes to the driver** costs as much as a whole request in the node (64 to 81 µs against 53 to 67): the node watches each passed stream and sends the driver a notice for each read, and the driver reads, parses and writes in a callback.
   - **The call through the node** adds 70 µs (Pi) to 110 µs (16-core) of CPU: the driver's request, the node's call to the app, and both answers, each crossing a ring, with the driver spinning while it waits.
3. **On 16 cores the drivers' pool limits throughput.** A native instance makes one call at a time and blocks its callback for it, so 4 drivers hold 4 requests in flight. With 8, the 16-core machine served 66,000 to 69,000 requests a second, 83% of the node's own, at 2.3 times its CPU. On the Pi's 4 cores CPU is the limit: 8 drivers gained 18%.
4. **TLS costs the same either way:** HTTPS adds 5 to 10% to both paths (keep-alive; handshakes not measured).
5. **Large bodies are slow on both paths.** A 1 MiB request costs 7 to 9.5 ms of CPU in the node itself on one connection (on the Pi with 32 connections, 32 ms): 64 KiB ring pieces each way, and the CBOR envelope built and parsed whole. The driver adds 20 to 80% to that. The driver reads its body 64 KiB per notice (16 callbacks for 1 MiB) before the call starts.

## Decisions

- **The driver accepts its own connections.** The node binds the listening socket (or the helper does, for a port below 1024) and passes it to the driver, which accepts, reads and writes with its own event loop: no notice through the node per read, and no node code on the connection at all. The port stays the node's across a driver's restart. The node's listeners keep the present model for apps (connection handlers).
- **The driver's profile** (DA1), checked against SECURITY.md's "What the node parses from outside":
  - its passed listening sockets, with `accept4` allowed on them alone (the seccomp filter compares the descriptor number; Capsicum's `CAP_ACCEPT` on FreeBSD); `socket` and `connect` still refused;
  - its certificate and key files, read-only, as a preopen of its own, so the node holds no ingress TLS key;
  - calls only to the apps whose routes it serves (a messaging ACL the node writes from the routes), stamped with the driver's own name;
  - its own user (PS2), and no buckets, secrets or volumes.
- **Frames a driver needs beyond an app's** (DA1):
  - **calls in flight from any thread:** a call request answered by a frame matched by its id, outside the callback model, so one driver process holds many requests at once without a pool of drivers or a spinning wait per call;
  - **a call to an app by name:** the node picks the instance by the app's routing (`hash:<header>`, round robin), as its ingress does today, so the driver learns no pids and the routing stays in one place;
  - **routes from the node:** the routes table and its changes sent to the driver as a frame, so deploys and upgrades reach it as they reach the ingress today.
- **The target for a request through the node:** at most 1.5 times the node's own ingress's CPU per request, and at least two thirds of its requests a second at 32 connections, HTTP and HTTPS, on both machines; one connection's p50 no more than 20% above the node's. Set after this prototype's run, not before as the item asked; the prototype misses it (2.4 to 3.1 times the CPU, 39 to 52% of the requests). DA1's driver, with the frames above, is measured against it. A driver calling an app over a ring of their own (PB8) skips the messaging ACL, the stamped sender and the node's power to revoke, so it stays unmeasured and is considered only if DA1's driver misses the target.
- **Large bodies** are a cost of the node's own path first: a body passed out of the ring (a sealed memfd named in the frame, "Zero-copy large messages" under Parked) is measured for calls in general, with the node's ingress and a driver both, not as a driver's need.

## Not done

- **The driver with its own event loop and calls in flight:** DA1's, against the target above.
- **The direct ring between driver and app** (PB8's shape): not measured.
- **TLS handshakes:** every connection is kept alive, so a handshake's cost is not in the numbers.
- **FreeBSD:** not run (the VM is for function checks; FB4).
- **Bodies through the memfd:** not built.

## Running

From the main checkout, on an idle machine:

```sh
cargo build --release -p bulkhead-native --bin bulkhead-runner
(cd spikes/s37 && cargo build --release)
sh spikes/s37/scripts/run.sh > report.txt      # ROUNDS, SECS, POOL, DRIVERS, WORKERS
sh spikes/s37/scripts/split.sh > split.txt     # DRIVERS_WIDE (8)
```
