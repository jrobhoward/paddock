# Manifest reference

An app's manifest is the `paddock.toml` next to its source. `paddock bundle` turns it into the signed bundle's manifest:
- `[app]` becomes the rustbridge `plugin` identity.
- `[appserver]` is carried as the bundle's `appserver` extension.

Unknown fields are errors, so a typo is refused, not ignored. `paddock bundle` and `paddock deploy` report every broken rule at once, with its path (`appserver.limits.memory_max`).

Fields marked *not enforced* parse and validate, but Phase 1 does not act on them. `paddock status` lists them under `warnings`, so a manifest written for a later phase still deploys.

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
| `kind` | `"wasm-component"` | `"native-process"` is *not enforced* (later phase). |
| `registered_names` | `[]` | Names the app may register with `ctx.register`. Each is a string (local name), or `{ name, scope = "global" }` (*not enforced*, Phase 2). Registering an unlisted name is refused. |
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

### `limits`

| Field | Default | Meaning |
| --- | --- | --- |
| `memory_max` | `"64MiB"` | Most linear memory per instance. Growing past it crashes the instance with `memory_limit`. At most 4 GiB, and at most 128 MiB on a node with the `dense` memory profile. |
| `callback_deadline_ms` | `5000` | Most guest CPU time per callback. Past it, the instance is killed with reason `timeout`. Time spent waiting in host calls does not count. |
| `mailbox_max` | `10000` | Most queued messages per instance. When full, casts are dropped (and counted under `DROPPED` in `paddock ps`), and calls fail at once with `overloaded`. |
| `message_max_bytes` | `"1MiB"` | Largest body accepted; larger ones fail with `too-large`. |
| `table_max` | engine default | Most table elements per instance. |
| `fuel_per_message`, `storage_quota`, `open_files`, `outbound_connections` | none | *Not enforced.* |

### `capabilities`

Each key requests one host interface, with the scope that narrows it. Importing an interface needs **both** a declaration here and a grant in the node's operator policy:

- **Imported but not declared:** the component is refused by `paddock bundle` and at deploy.
- **Declared but not granted:** the app deploys. Its imports are linked to stubs that return the interface's permission error (`access-denied` for keyvalue, for example) at call time. `paddock status` lists these under `denied`.

Always available, with no declaration: `paddock:runtime/process` and `types`, `wasi:clocks`, `wasi:random`, `wasi:io` and `wasi:cli` (stdout and stderr become log records).

| Key | Scope fields | Phase 1 |
| --- | --- | --- |
| `wasi:logging` | `max_level` (default `"info"`): records above it are dropped. `rate_limit_per_sec`: the excess is dropped and counted. | enforced |
| `wasi:config` | `key_prefixes`: node-level keys the app may read. Its own `config.defaults` keys are always readable. | enforced |
| `wasi:keyvalue` | `buckets.<name> = { durability, quota }`. `durability` is `"memory"` (survives instance crashes), `"disk"` (also survives node restarts) or `"replicated"` (*not enforced*, Phase 3). `quota` is a byte cap per bucket. Buckets are namespaced per app. | enforced |
| `wasi:filesystem` | `preopens."<guest path>" = { volume, mode, quota }`. `volume` names a host directory in the node's `[volumes]`; `mode` is `"ro"` or `"rw"`; `quota` is *not enforced*. `/priv` is reserved: it always holds the bundle's files, read-only. | enforced |
| `paddock:secrets`, `wasi:http/outgoing-handler`, `wasi:http/incoming-handler`, `paddock:metrics` | as documented in `paddock-manifest` | *not enforced* (Phase 2) |
| `wasi:sockets`, `paddock:sql` | as documented in `paddock-manifest` | *not enforced* (Phase 3) |
| `paddock:runtime/supervisor`, `paddock:admin` | as documented in `paddock-manifest` | *not enforced* (later) |

### `config`

| Field | Default | Meaning |
| --- | --- | --- |
| `defaults` | `{}` | String key-value pairs the app reads through `wasi:config`. The node's `[config.<app>]` table overrides them per key. |
| `schema` | none | Path of a JSON Schema in the bundle; documentation only. |
| `secrets` | `[]` | Secret names; *not enforced* (Phase 2). |

### `upgrade`

| Field | Default | Meaning |
| --- | --- | --- |
| `mode` | `"restart"` | `"restart"` stops the old version gracefully and starts the new one from `init`. `"snapshot"` carries state through `save-snapshot` and `load-snapshot`, keeping pid, names, links and queued messages, and reverts if the new version refuses. `"blue-green"` is *not enforced*. |
| `snapshot_version` | none | The snapshot format this release writes. Required in `snapshot` mode. |
| `from_versions` | `[]` (any) | Semver requirements for the versions this release can upgrade from. An upgrade from any other version is refused. |

An upgrade that adds capabilities needs `paddock upgrade --approve`. `--batch`, `--observe-ms` and `--max-restarts` control how a pool rolls out, and the rollout stops and reverts once restarts pass the threshold.

### `observability`

| Field | Default | Meaning |
| --- | --- | --- |
| `log_level` | `"info"` | The app's log level when `wasi:logging` sets no `max_level`. An operator can override either at run time with `paddock log-level <app> <level>` (and `reset`). |
| `metrics`, `trace_sampling` | `false`, `0.0` | *Not enforced* (Phase 2). |

### Documentation-only and later sections

| Section | Status |
| --- | --- |
| `api.messages` | The app's message catalog (`name`, `pattern` = `call` or `cast`, `content_type`, `request_schema`, `response_schema`, `description`). Documentation; messages are not checked against it. |
| `messaging_acl` | `accept_from`, `may_call`; *not enforced* (Phase 2). |
| `health` | `interval_ms`, `failure_threshold`; *not enforced* (Phase 2). |
| `placement` | `required_labels`, `preferred_labels`, `singleton`, `spread`; *not enforced* (Phase 3). |

## Operator side

Two files on the node decide what an app actually gets.

The node configuration (`paddock-node --config node.toml`):

```toml
node = 0                               # node id, carried in every pid
data_dir = "/var/lib/paddock"          # deploy store, keyvalue data, audit log; one node at a time
admin_socket = "/run/paddock.sock"     # default <data_dir>/admin.sock (Unix paths: about 107 bytes at most)
trusted_keys = ["RWQ..."]              # minisign public keys whose bundles are accepted
policy = "/etc/paddock/policy.toml"    # without one, only the always-available interfaces are granted
max_instances = 1000
memory_profile = "standard"            # or "dense": 128 MiB cap per instance, many more instances
epoch_tick_ms = 10                     # granularity of callback deadlines

[volumes]
orders-data = "/srv/paddock/orders"    # backs manifest preopens with volume = "orders-data"

[config.orders]
currency = "USD"                       # overrides the app's config.defaults
```

The operator policy (`policy`). Grants add up: the defaults, plus the app's own entry, plus the entry of each signer whose key verified the bundle.

```toml
[defaults]
grant = ["wasi:logging", "wasi:config"]

[apps.orders]
grant = ["wasi:keyvalue", "wasi:filesystem"]

[signers.payments-team]
key = "RWQ..."
grant = ["wasi:keyvalue"]
```

Policy grants or withholds whole capabilities. The scope an app requests (buckets, preopens) is used as written; narrowing scopes by policy is not supported in Phase 1.
