#!/usr/bin/env bash
# One node, the Rust `ping` app, and one `pong` app upgraded in place from
# Rust to Go to Python to JavaScript. After each upgrade `ping` finds the new
# `pong` by its registered name and calls it with the same CBOR message; its
# monitor sees each old instance go. See README.md.
#
#   examples/ping-pong/languages/demo.sh
#
# Languages whose toolchain build.sh does not find are left out. DEMO_DIR
# (default a new temporary directory) holds the node's config, data and log.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
OUT="$ROOT/target/ping-pong-languages"
PADDOCK="$ROOT/target/release/paddock"
RUST_WASM="$ROOT/target/wasm32-wasip2/release/example_ping_pong.wasm"
DEMO_DIR=${DEMO_DIR:-$(mktemp -d -t paddock-ping-pong-XXXXXX)}
# Unix socket paths are limited to about 107 bytes; keep this short.
export PADDOCK_SOCKET=/tmp/paddock-pp-$$.sock

echo "building (release; the first build takes a few minutes)..."
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p paddock
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
  --target wasm32-wasip2 -p example-ping-pong
"$HERE/build.sh"

cd "$DEMO_DIR"
[ -f signer.key ] || "$PADDOCK" keygen signer --no-password > /dev/null
cat > node.toml <<TOML
node = 1
data_dir = "$DEMO_DIR/data"
admin_socket = "$PADDOCK_SOCKET"
trusted_keys = ["$(tail -n 1 signer.pub)"]
# componentize-py's components have 14 core module instances; the default
# limit is 8.
max_core_instances_per_component = 16
TOML

"$PADDOCK" node --config node.toml >> node.log 2>&1 &
node_pid=$!
cleanup() {
  kill "$node_pid" 2> /dev/null || true
  wait 2> /dev/null || true
  rm -f "$PADDOCK_SOCKET"
  echo
  echo "node stopped; config, data and log are in $DEMO_DIR"
}
trap cleanup EXIT
for _ in $(seq 200); do [ -S "$PADDOCK_SOCKET" ] && break; sleep 0.05; done
[ -S "$PADDOCK_SOCKET" ] || { echo "the node did not start; see $DEMO_DIR/node.log" >&2; exit 1; }

# Bundle MANIFEST with COMPONENT as VERSION, into OUT.
bundle() {
  local manifest=$1 component=$2 version=$3 out=$4
  sed "s/^version = \".*\"/version = \"$version\"/" "$manifest" > "${out%.rbp}.toml"
  "$PADDOCK" bundle "${out%.rbp}.toml" "$component" --key signer.key --out "$out" > /dev/null
}

# Call ping until it answers "pong 42" (after an upgrade, the new pong may
# take a moment to register its name), then with an n that takes 4 bytes,
# whose reply takes 8. Each shows the reply and the version that gave it.
ping_pong() {
  local reply="" n
  for _ in $(seq 100); do
    reply=$("$PADDOCK" call ping ping:41 2> /dev/null || true)
    [ "$reply" = "pong 42" ] && break
    sleep 0.05
  done
  echo "  running: $("$PADDOCK" status pong | sed -n 's/^app: *//p')"
  for n in 41 4294967295; do
    [ "$n" = 41 ] || reply=$("$PADDOCK" call ping "ping:$n")
    echo "  \$ paddock call ping ping:$n"
    echo "  $reply"
    [ "$reply" = "pong $((n + 1))" ] || { echo "expected pong $((n + 1))" >&2; exit 1; }
  done
}

echo
echo "== pong in Rust (examples/ping-pong), with ping"
bundle "$ROOT/examples/ping-pong/pong.toml" "$RUST_WASM" 1.0.0 pong-rust.rbp
bundle "$ROOT/examples/ping-pong/ping.toml" "$RUST_WASM" 1.0.0 ping.rbp
"$PADDOCK" deploy pong-rust.rbp > /dev/null
"$PADDOCK" deploy ping.rbp > /dev/null
ping_pong

minor=0
for lang in go python js; do
  wasm="$OUT/pong-$lang.wasm"
  [ -f "$wasm" ] || { echo; echo "== pong in $lang: not built, left out"; continue; }
  minor=$((minor + 1))
  echo
  echo "== pong upgraded to $lang (1.$minor.0, $(du -k "$wasm" | cut -f1) KiB)"
  bundle "$HERE/$lang/pong.toml" "$wasm" "1.$minor.0" "pong-$lang.rbp"
  # --approve: each language's runtime declares capabilities the Rust
  # version does not (wasi:filesystem, and wasi:sockets or wasi:http).
  "$PADDOCK" upgrade pong "pong-$lang.rbp" --approve > /dev/null
  ping_pong
done

echo
echo "== what ping's monitor saw"
echo "  \$ paddock call ping downs"
echo "  $("$PADDOCK" call ping downs)"
echo "  \$ paddock status pong"
"$PADDOCK" status pong | sed 's/^/  /'
