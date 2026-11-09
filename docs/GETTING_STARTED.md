# Getting started

This walks through running a Bulkhead node and deploying the counter example: build, sign, deploy, call, upgrade with state kept, and roll back. It takes about ten minutes, most of it compiling.

A node runs apps on Linux: each is a process started through `bulkhead-runner`.

## 1. Build

Rust 1.99 or newer:

```sh
cargo build --release -p bulkhead -p bulkhead-native   # bulkhead, and bulkhead-runner beside it
export PATH="$PWD/target/release:$PATH"
```

## 2. Make a signing key

Nodes run only bundles signed by a key they trust. Create a minisign key pair:

```sh
mkdir -p ~/bulkhead-demo && cd ~/bulkhead-demo
bulkhead keygen signer --no-password      # writes signer.key and signer.pub, prints the public key
```

Leave out `--no-password` to encrypt the secret key. `bulkhead bundle` then reads the password from `BULKHEAD_KEY_PASSWORD`, or prompts for it.

## 3. Start a node

`node.toml`:

```toml
data_dir = "/home/you/bulkhead-demo/data"
trusted_keys = ["RWQ..."]          # the public key keygen printed (also in signer.pub)
```

```sh
bulkhead node --config node.toml
```

The node keeps running in the foreground; use a second terminal for the rest. Its admin socket is `<data_dir>/admin.sock`, and the CLI finds it through `BULKHEAD_SOCKET` or `--socket`:

```sh
export BULKHEAD_SOCKET=~/bulkhead-demo/data/admin.sock
bulkhead list                       # no apps deployed
```

Only one node can use a data directory at a time; a second one is refused.

## 4. Build, bundle and deploy the counter

From the Bulkhead checkout:

```sh
cargo build -p example-counter --release
bulkhead bundle examples/counter/bulkhead.toml \
  target/release/example-counter \
  --key ~/bulkhead-demo/signer.key --out ~/bulkhead-demo/counter-1.0.0.rbp
bulkhead deploy ~/bulkhead-demo/counter-1.0.0.rbp
```

`bulkhead bundle` checks the manifest and signs the bundle. `bulkhead deploy` uploads it, and the node verifies the signature against `trusted_keys` and starts the app.

```sh
bulkhead call counter inc           # 1
bulkhead call counter inc           # 2
bulkhead call counter get           # 1.0.0 count=2
bulkhead ps                         # PID, APP, VERSION, STATE, QUEUED, DROPPED, MEMORY, RESTARTS, LAST EXIT
bulkhead state 0.1.1@1              # the instance's state: (bulkhead-sdk, ..., format 1) {"count": 2}
```

Use the pid that `bulkhead ps` shows: `node.serial.incarnation@creation`, where the creation counts the node's starts.

## 5. Watch it recover

```sh
bulkhead kill 0.1.1@1               # its supervisor restarts it, after a 100 ms backoff
bulkhead ps                         # a new incarnation: 0.1.2
bulkhead status counter             # restarts: 1
bulkhead call counter get           # 1.0.0 count=0: the restarted instance began again
```

A killed or crashed instance restarts from `init`. State that must survive goes in a keyvalue bucket (see [WRITING_APPS.md](WRITING_APPS.md)).

## 6. Upgrade with state kept, then roll back

Version 2 also keeps the last three values, and reads version 1 snapshots:

```sh
bulkhead call counter inc && bulkhead call counter inc
cargo build -p example-counter --release --features v2
bulkhead bundle examples/counter/bulkhead-v2.toml \
  target/release/example-counter \
  --key ~/bulkhead-demo/signer.key --out ~/bulkhead-demo/counter-2.0.0.rbp
bulkhead upgrade counter ~/bulkhead-demo/counter-2.0.0.rbp
bulkhead call counter get           # 2.0.0 count=2 history=
bulkhead ps                         # same pid as before the upgrade
```

The node paused the instance's mailbox, called `save-snapshot` on version 1, loaded version 2 with `load-snapshot`, and resumed. Pid, registered name, links and queued messages all carried over. If version 2 had refused the snapshot, the node would have kept version 1 running (try `bulkhead-v3.toml` with `--features v3`).

```sh
bulkhead rollback counter
# Error: rollout stopped and reverted: instance-1: load-snapshot failed:
#   cannot read snapshot format 2 (this version writes format 1); --restart
#   (API: restart=true) moves the instances by restart instead, discarding
#   their in-memory state (refused)
bulkhead call counter get           # still 2.0.0: the refused rollback changed nothing
bulkhead rollback counter --restart
bulkhead call counter get           # 1.0.0 count=0: back on v1, state discarded
```

A rollback is an upgrade to the previous version, in that version's own upgrade mode. In `snapshot` mode it therefore needs the older release to read the newer snapshot format, the downgrade half of OTP's `code_change`. The counter's v1 cannot read format 2, so the node refuses the rollback and keeps v2 running. `--restart` rolls back anyway: each instance stops gracefully and v1 starts from `init`, so in-memory state is lost, and the audit log records the rollback with `"restart_requested": true`. An app whose upgrade mode is `restart` always rolls back this way. `bulkhead upgrade --restart` makes the same choice for an upgrade.

## 7. Look around

```sh
bulkhead logs counter -n 20         # the app's log records; -f to follow
bulkhead log-level counter debug    # raise its log level now; `reset` to undo
bulkhead events                     # deploys, upgrades, kills, exits and restarts as they happen
bulkhead cast counter inc           # fire and forget
bulkhead --json status counter      # every command takes --json
cat ~/bulkhead-demo/data/audit.log  # deploys, upgrades, kills, refusals
bulkhead stop counter && bulkhead start counter
bulkhead remove counter
```

`bulkhead shell` runs these commands at a prompt, with history and completion, and can `connect` to other nodes listed in a contacts file.

The API is JSON over HTTP on the socket, so `curl --unix-socket ~/bulkhead-demo/data/admin.sock http://bulkhead/v1/apps` works too. The `bulkhead::api` module documents every route.

## Next

- [The tutorial](../examples/tutorial/README.md): the same node, then a pair and a witness with failover, a hot standby and a database, as scripts.
- [WRITING_APPS.md](WRITING_APPS.md): write your own app with `bulkhead-sdk`.
- [MANIFEST.md](MANIFEST.md): every manifest field, capabilities, node configuration and operator policy.
- [OPERATIONS.md](OPERATIONS.md): running nodes and pairs.
- [`examples/`](../examples/): `ping-pong` (calls, links and monitors between apps), `chaos` (fails on purpose), and the apps the README lists.
