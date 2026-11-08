#!/usr/bin/env bash
# Run a Bulkhead node with traffic worth tracing, for the bpftrace scripts in
# examples/tracing/bt. See examples/tracing/README.md.
#
#   examples/tracing/demo.sh            # from the repository root; Ctrl-C stops it
#
# It builds bulkhead and the ping-pong and chaos examples, starts a node in
# $DEMO_DIR (default: a fresh temporary directory), deploys pong, ping and a
# crash-prone chaos, and then loops:
#   - ping calls pong (admin API -> ping -> pong) a few times a second;
#   - every 10 s, pong is killed: it exits, restarts, and ping gets `down`;
#   - every 15 s, chaos spins in a call with a 4-message mailbox while casts
#     arrive: the casts overflow and are dropped, and chaos exits with
#     `timeout` and restarts.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DEMO_DIR=${DEMO_DIR:-$(mktemp -d -t bulkhead-trace-XXXXXX)}
BULKHEAD="$ROOT/target/release/bulkhead"
PROGRAMS="$ROOT/target/release"

echo "building (release)..."
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p bulkhead
# The node runs apps through the bulkhead-runner beside it.
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p bulkhead-native --bin bulkhead-runner
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
  -p example-ping-pong -p example-chaos

cd "$DEMO_DIR"
"$BULKHEAD" keygen signer --no-password >/dev/null
# Unix socket paths are limited to about 107 bytes; keep this one short.
SOCKET="/tmp/bulkhead-trace-$$.sock"
cat > node.toml <<TOML
data_dir = "$DEMO_DIR/data"
admin_socket = "$SOCKET"
trusted_keys = ["$(tail -n 1 signer.pub)"]
TOML

# chaos with a tiny mailbox and a short deadline, so a spinning call drops
# the casts that arrive meanwhile and then times out.
cat > chaos.toml <<'TOML'
[app]
name = "chaos"
version = "1.0.0"

[appserver.limits]
mailbox_max = 4
callback_deadline_ms = 300

[appserver.supervision]
intensity = 100
period_ms = 60000

[appserver.capabilities."bulkhead:logging"]
max_level = "info"
TOML

"$BULKHEAD" node --config node.toml > node.log 2>&1 &
NODE=$!
export BULKHEAD_SOCKET="$SOCKET"
cleanup() {
  kill "$NODE" 2>/dev/null || true
  wait "$NODE" 2>/dev/null || true
  echo
  echo "node stopped; its files are in $DEMO_DIR"
}
trap cleanup EXIT
for _ in $(seq 100); do [ -S "$BULKHEAD_SOCKET" ] && break; sleep 0.05; done

bundle() {
  "$BULKHEAD" bundle "$1" "$PROGRAMS/$2" --key signer.key --out "$3" >/dev/null
  "$BULKHEAD" deploy "$3" >/dev/null
}
bundle "$ROOT/examples/ping-pong/pong.toml" example-ping-pong pong.rbp
bundle "$ROOT/examples/ping-pong/ping.toml" example-ping-pong ping.rbp
bundle chaos.toml example-chaos chaos.rbp

cat <<EOF

node running: pid $NODE, data in $DEMO_DIR
In another terminal, from $ROOT:

  sudo bpftrace -p $NODE examples/tracing/bt/calls.bt
  sudo bpftrace -p $NODE examples/tracing/bt/lifecycle.bt
  sudo bpftrace -p $NODE examples/tracing/bt/callbacks.bt
  sudo bpftrace -p $NODE examples/tracing/bt/everything.bt

and to watch the instances:

  BULKHEAD_SOCKET=$BULKHEAD_SOCKET $BULKHEAD ps

Traffic is running; Ctrl-C stops it and the node.
EOF

pid_of() { "$BULKHEAD" ps | awk -v app="$1" '$2 == app { print $1 }'; }

n=0
while true; do
  n=$((n + 1))
  "$BULKHEAD" call ping "ping:$n" >/dev/null || true
  if (( n % 40 == 0 )); then
    echo "$(date +%T) killing pong $(pid_of pong)"
    "$BULKHEAD" kill "$(pid_of pong)" >/dev/null || true
  fi
  if (( n % 60 == 30 )); then
    echo "$(date +%T) chaos: spin while casts arrive"
    "$BULKHEAD" call chaos spin >/dev/null 2>&1 &
    sleep 0.05
    for _ in 1 2 3 4 5 6 7 8; do "$BULKHEAD" cast chaos "echo:x" >/dev/null 2>&1 || true; done
  fi
  sleep 0.25
done
