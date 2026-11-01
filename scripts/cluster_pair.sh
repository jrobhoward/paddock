#!/usr/bin/env bash
# cluster_throughput with its two nodes as two processes on this machine:
# node 2 served at 127.0.0.1:7400, node 1 connecting from 127.0.0.1:7401.
# The benchmark's output is node 1's; node 2 is stopped at the end.
#
#   scripts/cluster_pair.sh [WORKERS]                    # components
#   CLUSTER_NATIVE=1 scripts/cluster_pair.sh [WORKERS]   # native programs
#
# Needs target/release/examples/cluster_throughput and its fixtures
# (scripts/bench.sh builds them; the example's header lists the commands).
set -euo pipefail

workers=${1:-4}
bin=target/release/examples/cluster_throughput
dir=$(mktemp -d -t bulkhead-cluster-XXXXXX)
serve=
cleanup() {
  # Node 2 ends by SIGTERM; under set -e its status would become ours.
  if [ -n "$serve" ] && kill "$serve" 2>/dev/null; then
    wait "$serve" 2>/dev/null || true
  fi
  rm -rf "$dir"
}
trap cleanup EXIT

"$bin" "$workers" keys "$dir"
"$bin" "$workers" serve "$dir" 127.0.0.1:7400 >"$dir/serve.log" 2>&1 &
serve=$!
for _ in $(seq 300); do
  [ -s "$dir/pids" ] && break
  kill -0 "$serve" 2>/dev/null || { cat "$dir/serve.log" >&2; exit 1; }
  sleep 0.1
done
[ -s "$dir/pids" ] || { echo "cluster_pair: node 2 did not start" >&2; exit 1; }
"$bin" "$workers" connect "$dir" 127.0.0.1:7401 127.0.0.1:7400
