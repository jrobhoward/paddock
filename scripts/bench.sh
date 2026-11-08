#!/usr/bin/env bash
# Run Bulkhead's benchmarks and write a dated section for docs/BASELINE.md.
# Each benchmark runs as its own process under GNU time, which
# adds its wall time, CPU time and peak resident memory (from the kernel's
# rusage; no root). Benchmarks run one at a time, on an idle machine: the
# script refuses to start while the load average is above BENCH_MAX_LOAD
# (default 1.5) unless --busy, which marks every number provisional.
#
#   scripts/bench.sh                 # every benchmark
#   scripts/bench.sh --list          # their names
#   scripts/bench.sh baseline watch  # only those
#   scripts/bench.sh --busy ...      # on a busy machine, marked so
#
# Output: target/bench/<UTC date>.md (the section) and a log beside it.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

# name | what it measures | command (run from the repo root)
BENCHES=(
  "cluster|calls, casts and payloads between two nodes over QUIC, from the host and from instances, and bare quinn's floor|R/examples/cluster_throughput 4; R/examples/quic_floor 4"
  "cluster-pair|cluster with the two nodes as two processes on this machine|scripts/cluster_pair.sh 4"
  "kv|keyvalue memory buckets from a native program, each operation a request to the node: single operations, ordered reads, 1 to 16 instances on one bucket, bytes per entry (1 and 4 workers)|R/examples/kv_cost 1; R/examples/kv_cost 4"
  "kv-rc|kv with every bucket read_concurrency (reads without a lock, spike S26), 4 workers|KV_READ_CONCURRENCY=1 R/examples/kv_cost 4"
  "native|native-process instances through bulkhead-runner: calls from the host and between instances, requests from a callback, casts, start (4 workers)|R/examples/native_cost 4"
  "beam-cluster|BEAM reference for cluster: two Erlang nodes as two processes on this machine, the same runs over distributed Erlang, plain TCP then TLS (4 schedulers)|scripts/beam_cluster_pair.sh; scripts/beam_cluster_pair.sh tls"
  "beam-ets|ETS reference for kv: the same loops in an Erlang process against set and ordered_set tables (1 and 4 schedulers)|ERL_FLAGS='+S 1:1' escript scripts/beam_ets.escript; ERL_FLAGS='+S 4:4' escript scripts/beam_ets.escript"
  "beam|Erlang/OTP reference: gen_server call, cast, spawn, process memory, send_after lateness (1 and 4 schedulers)|ERL_FLAGS='+S 1:1' escript scripts/beam_baseline.escript; ERL_FLAGS='+S 4:4' escript scripts/beam_baseline.escript"
)

busy=0 list=0 selected=()
for arg in "$@"; do
  case "$arg" in
    --busy) busy=1 ;;
    --list) list=1 ;;
    -h | --help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "bench: unknown option $arg" >&2; exit 2 ;;
    *) selected+=("$arg") ;;
  esac
done
if [ "$list" = 1 ]; then
  for b in "${BENCHES[@]}"; do IFS='|' read -r name what _ <<<"$b"; printf '%-12s %s\n' "$name" "$what"; done
  exit 0
fi
wanted() {
  [ "${#selected[@]}" = 0 ] && return 0
  local s; for s in "${selected[@]}"; do [ "$s" = "$1" ] && return 0; done
  return 1
}
for s in "${selected[@]}"; do
  printf '%s\n' "${BENCHES[@]}" | cut -d'|' -f1 | grep -qx "$s" || { echo "bench: no benchmark '$s' (--list)" >&2; exit 2; }
done

load=$(cut -d' ' -f1 /proc/loadavg)
max=${BENCH_MAX_LOAD:-1.5}
if awk -v l="$load" -v m="$max" 'BEGIN { exit !(l > m) }'; then
  if [ "$busy" = 0 ]; then
    echo "bench: load average $load is above $max; wait for an idle machine, or --busy to mark the numbers provisional" >&2
    exit 1
  fi
fi
[ -x /usr/bin/time ] || { echo "bench: needs GNU time at /usr/bin/time" >&2; exit 1; }

echo "bench: building (release)"
cargo build -q --release -p bulkhead-host --examples
cargo build -q --release -p bulkhead-native --bin bulkhead-runner
cargo build -q --release -p bulkhead-fixture-native
cargo build -q --release -p bulkhead-fixture-kvbench --bin bulkhead-fixture-kvbench
cargo build -q --release -p bulkhead-fixture-msgbench --bin bulkhead-fixture-msgbench

mkdir -p target/bench
stamp=$(date -u +%Y-%m-%dT%H%MZ)
out=target/bench/$stamp.md
log=target/bench/$stamp.log
: >"$log"
cpu=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //' || true)
# arm64 has no `model name`: the board's name and lscpu's core model.
if [ -z "$cpu" ]; then
  board=$(tr -d '\0' 2>/dev/null </proc/device-tree/model || true)
  core=$(lscpu 2>/dev/null | awk -F: '/^Model name/ { sub(/^ +/, "", $2); print $2; exit }')
  cpu="${board:-unknown board}${core:+, $core}"
fi
mem=$(awk '/MemTotal/ { printf "%.0f GiB", $2 / 1048576 }' /proc/meminfo)
{
  echo "## $(date -u +%Y-%m-%d): benchmark suite (scripts/bench.sh)"
  echo
  echo "$cpu, $(nproc) threads, $mem, Linux $(uname -r); $(rustc --version | cut -d' ' -f1-2); commit $(git rev-parse --short HEAD)$(git diff --quiet HEAD -- crates || echo ' with local changes'); load average $load at the start."
  if [ "$busy" = 1 ]; then
    echo
    echo "Provisional: run with --busy, on a machine with load average $load."
  fi
} >"$out"

for b in "${BENCHES[@]}"; do
  IFS='|' read -r name what commands <<<"$b"
  wanted "$name" || continue
  if [[ "$name" = beam* ]] && ! command -v escript >/dev/null; then
    echo "bench: $name skipped (no escript on the PATH)" >&2
    continue
  fi
  echo "bench: $name"
  { echo; echo "### $name: $what"; } >>"$out"
  IFS=';' read -ra cmds <<<"$commands"
  for cmd in "${cmds[@]}"; do
    cmd=$(echo "$cmd" | sed 's/^ *//')
    shown=${cmd/#R\//target\/release\/}
    # After a variable assignment too (`KV_NATIVE=1 R/...`).
    shown=${shown// R\// target\/release\/}
    shown=${shown/#S\//spikes\/target\/release\/}
    run=${shown}
    dir=$ROOT
    case "$cmd" in S/*) dir=$ROOT/spikes; run=${cmd/#S\//target\/release\/}; run=${run//spikes\//} ;; esac
    echo "\$ $shown" >>"$log"
    result=$( (cd "$dir" && /usr/bin/time -f '%e s wall, %U s user, %S s system, %M KiB peak RSS' bash -c "$run") 2>&1 | tee -a "$log") || {
      echo "bench: $name failed: $shown (see $log)" >&2
      { echo; echo "\`$shown\` failed."; } >>"$out"
      continue
    }
    usage=$(echo "$result" | tail -1)
    {
      echo
      echo "\`$shown\` ($usage):"
      echo
      echo '```text'
      echo "$result" | sed '$d' | grep -v '^\s*$'
      echo '```'
    } >>"$out"
  done
done
echo "bench: wrote $out"
