#!/usr/bin/env bash
# Tutorial chapter 5: replicated buckets. Chapter 2's pair and witness,
# with the board bucket replicated between the pair (`scope = "cluster"`):
# kill the primary's process and the standby takes over with the count
# where it was. See examples/tutorial/README.md.
#
#   examples/tutorial/05-replicated.sh          # pauses; you kill node 1
#   AUTO=1 examples/tutorial/05-replicated.sh   # runs straight through
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
      # Run replication for the pair's cluster buckets (set alike on both).
      echo "replication = true"
    fi
    echo "lease_ms = 2000"
    echo "margin_ms = 300"
    if [ "$n" -ne 3 ]; then
      echo "[http.main]"
      echo "address = \"127.0.0.1:$((HTTP_PORT + n - 1))\""
      # The partner's listener: the standby redirects the board there.
      echo "partner_url = \"http://127.0.0.1:$((HTTP_PORT + 2 - n))\""
      echo "[buckets.board]"
      echo "durability = \"disk\""
      # Kept whole on both nodes of the pair, through the replicated log.
      echo "scope = \"cluster\""
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
# Node N serves the board itself (not a redirect).
serves() { [ "$(curl -s -o /dev/null -w '%{http_code}' "$(url "$1")")" = 200 ]; }
until_ok 30 serves 1
# Node 2 takes over only once it has become the standby (a node still
# starting does not ask to be promoted; see the README).
until_ok 30 role 2 standby
for _ in 1 2 3; do curl -s -X POST "$(url 1)" >/dev/null; done

cat <<EOF

== Chapter 5: replicated buckets
  node 1  process ${NODE_PIDS[1]}   primary: runs the board   $(url 1)
  node 2  process ${NODE_PIDS[2]}   standby                   $(url 2)
  node 3  process ${NODE_PIDS[3]}   witness: decides who is promoted

The board on node 1 counts to 3 already, and the board bucket is now a
cluster bucket: each write returns once node 2 has it on disk too.
From another terminal:
  export PATH="$(dirname "$PADDOCK"):\$PATH"
  curl -s $(url 1)
  curl -s -X POST $(url 1)
  curl -si $(url 2)                # 307 to node 1: the standby does not run it
  curl -sL $(url 2)                # follows the redirect
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
show curl -s -o /dev/null -w '%{http_code} %{redirect_url}\n' "$(url 2)"
show curl -sL "$(url 2)"
showp 2 failover

if [ "$AUTO" = 1 ]; then
  pause "kill -9 node 1"
  kill -9 "${NODE_PIDS[1]}"
  { wait "${NODE_PIDS[1]}"; } 2>/dev/null || true
else
  pause "kill node 1 now (kill -9 ${NODE_PIDS[1]}), then"
fi
start=$(date +%s%N)
until_ok 60 serves 2
echo "node 2 serves the board $((($(date +%s%N) - start) / 1000000)) ms later:"
show curl -s "$(url 2)"
showp 2 failover
cat <<'EOF'
The count is still 3. The board app started from init on node 2, as in
chapter 2, but this time the board was already there: every write node 1
answered was on node 2's disk first. The app's code did not change; only
the bucket's scope in node config did.
EOF

pause "write while node 1 is down"
show curl -s -X POST "$(url 2)"
showp 2 failover
cat <<'EOF'
Node 2 commits alone now ("committing alone"). Before its first write
alone it had the witness record node 1 as out of date, so the witness will
not promote node 1 until it has caught up.
EOF

pause "restart node 1"
start_node 1
until_ok 30 role 1 standby
showp 1 failover
until_ok 10 curl -sf -o /dev/null "$(url 1)"
show curl -s -o /dev/null -w '%{http_code} %{redirect_url}\n' "$(url 1)"
caught_up() { ! p 2 failover | grep -q 'committing alone'; }
until_ok 30 caught_up
showp 2 failover
echo "Node 1 rejoins as the standby and catches up from node 2's log: the"
echo "write it missed is on its disk now, and node 2 commits with it again."
echo "Unlike chapter 2, node 1 holds no stale board: if node 2 failed next,"
echo "node 1 would take over at the latest count."

if [ "$AUTO" != 1 ]; then
  echo
  echo "The nodes keep running until Ctrl-C."
  wait
fi
