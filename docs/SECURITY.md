# Bulkhead: Security

2026-10-25, native apps 2026-11-01 · Companion to [ARCHITECTURE.md](ARCHITECTURE.md)

The threat model as built: what Bulkhead protects, whom it trusts, each boundary between trusted and untrusted parts with the controls on it and the gaps found, and the fuzz targets. Findings are numbered S1 and on; each is fixed, and a new one would be a ROADMAP item. First drafted in the 2026-10-10 review and kept current as Phases 2, 3 and 5 added boundaries.

## Reporting a vulnerability

Report it privately through the repository's Security tab ("Report a vulnerability"), not in a public issue. Bulkhead is a research project without support (README.md): reports are read, but there is no response time and no fixed releases.

## What is protected

- **Singleton exclusivity.** A singleton drives devices; two copies running at once is the worst outcome (DESIGN.md, "Distribution"; `docs/tla/VERIFICATION.md`).
- **App isolation.** Each app sees only the capabilities its manifest declares and the operator grants: its own buckets and files, shared buckets it was granted, the secrets it lists.
- **Secrets** bound on the node, and the node's keys: the cluster key and the operator's trusted signer keys.
- **Availability** of the node and of other apps when one app misbehaves.
- **Logs and the audit log**, which operators and the soak's checker read as the node's own record.

## Whom Bulkhead trusts

| Party | Trusted to | Not trusted to |
| --- | --- | --- |
| The operator (anyone who can open the admin socket: the node's user, mode 0600; or holds a key the `[admin]` operators file lists) | everything: deploy, kill, confirm failover, read state | — |
| A signer in `trusted_keys` | publish code the node may run | do anything its grant does not allow; `bulkhead-runner`'s restrictions and the grant are the boundary even for signed code |
| An app | use what it was granted | reach other apps' data, the host, or other apps' messages beyond what routing allows |
| A cluster member (a node whose key is in the peers file) | what its own instances could do on the node: call, cast, link and send exit signals within the messaging ACLs, as its stamp on each frame says; call an app by name from its host, as a standby's ingress does to forward an HTTP request (not subject to messaging ACLs, as host calls are not); take part in failover | speak for another node's instances (a frame whose sender pid is on another node is dropped); its stamps are trusted, so a member that lies about its senders' apps passes ACLs it should not |
| The witness | arbitrate its pair | — |
| An extension (host code built into the node by its operator: the Modbus extension, `examples/greeter`) | everything the node process can do; it runs as part of the node | — (apps reach it only through its package's declaration and grant, which the extension enforces itself; a native program's calls come through the node with the same state, so the same checks) |
| HTTP clients, listener clients, Modbus masters | nothing | anything beyond the routes, handlers and tables they reach |
| Modbus devices polled | return register values | crash or stall the node |

Cluster members trust one another fully, as nodes of an Erlang cluster do: mutual TLS with pinned keys decides who is a member, and a member is not checked further. A member compromised is a cluster compromised.

## Boundaries

### App and host

- **Controls.** the restricted process ("Native-process apps" below); `memory_max` and instance limits; the callback deadline, then a kill; mailboxes bounded in messages and bytes (`mailbox_max`, `mailbox_max_bytes`) and `message_max_bytes`; host-object limits (timers, monitors, links, pending calls, groups, watches); an `access-denied` answer to any request outside the grant. Exit signals into an instance are bounded by the node's instance count, timers and watch notices by their own limits.
- **Gaps.** None found; S2, S3, S6 and S11 are fixed. A writable volume is bounded only by the filesystem quota the operator gives it.

### App and app

- **Controls.** Buckets namespaced by app; shared buckets by grant, read-only or read-write; secrets by name and grant; groups by manifest scope; SQL datasources by name and grant, with a connection cap per app; registered names limited to those the manifest lists; messaging ACLs (`messaging_acl.accept_from`, `may_call`, by app name) on calls, casts, exit signals and links, checked on the target's node against the host-stamped sender.
- **Gaps.** S1 is fixed. Without a `messaging_acl`, an app may call, cast, link or send exit signals to any pid it learns, as before; the lists are opt-in. Monitors are not checked (they only observe).

### The admin socket and the remote admin API

- **Controls.** A Unix socket created owner-only; bundles verified against `trusted_keys` whoever uploads them; uploads capped at 512 MiB; changes recorded in `audit.log`. The remote admin API (`[admin]`) is TLS 1.3 with raw public keys both ways: the node presents its key, which the operator pins (`--node-key`), and accepts only a key its operators file lists; a handshake has 10 s to finish; every request but a `GET` is recorded in `audit.log` with the operator's name.
- **Gaps.** Every listed operator has every right the socket gives: there are no read-only operators. Anyone who can reach the address can open connections and start handshakes; there is no limit on either, so put the address on a management network. Removing an operator stops new connections, not one already open, including a stream (`bulkhead events`, `logs --follow`) that has run since. The shell keeps its history, call bodies included, in the operator's `~/.config/bulkhead/history`.

### Node and node

- **Controls.** QUIC with TLS 1.3 and raw public keys pinned per node id; a hello naming the node id, checked against its key; frames of at most 64 MiB; a versioned CBOR wire format; sessions that discard frames from an ended session or an earlier start.
- **Gaps.** None beyond the trust above; S5 and S8 are fixed. Calls from one peer in progress at once are capped (`[cluster] incoming_calls_max`). Credit for waited casts lets a peer's instances hold room in a local mailbox (`[cluster] cast_credit` casts and `cast_credit_bytes` per sender and target); all the credit in one mailbox holds at most half of its messages and bytes, and room a local sender waits for is never granted.

### Ingress clients

- **HTTP.** Routes by host and path prefix; body limit per route (1 MiB default); header timeout 10 s, body timeout 30 s; at most 1,024 connections per address; reply size limit; TLS with rustls on ring, certificates reloaded on change. No client certificates yet.
- **Listeners.** `max_connections` and an idle timeout per listener. An app's connections are passed to its instances, many to each, so a client's bytes reach the app's code unparsed by the node; the node keeps a copy of each socket and shuts it down when the listener stops or the connection idles (`TCP_INFO`), whatever the app does with its copy.
- **Gaps.** None found.

### HTTP egress

- **Controls.** An app reaches only the schemes, hosts and ports its manifest's `allow` lists, inside node config's `[egress] allow`, with the methods it lists, once policy grants `bulkhead:http-egress`; anything else is refused before a connection opens. A host name is resolved by the host, and one that resolves only to loopback or link-local addresses (cloud metadata at 169.254.169.254) is refused at each connection, so a name that later resolves there is refused too; an address is reached only when an entry names it. HTTPS verifies the server against the system's roots and node config's CA file. Requests in flight are capped per app, and timeouts bound the wait for headers and between body chunks. Redirects are not followed. Forbidden request headers (`connection`, `keep-alive`, `proxy-*`, `transfer-encoding`, `upgrade`, `host` set from the URL) are refused.
- **Gaps.** Private ranges (10/8, 172.16/12, 192.168/16, fc00::/7) are allowed for any name an entry allows, since internal services live there; a `*.internal` entry trusts whoever answers DNS for that zone. Responses are parsed by hyper in the host and passed to the app in one frame, so a response body is at most 16 MiB. No outbound proxy (HTTP `CONNECT`) and no client certificates.

### App sockets

- **Controls.** Once policy grants `bulkhead:sockets`, an app's sockets reach only the blocks and ports its manifest's `connect` rules list, bind only what its `bind` rules list, and listen only with `listen`, all inside node config's `[sockets]`; the node checks every address as it is used (bind, listen, accept, connect, each UDP send and receive) and passes an app only connected TCP streams, so a socket cannot move outside the rules after it opens. Loopback, link-local (cloud metadata at 169.254.169.254) and unspecified destinations are reached only through a rule inside that range, so a broad rule such as `0.0.0.0/0` does not reach the node itself. Name lookups are allowed only with `connect` rules, and their answers are checked like any address. Sockets open at once are capped per app (`max_sockets`).
- **Gaps.** Private ranges are reached by any rule that covers them. A rule names addresses, not hosts, so an app allowed a block reaches every service in it; a resolver the node uses can steer a name to any address inside the rules. The protocol on the socket is the app's own, parsed in its process: TLS, if any, runs in the app with its keys in its memory. Bytes in flight are bounded only by the app's memory and the kernel's socket buffers; there is no rate limit. A listening app's accepted connections count against `max_sockets`, which bounds them; there is no per-address limit.

### Native-process apps

An OS process restricted by `bulkhead-runner` confines an app's code, and the grant is enforced on the node, in the code that answers each request:

| | Native app |
| --- | --- |
| Memory | Its own address space; `memory_max` as a cgroup's `memory.max` or `RLIMIT_DATA` |
| What the code can call | The system calls on the seccomp allow-list (`bulkhead_native::runner::seccomp`); any other ends the process. Landlock fences files |
| CPU | The callback deadline in wall time, then a kill; threads may run between callbacks, and there is no CPU limit |
| Files and network | Read-only: system libraries, its own directory, its `priv/` files and read-only volumes; read-write: its read-write volumes, executing nothing from them; network: TCP streams the node connected or accepted on the grant's rules, or accepted on the app's listeners, and passed to it; listening sockets and UDP sockets stay in the node |
| Reaching other apps and the node | Frames on its channel, as its instance |

- **Controls.** A native app's program runs only through `bulkhead-runner`, which, before the program runs and while it has one thread: marks itself non-dumpable (no core dump, no ptrace from the node's user); restricts itself with Landlock to reading the system library directories, the program's own directory, the app's `priv/` files and its read-only volumes, reading and writing its read-write volumes (files, directories and symbolic links, and renames within one; no devices, sockets or pipes, no device ioctls), executing only the program (and its ELF interpreter), writing nothing else and binding or connecting no TCP port; and installs the seccomp filters (below). A kernel without Landlock is refused unless `[native] landlock_optional`. Both stay in force across the exec of an executable and in anything it starts. Not being dumpable does not (`execve` resets it for a readable program), so `bulkhead_native::app::serve` sets it again before the app's code runs. The node runs the process with an empty environment, in its own process group, with the parent-death signal (`SIGKILL`) and no core dumps; with `[native] cgroup`, in a cgroup of its own with `memory.max` (no swap) and `pids.max`. A singleton's process is killed when the node's lease ends, idle or in a callback (`crashed: lease_ended`), so it cannot drive devices past the lease while it runs threads of its own. The process reaches Bulkhead only over its pipe, as its instance: messages carry that instance's pid and app, checked by messaging ACLs as for a component, and its requests are the process functions a component imports, with the same host-object limits. A manifest may declare only `bulkhead:logging`, `bulkhead:keyvalue`, `bulkhead:groups`, `bulkhead:config`, `bulkhead:secrets`, `bulkhead:metrics`, `bulkhead:http-routes` (requests arrive as calls, as for a component), `bulkhead:files` (volumes the operator's `[volumes]` names, passed only with the grant and fenced by Landlock as above) `bulkhead:sql` (datasources answered by the node, whose credentials never reach the app) `bulkhead:http-egress` (each request sent by the node's egress client on a component's rules, so the app holds no egress socket and no TLS) and `bulkhead:sockets` (the node makes every socket on the rules and the address guard, and counts `max_sockets`: a passed TCP stream is connected, and the node shuts it down when the app closes it, so a kept copy is dead; UDP stays in the node, which checks each datagram's destination and source, since Linux's `sendmsg` overrides a connected UDP socket's peer); each request is answered by the code a component's import runs, on the same checks (the grant, the manifest's buckets, groups and secret names, read-only shared buckets, quotas and limits).
- **System calls.** The runner's seccomp filters are an allow-list (PS4, 2026-11-11): memory, threads and processes inside the fence, signals to itself, clocks, descriptors, the file calls (Landlock decides which files), and reading and writing sockets the node passed. A call outside the list raises `SIGSYS`: an app on `bulkhead_native::app::serve` records the call and ends, and its instance exits `crashed: syscall <name>`; any other process dies of the signal (`crashed: syscall refused`). Not on the list, among others: `bind` and `listen`, `unshare`, `setns`, `mount`, `chroot`, `bpf`, `keyctl`, `perf_event_open`, `userfaultfd`, `memfd_create`, `execveat`, `seccomp`, the credential calls, kernel modules and System V IPC. Some listed calls are refused with `EPERM` and the app runs on: signals to any process but itself (`kill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` by pid; `tkill`, `pidfd_send_signal` and `pidfd_getfd` always; for an instance bound to a failover lease, the runner first forks a watchdog that kills the app at the lease's end without the node, with no filesystem access and signals only to itself and the app; `bulkhead_native::lease`), `ptrace`, `process_vm_readv` and `process_vm_writev`, new sockets (`socket`), `connect`, `accept` and `accept4` (a passed stream cannot be moved or accept connections), `io_uring` (whose operations bypass the filters: its socket operation makes a socket without `socket`, and Landlock does not fence UDP), `clone` with a flag that makes a namespace (a user namespace would give the app capabilities over what it makes), `socketpair` outside `AF_UNIX`, and `ioctl` beyond the descriptor requests and the terminal check. `clone3` answers `ENOSYS`, since a filter cannot read its flags, and libc falls back to `clone`. Checked on x86-64 (Linux 6.8, glibc 2.39) and aarch64 (Linux 7.0, glibc 2.43) with every test app and example; an app built against another libc, or one whose libraries make other calls, finds out as that exit.
- **The channel.** The node reads and writes the rings through a mapping of a memfd the app also holds. The node trusts none of the positions the app writes there, copies each frame out before decoding it, and seals the memfd's size so the app cannot shrink the file under the mapping (S11).
- **Gaps.** Native code is not memory-safe from the node's point of view: everything above is the boundary, not the language. The allow-list is one list for every app; a list per driver profile comes with driver apps (ROADMAP DA1), and the allowed calls' arguments are checked only as listed above. A TCP stream the node passed is the app's to read and write with its own system calls: the node counts it against `max_sockets` (or the listener's `max_connections`) and shuts it down when the app closes it or exits, but sees none of its bytes, and nothing limits their rate. A native singleton is killed at its lease's end by the runner's watchdog when the node cannot, within the watchdog's wake-up (its tests require the kill within 200 ms of the deadline, and pass on both machines, 2026-11-07); with `[native] landlock_optional` on a kernel without Landlock, the watchdog runs under seccomp alone. An executable that speaks the protocol without `bulkhead_native::app::serve` stays dumpable: its crashes go through the system's core handler, and the node's user can trace it as far as Yama allows (spike S30). The exec restriction is not a boundary of its own: since the ELF interpreter must be executable, a program can run any readable binary through it, but that binary inherits the same Landlock rules and seccomp filter. Without `[native] cgroup` there is no CPU or process limit, and memory is bounded only by `RLIMIT_DATA`; with it there is still no CPU limit. A callback's deadline bounds a callback, but the program may run its own threads between callbacks. Landlock scopes (Linux 6.12) are not required: the seccomp filter covers signals, and refusing `socket` covers abstract Unix sockets; inherited descriptors are only stdin (empty), stdout and stderr (the node's log pipe), the channel's memfd, the bell's write end, the door's read end and its end of the descriptor socket, on which the node passes it descriptors and which the node's end never reads, so an app cannot pass descriptors to the node. A published bucket's snapshot goes only to instances whose manifest lists the bucket and whose grant includes it, as a memfd sealed against writes, shrinking and growing: an app reads what its grant lets it read, and cannot change what another app reads. The channel is memory the app shares with the node: the node reads the positions and frames the app writes there as it read the pipe's bytes, checking that a ring never holds more than its size, copying only inside its data area and decoding frames from its own copy, and ends an app that breaks the ring's rules. Each side spins for `[native] spin_us` (at most 10 ms) while waiting for the other, then sleeps, so a native app's traffic costs the node a worker's turns while it flows, and nothing while it stops. The program runs as the node's user: Landlock keeps it from that user's files, not from a kernel flaw an unprivileged process can reach. The seccomp filter refuses a short list, so the rest of the kernel's interface is reachable, where a component reaches only its imports. Tested on Linux 6.8 (Landlock ABI 4) on x86-64 and on a Raspberry Pi 5 with Linux 7.0 (aarch64), 2026-10-23, and with Phase 5's tests on both on 2026-11-01.

### SQL datasources

- **Credentials** come from a node secret (`password_secret`) and never reach an app; apps name datasources, not addresses.
- **Statements** are the app's own SQL, run with the datasource's user: what that user may do in the database is the operator's grant there. Parameters are bound, never interpolated by the host. A statement timeout (server-side) and a result size cap bound each call; a connection cap per app and an acquire timeout bound what one app holds.
- **Pooled connections are reset** with `ROLLBACK; DISCARD ALL` before reuse, so a transaction or session setting one call leaves does not reach the next caller, which may be another app.
- **TLS** verifies the server against the CA file the operator names; there are no system roots.
- **Gaps.** Apps that share a datasource share its database user, so the database cannot tell them apart; give each app its own datasource and user where that matters. Results from the server are parsed by `tokio-postgres` and decoded by the host.

### Modbus

- **Devices** are polled with a timeout per request; responses are parsed by tokio-modbus.
- **The server** answers only masters in `server.allow` (loopback by default): Modbus has no authentication of its own.
- **Gaps.** None found; S4 is fixed. At most `server.max_connections` masters (default 32) are connected at once.

### The metrics endpoint

- **Controls.** Off unless node config sets `[metrics] address`; it serves `GET /metrics` only, read-only, the Prometheus text the admin API's `GET /v1/metrics` also gives.
- **Gaps.** No authentication and no TLS: anyone who reaches the address reads every app's name, its instance counts, exit reasons, traffic and its own metrics' names and labels. Bind it to loopback or a management network.

### Liveness, membership and failover

- **Controls.** As for node and node; the failover rules R1 to R13 and their model (`docs/tla/VERIFICATION.md`). Serial heartbeat frames carry a CRC-16 and the sender's and receiver's node ids; frames that fail either check are dropped.
- **Serial line.** Frames on the pair's serial cable are not authenticated: whoever can reach the cable or a device on it can forge beats, which can keep a standby from promoting (a forged "primary alive") or renew a lease a primary would otherwise lose. The cable is treated as part of the pair's physical enclosure.
- **Gaps.** S5 is fixed. With a witness, an operator is asked to confirm a promotion only while the witness cannot be reached either, and a confirmation then is final: if the partner confirmed gone was alive, or restarts, and reaches the witness but not this node, both run until they hear each other (VERIFICATION.md, A9 and finding 6, accepted 2026-10-16); operators confirm only a partner that is down and stays down (powered off or fenced) until then. A primary paused past its lease no longer runs its singleton's callbacks when it continues (A8 in `docs/tla/VERIFICATION.md`).

### Replicated buckets

- **Controls.** Replication frames are taken only from the node's configured partner, over the cluster transport's mutual TLS, and are decoded by the replication module (CBOR) before anything is applied; an entry is applied only at the receiver's own epoch and after an entry that matches (spike S20's R6). Writes from the partner pass the same quota rules as local ones, with the quota the primary had. Linearizable reads are served only by a primary inside its lease.
- **Gaps.** The partner is trusted with the bucket's whole contents: a compromised partner can write, or snapshot over, every `cluster` bucket. A standby forwards any write a local app makes; the primary applies it without checking that the app is granted the bucket there (grants are per node, configured alike).

### Bundles, manifests and policy

- **Controls.** Signatures checked against operator keys only (never the key inside the bundle); checksums; manifest validation; the effective grant computed from the manifest and policy; `priv/` paths checked by Bulkhead on open and again on extraction (plain relative names only), as well as by rustbridge-bundle.
- **Gaps.** None found; S7 is fixed.
- **Programs** (`data_dir/compiled`) are the executables of deployed apps, written with mode 0500. Anyone who can write there as the node's user can run code as an app, as with the node's binary and configuration.

### Secrets and keys

- **Controls.** Secret values are read on each access and never printed: errors name the secret, and `Debug` shows only the source. Keys are written with mode 0600, and the cluster key and signer keys are refused on load if their group or others may access them, as ssh does. The node's process is not dumpable (Linux) unless node config sets `debuggable`, so other processes of its user cannot read its memory or environment, and it leaves no core.
- **Gaps.** None found; S9 is fixed. A test asserts that a secret's value appears in no log record, at any level or target, and in no error (`bulkhead-host` `tests/secrets.rs`).

## What the node parses from outside

Every input the node reads from outside its own process, who controls it, the code that reads it, and whether that code stays in the node (ROADMAP Phase 7; decided with the user on 2026-11-10 and 2026-11-11). The node holds every secret, the cluster key and all bucket data, so each row that stays is a way to all of them.

| Input | Controlled by | Parsed by | Stays in the node |
| --- | --- | --- | --- |
| A native app's frames and ring positions | an app | `bulkhead_native::hot` and the CBOR decoder, `channel::Ring`; fuzzed (`native_frame`; the ring's reader is T1b) | yes: this is the node's own interface |
| An app's snapshot, for `bulkhead state` | an app | `node/state_view.rs`; fuzzed | yes |
| HTTP ingress: requests and TLS handshakes | any client | hyper, rustls (`http.rs`) | no: an ingress driver (DA2) |
| HTTP egress: responses and TLS handshakes | any server an app is allowed | hyper, rustls (`services/egress`) | no: an egress driver (DA4); the policy check stays |
| SQL: Postgres replies | the database server | `tokio-postgres` | no: a driver per datasource (DA3) |
| Modbus: device replies and masters' requests | devices and masters | `tokio-modbus`, `bulkhead-modbus`; the server's requests fuzzed | no: a driver (DA5) |
| Extensions' inputs | whatever the operator's code reads | the extension, inside the node | no: the `Extension` trait goes after DA5 (DA8) |
| The remote admin API: TLS handshakes and HTTP requests | anyone who reaches the address, before the operator's key is checked | rustls, hyper (`node/daemon/remote.rs`) | no: a driver that forwards to the admin socket (DA7) |
| The metrics endpoint: HTTP requests | anyone who reaches the address | hyper (`node/server.rs`) | no: with the remote admin API (DA7) |
| The admin Unix socket: HTTP requests and JSON | the operator | hyper, `serde_json` | yes: the operator is trusted with everything |
| Bundles | the operator who uploads one; the signer for what is inside | the zip container and the manifest are read by `rustbridge-bundle` before the signature is checked; the program and `priv/` files after it; fuzzed (`bundle_open`, `manifest`) | yes: a deploy is the operator's act. The zip reader runs on bytes no key has vouched for, which is accepted from an operator |
| The cluster transport before a peer is known: QUIC packets and the TLS 1.3 handshake | anyone who reaches the cluster address | quinn, rustls | yes, as an accepted risk: failover and replication need the transport in the node, and a driver between them would add a hand-off to every frame. Put the cluster address on a network of its own |
| The cluster transport after it: frames, heartbeats, replication entries | a cluster member | `transport::wire`, `failover::Status`, the replication module; fuzzed (`wire_packet`, `failover_status`) | yes: members are trusted |
| Serial heartbeat frames | whoever reaches the cable | `serial/frame.rs` (a CRC-16 and node ids) | yes: the cable is part of the pair's enclosure |
| App listeners and app sockets: accepts, UDP datagrams, name lookups | any client; the resolver | addresses only, checked against the rules; the bytes go to the app unparsed; names through the system's resolver (`tokio::net::lookup_host`) | yes for now: reviewed again after DA2 (DA6) |
| Node config, policy, peers and operators files | the operator | `toml`, the key parsers | yes |

## Findings

| # | Severity | Finding | Fix |
| --- | --- | --- | --- |
| S1 | medium, fixed 2026-10-10 | Two deployed apps could list the same registered name. The first instance to register it holds it; names are released when the holder exits, so each restart is a race the other app can win and receive the messages meant for the first. | Fixed: a deploy, upgrade or rollback that lists a name another deployed app lists, running or stopped, is refused (`conflict`). |
| S2 | medium, fixed 2026-10-10 | App log messages and contexts were written as given. A newline starts what looks like a new host record, with any timestamp and target: checked with tracing-subscriber 0.3.23, which escapes ANSI sequences but not newlines. Operators' logs, and the soak's checker, which reads the gate's records, can be fooled. | Fixed: control characters in app text (log messages and contexts, stdout and stderr lines, crash reasons) are escaped. |
| S3 | medium, fixed 2026-10-10 | App stdout and stderr became log records with no rate limit, and an app's own `rate_limit_per_sec` was optional, so one app could flood the node's log. | Fixed: every app has a budget, the node's `[limits] log_records_per_sec` (default 1000) or its own lower rate, shared by its log records and its stdout and stderr lines. |
| S4 | medium, fixed 2026-10-10 | The Modbus server accepted reads and writes from any master that reached its address; a write changes the served table and is cast to the `deliver_to` app. | Fixed: `server.allow` lists the masters' addresses or networks, loopback only by default; MANIFEST.md says to bind it to the SCADA interface. `server.max_connections` (default 32) caps the masters connected at once (2026-10-11). |
| S5 | low, fixed 2026-10-10 | A pair node took failover answers (`Granted`, `LeaseOk`, `Fenced`, `NotYet`, `Refused`) from any member, not only its witness: a faulty member's `Granted` promotes the standby. | Fixed: answers from any node but the configured witness are dropped and logged. |
| S6 | low, fixed 2026-10-10 | A volume's `quota` was accepted but not enforced (documented): an app with a writable volume can fill the disk. | Fixed: a manifest `quota` on a preopen is refused, since nothing enforces one; MANIFEST.md tells operators to give each writable volume a filesystem quota. |
| S7 | low, fixed 2026-10-10 | `priv/` extraction relied on rustbridge-bundle for path safety. | Fixed: Bulkhead refuses a bundle with a `priv/` path that is not plain relative names (`..`, `.`, a root, an empty part, `:` or a backslash), on open and again on extraction. |
| S8 | low, fixed 2026-10-10 | Each incoming `Frame::Call` started a task with no bound; an honest peer is bounded by its instances' `pending_calls_max`, a faulty one was not. | Fixed: at most `[cluster] incoming_calls_max` (default 10,000) calls from one peer run at once; beyond it a call is answered `overloaded`, with one warning until the peer is back to half the limit. |
| S9 | low, fixed 2026-10-10 | The cluster key and signer keys were loaded without checking their permissions. | Fixed: a key its group or others may access is refused, with a message to `chmod 600` it. |
| S10 | medium, fixed 2026-11-08 | A native app whose manifest declared `bulkhead:files` kept its volumes and `priv/` files when the operator's policy denied the capability: the node did not check the grant. | Fixed: the node passes a native app its preopens only with the grant (`native/port.rs`); `tests/services.rs` and the node's `tests/node.rs` hold it. |
| S11 | high, fixed 2026-11-11 | The memfd holding a native instance's rings was made without seals. The app holds it read-write (an app that does not use `bulkhead_native::app::serve`, or keeps a copy before it, has the descriptor), and neither the seccomp filter nor Landlock refuses `ftruncate` on an inherited descriptor. Truncated, the node's next read of the ring faulted with `SIGBUS`: one app ended the node and every app on it. Found in a review of the code against the goals; reproduced with the native fixture. | Fixed: `Channel::create` seals the memfd's size (`F_SEAL_SHRINK`, `F_SEAL_GROW`, `F_SEAL_SEAL`; writes stay open, since both sides write). `bulkhead-native`'s `create____memfd_resized____refused` and `bulkhead-host`'s `executable____shrinks_its_channel____refused_and_the_node_runs_on` hold it. |

## Fuzz targets

In `fuzz/`, run nightly for five minutes each: `bundle_open`, `manifest`, `parse` (the small parsers: pids, sizes, routing, port ranges, CIDRs, capability keys, package names), `routing_key`, `process_ops`, and these, by who controls the input:

| Target | Input from | Function |
| --- | --- | --- |
| `wire_packet` | cluster members | `transport::wire::decode` and the hello's version agreement |
| `failover_status` | the partner | `failover::Status::decode` (heartbeat payloads) |
| `http_reply` | apps | the HTTP reply envelope decoder in `http.rs` |
| `state_view` | apps | `node/state_view.rs`, which decodes an app's snapshot for `bulkhead state` |
| `modbus_request` | Modbus masters | the server's request handling against its tables (address and count bounds) |
| `native_frame` | native apps | `bulkhead_native::hot::parse_from_app` (hot frames) and the CBOR frame decoder the port uses |

Private functions are exported for fuzzing the way `routing_key` is: `#[cfg(fuzzing)] #[doc(hidden)]`.

## Not reviewed

Dependencies beyond `cargo deny` (advisories, licences, sources); denial of service by a cluster member, which the trust model above accepts; the SDK as an attack surface for apps themselves.
