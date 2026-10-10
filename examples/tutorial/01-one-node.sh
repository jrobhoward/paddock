#!/usr/bin/env bash
# Tutorial chapter 1: one node. A counter on a bulletin board (a shared disk
# bucket) over HTTP; crash it and the count is still there. See
# examples/tutorial/README.md.
#
#   examples/tutorial/01-one-node.sh          # pauses between steps
#   AUTO=1 examples/tutorial/01-one-node.sh   # runs straight through
#
# HTTP_PORT (default 8080) is the ingress port.
source "$(dirname "$0")/common.sh"
HTTP_PORT=${HTTP_PORT:-8080}
URL="http://127.0.0.1:$HTTP_PORT/board"

cat > policy.toml <<'TOML'
[apps.board]
grant = ["wasi:http/incoming-handler", "wasi:keyvalue", "wasi:logging"]
buckets = ["board"]

[apps.board-watch]
grant = ["wasi:keyvalue", "wasi:logging"]
buckets = ["board"]
TOML
cat > node-1.toml <<TOML
node = 1
data_dir = "$DEMO_DIR/data-1"
admin_socket = "$(sock 1)"
trusted_keys = ["$SIGNER"]
policy = "$DEMO_DIR/policy.toml"

[http.main]
address = "127.0.0.1:$HTTP_PORT"

[buckets.board]
durability = "disk"
TOML

start_node 1
deploy_board 1 "$ROOT/examples/board/paddock.toml"
deploy_board 1 "$ROOT/examples/board/watch.toml"
until_ok 30 curl -sf "$URL"

cat <<EOF

== Chapter 1: one node
A node is running (process ${NODE_PIDS[1]}), with two apps:
  board        the counter, over HTTP at $URL; it keeps the count in the
               node's 'board' bucket, on disk, not in its own memory
  board-watch  watches the count on the board and logs each change

Use it from another terminal:
  export PATH="$(dirname "$PADDOCK"):\$PATH" PADDOCK_SOCKET=$(sock 1)

  # read the count; the reply names the instance that answered
  curl -s $URL
  # add one, or ten
  curl -s -X POST $URL
  curl -s -X POST -d 10 $URL
  # make the app trap, as a bug would; then read again
  curl -s -X POST $URL/crash

  paddock ps                  # instances and their pids
  paddock logs board-watch    # every change the watcher saw
  paddock status board
EOF

pause "count to 3"
for _ in 1 2 3; do show curl -s -X POST "$URL"; done

pause "crash the app"
before=$(curl -s "$URL")
show curl -s -o /dev/null -w '%{http_code}\n' -X POST "$URL/crash"
until_ok 30 curl -sf "$URL"
show curl -s "$URL"
echo "Before the crash: $before"
echo "The pid's incarnation went up (a restarted instance), and the count"
echo "survived: it lives on the board, not in the instance."

pause "what the watcher saw"
showp 1 logs board-watch

pause "restart the whole node"
kill "${NODE_PIDS[1]}"
wait "${NODE_PIDS[1]}" 2>/dev/null || true
start_node 1
until_ok 30 curl -sf "$URL"
show curl -s "$URL"
echo "A new node process, a new instance (a new creation after the @), and"
echo "the count is still there: the bucket is on disk."

if [ "$AUTO" != 1 ]; then
  echo
  echo "The node keeps running until Ctrl-C. Try the commands above."
  wait "${NODE_PIDS[1]}"
fi
