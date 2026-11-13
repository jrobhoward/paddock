#!/bin/sh
# S37's measurements, from the main checkout, after
#   cargo build --release -p bulkhead-native --bin bulkhead-runner
#   (cd spikes/s37 && cargo build --release)
# On an idle machine only. Each row alternates the node's ingress and the
# driver; ROUNDS rounds; SECS seconds a run after a second's warm-up.
#   sh spikes/s37/scripts/run.sh > report.txt
set -eu
ROUNDS=${ROUNDS:-2}
SECS=${SECS:-10}
POOL=${POOL:-4}
DRIVERS=${DRIVERS:-4}
WORKERS=${WORKERS:-4}
# DRIVER=s37-driver-da1 S37_DRIVER=da1: DA1's driver (one process, DRIVERS
# worker threads, its own accepts, calls by name in flight) in place of
# S37's connection handler.
DRIVER=${DRIVER:-s37-driver}
S=spikes/s37/target/release
LOG=$(mktemp)
S37_WORKERS=$WORKERS $S/s37-bench node target/release/bulkhead-runner $S/s37-app $S/$DRIVER $POOL $DRIVERS > "$LOG" 2>&1 &
NODE=$!
trap 'kill $NODE 2>/dev/null; rm -f "$LOG"' EXIT
for _ in $(seq 300); do grep -q '^ready' "$LOG" && break; sleep 0.1; done
L=$(grep '^ready' "$LOG")
get() { echo "$L" | tr ' ' '\n' | grep "^$1=" | cut -d= -f2; }
echo "# S37 run: $(uname -srm), $(nproc) CPUs, pool $POOL, drivers $DRIVERS ($DRIVER), $WORKERS node and client workers"
echo "# start load: $(cut -d' ' -f1-3 /proc/loadavg)"
row() { # path-pair label conns body tls
    for side in ingress driver; do
        if [ "$4" = tls ]; then a=$(get $side-https); else a=$(get $side-http); fi
        printf '%s %s ' "$1" "$side"
        $S/s37-bench load "$a" "$2" "$SECS" "$3" "$4" $NODE "$WORKERS"
    done
}
for r in $(seq "$ROUNDS"); do
    echo "## round $r (load $(cut -d' ' -f1 /proc/loadavg))"
    for c in 1 32 256; do row "GET" $c 0 plain; done
    for c in 1 32 256; do row "GET" $c 0 tls; done
    for b in 1024 65536 1048576; do for c in 1 32; do row "POST" $c $b plain; done; done
done
