#!/usr/bin/env bash
# Run Paddock's benchmarks and write a dated section for docs/BASELINE.md
# (ROADMAP B7). Each benchmark runs as its own process under GNU time, which
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
  "baseline|call, cast, instantiate, idle memory (1 and 4 workers, dense)|R/examples/baseline 1 1000 standard; R/examples/baseline 4 1000 standard; R/examples/baseline 1 5000 dense"
  "pool_load|concurrent clients against a pool (1, 4, 16 workers)|R/examples/pool_load 1; R/examples/pool_load 4; R/examples/pool_load 16"
  "epoch|epoch-check overhead|R/examples/epoch_cost"
  "log|guest log records|R/examples/log_cost"
  "admin|admin API latency (1 and 4 workers)|R/examples/admin_latency 1; R/examples/admin_latency 4"
  "watch|keyvalue writes with watchers, memory and disk buckets|R/examples/watch_cost 4; R/examples/watch_cost 4 disk"
  "publish|a publish to 1 to 1,000 group members|R/examples/publish_cost 4"
  "ingress|HTTP ingress throughput: requests as calls to a pool, and an HTTP handler's instance per request (E6)|R/examples/ingress_throughput 4 4; R/examples/ingress_throughput 4 4 handler"
  "batch|points per second between apps, one cast each or batched (F4)|R/examples/batch_cost 1; R/examples/batch_cost 4"
  "egress|outgoing HTTP from a guest through the pool|R/examples/egress_cost 1; R/examples/egress_cost 4; R/examples/egress_cost 4 nopool"
  "cluster|calls, casts and payloads between two nodes over QUIC|R/examples/cluster_throughput 4"
  "passivation|wake cost, instances per Store slot, random calls|R/examples/passivation_cost 4 1000 100000; R/examples/passivation_cost 4 100 2000; R/examples/passivation_cost 4 1000 1000 off"
  "modbus|Modbus points through the scada pool|R/examples/modbus_load 16 100 10 4 4; R/examples/modbus_load 64 125 1 4 4; R/examples/modbus_load 64 125 1 8 8"
  "s14|spike S14: storage in the guest and host services|S/s14-bench spikes/target/wasm32-wasip2/release/s14_guest.wasm"
  "s15|spike S15: HTTP in the guest and the host, and the connection sweep|S/s15-bench 4; S/s15-bench 4 sweep"
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
cargo build -q --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
cargo build -q --release --target wasm32-wasip2 -p paddock-fixture-sdk -p paddock-fixture-counter \
  -p paddock-fixture-http -p example-chaos -p example-scada-points
cargo build -q --release -p paddock-host --examples
cargo build -q --release -p paddock --example admin_latency
cargo build -q --release -p paddock-modbus --example modbus_load
if wanted s14 || wanted s15; then
  (cd spikes && cargo build -q --release --target wasm32-wasip2 -p s14-guest -p s15-proxy -p s15-client -p s15-tcpserver \
    && cargo build -q --release -p s14-bench -p s15-bench)
fi

mkdir -p target/bench
stamp=$(date -u +%Y-%m-%dT%H%MZ)
out=target/bench/$stamp.md
log=target/bench/$stamp.log
: >"$log"
cpu=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')
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
  echo "bench: $name"
  { echo; echo "### $name: $what"; } >>"$out"
  IFS=';' read -ra cmds <<<"$commands"
  for cmd in "${cmds[@]}"; do
    cmd=$(echo "$cmd" | sed 's/^ *//')
    shown=${cmd/#R\//target\/release\/}
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
