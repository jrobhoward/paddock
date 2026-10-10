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
| The operator (anyone who can open the admin socket: the node's user, mode 0600) | everything: deploy, kill, confirm failover, read state | — |
| A signer in `trusted_keys` | publish code the node may run | do anything its grant does not allow; the sandbox and the grant are the boundary even for signed code |
| An app | use what it was granted | reach other apps' data, the host, or other apps' messages beyond what routing allows |
| A cluster member (a node whose key is in the peers file) | everything a local process could do: call, cast, link, kill any pid on the node, take part in failover | — (messaging ACLs, D5, are planned) |
| The witness | arbitrate its pair | — |
| HTTP clients, listener clients, Modbus masters | nothing | anything beyond the routes, handlers and tables they reach |
| Modbus devices polled | return register values | crash or stall the node |

Cluster members trust one another fully, as nodes of an Erlang cluster do: mutual TLS with pinned keys decides who is a member, and a member is not checked further. A member compromised is a cluster compromised.

## Boundaries

### Guest and host

- **Controls.** wasmtime's sandbox with a pooling allocator; `memory_max`, table and instance limits; the callback deadline enforced at epoch ticks; bounded mailboxes and `message_max_bytes`; host-object limits (timers, monitors, links, pending calls, groups, watches); deny stubs for ungranted imports; imports checked against the manifest at deploy. Exit signals into an instance are bounded by the node's instance count, timers and watch notices by their own limits.
- **Gaps.** S2 (log injection), S3 (stdout floods), S6 (volume quota).

### App and app

- **Controls.** Buckets namespaced by app; shared buckets by grant, read-only or read-write; secrets by name and grant; groups by manifest scope; registered names limited to those the manifest lists.
- **Gaps.** S1 (two apps may declare one name). Any app may call or cast any pid or registered name on the node; messaging ACLs are D5.

### The admin socket

- **Controls.** A Unix socket created owner-only; bundles verified against `trusted_keys` whoever uploads them; uploads capped at 512 MiB; changes recorded in `audit.log`.
- **Gaps.** None beyond the trust above. The remote admin API (D6) needs authentication of its own.

### Node and node

- **Controls.** QUIC with TLS 1.3 and raw public keys pinned per node id; a hello naming the node id, checked against its key; frames of at most 64 MiB; a versioned CBOR wire format; sessions that discard frames from an ended session or an earlier start.
- **Gaps.** S5 (failover answers from any member), S8 (unbounded tasks for incoming calls). Wire decoding is not fuzzed (see below).

### Ingress clients

- **HTTP.** Routes by host and path prefix; body limit per route (1 MiB default); header timeout 10 s, body timeout 30 s; at most 1,024 connections per address; reply size limit; TLS with rustls on ring, certificates reloaded on change. No client certificates yet.
- **Listeners.** `max_connections` and an idle timeout per listener; each connection is its own instance with its own limits.
- **Gaps.** None found.

### Modbus

- **Devices** are polled with a timeout per request; responses are parsed by tokio-modbus.
- **The server** answers any master that reaches its address: Modbus has no authentication.
- **Gaps.** S4 (no allow-list of masters).

### Liveness, membership and failover

- **Controls.** As for node and node; the failover rules R1 to R11 and their model (`docs/tla/VERIFICATION.md`).
- **Gaps.** S5; Q8, Q10, Q11 in the roadmap.

### Bundles, manifests and policy

- **Controls.** Signatures checked against operator keys only (never the key inside the bundle); checksums; manifest validation; the effective grant computed from the manifest and policy; `priv/` paths checked by rustbridge-bundle (no absolute paths, no `..`).
- **Gaps.** S7 (path safety rests on a dependency).

### Secrets and keys

- **Controls.** Secret values are read on each access and never printed: errors name the secret, and `Debug` shows only the source. Keys are written with mode 0600.
- **Gaps.** S9 (key permissions not checked on load). No test asserts that a secret's value stays out of logs and errors (ROADMAP Q5).

## Findings

| # | Severity | Finding | Fix |
| --- | --- | --- | --- |
| S1 | medium | Two deployed apps may list the same registered name. The first instance to register it holds it; names are released when the holder exits, so each restart is a race the other app can win and receive the messages meant for the first. | Refuse a deploy that lists a name another deployed app lists. |
| S2 | medium | Guest log messages and contexts are written as given. A newline starts what looks like a new host record, with any timestamp and target: checked with tracing-subscriber 0.3.23, which escapes ANSI sequences but not newlines. Operators' logs, and the soak's checker, which reads the gate's records, can be fooled. | Escape control characters in guest text (log message and context; carriage returns in stdout lines). |
| S3 | medium | Guest stdout and stderr become log records with no level filter and no rate limit, so one app can flood the node's log. | Count captured lines against the app's logging rate limit. |
| S4 | medium | The Modbus server accepts reads and writes from any master that reaches its address; a write changes the served table and is cast to the `deliver_to` app. | An allow-list of master addresses and a connection cap in node config; say in MANIFEST.md to bind it to a private interface. |
| S5 | low | A pair node takes failover answers (`Granted`, `LeaseOk`, `Fenced`, `NotYet`, `Refused`) from any member, not only its witness: a faulty member's `Granted` promotes the standby. | Drop answers whose sender is not the configured witness. |
| S6 | low | A volume's `quota` is not enforced (documented): an app with a writable volume can fill the disk. | Enforce the quota, or refuse `rw` volumes without a filesystem-level quota. |
| S7 | low | `priv/` extraction relies on rustbridge-bundle for path safety. | Check each path in Paddock as well (normal components only), with a test bundle that tries `..` and absolute paths. |
| S8 | low | Each incoming `Frame::Call` starts a task with no bound; an honest peer is bounded by its instances' `pending_calls_max`, a faulty one is not. | A limit per peer on calls in progress. |
| S9 | low | The cluster key and signer keys are loaded without checking their permissions. | Refuse or warn on a key readable by group or others, as ssh does. |

## Fuzz targets

Existing (`fuzz/`, nightly, five minutes each): `bundle_open`, `manifest`, `parse` (the small parsers: pids, sizes, routing, port ranges, CIDRs, capability keys, import names), `routing_key`, `process_ops`.

To add, by who controls the input:

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
