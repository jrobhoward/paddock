# Getting started

This walks through running a Paddock node and deploying the counter example: build, sign, deploy, call, upgrade with state kept, and roll back. It takes about ten minutes, most of it compiling.

Phase 1 runs on Linux and macOS. The admin API is a Unix socket, and Windows is not supported yet.

## 1. Build

Rust 1.96 or newer, plus the WebAssembly target that apps compile to:

```sh
rustup target add wasm32-wasip2
cargo build --release -p paddock
export PATH="$PWD/target/release:$PATH"     # paddock
```

## 2. Make a signing key

Nodes run only bundles signed by a key they trust. Create a minisign key pair:

```sh
mkdir -p ~/paddock-demo && cd ~/paddock-demo
paddock keygen signer --no-password      # writes signer.key and signer.pub, prints the public key
```

Leave out `--no-password` to encrypt the secret key. `paddock bundle` then reads the password from `PADDOCK_KEY_PASSWORD`, or prompts for it.

## 3. Start a node

`node.toml`:

```toml
data_dir = "/home/you/paddock-demo/data"
trusted_keys = ["RWQ..."]          # the public key keygen printed (also in signer.pub)
```

```sh
paddock node --config node.toml
```

The node keeps running in the foreground; use a second terminal for the rest. Its admin socket is `<data_dir>/admin.sock`, and the CLI finds it through `PADDOCK_SOCKET` or `--socket`:

```sh
export PADDOCK_SOCKET=~/paddock-demo/data/admin.sock
paddock list                       # no apps deployed
```

Only one node can use a data directory at a time; a second one is refused.

## 4. Build, bundle and deploy the counter

From the Paddock checkout:

```sh
cargo build -p example-counter --target wasm32-wasip2 --release
paddock bundle examples/counter/paddock.toml \
  target/wasm32-wasip2/release/example_counter.wasm \
  --key ~/paddock-demo/signer.key --out ~/paddock-demo/counter-1.0.0.rbp
paddock deploy ~/paddock-demo/counter-1.0.0.rbp
```

`paddock bundle` checks the component against its manifest, refusing any interface the component imports but the manifest does not declare. It then signs the bundle. `paddock deploy` uploads it, and the node verifies the signature against `trusted_keys` and starts the app.

```sh
paddock call counter inc           # 1
paddock call counter inc           # 2
paddock call counter get           # 1.0.0 count=2
paddock ps                         # PID, APP, VERSION, STATE, QUEUED, DROPPED
paddock state 0.1.1                # the instance's state: (paddock-sdk, ..., format 1) {"count": 2}
```

Use the pid that `paddock ps` shows: `node.id.incarnation`.

## 5. Watch it recover

```sh
paddock kill 0.1.1                 # its supervisor restarts it, after a 100 ms backoff
paddock ps                         # a new incarnation: 0.1.2
paddock status counter             # restarts: 1
paddock call counter get           # 1.0.0 count=0: the restarted instance began again
```

A killed or crashed instance restarts from `init`. State that must survive goes in a keyvalue bucket (see [WRITING_APPS.md](WRITING_APPS.md)).

## 6. Upgrade with state kept, then roll back

Version 2 also keeps the last three values, and reads version 1 snapshots:

```sh
paddock call counter inc && paddock call counter inc
cargo build -p example-counter --target wasm32-wasip2 --release --features v2
paddock bundle examples/counter/paddock-v2.toml \
  target/wasm32-wasip2/release/example_counter.wasm \
  --key ~/paddock-demo/signer.key --out ~/paddock-demo/counter-2.0.0.rbp
paddock upgrade counter ~/paddock-demo/counter-2.0.0.rbp
paddock call counter get           # 2.0.0 count=2 history=
paddock ps                         # same pid as before the upgrade
```

The node paused the instance's mailbox, called `save-snapshot` on version 1, loaded version 2 with `load-snapshot`, and resumed. Pid, registered name, links and queued messages all carried over. If version 2 had refused the snapshot, the node would have kept version 1 running (try `paddock-v3.toml` with `--features v3`).

```sh
paddock rollback counter
# Error: rollout stopped and reverted: instance-1: load-snapshot failed:
#   cannot read snapshot format 2 (this version writes format 1); --restart
#   (API: restart=true) moves the instances by restart instead, discarding
#   their in-memory state (refused)
paddock call counter get           # still 2.0.0: the refused rollback changed nothing
paddock rollback counter --restart
paddock call counter get           # 1.0.0 count=0: back on v1, state discarded
```

A rollback is an upgrade to the previous version, in that version's own upgrade mode. In `snapshot` mode it therefore needs the older release to read the newer snapshot format, the downgrade half of OTP's `code_change`. The counter's v1 cannot read format 2, so the node refuses the rollback and keeps v2 running. `--restart` rolls back anyway: each instance stops gracefully and v1 starts from `init`, so in-memory state is lost, and the audit log records the rollback with `"restart_requested": true`. An app whose upgrade mode is `restart` always rolls back this way. `paddock upgrade --restart` makes the same choice for an upgrade.

## 7. Look around

```sh
paddock logs counter -n 20         # the app's log records
paddock log-level counter debug    # raise its log level now; `reset` to undo
paddock cast counter inc           # fire and forget
paddock --json status counter      # every command takes --json
cat ~/paddock-demo/data/audit.log  # deploys, upgrades, kills, refusals
paddock stop counter && paddock start counter
paddock remove counter
```

The API is JSON over HTTP on the socket, so `curl --unix-socket ~/paddock-demo/data/admin.sock http://paddock/v1/apps` works too. The `paddock::api` module documents every route.

## Next

- [WRITING_APPS.md](WRITING_APPS.md): write your own app with `paddock-sdk`.
- [MANIFEST.md](MANIFEST.md): every manifest field, capabilities and operator policy.
- [`examples/`](../examples/): `counter`, `ping-pong` (calls, links and monitors between apps) and `chaos` (fails on purpose).
