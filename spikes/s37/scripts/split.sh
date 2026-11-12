#!/bin/sh
# S37's split: the driver answering `/local` itself (no call) against a
# full request through it, and the node's ingress, at 1 and 32
# connections; then DRIVERS_WIDE drivers. Same build and setup as run.sh.
set -eu
SECS=${SECS:-10}
POOL=${POOL:-4}
WORKERS=${WORKERS:-4}
S=spikes/s37/target/release
start() { # drivers
    LOG=$(mktemp)
    S37_WORKERS=$WORKERS $S/s37-bench node target/release/bulkhead-runner $S/s37-app $S/s37-driver $POOL "$1" > "$LOG" 2>&1 &
    NODE=$!
    for _ in $(seq 300); do grep -q '^ready' "$LOG" && break; sleep 0.1; done
    L=$(grep '^ready' "$LOG")
}
get() { echo "$L" | tr ' ' '\n' | grep "^$1=" | cut -d= -f2; }
one() { printf '%s ' "$1"; $S/s37-bench load "$(get "$2")" "$3" "$SECS" 0 plain $NODE "$WORKERS" "$4"; }
echo "# S37 split: $(uname -srm), $(nproc) CPUs, pool $POOL; load $(cut -d' ' -f1 /proc/loadavg)"
start 4
for c in 1 32; do
    one "ingress, a call        " ingress-http $c /x
    one "driver, answered itself" driver-http $c /local
    one "driver, a call         " driver-http $c /x
done
kill $NODE; sleep 1
start "${DRIVERS_WIDE:-8}"
for c in 32 256; do one "driver x${DRIVERS_WIDE:-8}, a call     " driver-http $c /x; done
kill $NODE
