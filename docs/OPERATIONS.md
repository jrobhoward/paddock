# Paddock: Operating a node

Where a node keeps its files, what it reads from the environment, what to back up, and how to run it as a service. Node configuration keys are in [MANIFEST.md](MANIFEST.md); the build features are in the [README](../README.md#building).

## Paths

A node reads its configuration, keys and policy from wherever its config file names them (`/etc/paddock` by convention), and writes only under `data_dir` (`/var/lib/paddock` by convention). Logs go to stderr; there are no log files.

| Under `data_dir` | What | Back up |
| --- | --- | --- |
| `state.json` | Deployed apps: versions, running or stopped, deploy order | yes |
| `bundles/<app>/<version>.rbp` | The signed bundles of each app's current and previous version | yes |
| `kv/` | Keyvalue data: each app's buckets and the shared buckets (redb) | yes, from a stopped node or a filesystem snapshot |
| `audit.log` | Every admin change, one JSON record per line | as policy requires |
| `creation` | Raised on every start; part of every pid | never restore an older one (see below) |
| `failover-epoch` | A pair node's highest failover epoch | never restore an older one |
| `witness-state` | A witness's epoch and the node it last promoted | never restore an older one |
| `priv/<app>/<version>/` | Each bundle's `priv/` files, extracted again on every start | no |
| `compiled/<app>/<version>/` | Compiled components, so a restart does not compile again; only files owned by the node's user and not writable by group or others are read | no |
| `tmp/` | Uploads being verified | no |
| `node.lock` | Held while the node runs; a second node on the same `data_dir` is refused | no |
| `admin.sock` | The admin API (mode 0600), unless `admin_socket` names another path | no |

Restoring a backup: keep the current `creation`, `failover-epoch` and `witness-state` files rather than the backup's. Each must never go back: `creation` keeps pids from an earlier run from being reused, and the epochs are failover's fencing tokens.

Volumes (`[volumes]`) are the operator's directories, outside `data_dir`; back them up with the apps' data.

## Environment

| Variable | Read by | Meaning |
| --- | --- | --- |
| `PADDOCK_CONFIG` | `paddock node` | The node config file, instead of `--config` |
| `PADDOCK_SOCKET` | every admin command | The admin socket, instead of `--socket` (default `/var/lib/paddock/admin.sock`) |
| `PADDOCK_REMOTE`, `PADDOCK_NODE_KEY`, `PADDOCK_KEY` | every admin command | A node's remote admin API (`host:port`), its public key, and the operator's private key, instead of `--remote`, `--node-key` and `--key` |
| `PADDOCK_KEY_PASSWORD` | `paddock bundle`, `paddock keygen` | The signing key's password, instead of a prompt |
| `RUST_LOG` | `paddock node` | Log filter (default `info`), for example `info,paddock::lease=debug` |

Secrets may also come from variables the node config names (`[secrets.<name>] env = ...`).

## Signals

- **SIGTERM or Ctrl-C** stops every app, in reverse start order, and leaves the cluster.
- **SIGHUP** reloads the HTTPS certificates and the cluster key and peers file (as `paddock reload-tls` does). Both also reload by themselves when their files change (`tls_reload_s`, `identity_reload_s`).

## Running as a service

[`examples/systemd/paddock.service`](../examples/systemd/paddock.service) runs a node as its own unprivileged user. Privileges:

- No root. `CAP_NET_BIND_SERVICE` is the only capability, and only for ingress or listener ports below 1024; leave it out otherwise.
- `dialout` for serial lines (Modbus RTU and the failover serial heartbeat, both planned), with `PrivateDevices` off.
- `StateDirectory` creates `data_dir` owned by the service user, mode 0750; the node creates what is under it.

Reaching the primary of a pair from outside needs no root. Either put a load balancer in front of both nodes, with a health check on the app's own route expecting 200, or set `partner_url` on each node's `[http.<name>]` listener to the other node's address for it, as clients reach it: the standby then answers a singleton's requests with `307 Temporary Redirect` to the primary, and clients that follow redirects can use either address (tutorial chapter 2). Or set `standby = "forward"` on the listener: the standby passes a singleton's requests over the cluster to the primary's app and answers with its reply, so either address works for any client. For one address and no load balancer, a separate agent can move a virtual IP (below).

### A virtual IP that follows the primary

`paddock failover --watch` prints the node's standing once, then a line each time its leadership changes: role, epoch, and whether singletons may run here. With `--json`, each line is one object, such as `{"awaiting_confirmation":false,"epoch":3,"may_run":true,"role":"primary"}`. When the node cannot be reached, it prints `{"may_run":false,"role":"unreachable"}` once and tries again every second. It reads `GET /v1/failover?after=<generation>&wait_ms=<ms>`, a long poll that answers when the leadership has changed since that generation (`generation` in `paddock failover --json`), or after the wait (at most 60 s).

An agent beside each node of the pair holds the address while its node may run singletons. [`examples/systemd/paddock-vip.sh`](../examples/systemd/paddock-vip.sh) does this with `ip addr` and a gratuitous `arping`, and [`paddock-vip.service`](../examples/systemd/paddock-vip.service) runs it as the `paddock` user with `CAP_NET_ADMIN` and `CAP_NET_RAW` only. `paddock node` keeps neither. A cloud provider's API for moving an address fits the same loop. Neither the script nor the unit is tested in CI, since both need those capabilities.

- **When the address moves.** It follows `may_run`, so it moves when the singletons do: the old primary releases it when its lease ends, and the new primary takes it when it is promoted, about the time of a takeover (3 to 4 s with the tutorial's timings). Two nodes never hold it while both may run singletons, since the lease rules out two primaries; for a moment they can both hold it while one is releasing, and neighbours follow the last gratuitous ARP.
- **A lost node.** The watch prints `unreachable` when the agent cannot reach its own node, so the agent releases the address. A node that is gone with its agent (a power cut) releases nothing, and the address moves when the other node takes it.
- **keepalived instead.** A site that already runs keepalived can have it follow Paddock's role. A `vrrp_script` that runs `paddock --json failover` and succeeds only when `"may_run": true` appears, with a weight above the gap between the two nodes' priorities, makes the Paddock primary the VRRP master. VRRP then runs its own election beside Paddock's, so the address can lag a takeover by keepalived's own interval, and during a network cut each side's keepalived decides on its own. The watch agent above does not have these gaps.
