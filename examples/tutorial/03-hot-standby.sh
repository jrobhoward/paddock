#!/usr/bin/env bash
# Tutorial chapter 3: a hot standby. The board app runs on both nodes of
# the pair; the primary puts each write on the standby's board, through a
# process group across the nodes, before it answers. Kill the primary's
# process and the count is still there on node 2. See
# examples/tutorial/README.md.
#
#   examples/tutorial/03-hot-standby.sh          # pauses; you kill node 1
#   AUTO=1 examples/tutorial/03-hot-standby.sh   # runs straight through
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
grant = ["wasi:http/incoming-handler", "wasi:keyvalue", "wasi:logging", "paddock:groups"]
buckets = ["board"]
TOML
echo "[peers]" > peers.toml
for n in 1 2 3; do
  [ -f "node-$n.key" ] || "$PADDOCK" cluster keygen "node-$n.key" >/dev/null
  echo "$n = [\"$("$PADDOCK" cluster pubkey "node-$n.key")\"]" >> peers.toml
done
# The same short timings as chapter 2.
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
# The board as a hot standby (examples/board/hot-standby.toml): on both
# nodes, told its role, in the `board` process group across the pair.
deploy_board 1 "$ROOT/examples/board/hot-standby.toml"
deploy_board 2 "$ROOT/examples/board/hot-standby.toml"
role() { p "$1" --json failover | grep -q "\"role\": \"$2\""; }
# A write on node N that reached a standby.
replicated() { curl -sf -X POST "$(url "$1")" | grep -q '"replicas":1'; }
until_ok 30 role 1 primary
until_ok 30 role 2 standby
# The first write that reaches the standby: the primary has seen the
# standby's instance join the group.
until_ok 30 replicated 1
for _ in 1 2; do curl -s -X POST "$(url 1)" >/dev/null; done

cat <<EOF

== Chapter 3: a hot standby
  node 1  process ${NODE_PIDS[1]}   primary: takes writes           $(url 1)
  node 2  process ${NODE_PIDS[2]}   standby: holds every write too  $(url 2)
  node 3  process ${NODE_PIDS[3]}   witness

The board counts to 3 on both nodes. From another terminal:
  export PATH="$(dirname "$PADDOCK"):\$PATH"
  curl -s -X POST $(url 1)          # "replicas":1: on node 2's board too
  curl -s $(url 2)                  # the standby reads its own board
  curl -s -X POST $(url 2)          # 503: writes go to the primary
  paddock --socket $(sock 1) logs board

Then kill the primary:
  kill -9 ${NODE_PIDS[1]}
EOF

pause "the board on each node"
show curl -s "$(url 1)"
show curl -s "$(url 2)"
show curl -s -w '%{http_code}\n' -X POST "$(url 2)"

if [ "$AUTO" = 1 ]; then
  pause "kill -9 node 1"
  kill -9 "${NODE_PIDS[1]}"
  { wait "${NODE_PIDS[1]}"; } 2>/dev/null || true
else
  pause "kill node 1 now (kill -9 ${NODE_PIDS[1]}), then"
fi
start=$(date +%s%N)
until_ok 60 role 2 primary
# The app hears its role a moment after the node: until it takes a write
# (adding 0 changes nothing).
until_ok 10 curl -sf -X POST -d 0 "$(url 2)"
echo "node 2 is the primary $((($(date +%s%N) - start) / 1000000)) ms later:"
show curl -s "$(url 2)"
show curl -s -X POST "$(url 2)"
cat <<'EOF'
The count carried on from where node 1 left it: every write node 1
answered was on node 2's board before node 1 answered it. The app was
already running on node 2, so nothing had to start: it was told it is the
primary and takes writes now. "replicas":0: there is no standby to copy to.
EOF

pause "restart node 1"
start_node 1
until_ok 30 role 1 standby
until_ok 30 replicated 2
show curl -s "$(url 1)"
echo "Node 1 rejoined as the standby, its instance joined the group again,"
echo "and the next write brought its board up to date."

if [ "$AUTO" != 1 ]; then
  echo
  echo "The nodes keep running until Ctrl-C."
  wait
fi
