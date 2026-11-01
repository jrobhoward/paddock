#!/usr/bin/env bash
# Spike S30: build the app and the bench here and the real host's runner
# and fixture in the main workspace, then run every phase.
set -euo pipefail
cd "$(dirname "$0")/.."
# Two builds: `--bin` would filter the fixture's binary out of one.
(cd .. && cargo build --release -p bulkhead-native --bin bulkhead-runner)
(cd .. && cargo build --release -p bulkhead-fixture-native)
cargo build --release -p s30-app -p s30-bench
uname -srm
nproc
uptime
target/release/s30-bench target/release/s30-app "$@"
