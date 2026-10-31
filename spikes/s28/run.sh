#!/usr/bin/env bash
# Spike S28: build the guest and the bench, then run every phase, with
# read_concurrency on (the default) and the read phase with it off.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p s28-guest --target wasm32-wasip2
cargo build --release -p s28-bench
guest=target/wasm32-wasip2/release/s28_guest.wasm
uname -srm
nproc
uptime
target/release/s28-bench "$guest"
S28_RC=0 target/release/s28-bench "$guest" read
