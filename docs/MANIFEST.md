# Manifest reference

An app's manifest is the `paddock.toml` next to its source. `paddock bundle` turns it into the signed bundle's manifest:
- `[app]` becomes the rustbridge `plugin` identity.
- `[appserver]` is carried as the bundle's `appserver` extension.

Unknown fields are errors, so a typo is refused, not ignored. `paddock bundle` and `paddock deploy` report every broken rule at once, with its path (`appserver.limits.memory_max`).

Fields marked *not enforced* parse and validate, but the node does not act on them. `paddock status` lists each one a manifest sets under `warnings` ("`placement.spread` is not enforced"), so a manifest that sets them still deploys.

Durations are integers in milliseconds (`*_ms`). Sizes are strings: a whole number and a unit, such as `"64MiB"`, `"512KiB"` or `"1GiB"`. The binary units are `KiB`, `MiB`, `GiB` and `TiB`, the decimal ones `KB`, `MB`, `GB` and `TB`, and a number with no unit (or `B`) counts bytes.

```toml
[app]
name = "orders"
version = "1.2.0"
description = "Order intake"
authors = ["Payments team"]
license = "MIT OR Apache-2.0"
repository = "https://example.com/orders"

[appserver]
registered_names = ["orders"]

[[appserver.dependencies]]
app = "inventory"
version = "^2"

[appserver.lifecycle]
restart = "permanent"
init_timeout_ms = 5000
shutdown_timeout_ms = 5000
start_args = { region = "eu" }

[appserver.supervision]
strategy = "one_for_one"
intensity = 5
period_ms = 10000
backoff = { initial_ms = 100, max_ms = 5000, jitter = true }

[appserver.instances]
count = 4
routing = "hash:customer_id"

[appserver.limits]
memory_max = "64MiB"
callback_deadline_ms = 5000
mailbox_max = 10000
message_max_bytes = "1MiB"

[appserver.capabilities."wasi:logging"]
max_level = "info"
rate_limit_per_sec = 100

[appserver.capabilities."wasi:config"]

[appserver.capabilities."wasi:keyvalue".buckets]
orders = { durability = "disk", quota = "256MiB" }
cache = { durability = "memory" }

[appserver.capabilities."wasi:filesystem".preopens]
"/data" = { volume = "orders-data", mode = "rw" }

[appserver.config.defaults]
currency = "EUR"

[appserver.upgrade]
mode = "snapshot"
snapshot_version = 3
from_versions = ["^1.1"]

[appserver.observability]
log_level = "info"
```

## `[app]`

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | yes | App name: lowercase ASCII letters, digits, `-` and `_`, starting with a letter or digit, at most 64 characters. Names the app in every command and namespaces its storage. |
| `version` | yes | Semantic version. A node keeps the current and the previous version, for rollback. |
| `description`, `authors`, `license`, `repository` | no | Shown in the bundle; no effect on running. |

## `[appserver]`

| Field | Default | Meaning |
| --- | --- | --- |
| `kind` | `"wasm-component"` | `"native-process"`: a native program run as a process (see "Native-process apps" below). |
| `native` | | For `kind = "native-process"` only: `program = "library"` (default; a rustbridge plugin) or `"executable"`. |
| `registered_names` | `[]` | Names the app may register with `ctx.register`. Each is a string (local name), or `{ name, scope = "global" }` (*not enforced*: global names are parked). Registering an unlisted name is refused. |
| `dependencies` | `[]` | `{ app, version }` pairs (`version` is a semver requirement, such as `"^2"`). The app starts after its dependencies, in list order, and stops before them. It fails to start if one is not running, and a dependency cannot be removed while needed. |

### `lifecycle`

| Field | Default | Meaning |
| --- | --- | --- |
| `restart` | `"permanent"` | `permanent` (always restart), `transient` (only after an abnormal exit) or `temporary` (never). |
| `init_timeout_ms` | `5000` | Longest `init` may run; past it, the start fails with `init_timeout`. |
| `shutdown_timeout_ms` | `5000` | Longest `terminate` may run before the instance is killed. |
| `start_args` | none | Any TOML value. It reaches `init` as JSON bytes. |

### `supervision`

The app's root supervisor, with OTP semantics.

| Field | Default | Meaning |
| --- | --- | --- |
| `strategy` | `"one_for_one"` | `one_for_one`, `one_for_all`, `rest_for_one`, or `simple_one_for_one` (a pool built from `children[0]`). |
| `intensity` | `5` | Most restarts within `period_ms`. One more, and the supervisor gives up: the app is marked `failed`, and the reason shows in `paddock status`. |
| `period_ms` | `10000` | The window for `intensity`. |
| `backoff.initial_ms` | `100` | Delay before the first restart. It doubles on each consecutive restart. |
| `backoff.max_ms` | `5000` | Upper bound on the delay. |
| `backoff.jitter` | `true` | Randomize delays, so pools don't restart in lockstep. |
| `children` | `[]` | Other deployed apps this supervisor also starts, after the app's own instances: `{ id, app, restart?, shutdown_timeout_ms?, start_args? }`. The optional fields override the child app's own. |

### `instances`

| Field | Default | Meaning |
| --- | --- | --- |
| `count` | `1` | Instances to run. |
| `min`, `max` | none | Autoscaling bounds; *not enforced*. |
| `routing` | `"round-robin"` | How calls and casts to the app pick an instance: `"round-robin"`, or `"hash:<field>"`. With hash routing, messages whose JSON or CBOR body has the same top-level `<field>` value go to the same instance. Other bodies fall back to round robin. |
| `passivate_after_ms` | none | Passivate an instance after this long with nothing to do: `save-snapshot`, then its Store is dropped and its pid, mailbox, links, monitors, names, timers, watches and group memberships kept. The next message, signal or operation wakes it with `load-snapshot` of its own version (about 50 to 100 µs). The app's `load-snapshot` must read its own snapshots: the SDK's default refuses them, and a wake that fails ends the instance. An instance stays live while a caller waits on a deferred reply or its guest holds a host resource (a bucket handle, a stream). Each health check wakes it. Lets a node hold more instances than Store slots (`max_processes` in node config). |

### `limits`

| Field | Default | Meaning |
| --- | --- | --- |
| `memory_max` | `"64MiB"` | Most linear memory per instance. Growing past it crashes the instance with `memory_limit`. At most 4 GiB, and at most 128 MiB on a node with the `dense` memory profile. |
| `callback_deadline_ms` | `5000` | Most guest CPU time per callback. Past it, the instance is killed with reason `timeout`. Time spent waiting does not count: in host calls, or for a worker after a preemption. Host work that does not wait (a memory-bucket write, for one) counts. |
| `mailbox_max` | `10000` | Most queued messages per instance. When full, casts are dropped (and counted under `DROPPED` in `paddock ps`), and calls fail at once with `overloaded`. |
| `message_max_bytes` | `"1MiB"` | Largest body accepted; larger ones fail with `too-large`. |
| `table_max` | engine default | Most table elements per instance. |
| `timers_max` | node's `[limits]` (10000) | Most pending `send-after` timers per instance. One more crashes the instance with `timer_limit`. |
| `monitors_max` | node's `[limits]` (10000) | Most monitors the instance holds on others. One more crashes it with `monitor_limit`. |
| `links_max` | node's `[limits]` (10000) | Most links the instance holds when it links with a new instance; past it, the instance crashes with `link_limit`. Links other instances make to it are charged to them. |
| `groups_max` | node's `[limits]` (1000) | Most process groups the instance is in (`paddock:groups`). One more `join` returns `limit-reached`. |
| `watches_max` | node's `[limits]` (1000) | Most keyvalue watches the instance holds (`paddock:watch`). One more `watch` returns `limit-reached`. |
| `pending_calls_max` | node's `[limits]` (10000) | Most calls answered `noreply` and not yet replied to. One more crashes the instance with `pending_call_limit`, and its waiting callers get `exited`. |
| `fuel_per_message`, `storage_quota`, `open_files`, `outbound_connections` | none | *Not enforced.* |

A `send-after` timer or a guest sleep (`wasi:clocks`) never fires early, and on an idle node fires 1 to 2 ms late: the host's timers have a 1 ms grain and round a deadline up to it, then round the wait up to it again. A busy node adds up to an epoch tick, or more when every worker is busy (`docs/BASELINE.md`, 2026-10-24: `send-after` 1.0 to 1.2 ms late at p50 and about 2 ms at p99, idle, on a 16-core machine and on a Raspberry Pi 5).

### `messaging_acl`

Which apps this app's instances may send to, and which may send to them, by app name. A call, cast, `process.exit` or link from an instance of app A to one of app B passes when A's `may_call` lists B and B's `accept_from` lists A; an empty or missing list allows any. Instances of the same app always pass, and senders that are not instances (the admin API, the HTTP ingress, health checks, extensions) are not checked; neither are monitors. The check runs on the target's node, against the sender's app as its node stamps it, so it holds across nodes. A refused call fails with `access-denied`; a refused cast or exit signal is dropped; a refused link answers at once with an exit `noproc`, as a dead pid does. An upgrade's lists apply from the swap.

| Field | Default | Meaning |
| --- | --- | --- |
| `accept_from` | `[]` (any) | Apps that may send to this app. |
| `may_call` | `[]` (any) | Apps this app may send to. |

### `health`

With this section, the host checks each instance on a timer, from the moment `init` returns: it calls the instance with a `text/plain` envelope of schema `paddock/health`. A reply whose body is `ok` is healthy; any other reply, an error, or no reply within the interval is a failure, logged with the reply's text. After `failure_threshold` failures in a row the instance is killed with `crashed: unhealthy`, and its supervisor restarts it. The call goes through the mailbox, so an instance that has stopped taking messages fails it too. `paddock-sdk` answers the call from `App::health` (healthy unless overridden); an app without the SDK answers it in `handle-call`. Without the section, nothing is checked.

| Field | Default | Meaning |
| --- | --- | --- |
| `interval_ms` | `10000` | Time between checks, and how long one may take. |
| `failure_threshold` | `3` | Failures in a row before the instance is killed. |

### `capabilities`

Each key requests one host interface, with the scope that narrows it. Importing an interface needs **both** a declaration here and a grant in the node's operator policy:

- **Imported but not declared:** the component is refused by `paddock bundle` and at deploy.
- **Declared but not granted:** the app deploys. Its imports are linked to stubs that return the interface's permission error (`access-denied` for keyvalue, for example) at call time. `paddock status` lists these under `denied`.

Always available, with no declaration: `paddock:runtime/process` and `types`, `paddock:io` (readiness of a pollable as the host event `io-ready`; see `wasi:sockets`), `wasi:clocks`, `wasi:random`, `wasi:io` and `wasi:cli` (stdout and stderr become log records).

| Key | Scope fields | Status |
| --- | --- | --- |
| `wasi:logging` | `max_level` (default `"info"`): records above it are dropped. `rate_limit_per_sec`: the excess is dropped and counted; it can only lower the node's `[limits] log_records_per_sec` (default 1000), and captured stdout and stderr lines count against the same budget. Control characters in the text are escaped, so a record stays on one line. | enforced |
| `wasi:config` | `key_prefixes`: node-level keys the app may read. Its own `config.defaults` keys are always readable. | enforced |
| `wasi:keyvalue` | `buckets.<name> = { durability, quota }`. `durability` is `"memory"` (survives instance crashes), `"disk"` (also survives node restarts) or `"replicated"` (kept whole on both nodes of an HA pair, as a `cluster` shared bucket is, so it survives losing either node; needs a node with `[failover] replication = true`, or the deploy is refused). A bucket whose durability changes in an upgrade starts empty in its new durability. `quota` is a byte cap per bucket. Buckets are namespaced per app. `shared.<name> = "ro"` or `"rw"`: a shared bucket from the node's `[buckets]`, which every app listing it and granted it sees; `"ro"` refuses writes with `access-denied`. A name is an app bucket or a shared one, not both. The same declaration covers `paddock:watch` (revisions, compare-and-swap, watches) on these buckets. | enforced |
| `paddock:groups` | `join`: groups the app's instances may join and leave. `publish`: groups the app may publish to. Listing members is allowed for a group in either list. Groups exist while they have members; nothing in node configuration binds them. Groups span the cluster: members on every node are listed and published to, except where `messaging_acl` keeps two apps apart. | enforced |
| `wasi:filesystem` | `preopens."<guest path>" = { volume, mode }`. `volume` names a host directory in the node's `[volumes]`; `mode` is `"ro"` or `"rw"`. Paddock enforces no byte quota on a volume and refuses `quota` here: give each writable volume a filesystem quota of its own (a ZFS dataset quota, an XFS project quota, or a separate mount). `/priv` is reserved: it always holds the bundle's files, read-only. | enforced |
| `wasi:http/incoming-handler` | `routes = [{ host, path_prefix, hash_header }]`: requests the node's HTTP ingress sends to the app, each as a call (`host` optional; `path_prefix` matched on segments; `hash_header` copies a header into the request's `key` for `instances.routing = "hash:key"`). `max_body` (default `"1MiB"`), `timeout_ms` for the reply (default `30000`, then 504). The app imports nothing for this; the routes go live only when the operator grants the key, and need `[http]` in node config. | enforced |
| `paddock:secrets` | `secrets = ["db-password"]`: names the app reads. Each name needs a `[secrets.<name>]` binding on the node (or the deploy is refused) and a grant in policy (`secrets = [...]`); reading an ungranted one returns `access-denied`. | enforced |
| `paddock:metrics` | `prefix` (required; letters, digits, `_`): every metric is exported as `<prefix>_<name>` with an `app` label. `max_series` (default 1000): most name and label combinations the app may hold; past it, registering returns `limit-reached`. | enforced |
| `wasi:http/outgoing-handler` | `allow`: what the app may reach, each entry a scheme, host and port: `https://api.example.com` (port 443), `http://plc.internal:8080`, a bare `api.example.com` (HTTPS), `*.internal` (any host under `internal`, HTTPS, 443), or an address, `http://10.0.0.5:8080`, `https://[fd00::1]` (reached only as written, never through a name). No path, query or user. `methods` (upper case; empty for any). `max_concurrent`: requests in flight at once across the app's instances (default: the node's `[egress] max_concurrent`, 64); past it a request fails at once with `connection-limit-reached`. Anything else is refused with `HTTP-request-denied`, and a name that resolves only to loopback or link-local addresses with `destination-IP-prohibited`, unless node config sets `allow_loopback`. Node config's `[egress] allow` is a ceiling: a deploy asking for more is refused. Cargo feature `egress` (on by default); `paddock_sdk::http_client` is the SDK's client. | enforced |
| `paddock:sql` | `datasources.<name>.max_connections` (default 4): the SQL datasources the app uses, and how many connections its instances hold on each at once, transactions included. Each name needs a `[datasources.<name>]` binding on the node (or the deploy is refused) and a grant in policy (`datasources = [...]`); using an ungranted one returns `access-denied`. A call that cannot get a connection within the datasource's `acquire_timeout_ms` returns `limit-reached`. Postgres only; see `wit/sql/sql.wit` for what values carry. | enforced |
| `wasi:sockets` | `connect = [{ cidr, ports }]`: where the app may connect TCP and send UDP (`ports` is `"1883"` or `"8000-8100"`; unset for any). `bind = [{ cidr, ports }]`: what it may bind; the unspecified address with port 0, the bind before every connect, needs no rule. `listen` (default `false`): whether it may listen on a bound TCP address, accept connections and take UDP datagrams from any peer (without it, datagrams only from `connect` addresses). `max_sockets` (default 16): sockets open at once across the app's instances, TCP and UDP, accepted ones included; past it, creating or accepting one fails with `new-socket-limit`. Loopback, link-local (`169.254.0.0/16`, cloud metadata) and unspecified destinations are reached only through a rule whose block lies inside that range (`127.0.0.1/32` reaches loopback; `0.0.0.0/0` does not). Name lookups are allowed only with `connect` rules; the addresses they return are checked like any other. Anything else fails with `access-denied` (a refused name lookup as `permanent-resolver-failure`). Node config's `[sockets]` is a ceiling: a deploy asking for more is refused. Sockets close when the instance exits or is upgraded, and an instance holding one is not passivated. `paddock_sdk::net` is the SDK's TCP client and listener; it waits for bytes with `paddock:io` instead of blocking the instance. | enforced |
| `paddock:runtime/supervisor`, `paddock:admin` | as documented in `paddock_bundle::manifest` | *not enforced* |
| `extensions."<package>"` | Free-form: the table the extension's host service reads. | enforced by the node's extension |

`extensions` declares packages served by host services built into a node from outside Paddock (extensions; see [`examples/greeter`](../examples/greeter/README.md)). The key is the WIT package, `namespace:package`, outside the reserved `wasi` and `paddock` namespaces; an import from any interface of that package needs the declaration:

```toml
[appserver.capabilities.extensions."example:greeting"]
salutation = "Howdy"
```

A node built without the extension links deny stubs for the package, as for any declared interface no host provides.

### Capabilities at a glance

What each host service needs from the app, the node and the operator. Every capability also needs a grant in policy (`grant = [...]`); the "Policy" column lists what policy names beyond that.

| Service | Manifest | Node config | Policy | `paddock-sdk` | Node build |
| --- | --- | --- | --- | --- | --- |
| Logging | `wasi:logging` | `[limits] log_records_per_sec` (optional) | | always | |
| Configuration | `wasi:config`, `config.defaults` | `[config.<app>]` (optional) | | `config` | |
| Keyvalue, the app's own buckets | `wasi:keyvalue` `buckets` | `[failover] replication = true` for `replicated` | | `keyvalue`; `watch` for revisions and watches | |
| Keyvalue, shared buckets | `wasi:keyvalue` `shared` | `[buckets.<name>]` | `buckets` | `keyvalue`, `watch` | |
| Process groups | `paddock:groups` | | | `groups` | |
| Files | `wasi:filesystem` `preopens` | `[volumes]` | | `std::fs` | |
| Secrets | `paddock:secrets` | `[secrets.<name>]` | `secrets` | `secrets` | |
| Metrics | `paddock:metrics` | `[metrics] address` to scrape (optional) | | `metrics` | |
| SQL | `paddock:sql` | `[datasources.<name>]` | `datasources` | `sql` | `sql` (default) |
| HTTP in (as calls) | `wasi:http/incoming-handler` `routes` | `[http.<name>]` | | `http` | `http` (default) |
| HTTP out | `wasi:http/outgoing-handler` `allow` | `[egress]` (optional ceiling) | | `http-client` | `egress` (default) |
| Sockets | `wasi:sockets` | `[sockets]` (optional ceiling) | | `net` | |
| TCP connections (a connection handler) | `listeners` (not a capability) | `[listeners.<name>]` | | `connection` | |
| An extension | `extensions."<package>"` | `[extensions."<package>"]` | the package in `grant` | the extension's own bindings | the extension (`modbus` for Modbus) |

Always available, with nothing to declare: `paddock:runtime/process` (calls, casts, links, monitors, names, timers), `paddock:io`, clocks, random, and stdout and stderr as log records.

### `config`

| Field | Default | Meaning |
| --- | --- | --- |
| `defaults` | `{}` | String key-value pairs the app reads through `wasi:config`. The node's `[config.<app>]` table overrides them per key. |
| `schema` | none | Path of a JSON Schema in the bundle; documentation only. |
| `secrets` | `[]` | Secret names; *not enforced*. `capabilities."paddock:secrets"` declares the secrets an app reads. |

### `upgrade`

| Field | Default | Meaning |
| --- | --- | --- |
| `mode` | `"restart"` | `"restart"` stops the old version gracefully and starts the new one from `init`. `"snapshot"` carries state through `save-snapshot` and `load-snapshot`, keeping pid, names, links and queued messages, and reverts if the new version refuses. `"blue-green"` starts the new version beside the old and moves traffic to it in steps (`paddock upgrade --steps`, `paddock traffic`), then stops the old one; for an app whose root supervisor runs only its own instances, without `registered_names`, not a singleton, with the same HTTP routes in both versions. No state moves between the versions. |
| `snapshot_version` | none | The snapshot format this release writes. Required in `snapshot` mode. |
| `from_versions` | `[]` (any) | Semver requirements for the versions this release can upgrade from. An upgrade from any other version is refused. |

An upgrade that adds capabilities needs `paddock upgrade --approve`. `--batch`, `--observe-ms` and `--max-restarts` control how a pool rolls out, and the rollout stops and reverts once restarts pass the threshold. In `blue-green` mode, `--steps 10,50,100` gives the new version's share of traffic at each step (100 by default), each watched for `--observe-ms` with the new version's restarts counted against `--max-restarts`; a failed step stops the new version. Steps that end below 100 leave both running: `paddock traffic <app> <percent>` moves the share, `--promote` stops the old version (after up to `--drain-ms` for its mailboxes to empty) and `--drop` the new one, as `paddock rollback` does. A node that restarts meanwhile starts both at the same share.

### `observability`

| Field | Default | Meaning |
| --- | --- | --- |
| `log_level` | `"info"` | The app's log level when `wasi:logging` sets no `max_level`. An operator can override either at run time with `paddock log-level <app> <level>` (and `reset`). |
| `metrics`, `trace_sampling` | `false`, `0.0` | *Not enforced*. `capabilities."paddock:metrics"` exports an app's metrics. |

### `listeners`

Only for a connection handler (a component exporting `paddock:connection/handler`): the listeners whose connections it serves, one instance per connection. An app world or an HTTP handler declares none, and a connection handler at least one.

```toml
[[appserver.listeners]]
name = "lines"            # bound to an address by the node: [listeners.lines]
idle_timeout_ms = 60000   # default 60000: no bytes in either direction this long closes the connection
max_connections = 256     # default 1024: more are refused
```

Names are unique within an app and follow the app-name rules. Upgrades keep the same listener names.

### HTTP handlers

A component exporting `wasi:http/incoming-handler` (any 0.2 version; the `wasi:http/proxy` world, as `wasmtime serve` runs) is an HTTP handler: each request on its routes gets a new instance, with streaming bodies, instead of a call to a running app. The world is read from the component; nothing in the manifest selects it.

- `wasi:http/incoming-handler` must declare at least one route; `max_body` bounds the request body as it is read (a declared length over it is 413; past it the guest's body stream fails), and `timeout_ms` runs until the response headers (504), then again as an idle timer while either body moves (the stream is cut).
- `limits.callback_deadline_ms` bounds the instance's guest execution; `memory_max` and the other limits apply as for any instance.
- `listeners`, `paddock:groups` and a `placement.mode` other than `any` are refused. `instances` is not used.
- Imports follow the rules above; `paddock:runtime/process` is available, so a handler can call apps by registered name.
- Each request holds one of the node's `max_instances` Store slots; none free answers 503 with `Retry-After`. Upgrades apply to new requests.
- Needs `[http]` in node config and a node built with the `http` and `egress` features (both default).

### Native-process apps

`kind = "native-process"` runs native code as a process the node spawns, an Erlang-style port, instead of a component. The bundle carries a release variant for the node's platform (`linux-x86_64`, `linux-aarch64`) in place of `wasm32-wasip2`; deploy writes it under `data_dir`, readable and executable by the node's user only.

- `native.program = "library"` is a rustbridge plugin (`.so`), loaded by `paddock-runner`: calls and casts go to its `handle_request` with the envelope's schema id as the type tag, in JSON (`application/json`); a reply carries the same schema id, and an error the schema id `rustbridge.error` with `{"error": "..."}`. The init arguments become the plugin's config `data`. It cannot call other apps, since rustbridge's interface has no call into the host.
- The process's environment holds only `PADDOCK_PRIV`, the directory of the bundle's `priv/` files when it has any. `paddock bundle` packs a native app's program as the variant for the machine it runs on.
- `native.program = "executable"` is a program linking `paddock-native`'s app API (feature `app`): `init`, `handle_call`, `handle_cast`, `handle_info` and `terminate`, with the process functions during a callback (send, cast, call, reply later, links, monitors, registered names, timers, exit signals). What it prints goes to the log as stderr does.
- Each instance is one process, with a pid, mailbox, links, monitors and supervision like a component's, one callback at a time. `limits.callback_deadline_ms` bounds each callback's wall time, less time spent answering the app's requests; past it the process is killed (`timeout`). An idle instance is pinged (`[native]`); no answer kills it (`timeout`). The process exiting is `crashed` with its status or signal, a panic in a callback `crashed` with the panic's message, and going over `memory_max` with a cgroup `crashed: memory_limit`.
- `memory_max` is the process's cgroup `memory.max` with node config `[native] cgroup`, else its `RLIMIT_DATA` (a failed allocation then aborts it).
- Only `wasi:logging` may be declared: any other capability, an extension, `listeners`, `instances.passivate_after_ms` and `upgrade.mode = "snapshot"` are refused. Upgrades are `restart` or `blue-green`.
- The runner restricts the program before it runs (SECURITY.md, "Native-process apps"): it reads only the system libraries, its own directory and the app's `priv/` files, executes nothing else, opens no sockets, and signals only itself. Linux only.

### Documentation-only and later sections

| Section | Status |
| --- | --- |
| `api.messages` | The app's message catalog (`name`, `pattern` = `call` or `cast`, `content_type`, `request_schema`, `response_schema`, `description`). Documentation; messages are not checked against it. |
| `placement` | `required_labels`, `preferred_labels`, `spread` (a node label to spread instances across), and `mode`: `any` (default; as many instances as `instances` asks for, on any eligible nodes), `singleton` (one instance across the cluster, moved when its node fails; `instances` may not ask for more than one), `all-nodes` (one on every eligible node), or `hot-standby` (one on every eligible node, one of which leads and each told its role through a `host-event`). On a node with `[failover]`, `singleton` runs on the pair's primary only, and its instances run guest code only while the node's lease is current: out of lease a message waits, and a callback running when the lease ends is ended with `crashed: lease_ended`; `hot-standby` runs on both nodes, each told `host-event("failover-role", "primary" \| "standby" \| "down")`. *Not enforced*: `all-nodes`, labels and `spread` (placement across many nodes is parked). |

## Operator side

Two files on the node decide what an app actually gets.

The node configuration (`paddock node --config node.toml`):

```toml
node = 0                               # node id, carried in every pid
data_dir = "/var/lib/paddock"          # deploy store, keyvalue data, audit log; one node at a time
admin_socket = "/run/paddock.sock"     # default <data_dir>/admin.sock (Unix paths: about 107 bytes at most)
trusted_keys = ["RWQ..."]              # minisign public keys whose bundles are accepted
policy = "/etc/paddock/policy.toml"    # without one, only the always-available interfaces are granted
max_instances = 1000                   # instances holding a Store at once (each reserves address space)
max_processes = 20000                  # all instances, passivated ones too; default max_instances
memory_profile = "standard"            # or "dense": 128 MiB cap per instance, many more instances
keep_resident = "16MiB"                # per free Store slot: pages kept, zeroed, for the next instance; default 16MiB, 0 with "dense"
epoch_tick_ms = 2                      # granularity of callback deadlines and preemption
log_format = "text"                    # the node's own log on stderr; or "json", one object per line

[volumes]
orders-data = "/srv/paddock/orders"    # backs manifest preopens with volume = "orders-data"; give it a filesystem quota

[listeners.lines]
address = "0.0.0.0:7070"               # binds a manifest listener named "lines"
acceptors = 4                          # default 4: tasks accepting connections at once

[secrets.db-password]
file = "/run/secrets/db-password"      # or env = "DB_PASSWORD"; read on every access

[datasources.main]                     # a Postgres database for paddock:sql; pooled, opened on first use
host = "db.internal"                   # or a Unix socket directory, such as "/run/postgresql"
port = 5432
dbname = "orders"
user = "orders"
password_secret = "db-password"        # a name in [secrets]; omit for trust or peer authentication  (secret-scan: allow)
tls = "require"                        # or "disable" (the default); require needs ca_file
ca_file = "/etc/paddock/db-ca.pem"     # the CAs the server must chain to (no system roots)
pool_size = 10                         # most connections, every app together
connect_timeout_ms = 5000
acquire_timeout_ms = 5000              # a call waiting longer for a connection gets limit-reached
statement_timeout_ms = 30000           # the server cancels longer statements (timeout)
max_result_bytes = "16MiB"             # larger results are refused (too-large)

[egress]                               # HTTP requests from apps (wasi:http/outgoing-handler); every key optional
allow = ["*.internal", "https://api.example.com"]  # the most any app may reach (manifest syntax); unset: no ceiling
system_roots = true                    # trust the OS root certificates (SSL_CERT_FILE and SSL_CERT_DIR are read)
ca_file = "/etc/paddock/egress-ca.pem" # more CAs, PEM (a private CA)
allow_loopback = false                 # names resolving to loopback or link-local (169.254.x.x, fe80::) are refused
connect_timeout_ms = 10000
first_byte_timeout_ms = 30000          # to the response headers; an app's own timeout may only be shorter
between_bytes_timeout_ms = 30000       # between chunks of a response body
pool_idle_timeout_ms = 90000           # idle pooled connections are closed after this
pool_max_idle_per_host = 32
max_concurrent = 64                    # requests in flight per app, unless its manifest says

[sockets]                              # TCP and UDP from apps (wasi:sockets); unset: no ceiling
connect = [{ cidr = "10.20.0.0/16", ports = "1883" }]  # the most any app may connect to (manifest syntax)
bind = [{ cidr = "0.0.0.0/0", ports = "9000-9099" }]   # the most any app may bind
listen = false                         # whether any app may listen

[native]                               # native-process apps
runner = "/usr/local/bin/paddock-runner"  # unset: paddock-runner beside the paddock executable
cgroup = "/sys/fs/cgroup/system.slice/paddock.service"  # a delegated cgroup v2 directory (Delegate=yes), the node its only process; unset: memory_max as RLIMIT_DATA
pids_max = 64                          # most processes and threads per native instance, with cgroup
ping_interval_ms = 10000               # an idle native instance is pinged this often
ping_timeout_ms = 5000                 # and killed (timeout) without an answer within this
landlock_optional = false              # run native apps on a kernel without Landlock

[metrics]
address = "127.0.0.1:9100"             # GET /metrics for Prometheus; unauthenticated, keep it off public networks

[admin]
address = "10.0.0.1:7443"              # the admin API over TCP with TLS 1.3 (cargo feature `tls`, on by default), for operators on other machines
operators = "/etc/paddock/operators.toml"  # [operators] alice = ["ed25519:..."]; reread on SIGHUP and `paddock reload-tls`
key = "/etc/paddock/node.key"          # default: [cluster] key; operators pin its public key (`paddock cluster pubkey`)

[http.main]
address = "0.0.0.0:8080"               # HTTP ingress (cargo feature `http`, on by default): serves every app's routes
acceptors = 4                          # default 4
max_connections = 1024                 # default 1024; more are closed on accept
header_timeout_ms = 10000              # default 10000: time to send headers (also between kept-alive requests)
body_timeout_ms = 30000                # default 30000: time to send a body, then 408
max_response = "16MiB"                 # default "16MiB": larger app replies become 502
tls = [                                # HTTPS (cargo feature `tls`, on by default): PEM chain and key; SNI picks by hosts; the first is the default
  { cert = "/etc/paddock/shop.pem", key = "/etc/paddock/shop.key", hosts = ["shop.example", "*.shop.example"] },
]
tls_reload_s = 60                      # default 60: reload when the files change (0: only SIGHUP or `paddock reload-tls`)
partner_url = "https://node2.example:8443"  # with [failover]: the partner's listener, as clients reach it
standby = "redirect"                   # with [failover], a request for a singleton running on the partner:
                                       # "redirect" (307 to partner_url), "forward" (over the cluster to the
                                       # primary's app) or "refuse" (503); default redirect with partner_url, else refuse

[config.orders]
currency = "USD"                       # overrides the app's config.defaults

[cluster]
role = "member"                        # or "witness": runs no apps, refuses deploys
heartbeat_interval_ms = 1000           # how often each peer is told this node is alive
node_timeout_ms = 5000                 # silence that ends a peer's session; more than the interval
address = "10.0.0.1:7400"              # UDP: QUIC between nodes; unset runs no cluster transport
advertise = "192.0.2.7:7400"           # optional: where peers reach this node (NAT, a proxy); default address
seeds = ["10.0.0.2:7400"]              # nodes to join through, redialed whenever lost
key = "/etc/paddock/node.key"          # this node's private key: paddock cluster keygen; mode 0600
peers = "/etc/paddock/peers.toml"      # every node's public key: [peers] 2 = ["ed25519:..."]
identity_reload_s = 60                 # reload key and peers on change; also SIGHUP, paddock reload-tls
incoming_calls_max = 10000             # calls from one peer handled at once; beyond: overloaded; 0: no limit
send_buffer_bytes = 67108864           # messages queued for one peer; beyond: cast dropped, call overloaded (notices still queued)

[failover]                             # one node of an HA pair (needs cluster.address)
partner = 2                            # the other node of the pair
arbiter = "witness"                    # or "paths" (promote when every path is silent), "operator"
witness = 3                            # the witness's node id, with arbiter "witness"
lease_ms = 3000                        # how long a renewal keeps the primary's lease
margin_ms = 500                        # singletons stop this long before the lease ends; under half of lease_ms
serial = { device = "/dev/ttyUSB0", baud = 115200 }  # optional: a second heartbeat path over a null-modem
                                       # cable to the partner (Unix; baud default 115200, the same on both nodes)
replication = true                     # run replication for apps' "replicated" buckets (needs arbiter "witness";
                                       # implied by a cluster bucket; set alike on both nodes)
replication_log_max = "64MiB"          # with replication: the replicated log keeps at most this
replication_log_age_s = 3600           # and no entry older; a partner back within both catches up
                                       # from the log, past them from a snapshot

[witness]                              # on the witness instead (cluster role "witness"): the pair it arbitrates
pair = [1, 2]                          # the pair's node ids
first = 1                              # the node primary at epoch 1
lease_ms = 3000                        # as the pair's lease_ms and margin_ms (defaults 3000 and 500)
margin_ms = 500

[limits]                               # host objects per instance; the value for apps that
timers_max = 10000                     # set none, and the most a manifest may ask for (a
monitors_max = 10000                   # larger value refuses the deploy)
links_max = 10000
pending_calls_max = 10000
groups_max = 1000
watches_max = 1000
log_records_per_sec = 1000             # per app, logging records and stdout/stderr lines together; 0: none

[buckets.points]                       # a shared keyvalue bucket, opened by apps that list it
durability = "disk"                    # under wasi:keyvalue's shared and are granted it; or "memory"
quota = "256MiB"                       # across every app writing it
scope = "node"                         # or "cluster": replicated on the HA pair (below)
```

A deploy is refused if the manifest lists a shared bucket the node does not configure.

Bucket names here follow the manifest's rule: lowercase letters, digits, `-` and `_`, starting with a letter or digit.

A `cluster` bucket is kept whole on both nodes of an HA pair: `durability = "disk"`, on a node with `[failover]` and `arbiter = "witness"`, configured alike on both nodes. A write from either node commits on the primary once the standby has it on disk (or, with the standby gone, once the witness has recorded it out of date) and returns then; reads and watches use the node's own copy, which may trail the primary slightly, and `paddock:watch`'s `get-linearizable` reads through the primary. Without a primary holding its lease, writes fail (`unavailable` from `paddock:watch`). A bucket that changes scope starts empty. `paddock failover` shows where the log stands.

Extensions read their own node configuration from `[extensions."<package>"]`; a table for an extension the node was not built with refuses the start. `paddock node` built with the `modbus` cargo feature (off by default: `cargo build -p paddock --features modbus`) includes the Modbus TCP extension (`paddock-modbus:modbus`, `crates/paddock-modbus`):

```toml
[extensions."paddock-modbus:modbus".devices.pump-1]
address = "10.0.0.5:502"
unit = 1                               # default 1
poll_ms = 1000                         # default 1000
timeout_ms = 1000                      # connect and per request; default 1000
full_every_ms = 60000                  # optional integrity scan: every point, changed or not
deliver_to = "points"                  # the app each scan's changes are cast to
primary_only = false                   # poll only while the node runs singletons (an HA pair's primary)
holding = [{ start = 0, count = 10 }]  # also input, coils, discrete
```

With `primary_only = true`, a device on an HA pair is polled by the primary alone, while its lease holds, and a `cluster` bucket carries the table to the standby. The standby closes its connection to the device; apps' reads and writes through the `client` interface still reach it from either node. The first poll after a node becomes primary sends every point. A node without `[failover]` always polls.

```toml
[extensions."paddock-modbus:modbus".server]
listen = "0.0.0.0:5020"
deliver_to = "points"                  # masters' writes
holding = 100                          # table sizes from address 0; also input, coils, discrete
allow = ["10.20.0.0/24"]               # masters allowed (addresses or networks); default loopback only
max_connections = 32                   # default 32: masters connected at once; more are closed on accept
```

Modbus has no authentication: any master that can connect reads and writes the served tables. A master whose address is not in `allow` is refused; list the SCADA network, and bind `listen` to the interface that reaches it.

Apps declare which devices they may read or write, and whether they set the served tables, and the operator grants the package:

```toml
[appserver.capabilities.extensions."paddock-modbus:modbus"]
read = ["pump-1"]
write = ["pump-1"]                     # writing implies reading
serve = true
```

The operator policy (`policy`). Grants add up: the defaults, plus the app's own entry, plus the entry of each signer whose key verified the bundle.

```toml
[defaults]
grant = ["wasi:logging", "wasi:config"]

[apps.orders]
grant = ["wasi:keyvalue", "wasi:filesystem"]
buckets = ["points"]                   # shared buckets; needs wasi:keyvalue granted too
secrets = ["db-password"]              # secrets; needs paddock:secrets granted too
datasources = ["main"]                 # SQL datasources; needs paddock:sql granted too

[signers.payments-team]
key = "RWQ..."
grant = ["wasi:keyvalue"]
```

A `grant` list takes extension packages (`"example:greeting"`) as well as capability keys. Policy grants or withholds whole capabilities, and each shared bucket by name (`buckets`, in any entry). A deploy reports shared buckets as `bucket:<name>` under `granted` or `denied`; opening a denied one returns `access-denied`. Other scopes an app requests (its own buckets, preopens) are used as written; narrowing them by policy is not supported.
