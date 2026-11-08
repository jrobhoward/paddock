# Bulkhead: Operating a node

Where a node keeps its files, what it reads from the environment, what to back up, and how to run it as a service. Node configuration keys are in [MANIFEST.md](MANIFEST.md); the build features are in the [README](../README.md#building).

## Paths

A node reads its configuration, keys and policy from wherever its config file names them (`/etc/bulkhead` by convention), and writes only under `data_dir` (`/var/lib/bulkhead` by convention). Logs go to stderr; there are no log files.

| Under `data_dir` | What | Back up |
| --- | --- | --- |
| `state.json` | Deployed apps: versions, running or stopped, deploy order | yes |
| `bundles/<app>/<version>.rbp` | The signed bundles of each app's current and previous version | yes |
| `kv/` | Keyvalue data: each app's buckets and the shared buckets, and on a pair node the replicated log with its `cluster` buckets (redb) | yes, from a stopped node or a filesystem snapshot |
| `audit.log` | Every admin change, one JSON record per line | as policy requires |
| `creation` | Raised on every start; part of every pid | never restore an older one (see below) |
| `failover-epoch` | A pair node's highest failover epoch | never restore an older one |
| `witness-state` | A witness's epoch, the node it last promoted, and `outdated` while that node's partner lacks replicated writes | never restore an older one |
| `priv/<app>/<version>/` | Each bundle's `priv/` files, extracted again on every start | no |
| `compiled/<app>/<version>/` | Each version's native program, written from its bundle; only the node can change it. Rebuilt from the bundle if missing. |
| `tmp/` | Uploads being verified | no |
| `node.lock` | Held while the node runs; a second node on the same `data_dir` is refused | no |
| `admin.sock` | The admin API (mode 0600), unless `admin_socket` names another path | no |

Restoring a backup: keep the current `creation`, `failover-epoch` and `witness-state` files rather than the backup's. Each must never go back: `creation` keeps pids from an earlier run from being reused, and the epochs are failover's fencing tokens.

`cluster` buckets are kept on both nodes of a pair, so a node whose `kv/` is lost or older needs no restore: started while its partner runs as primary, it rejoins as the standby and the primary brings it up to date (from its log, or with a snapshot). Restoring an older `kv/` on a node that then leads (the only node left, or both nodes) loses every write since the backup.

Volumes (`[volumes]`) are the operator's directories, outside `data_dir`; back them up with the apps' data.

## Environment

| Variable | Read by | Meaning |
| --- | --- | --- |
| `BULKHEAD_CONFIG` | `bulkhead node` | The node config file, instead of `--config` |
| `BULKHEAD_SOCKET` | every admin command | The admin socket, instead of `--socket` (default `/var/lib/bulkhead/admin.sock`) |
| `BULKHEAD_REMOTE`, `BULKHEAD_NODE_KEY`, `BULKHEAD_KEY` | every admin command | A node's remote admin API (`host:port`), its public key, and the operator's private key, instead of `--remote`, `--node-key` and `--key` |
| `BULKHEAD_CONTACTS` | `bulkhead shell` | The contacts file, instead of `--contacts` (default `contacts.toml` under `$XDG_CONFIG_HOME/bulkhead` or `~/.config/bulkhead`) |
| `XDG_CONFIG_HOME` | `bulkhead shell` | Where the contacts file and the shell's history (`bulkhead/history`) are |
| `BULKHEAD_KEY_PASSWORD` | `bulkhead bundle`, `bulkhead keygen` | The signing key's password, instead of a prompt |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | `bulkhead node` | Where HTTP egress reads the system's root certificates, instead of the OS defaults (`[egress] system_roots`) |
| `RUST_LOG` | `bulkhead node` | Log filter (default `info`), for example `info,bulkhead::lease=debug` |
| `BULKHEAD_PRIV`, `BULKHEAD_PREOPENS`, `BULKHEAD_SPIN_US` | a native-process app, set by the node | The directory of the bundle's `priv/` files; where each preopened directory is, one guest path and host directory per line, separated by a tab (with `\\`, `\t` and `\n` escaped), for `bulkhead_sdk::files::path`; and how long the app spins waiting for the node before it sleeps (`[native] spin_us`). A native app's environment holds nothing else |

Secrets may also come from variables the node config names (`[secrets.<name>] env = ...`).

## The remote admin API

`[admin] address` serves the admin API over TCP with TLS 1.3 to operators on other machines (cargo feature `tls`, on by default), with the same routes and rights as the admin socket. Keys are pinned both ways, with no certificate authority:

- The node presents `[admin] key`, or its `[cluster] key` when unset. `bulkhead cluster pubkey <key file>` prints the public key operators pin with `--node-key` (or `BULKHEAD_NODE_KEY`).
- Each operator makes a key with `bulkhead cluster keygen <path>`, and the node's operators file lists the public keys it accepts, by name: `[operators] alice = ["ed25519:..."]`. The file is read again on SIGHUP and `bulkhead reload-tls`; removing a key stops new connections, not open ones.
- `bulkhead --remote <host:port> --node-key ed25519:... --key <operator key> <command>` runs any command against the node; the shell keeps these in a contacts file (below).

Each connection is logged with the operator's name, and every request but a `GET` is written to the audit log with it. Every listed operator has every right; put the address on a management network (SECURITY.md, "The admin socket and the remote admin API").

## The shell

`bulkhead shell` runs `bulkhead` commands at a prompt against one node: the admin socket, or the node `--remote` names. `connect <name>` (or a node id, or `local` for the socket) moves the session to a node in the operator's contacts file, reached over its remote admin API (`[admin]`) with the key pinned there:

```toml
# ~/.config/bulkhead/contacts.toml
key = "operator.key"        # the operator's private key, relative to this file; --key overrides it

[[node]]
name = "alpha"
id = 1                      # optional: `connect 1`, and the CONTACT column of `members`
address = "10.0.0.1:7443"   # the node's [admin] address
node_key = "ed25519:..."    # the node's public key
```

Ctrl-C stops the command running (a follow, a slow call); Ctrl-D or `exit` leaves. History is kept in `~/.config/bulkhead/history`.

## Signals

- **SIGTERM or Ctrl-C** stops every app, in reverse start order, and leaves the cluster.
- **SIGHUP** reloads the HTTPS certificates, the cluster key and peers file, and the remote admin operators file (as `bulkhead reload-tls` does). The certificates and the cluster identity also reload by themselves when their files change (`tls_reload_s`, `identity_reload_s`).

## Running as a service

[`examples/systemd/bulkhead.service`](../examples/systemd/bulkhead.service) runs a node as its own unprivileged user. Privileges:

- No root. `CAP_NET_BIND_SERVICE` is the only capability, and only for ingress or listener ports below 1024; leave it out otherwise.
- `dialout` (or the device's group) for the failover serial line (`[failover] serial`), with `PrivateDevices` off. A device that cannot be opened is logged and retried every second; the path stays silent meanwhile, and `bulkhead failover` shows it (`paths: link heard, serial silent`).
- `StateDirectory` creates `data_dir` owned by the service user, mode 0750; the node creates what is under it.
- Native-process apps need `bulkhead-runner` beside `bulkhead` (or `[native] runner`), built with `cargo build --release -p bulkhead-native`. For their memory and process limits, give the node a cgroup: `Delegate=yes` in the unit, `ProtectControlGroups` removed, and `[native] cgroup` set to the unit's cgroup (`/sys/fs/cgroup/system.slice/bulkhead.service`); the node must be the only process in it, and moves itself into its `node` child.
- Native-process apps and the node spin for `[native] spin_us` (default 20 µs) waiting for each other before they sleep: calls and requests within a node cost a fifth to two fifths of what blocking waits cost, for 1.5 to 1.9 times the CPU per call while many instances are busy. On a small machine with many busy native instances (a Raspberry Pi 5 with 100), that CPU cost half the throughput until 2026-11-05; since T1b neither side spins after a callback with nothing queued behind it that came longer than the spin after the one before, which gave the Pi about 1.8 times the calls in flight and the 16-core machine a third less CPU per call, with sequential calls unchanged (BASELINE.md, 2026-11-05, "spinning only where a callback follows soon"). `spin_us = 0` is no longer needed for that; it still trades requests of 3 to 4 µs for 13 to 28. The spinning stops when the traffic does. A published bucket (`[buckets.<name>] publish_ms`) is rebuilt whole after each change, by one thread per node: for data written far less often than it is read.
- On aarch64 CPUs with LSE atomics (ARMv8.1 and later: Raspberry Pi 5, AWS Graviton 2 and later, Apple silicon), building with `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+lse"` makes messages between instances 3 to 9% cheaper (BASELINE.md, 2026-10-29). Such a binary does not run on ARMv8.0 CPUs (Raspberry Pi 4).

Reaching the primary of a pair from outside needs no root. Either put a load balancer in front of both nodes, with a health check on the app's own route expecting 200, or set `partner_url` on each node's `[http.<name>]` listener to the other node's address for it, as clients reach it: the standby then answers a singleton's requests with `307 Temporary Redirect` to the primary, and clients that follow redirects can use either address (tutorial chapter 2). Or set `standby = "forward"` on the listener: the standby passes a singleton's requests over the cluster to the primary's app and answers with its reply, so either address works for any client. For one address and no load balancer, a separate agent can move a virtual IP (below).

### Planned switchover

`bulkhead failover switchover` on the primary hands its singletons to the standby, for maintenance of the primary or to swap roles on purpose. The primary stops its singletons gracefully (each runs `terminate`; one in `snapshot` upgrade mode, on a pair with `[failover] replication = true`, instead hands its state over and starts on the standby from it, logged as `singleton stopped: planned switchover, state moved` and `singleton moved in`), then steps down and releases its lease at the witness; the standby runs them within a few heartbeats (about a lease and its margins, 3.5 to 4 s with the defaults, if the release does not reach the witness, or the witness runs the previous release; no node timeout either way), and the old primary rejoins as its standby. It is refused, and nothing stops, when the node is not the primary, its arbiter is not a witness, it does not hear its partner as a standby (a moment after the partner becomes one, try again), or the partner is still catching up on replicated writes (the witness has it recorded out of date). Recorded in the audit log as `failover_switchover`.

### Confirming a promotion

A standby asks the operator to confirm only while it cannot hear its partner on any path and cannot reach the witness either (with arbiter `"operator"`, whenever the partner looks gone); `bulkhead failover` then shows `waiting:`. `bulkhead failover confirm` promotes it once the partner's lease has run out, and the confirmation is final. Confirm only when the old primary is down and stays down, powered off or fenced, until the two nodes hear each other again: the witness never learns of the confirmation, so an old primary that restarts and reaches the witness resumes, and both run the singleton until they hear each other (docs/tla/VERIFICATION.md, finding 6).

### Upgrading the builds of a pair

A node speaks its own wire protocol version and the one before, so the witness and the two nodes can be upgraded one at a time, each to the next release: the witness first, then the standby, a planned switchover, then the other node. `bulkhead release` shows the versions a node speaks, the version each connected member agreed on, and the version of the formats the pair keeps. Until the pair is finalized, either node can be rolled back to the build before. With every node on the new build, `bulkhead release finalize` on either node of a pair with `[failover] replication = true` raises the pair's version to the build's; it is refused unless the partner and the witness are connected at that version. From then on a node refuses to start a build older than the pair's version, naming both versions; one rolled back while it was cut off from the raise learns of it from its partner, logs `the pair was finalized at a version newer than this build`, and shuts down with the same message. Recorded in the audit log as `release_finalized`. Builds before 2026-10-21 do not check, so a rollback to one of those is not stopped. The keyvalue file (`kv/`) needs nothing either way: builds from 2026-10-25 on use redb 4 and earlier ones redb 3, and each opens and writes the other's files (checked with Bulkhead's tables in both directions, compaction included, 2026-10-25).

Native apps are held to the same window: a node accepts apps built against its native protocol version (`bulkhead_native::protocol::VERSION`) and the one before, so the apps deployed keep running across one node upgrade. A release whose node changes that version twice since an app was built refuses the app at its start (`crashed: native protocol version N; this node accepts ...`); rebuild and redeploy apps before then. The CHANGELOG says when a release changes the version.

### Releasing an app across a pair

`bulkhead release upgrade <app> <bundle> --pair pair.toml` upgrades an app on both nodes: the standby, then the primary in place, waiting after each for the app to run at the new version there (`--timeout-s`, 60 by default). The pair file lists each node as `[[node]]` with its `id` and either `socket` (its admin socket) or `address` and `key` (its remote admin API and public key, with `--key` for the operator's private key). Each run reads both nodes first, so after a stop (a node out of reach, a change of roles) running the same command continues; a refused step or a failed gate rolls back what the run changed (`--no-abort` leaves it). `--standby-only` upgrades the standby alone and leaves the primary for a later run without it, to look at the new version before the primary moves (`examples/scada/drill.sh` uses it). `bulkhead release rollback <app> --pair pair.toml` returns both nodes to their common previous version. The rollout settings of `bulkhead upgrade` (`--approve`, `--batch`, `--observe-ms`, `--max-restarts`, `--restart`, and for a blue-green app `--steps` and `--drain-ms`) apply on each node. With `--json` the report lists each step and the outcome (`done`, `stopped`, `aborted`); the command exits non-zero unless it is `done`.

### A virtual IP that follows the primary

`bulkhead failover --watch` prints the node's standing once, then a line each time its leadership changes: role, epoch, and whether singletons may run here. With `--json`, each line is one object, such as `{"awaiting_confirmation":false,"epoch":3,"may_run":true,"role":"primary"}`. When the node cannot be reached, it prints `{"may_run":false,"role":"unreachable"}` once and tries again every second. It reads `GET /v1/failover?after=<generation>&wait_ms=<ms>`, a long poll that answers when the leadership has changed since that generation (`generation` in `bulkhead failover --json`), or after the wait (at most 60 s).

An agent beside each node of the pair holds the address while its node may run singletons. [`examples/systemd/bulkhead-vip.sh`](../examples/systemd/bulkhead-vip.sh) does this with `ip addr` and a gratuitous `arping`, and [`bulkhead-vip.service`](../examples/systemd/bulkhead-vip.service) runs it as the `bulkhead` user with `CAP_NET_ADMIN` and `CAP_NET_RAW` only. `bulkhead node` keeps neither. A cloud provider's API for moving an address fits the same loop. Neither the script nor the unit is tested in CI, since both need those capabilities.

- **When the address moves.** It follows `may_run`, so it moves when the singletons do: the old primary releases it when its lease ends, and the new primary takes it when it is promoted, about the time of a takeover (3 to 4 s with the tutorial's timings). Two nodes never hold it while both may run singletons, since the lease rules out two primaries; for a moment they can both hold it while one is releasing, and neighbours follow the last gratuitous ARP.
- **A lost node.** The watch prints `unreachable` when the agent cannot reach its own node, so the agent releases the address. A node that is gone with its agent (a power cut) releases nothing, and the address moves when the other node takes it.
- **keepalived instead.** A site that already runs keepalived can have it follow Bulkhead's role. A `vrrp_script` that runs `bulkhead --json failover` and succeeds only when `"may_run": true` appears, with a weight above the gap between the two nodes' priorities, makes the Bulkhead primary the VRRP master. VRRP then runs its own election beside Bulkhead's, so the address can lag a takeover by keepalived's own interval, and during a network cut each side's keepalived decides on its own. The watch agent above does not have these gaps.
