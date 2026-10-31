#!/usr/bin/env bash
# Builds the guest with tables of 100,000, 400,000 and 1,500,000 records
# (17, 67 and 252 MiB), and the 400,000-record table again with other data
# (version 2), into spikes/target/s27/. Run from spikes/.
set -euo pipefail
out=target/s27
mkdir -p "$out"
build() { # entries version
  S27_ENTRIES=$1 S27_VERSION=$2 CARGO_PROFILE_RELEASE_DEBUG=0 \
    cargo build --release -p s27-guest --target wasm32-wasip2 --quiet
  cp target/wasm32-wasip2/release/s27_guest.wasm "$out/guest-$1-v$2.wasm"
}
build 100000 1
build 400000 1
build 400000 2
build 1500000 1
cargo build --release -p s27-bench --quiet
ls -l "$out"
