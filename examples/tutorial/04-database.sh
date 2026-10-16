#!/usr/bin/env bash
# Tutorial chapter 4: state outside the pair. The tally app keeps its count
# in Postgres (paddock:sql) and runs as a singleton on an HA pair, as the
# board did in chapter 2; kill the primary's process and node 2 carries on
# from the same count, because both nodes reach the same database. See
# examples/tutorial/README.md.
#
#   examples/tutorial/04-database.sh          # pauses; you kill node 1
#   AUTO=1 examples/tutorial/04-database.sh   # runs straight through
#
# Needs Postgres's server programs (on Ubuntu: apt install postgresql; the
# system service is not used and can be disabled). This script starts its
# own server in the demo directory, as you, on a Unix socket only, and stops
# it at the end. PADDOCK_PG_BIN names the directory holding initdb and
# pg_ctl if they are not under /usr/lib/postgresql/*/bin.
#
# HTTP_PORT (default 8081) is node 1's ingress port, and one more is node
# 2's; BASE_PORT (default 7401) the first of the three cluster UDP ports.
source "$(dirname "$0")/common.sh"
HTTP_PORT=${HTTP_PORT:-8081}
BASE_PORT=${BASE_PORT:-7401}
url() { echo "http://127.0.0.1:$((HTTP_PORT + $1 - 1))/tally"; }
addr() { echo "127.0.0.1:$((BASE_PORT + $1 - 1))"; }

PG_BIN=${PADDOCK_PG_BIN:-$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -n 1)}
if [ ! -x "$PG_BIN/initdb" ] || [ ! -x "$PG_BIN/pg_ctl" ]; then
  echo "this chapter needs Postgres's initdb and pg_ctl (apt install postgresql," >&2
  echo "or set PADDOCK_PG_BIN to the directory holding them)" >&2
  exit 1
fi
PG_PORT=5433
pg_stop() { "$PG_BIN/pg_ctl" -D "$DEMO_DIR/pg" -m fast stop >/dev/null 2>&1 || true; }
trap 'cleanup; pg_stop' EXIT
if [ ! -d "$DEMO_DIR/pg" ]; then
  "$PG_BIN/initdb" -A trust -U paddock -E UTF8 -D "$DEMO_DIR/pg" >/dev/null
fi
"$PG_BIN/pg_ctl" -D "$DEMO_DIR/pg" -l "$DEMO_DIR/pg.log" -w start \
  -o "-k $DEMO_DIR -p $PG_PORT -c listen_addresses=''" >/dev/null
echo "Postgres is running in $DEMO_DIR/pg, on the socket in $DEMO_DIR"

cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
  --target wasm32-wasip2 -p example-tally
TALLY_WASM="$ROOT/target/wasm32-wasip2/release/example_tally.wasm"

cat > policy.toml <<'TOML'
[apps.tally]
grant = ["wasi:http/incoming-handler", "paddock:sql", "wasi:logging"]
datasources = ["main"]
TOML
echo "[peers]" > peers.toml
for n in 1 2 3; do
  [ -f "node-$n.key" ] || "$PADDOCK" cluster keygen "node-$n.key" >/dev/null
  echo "$n = [\"$("$PADDOCK" cluster pubkey "node-$n.key")\"]" >> peers.toml
done
# Chapter 2's pair and witness, with the same short timings, and the
# datasource on both nodes of the pair.
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
      echo "[datasources.main]"
      echo "host = \"$DEMO_DIR\""
      echo "port = $PG_PORT"
      echo "dbname = \"postgres\""
      echo "user = \"paddock\""
    fi
  } > "node-$n.toml"
done

start_node 3
start_node 1
start_node 2
# The tally, as a singleton: it runs on the primary only.
cp "$ROOT/examples/tally/paddock.toml" tally.toml
printf '\n[appserver.placement]\nmode = "singleton"\n' >> tally.toml
"$PADDOCK" bundle tally.toml "$TALLY_WASM" --key signer.key --out tally.rbp >/dev/null
p 1 deploy tally.rbp >/dev/null
p 2 deploy tally.rbp >/dev/null
role() { p "$1" --json failover | grep -q "\"role\": \"$2\""; }
# Node N serves the tally itself.
serves() { [ "$(curl -s -o /dev/null -w '%{http_code}' "$(url "$1")")" = 200 ]; }
until_ok 30 serves 1
until_ok 30 role 2 standby
curl -s -X DELETE "$(url 1)" >/dev/null
for _ in 1 2 3; do curl -s -X POST "$(url 1)" >/dev/null; done

cat <<EOF

== Chapter 4: state outside the pair
  node 1  process ${NODE_PIDS[1]}   primary: runs the tally   $(url 1)
  node 2  process ${NODE_PIDS[2]}   standby                   $(url 2)
  node 3  process ${NODE_PIDS[3]}   witness
  Postgres                  $DEMO_DIR (socket), port $PG_PORT

The tally on node 1 counts to 3, in Postgres. From another terminal:
  export PATH="$(dirname "$PADDOCK"):\$PATH"
  curl -s $(url 1)
  curl -s -X POST -d 10 $(url 1)
  $PG_BIN/psql -h $DEMO_DIR -p $PG_PORT -U paddock postgres -c 'SELECT * FROM tally_log'

Then kill the primary, as a power cut would:
  kill -9 ${NODE_PIDS[1]}
EOF

pause "the tally on node 1"
show curl -s "$(url 1)"

if [ "$AUTO" = 1 ]; then
  pause "kill -9 node 1"
  kill -9 "${NODE_PIDS[1]}"
  { wait "${NODE_PIDS[1]}"; } 2>/dev/null || true
else
  pause "kill node 1 now (kill -9 ${NODE_PIDS[1]}), then"
fi
start=$(date +%s%N)
until_ok 60 serves 2
echo "node 2 serves the tally $((($(date +%s%N) - start) / 1000000)) ms later:"
show curl -s "$(url 2)"
show curl -s -X POST "$(url 2)"
cat <<'EOF'
The count carried on from 3: the tally on node 2 started from init, as in
chapter 2, but its state was never on node 1. Each change was committed in
Postgres, in one transaction with its line in tally_log (whose "by" names
the instance that made it), before the node answered. The database is
outside the pair; making it highly available is the database's own matter
(replication, a managed service), not Paddock's.
EOF

pause "restart node 1"
start_node 1
until_ok 30 role 1 standby
showp 1 failover
echo "Node 1 rejoins as the standby; the count is still in Postgres."

if [ "$AUTO" != 1 ]; then
  echo
  echo "The nodes and Postgres keep running until Ctrl-C."
  wait
fi
