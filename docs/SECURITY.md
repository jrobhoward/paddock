# Paddock: Security

2026-10-10 · Draft (ROADMAP R3); W1 finishes it · Companion to [ARCHITECTURE.md](ARCHITECTURE.md)

The threat model as built: what Paddock protects, whom it trusts, each boundary between trusted and untrusted parts with the controls on it and the gaps found, and the fuzz targets. Findings are numbered S1 and on; their fixes are ROADMAP stage 3 items.

## What is protected

- **Singleton exclusivity.** A singleton drives devices; two copies running at once is the worst outcome (DESIGN.md, "Distribution"; `docs/tla/VERIFICATION.md`).
- **App isolation.** Each app sees only the capabilities its manifest declares and the operator grants: its own buckets and files, shared buckets it was granted, the secrets it lists.
- **Secrets** bound on the node, and the node's keys: the cluster key and the operator's trusted signer keys.
- **Availability** of the node and of other apps when one app misbehaves.
- **Logs and the audit log**, which operators and the soak's checker read as the node's own record.

## Whom Paddock trusts

| Party | Trusted to | Not trusted to |
| --- | --- | --- |
| The operator (anyone who can open the admin socket: the node's user, mode 0600; or holds a key the `[admin]` operators file lists) | everything: deploy, kill, confirm failover, read state | — |
| A signer in `trusted_keys` | publish code the node may run | do anything its grant does not allow; the sandbox and the grant are the boundary even for signed code |
| An app | use what it was granted | reach other apps' data, the host, or other apps' messages beyond what routing allows |
| A cluster member (a node whose key is in the peers file) | what its own instances could do on the node: call, cast, link and send exit signals within the messaging ACLs, as its stamp on each frame says; call an app by name from its host, as a standby's ingress does to forward an HTTP request (not subject to messaging ACLs, as host calls are not); take part in failover | speak for another node's instances (a frame whose sender pid is on another node is dropped); its stamps are trusted, so a member that lies about its senders' apps passes ACLs it should not |
| The witness | arbitrate its pair | — |
| HTTP clients, listener clients, Modbus masters | nothing | anything beyond the routes, handlers and tables they reach |
| Modbus devices polled | return register values | crash or stall the node |

Cluster members trust one another fully, as nodes of an Erlang cluster do: mutual TLS with pinned keys decides who is a member, and a member is not checked further. A member compromised is a cluster compromised.

## Boundaries

### Guest and host

- **Controls.** wasmtime's sandbox with a pooling allocator; `memory_max`, table and instance limits; the callback deadline enforced at epoch ticks; bounded mailboxes and `message_max_bytes`; host-object limits (timers, monitors, links, pending calls, groups, watches); deny stubs for ungranted imports; imports checked against the manifest at deploy. Exit signals into an instance are bounded by the node's instance count, timers and watch notices by their own limits.
- **Gaps.** None found; S2, S3 and S6 are fixed. A writable volume is bounded only by the filesystem quota the operator gives it.

### App and app

- **Controls.** Buckets namespaced by app; shared buckets by grant, read-only or read-write; secrets by name and grant; groups by manifest scope; SQL datasources by name and grant, with a connection cap per app; registered names limited to those the manifest lists; messaging ACLs (`messaging_acl.accept_from`, `may_call`, by app name) on calls, casts, exit signals and links, checked on the target's node against the host-stamped sender.
- **Gaps.** S1 is fixed. Without a `messaging_acl`, an app may call, cast, link or send exit signals to any pid it learns, as before; the lists are opt-in. Monitors are not checked (they only observe).

### The admin socket and the remote admin API

- **Controls.** A Unix socket created owner-only; bundles verified against `trusted_keys` whoever uploads them; uploads capped at 512 MiB; changes recorded in `audit.log`. The remote admin API (`[admin]`) is TLS 1.3 with raw public keys both ways: the node presents its key, which the operator pins (`--node-key`), and accepts only a key its operators file lists; a handshake has 10 s to finish; every request but a `GET` is recorded in `audit.log` with the operator's name.
- **Gaps.** Every listed operator has every right the socket gives: there are no read-only operators. Anyone who can reach the address can open connections and start handshakes; there is no limit on either, so put the address on a management network. Removing an operator stops new connections, not one already open, including a stream (`paddock events`, `logs --follow`) that has run since. The shell keeps its history, call bodies included, in the operator's `~/.config/paddock/history`.

### Node and node

- **Controls.** QUIC with TLS 1.3 and raw public keys pinned per node id; a hello naming the node id, checked against its key; frames of at most 64 MiB; a versioned CBOR wire format; sessions that discard frames from an ended session or an earlier start.
- **Gaps.** None beyond the trust above; S5 and S8 are fixed. Calls from one peer in progress at once are capped (`[cluster] incoming_calls_max`).

### Ingress clients

- **HTTP.** Routes by host and path prefix; body limit per route (1 MiB default); header timeout 10 s, body timeout 30 s; at most 1,024 connections per address; reply size limit; TLS with rustls on ring, certificates reloaded on change. No client certificates yet.
- **Listeners.** `max_connections` and an idle timeout per listener; each connection is its own instance with its own limits.
- **HTTP handlers.** Each request is its own instance with the app's limits and a Store slot; the request body streams to the guest under the route's `max_body`, and the route's timeout bounds the wait for headers and each pause in either body. HTTP is still parsed by hyper in the node (wasmtime's `wasi:http` host hands the guest a parsed request).
- **Gaps.** None found.

### HTTP egress

- **Controls.** An app reaches only the schemes, hosts and ports its manifest's `allow` lists, inside node config's `[egress] allow`, with the methods it lists, once policy grants `wasi:http/outgoing-handler`; anything else is refused before a connection opens. A host name is resolved by the host, and one that resolves only to loopback or link-local addresses (cloud metadata at 169.254.169.254) is refused at each connection, so a name that later resolves there is refused too; an address is reached only when an entry names it. HTTPS verifies the server against the system's roots and node config's CA file. Requests in flight are capped per app, and timeouts bound the wait for headers and between body chunks. Redirects are not followed. Forbidden request headers (wasmtime's list: `connection`, `keep-alive`, `proxy-*`, `transfer-encoding`, `upgrade`, `host` set from the URL) are refused.
- **Gaps.** Private ranges (10/8, 172.16/12, 192.168/16, fc00::/7) are allowed for any name an entry allows, since internal services live there; a `*.internal` entry trusts whoever answers DNS for that zone. Responses are parsed by hyper in the host and passed to the guest as they come; a response body has no size cap other than the guest's own memory. No outbound proxy (HTTP `CONNECT`) and no client certificates.

### SQL datasources

- **Credentials** come from a node secret (`password_secret`) and never reach a guest; apps name datasources, not addresses.
- **Statements** are the guest's own SQL, run with the datasource's user: what that user may do in the database is the operator's grant there. Parameters are bound, never interpolated by the host. A statement timeout (server-side) and a result size cap bound each call; a connection cap per app and an acquire timeout bound what one app holds.
- **Pooled connections are reset** with `ROLLBACK; DISCARD ALL` before reuse, so a transaction or session setting one call leaves does not reach the next caller, which may be another app.
- **TLS** verifies the server against the CA file the operator names; there are no system roots.
- **Gaps.** Apps that share a datasource share its database user, so the database cannot tell them apart; give each app its own datasource and user where that matters. Results from the server are parsed by `tokio-postgres` and decoded by the host.

### Modbus

- **Devices** are polled with a timeout per request; responses are parsed by tokio-modbus.
- **The server** answers only masters in `server.allow` (loopback by default): Modbus has no authentication of its own.
- **Gaps.** None found; S4 is fixed. At most `server.max_connections` masters (default 32) are connected at once.

### Liveness, membership and failover

- **Controls.** As for node and node; the failover rules R1 to R11 and their model (`docs/tla/VERIFICATION.md`). Serial heartbeat frames carry a CRC-16 and the sender's and receiver's node ids; frames that fail either check are dropped.
- **Serial line.** Frames on the pair's serial cable are not authenticated: whoever can reach the cable or a device on it can forge beats, which can keep a standby from promoting (a forged "primary alive") or renew a lease a primary would otherwise lose. The cable is treated as part of the pair's physical enclosure.
- **Gaps.** S5 is fixed. With a witness, an operator is asked to confirm a promotion only while the witness cannot be reached either, and a confirmation then is final: if the partner confirmed gone was alive, or restarts, and reaches the witness but not this node, both run until they hear each other (VERIFICATION.md, A9 and finding 6, accepted 2026-10-16); operators confirm only a partner that is down and stays down (powered off or fenced) until then. A primary paused past its lease no longer runs its singleton's callbacks when it continues (A8 in `docs/tla/VERIFICATION.md`).

### Bundles, manifests and policy

- **Controls.** Signatures checked against operator keys only (never the key inside the bundle); checksums; manifest validation; the effective grant computed from the manifest and policy; `priv/` paths checked by Paddock on open and again on extraction (plain relative names only), as well as by rustbridge-bundle.
- **Gaps.** None found; S7 is fixed.
- **Compiled components** (`data_dir/compiled`) are machine code a restart runs without compiling or checking it against the bundle again. A file is read only if it and its directory belong to the node's user and neither can be written by group or others; anyone who can write there as that user can run code as the node, as with the node's binary and configuration.

### Secrets and keys

- **Controls.** Secret values are read on each access and never printed: errors name the secret, and `Debug` shows only the source. Keys are written with mode 0600, and the cluster key and signer keys are refused on load if their group or others may access them, as ssh does.
- **Gaps.** None found; S9 is fixed. A test asserts that a secret's value appears in no log record, at any level or target, and in no error (`paddock-host` `tests/secrets.rs`).

## Findings

| # | Severity | Finding | Fix |
| --- | --- | --- | --- |
| S1 | medium, fixed 2026-10-10 | Two deployed apps could list the same registered name. The first instance to register it holds it; names are released when the holder exits, so each restart is a race the other app can win and receive the messages meant for the first. | Fixed: a deploy, upgrade or rollback that lists a name another deployed app lists, running or stopped, is refused (`conflict`). |
| S2 | medium, fixed 2026-10-10 | Guest log messages and contexts were written as given. A newline starts what looks like a new host record, with any timestamp and target: checked with tracing-subscriber 0.3.23, which escapes ANSI sequences but not newlines. Operators' logs, and the soak's checker, which reads the gate's records, can be fooled. | Fixed: control characters in guest text (log messages and contexts, stdout and stderr lines, crash reasons) are escaped. |
| S3 | medium, fixed 2026-10-10 | Guest stdout and stderr became log records with no rate limit, and an app's own `rate_limit_per_sec` was optional, so one app could flood the node's log. | Fixed: every app has a budget, the node's `[limits] log_records_per_sec` (default 1000) or its own lower rate, shared by its log records and its stdout and stderr lines. |
| S4 | medium, fixed 2026-10-10 | The Modbus server accepted reads and writes from any master that reached its address; a write changes the served table and is cast to the `deliver_to` app. | Fixed: `server.allow` lists the masters' addresses or networks, loopback only by default; MANIFEST.md says to bind it to the SCADA interface. `server.max_connections` (default 32) caps the masters connected at once (2026-10-11). |
| S5 | low, fixed 2026-10-10 | A pair node took failover answers (`Granted`, `LeaseOk`, `Fenced`, `NotYet`, `Refused`) from any member, not only its witness: a faulty member's `Granted` promotes the standby. | Fixed: answers from any node but the configured witness are dropped and logged. |
| S6 | low, fixed 2026-10-10 | A volume's `quota` was accepted but not enforced (documented): an app with a writable volume can fill the disk. | Fixed: a manifest `quota` on a preopen is refused, since wasmtime-wasi offers no place to enforce one; MANIFEST.md tells operators to give each writable volume a filesystem quota. |
| S7 | low, fixed 2026-10-10 | `priv/` extraction relied on rustbridge-bundle for path safety. | Fixed: Paddock refuses a bundle with a `priv/` path that is not plain relative names (`..`, `.`, a root, an empty part, `:` or a backslash), on open and again on extraction. |
| S8 | low, fixed 2026-10-10 | Each incoming `Frame::Call` started a task with no bound; an honest peer is bounded by its instances' `pending_calls_max`, a faulty one was not. | Fixed: at most `[cluster] incoming_calls_max` (default 10,000) calls from one peer run at once; beyond it a call is answered `overloaded`, with one warning until the peer is back to half the limit. |
| S9 | low, fixed 2026-10-10 | The cluster key and signer keys were loaded without checking their permissions. | Fixed: a key its group or others may access is refused, with a message to `chmod 600` it. |

## Fuzz targets

In `fuzz/`, run nightly for five minutes each: `bundle_open`, `manifest`, `parse` (the small parsers: pids, sizes, routing, port ranges, CIDRs, capability keys, import names), `routing_key`, `process_ops`, and these, by who controls the input:

| Target | Input from | Function |
| --- | --- | --- |
| `wire_packet` | cluster members | `transport::wire::decode` and the hello's version agreement |
| `failover_status` | the partner | `failover::Status::decode` (heartbeat payloads) |
| `http_reply` | guests | the HTTP reply envelope decoder in `http.rs` |
| `state_view` | guests | `node/state_view.rs`, which decodes a guest's snapshot for `paddock state` |
| `modbus_request` | Modbus masters | the server's request handling against its tables (address and count bounds) |

Private functions are exported for fuzzing the way `routing_key` is: `#[cfg(fuzzing)] #[doc(hidden)]`.

## Not reviewed

Dependencies beyond `cargo deny` (advisories, licences, sources); wasmtime's own sandbox and its side-channel mitigations; denial of service by a cluster member, which the trust model above accepts; the guest SDK as an attack surface for guests themselves.
