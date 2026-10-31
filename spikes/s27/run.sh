#!/usr/bin/env bash
# Every S27 measurement, after build.sh. Run from spikes/ on an idle machine.
set -euo pipefail
b=target/release/s27-bench
g=target/s27
uname -srm; nproc; uptime
for n in 100000 400000 1500000; do
  echo; echo "== $n records, through the compile cache (as the node loads)"
  "$b" "$g/guest-$n-v1.wasm"
  echo; echo "== $n records, read from that cache by a process that never compiled"
  S27_REUSE=1 "$b" "$g/guest-$n-v1.wasm" | grep -E "component|loaded|  1 instances"
done
echo; echo "== upgrade, 400000 records: to the version read back from the cache"
"$b" "$g/guest-400000-v1.wasm" "$g/guest-400000-v2.wasm" | sed -n '/v2 compiled/,$p'
echo; echo "== upgrade, 400000 records: to the version as compiled (a node's first deploy)"
S27_V2_FIRST=1 "$b" "$g/guest-400000-v1.wasm" "$g/guest-400000-v2.wasm" | sed -n '/v2 compiled/,$p'
echo; echo "== 400000 records without the cache (memfd image)"
S27_MEMFD=1 "$b" "$g/guest-400000-v1.wasm"
