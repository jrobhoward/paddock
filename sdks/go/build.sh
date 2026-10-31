#!/usr/bin/env bash
# Build a Go app on the Bulkhead Go SDK into a component.
#
#   sdks/go/build.sh <app-package-dir> <out.wasm>
#
# Passes TinyGo the SDK's WIT world (wit/world.wit: the app world, the
# services the SDK wraps and wasi:cli's imports) and -scheduler=none: the
# default scheduler (asyncify) gives garbage-collection pauses of 1 to 7 ms
# per call (spike S21), and an app, driven one callback at a time, has no
# use for goroutines. TinyGo is found on PATH, or set by TINYGO.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <app-package-dir> <out.wasm>" >&2
  exit 2
fi
SDK=$(cd "$(dirname "$0")" && pwd)
TINYGO=${TINYGO:-tinygo}
command -v "$TINYGO" > /dev/null || {
  echo "tinygo not found: install TinyGo 0.43.0 or set TINYGO" >&2
  exit 1
}
out=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
cd "$1"
"$TINYGO" build -target=wasip2 -scheduler=none -opt=2 \
  --wit-package "$SDK/wit" --wit-world bulkhead:go-sdk/app \
  -o "$out" .
