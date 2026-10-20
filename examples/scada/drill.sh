#!/usr/bin/env bash
# The SCADA release drill (ROADMAP X1, X2): the example's apps on an HA
# pair and a witness, three `paddock node` processes, with two fake Modbus
# devices, taken through a release cycle while a probe per node asks the
# HMI for the point table and the alarms ten times a second:
#
#   deploy; upgrade control to v2 on the standby, then the primary; a
#   planned switchover; a rollback on both nodes; an upgrade of the
#   standby, then the primary lost (kill -9) before its turn, and brought
#   back and upgraded.
#
# It reports, for each step and node, the failed point and alarm requests
# out of those made, and the longest time between point updates the probe
# saw while the node answered (fake register pump-1/holding/0 counts up
# every 100 ms), and the
# pump-1-pressure alarm (raised and acknowledged before the cycle) as the
# primary sees it after each step. The point table is a `cluster` bucket
# polled by the primary alone, and control's buckets are `replicated`.
# Until moving instances (M1) and pair rollouts (U1), it shows where the
# cycle is not yet zero-downtime; X2 turns it into a pass or fail.
#
#   examples/scada/drill.sh                 # STEP_SECS=5 between steps
#   DRILL_DIR=/tmp/drill examples/scada/drill.sh
#
# HTTP_PORT (default 8091) is node 1's ingress port, and one more is node
# 2's; BASE_PORT (default 7501) the first of the three cluster UDP ports;
# DEVICE_PORT (default 5021) the first of the two devices' ports.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DRILL_DIR=${DRILL_DIR:-$(mktemp -d -t paddock-drill-XXXXXX)}
STEP_SECS=${STEP_SECS:-5}
HTTP_PORT=${HTTP_PORT:-8091}
BASE_PORT=${BASE_PORT:-7501}
DEVICE_PORT=${DEVICE_PORT:-5021}
PADDOCK="$ROOT/target/release/paddock"
WASM="$ROOT/target/wasm32-wasip2/release"
WASM_V2="$ROOT/target/fixtures/scada-drill-v2/wasm32-wasip2/release"

echo "building (release; the first build takes a few minutes)..."
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p paddock --features modbus
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
  -p paddock-modbus --example fake_device
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" --target wasm32-wasip2 \
  -p example-scada-points -p example-scada-hmi -p example-scada-control
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" --target wasm32-wasip2 \
  -p example-scada-control --features v2 --target-dir "$ROOT/target/fixtures/scada-drill-v2"

cd "$DRILL_DIR"
url() { echo "http://127.0.0.1:$((HTTP_PORT + $1 - 1))"; }
addr() { echo "127.0.0.1:$((BASE_PORT + $1 - 1))"; }
# Unix socket paths are limited to about 107 bytes; keep these short.
sock() { echo "/tmp/paddock-drill-$$-$1.sock"; }
p() {
  local n=$1
  shift
  "$PADDOCK" --socket "$(sock "$n")" "$@"
}
now_ms() { date +%s%3N; }

declare -A NODE_PIDS
PIDS=()
start_node() {
  rm -f "$(sock "$1")"
  "$PADDOCK" node --config "$DRILL_DIR/node-$1.toml" >> "$DRILL_DIR/node-$1.log" 2>&1 &
  NODE_PIDS[$1]=$!
  for _ in $(seq 200); do [ -S "$(sock "$1")" ] && return; sleep 0.05; done
  echo "node $1 did not start; see $DRILL_DIR/node-$1.log" >&2
  exit 1
}
cleanup() {
  for pid in "${NODE_PIDS[@]}" "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -f /tmp/paddock-drill-$$-*.sock
  echo "configs, data and logs are in $DRILL_DIR"
}
trap cleanup EXIT

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
role() { p "$1" --json failover | grep -q "\"role\": \"$2\""; }

# Keys, signer, policy and the three nodes' configs.
[ -f signer.key ] || "$PADDOCK" keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)
cat > policy.toml <<'TOML'
[apps.points]
grant = ["wasi:logging", "wasi:keyvalue"]
buckets = ["points"]
[apps.hmi]
grant = ["wasi:http/incoming-handler", "wasi:keyvalue", "wasi:logging"]
buckets = ["points"]
[apps.control]
grant = ["wasi:http/incoming-handler", "wasi:keyvalue", "wasi:logging", "wasi:config",
         "paddock-modbus:modbus"]
buckets = ["points"]
TOML
echo "[peers]" > peers.toml
for n in 1 2 3; do
  [ -f "node-$n.key" ] || "$PADDOCK" cluster keygen "node-$n.key" >/dev/null
  echo "$n = [\"$("$PADDOCK" cluster pubkey "node-$n.key")\"]" >> peers.toml
done
for n in 1 2 3; do
  {
    echo "node = $n"
    echo "data_dir = \"$DRILL_DIR/data-$n\""
    echo "admin_socket = \"$(sock "$n")\""
    echo "trusted_keys = [\"$SIGNER\"]"
    echo "policy = \"$DRILL_DIR/policy.toml\""
    echo "max_instances = 64"
    echo "[cluster]"
    echo "address = \"$(addr "$n")\""
    [ "$n" -ne 3 ] && echo "seeds = [\"$(addr 3)\"]"
    echo "key = \"$DRILL_DIR/node-$n.key\""
    echo "peers = \"$DRILL_DIR/peers.toml\""
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
      # control's own buckets, replicated on the pair (K3).
      echo "replication = true"
    fi
    echo "lease_ms = 2000"
    echo "margin_ms = 300"
    if [ "$n" -ne 3 ]; then
      echo "[http.main]"
      echo "address = \"127.0.0.1:$((HTTP_PORT + n - 1))\""
      # control's routes: from the standby to the primary, over the cluster.
      echo "standby = \"forward\""
      # The point table, one copy on each node of the pair (K2), written
      # by the primary's polls only.
      echo "[buckets.points]"
      echo "durability = \"disk\""
      echo "scope = \"cluster\""
      for d in 1 2; do
        echo "[extensions.\"paddock-modbus:modbus\".devices.pump-$d]"
        echo "address = \"127.0.0.1:$((DEVICE_PORT + d - 1))\""
        echo "poll_ms = 100"
        echo "timeout_ms = 500"
        echo "deliver_to = \"points\""
        echo "primary_only = true"
        echo "holding = [{ start = 0, count = 4 }]"
      done
    fi
  } > "node-$n.toml"
done

bundle() {
  "$PADDOCK" bundle "$ROOT/examples/scada/$1" "$2" --key signer.key --out "$3" >/dev/null
}
bundle points/paddock.toml "$WASM/example_scada_points.wasm" points.rbp
bundle hmi/paddock.toml "$WASM/example_scada_hmi.wasm" hmi.rbp
bundle control/paddock.toml "$WASM/example_scada_control.wasm" control.rbp
bundle control/paddock-v2.toml "$WASM_V2/example_scada_control.wasm" control-v2.rbp

"$ROOT/target/release/examples/fake_device" \
  "127.0.0.1:$DEVICE_PORT" "127.0.0.1:$((DEVICE_PORT + 1))" > devices.log 2>&1 &
PIDS+=($!)
start_node 3
start_node 1
start_node 2
for n in 1 2; do
  for app in points hmi control; do p "$n" deploy "$app.rbp" >/dev/null; done
done
until_ok 30 role 1 primary
until_ok 30 role 2 standby
until_ok 30 curl -sf "$(url 1)/scada/alarms"
until_ok 30 curl -sf "$(url 2)/scada/alarms"

# The probes, two loops per node so a slow alarm request does not delay
# the next point read: lines `ms p status ramp` and `ms a status`.
probe_points() {
  local base t out code ramp
  base=$(url "$1")
  while :; do
    t=$(now_ms)
    out=$(curl -s -m 2 -w ' %{http_code}' "$base/scada/points" 2>/dev/null || true)
    code=${out##* }
    ramp=$(grep -o '"pump-1/holding/0":[0-9]*' <<<"$out" | cut -d: -f2 || true)
    echo "$t p ${code:-000} ${ramp:--}" >> "probe-$1.log"
    sleep 0.1
  done
}
probe_alarms() {
  local base t code
  base=$(url "$1")
  while :; do
    t=$(now_ms)
    code=$(curl -s -m 2 -o /dev/null -w '%{http_code}' "$base/scada/alarms" 2>/dev/null || true)
    echo "$t a ${code:-000}" >> "probe-$1.log"
    sleep 0.1
  done
}
for n in 1 2; do
  probe_points "$n" &
  PIDS+=($!)
  probe_alarms "$n" &
  PIDS+=($!)
done

primary() { if role 1 primary; then echo 1; else echo 2; fi; }
# The step's start, and after it the alarm as the primary's control sees it.
mark() { echo "$(now_ms) $*" >> steps.log; echo "-- $*"; }
settle() {
  sleep "$STEP_SECS"
  local a
  a=$(curl -s "$(url "$(primary)")/scada/alarms" || true)
  echo "$(tail -n 1 steps.log | cut -d' ' -f2-)|$(grep -o '"version":"[^"]*"' <<<"$a" || true) \
$(grep -o '"pump-1-pressure":{[^}]*"status":"[a-z]*"' <<<"$a" | grep -o '"status":"[a-z]*"' || true)" \
    >> states.log
}
write() {
  curl -sf -X POST -d "{\"device\":\"pump-1\",\"table\":\"holding\",\"address\":1,\"value\":$1}" \
    "$(url 1)/scada/write" >/dev/null
}
active() { curl -s "$(url 1)/scada/alarms" | grep -q '"pump-1-pressure":{[^}]*"status":"active"'; }

mark "steady, deployed on both"
write 200
until_ok 10 active
curl -sf -X POST "$(url 2)/scada/alarms/pump-1-pressure/ack" >/dev/null
settle
mark "upgrade standby (2) to v2"
p 2 upgrade control control-v2.rbp
settle
mark "upgrade primary (1) to v2"
p 1 upgrade control control-v2.rbp
settle
mark "switchover 1 -> 2"
p 1 failover switchover >/dev/null
until_ok 30 role 2 primary
until_ok 30 role 1 standby
settle
mark "rollback standby (1)"
p 1 rollback control
settle
mark "rollback primary (2)"
p 2 rollback control
settle
mark "upgrade standby (1) to v2"
p 1 upgrade control control-v2.rbp
settle
mark "primary (2) lost before its upgrade"
kill -9 "${NODE_PIDS[2]}"
{ wait "${NODE_PIDS[2]}"; } 2>/dev/null || true
until_ok 60 role 1 primary
settle
mark "node 2 back as standby, upgraded"
start_node 2
until_ok 60 role 2 standby
p 2 upgrade control control-v2.rbp
settle
mark "end"

echo
echo "== Per step and node: point requests failed, alarm requests failed, and"
echo "   the longest time between point updates while the node answered (ms)"
awk '
  FNR == 1 { file++ }
  file == 1 { at[++steps] = $1; $1 = ""; name[steps] = substr($0, 2); next }
  {
    node = file - 1; t = $1
    s = 0
    for (i = 1; i <= steps; i++) if (t >= at[i]) s = i
    if (s == 0 || s == steps) next
    k = s SUBSEP node
    if ($2 == "a") { areq[k]++; if ($3 != 200) aerr[k]++; next }
    preq[k]++
    if ($3 != 200) { perr[k]++; delete seen[node]; next }
    if ($4 != "-" && $4 != last[node]) {
      if (node in seen && t - seen[node] > gap[k]) gap[k] = t - seen[node]
      seen[node] = t; last[node] = $4
    }
  }
  END {
    printf "%-38s %5s   %-28s %-28s\n", "step", "secs", "node 1", "node 2"
    for (s = 1; s < steps; s++) {
      line = sprintf("%-38s %5.1f  ", name[s], (at[s + 1] - at[s]) / 1000)
      for (n = 1; n <= 2; n++) {
        k = s SUBSEP n
        line = line sprintf(" %3d/%-3d %3d/%-3d %5d ms ", perr[k], preq[k], aerr[k], areq[k], gap[k])
      }
      print line
    }
  }
' steps.log probe-1.log probe-2.log | tee report.txt
echo
echo "== control on the primary after each step (pump-1-pressure was acknowledged)"
column -t -s '|' states.log | tee -a report.txt
