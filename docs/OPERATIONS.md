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

Reaching the primary of a pair from outside needs no root. Either put a load balancer in front of both nodes, with a health check on the app's own route expecting 200, or set `partner_url` on each node's `[http.<name>]` listener to the other node's address for it, as clients reach it: the standby then answers a singleton's requests with `307 Temporary Redirect` to the primary, and clients that follow redirects can use either address (tutorial chapter 2). Or set `standby = "forward"` on the listener: the standby passes a singleton's requests over the cluster to the primary's app and answers with its reply, so either address works for any client. A virtual IP moved by a separate agent is ROADMAP D11.
