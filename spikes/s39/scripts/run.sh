#!/bin/sh
# S39's runs on this machine: every mechanism with the callback thread
# idle (20,000 operations a thread), then busy with 1 ms callbacks (2,000,
# since a queue drained between callbacks waits a callback or two per
# operation). Two rounds. Usage: scripts/run.sh <results file>
set -eu
cd "$(dirname "$0")/.."
out=$1
bin=target/release/s39
{
  echo "# $(uname -srm), $(nproc) cpus, $(date -Iseconds)"
  echo "# load at start: $(cut -d' ' -f1-3 /proc/loadavg)"
  $bin bench --rounds 2 --busy 0 --n 20000 --warm 2000
  echo "# busy pass, load: $(cut -d' ' -f1-3 /proc/loadavg)"
  $bin bench --rounds 2 --busy 1000 --mechs m1,m2,m2r,m3 --n 2000 --warm 200
  echo "# end, load: $(cut -d' ' -f1-3 /proc/loadavg)"
} | tee "$out"
