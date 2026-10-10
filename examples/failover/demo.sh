#!/usr/bin/env bash
# Fail over a singleton between two Paddock nodes, with a witness, as three
# `paddock node` processes on loopback. See examples/failover/README.md.
#
#   examples/failover/demo.sh            # from the repository root
#
# It builds paddock and the counter example, makes a key per node and a
# peers file, starts a witness (node 3) and a pair (nodes 1 and 2), and
# deploys the counter to both members as a singleton. Then it:
#   1. waits for node 1 to be primary and run the counter, and counts to 3;
#   2. kills node 1 with SIGKILL and times how long until node 2 runs it;
#   3. restarts node 1 and shows it rejoin as standby at the new epoch.
# Everything is stopped at the end; the files stay in $DEMO_DIR.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DEMO_DIR=${DEMO_DIR:-$(mktemp -d -t paddock-failover-XXXXXX)}
BASE_PORT=${BASE_PORT:-7401}
PADDOCK="$ROOT/target/release/paddock"
WASM="$ROOT/target/wasm32-wasip2/release/example_counter.wasm"

echo "building (release)..."
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p paddock
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
  --target wasm32-wasip2 -p example-counter

cd "$DEMO_DIR"
"$PADDOCK" keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)

# Unix socket paths are limited to about 107 bytes; keep these short.
sock() { echo "/tmp/paddock-fo-$$-$1.sock"; }
addr() { echo "127.0.0.1:$((BASE_PORT + $1 - 1))"; }
p() {
  local n=$1
  shift
  "$PADDOCK" --socket "$(sock "$n")" "$@"
}

echo "[peers]" > peers.toml
for n in 1 2 3; do
  echo "$n = [\"$("$PADDOCK" cluster keygen "node-$n.key")\"]" >> peers.toml
done

# Fast timings for the demo: 200 ms heartbeats, a 1 s node timeout, a 2 s
# lease and a 300 ms margin. The defaults (1 s, 5 s, 3 s, 500 ms) hand over
# in about 9 to 10 s.
LEASE="lease_ms = 2000
margin_ms = 300"
for n in 1 2 3; do
  {
    echo "node = $n"
    echo "data_dir = \"$DEMO_DIR/data-$n\""
    echo "admin_socket = \"$(sock "$n")\""
    echo "trusted_keys = [\"$SIGNER\"]"
    echo "[cluster]"
    echo "address = \"$(addr "$n")\""
    [ "$n" -ne 3 ] && echo "seeds = [\"$(addr 3)\"]"
    echo "key = \"$DEMO_DIR/node-$n.key\""
    echo "peers = \"$DEMO_DIR/peers.toml\""
    echo "heartbeat_interval_ms = 200"
    echo "node_timeout_ms = 1000"
    if [ "$n" -eq 3 ]; then
      echo "role = \"witness\""
      echo "[witness]"
      echo "pair = [1, 2]"
      echo "first = 1"
    else
      echo "[failover]"
      echo "partner = $((3 - n))"
      echo "arbiter = \"witness\""
      echo "witness = 3"
    fi
    echo "$LEASE"
  } > "node-$n.toml"
done

declare -A PIDS
start() {
  "$PADDOCK" node --config "node-$1.toml" >> "node-$1.log" 2>&1 &
  PIDS[$1]=$!
  for _ in $(seq 200); do [ -S "$(sock "$1")" ] && return; sleep 0.05; done
  echo "node $1 did not start; see $DEMO_DIR/node-$1.log" >&2
  exit 1
}
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -f "$(sock 1)" "$(sock 2)" "$(sock 3)"
  echo
  echo "nodes stopped; their configs, data and logs are in $DEMO_DIR"
}
trap cleanup EXIT

# Wait up to $1 seconds for the command after it to succeed.
until_ok() {
  local deadline=$(($(date +%s) + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "timed out waiting for: $*" >&2
      exit 1
    fi
    sleep 0.05
  done
}
running() { p "$1" --json status counter | grep -q '"status": "running"'; }
role() { p "$1" --json failover | grep -q "\"role\": \"$2\""; }

start 3
start 1
start 2

# The counter, made a singleton: one instance, on the primary only.
cp "$ROOT/examples/counter/paddock.toml" counter.toml
printf '\n[appserver.placement]\nmode = "singleton"\n' >> counter.toml
"$PADDOCK" bundle counter.toml "$WASM" --key signer.key --out counter.rbp >/dev/null
p 1 deploy counter.rbp >/dev/null
p 2 deploy counter.rbp >/dev/null

echo
echo "== 1. node 1 primary, node 2 standby"
until_ok 30 running 1
until_ok 30 role 2 standby
for _ in 1 2 3; do p 1 call counter inc >/dev/null; done
echo "node 1: counter $(p 1 status counter | sed -n "s/^status: *//p")"
echo "node 1: $(p 1 call counter get)"
echo "node 2: counter $(p 2 status counter | sed -n "s/^status: *//p")"
p 2 failover

echo
echo "== 2. kill -9 node 1"
kill -9 "${PIDS[1]}"
wait "${PIDS[1]}" 2>/dev/null || true
unset 'PIDS[1]'
START=$(date +%s%N)
until_ok 60 running 2
TOOK=$((($(date +%s%N) - START) / 1000000))
echo "node 2 runs the counter $TOOK ms after node 1 was killed"
echo "node 2: $(p 2 call counter get)   (a singleton starts from init: state is not carried over)"
p 2 failover

echo
echo "== 3. restart node 1"
start 1
until_ok 30 role 1 standby
echo "node 1: counter $(p 1 status counter | sed -n "s/^status: *//p")"
p 1 failover
