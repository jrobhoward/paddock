#!/usr/bin/env bash
# scripts/beam_cluster.escript with its two Erlang nodes as two processes
# on this machine, bench2@127.0.0.1 serving and bench1@127.0.0.1 running
# the figures; `tls` uses TLS distribution, with certificates made for the
# run. Schedulers from SCHEDULERS (default 4).
#
#   scripts/beam_cluster_pair.sh [tls]
set -euo pipefail

flags="+S ${SCHEDULERS:-4}:${SCHEDULERS:-4}"
dir=$(mktemp -d -t bulkhead-beam-cluster-XXXXXX)
serve=
cleanup() {
  [ -n "$serve" ] && kill "$serve" 2>/dev/null && wait "$serve" 2>/dev/null
  rm -rf "$dir"
}
trap cleanup EXIT

if [ "${1:-}" = tls ]; then
  "$(dirname "$0")/beam_cluster_tls.sh" "$dir" >/dev/null
  flags="$flags -proto_dist inet_tls -ssl_dist_optfile $dir/ssl_dist.conf"
fi
cookie=bench$RANDOM$RANDOM
ERL_FLAGS="$flags" escript "$(dirname "$0")/beam_cluster.escript" serve bench2@127.0.0.1 "$cookie" \
  >"$dir/serve.log" 2>&1 &
serve=$!
for _ in $(seq 100); do
  grep -q registered "$dir/serve.log" 2>/dev/null && break
  kill -0 "$serve" 2>/dev/null || { cat "$dir/serve.log" >&2; exit 1; }
  sleep 0.1
done
ERL_FLAGS="$flags" escript "$(dirname "$0")/beam_cluster.escript" connect bench1@127.0.0.1 "$cookie" \
  bench2@127.0.0.1
