#!/usr/bin/env bash
# Tutorial chapter 2: two nodes and a witness. The board app runs as a
# singleton on the primary; kill the primary's process and the standby
# takes over, with an empty board. See examples/tutorial/README.md.
#
#   examples/tutorial/02-failover.sh          # pauses; you kill node 1
#   AUTO=1 examples/tutorial/02-failover.sh   # runs straight through
#
# HTTP_PORT (default 8081) is node 1's ingress port, and one more is node
# 2's; BASE_PORT (default 7401) the first of the three cluster UDP ports.
source "$(dirname "$0")/common.sh"
HTTP_PORT=${HTTP_PORT:-8081}
BASE_PORT=${BASE_PORT:-7401}
url() { echo "http://127.0.0.1:$((HTTP_PORT + $1 - 1))/board"; }
addr() { echo "127.0.0.1:$((BASE_PORT + $1 - 1))"; }

cat > policy.toml <<'TOML'
[apps.board]
grant = ["wasi:http/incoming-handler", "wasi:keyvalue", "wasi:logging"]
buckets = ["board"]
TOML
echo "[peers]" > peers.toml
for n in 1 2 3; do
  [ -f "node-$n.key" ] || "$PADDOCK" cluster keygen "node-$n.key" >/dev/null
  echo "$n = [\"$("$PADDOCK" cluster pubkey "node-$n.key")\"]" >> peers.toml
done
# Short timings, so a takeover takes seconds: 200 ms heartbeats, a 1 s
# node timeout, a 2 s lease. The defaults take about 9 to 10 s.
for n in 1 2 3; do
  {
    echo "node = $n"
    echo "data_dir = \"$DEMO_DIR/data-$n\""
    echo "admin_socket = \"$(sock "$n")\""
    echo "trusted_keys = [\"$SIGNER\"]"
    echo "policy = \"$DEMO_DIR/policy.toml\""
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
    echo "lease_ms = 2000"
    echo "margin_ms = 300"
    if [ "$n" -ne 3 ]; then
      echo "[http.main]"
      echo "address = \"127.0.0.1:$((HTTP_PORT + n - 1))\""
      echo "[buckets.board]"
      echo "durability = \"disk\""
    fi
  } > "node-$n.toml"
done

start_node 3
start_node 1
start_node 2
# The same app as chapter 1, deployed to both nodes, made a singleton: it
# runs on the primary only.
SINGLETON='[appserver.placement]
mode = "singleton"'
deploy_board 1 "$ROOT/examples/board/paddock.toml" "$SINGLETON"
deploy_board 2 "$ROOT/examples/board/paddock.toml" "$SINGLETON"
role() { p "$1" --json failover | grep -q "\"role\": \"$2\""; }
until_ok 30 curl -sf "$(url 1)"
# Node 2 takes over only once it has become the standby (a node still
# starting does not ask to be promoted; see the README).
until_ok 30 role 2 standby
for _ in 1 2 3; do curl -s -X POST "$(url 1)" >/dev/null; done

cat <<EOF

== Chapter 2: two nodes and a witness
  node 1  process ${NODE_PIDS[1]}   primary: runs the board   $(url 1)
  node 2  process ${NODE_PIDS[2]}   standby                   $(url 2)
  node 3  process ${NODE_PIDS[3]}   witness: decides who is promoted

The board on node 1 counts to 3 already. From another terminal:
  export PATH="$(dirname "$PADDOCK"):\$PATH"
  curl -s $(url 1)
  curl -s -X POST $(url 1)
  curl -s $(url 2)                 # 503: the standby does not run it
  paddock --socket $(sock 1) failover
  paddock --socket $(sock 2) status board

Then kill the primary, as a power cut would:
  kill -9 ${NODE_PIDS[1]}
and read node 2 again (a few seconds later):
  curl -s $(url 2)
  paddock --socket $(sock 2) failover
The script restarts node 1 for you at the next step.
EOF

pause "the board on each node"
show curl -s "$(url 1)"
show curl -s -w '%{http_code}\n' "$(url 2)"
showp 2 failover

if [ "$AUTO" = 1 ]; then
  pause "kill -9 node 1"
  kill -9 "${NODE_PIDS[1]}"
  { wait "${NODE_PIDS[1]}"; } 2>/dev/null || true
else
  pause "kill node 1 now (kill -9 ${NODE_PIDS[1]}), then"
fi
start=$(date +%s%N)
until_ok 60 curl -sf "$(url 2)"
echo "node 2 serves the board $((($(date +%s%N) - start) / 1000000)) ms later:"
show curl -s "$(url 2)"
showp 2 failover
cat <<'EOF'
The count is back to 0. The board app moved, but its board did not: a
disk bucket lives on one node, and node 1's copy went down with it. To
keep the count across nodes, the app's state has to reach node 2 before
node 1 is lost: chapter 3 (hot standby, after ROADMAP D4) and replicated
buckets (Phase 3).
EOF

pause "restart node 1"
start_node 1
until_ok 30 role 1 standby
showp 1 failover
echo "Node 1 rejoins as the standby; node 2 keeps the board until it fails."
echo "Node 1's own board still holds 3: it would come back if node 1 were"
echo "promoted again, stale. That is the other half of why state must move."

if [ "$AUTO" != 1 ]; then
  echo
  echo "The nodes keep running until Ctrl-C."
  wait
fi
