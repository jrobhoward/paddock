#!/usr/bin/env bash
# Build pong in Go, Python and JavaScript, each with its own toolchain, into
# target/ping-pong-languages/pong-<language>.wasm. A language whose toolchain
# is not found is skipped with a message. See README.md for the toolchains.
#
#   examples/ping-pong/languages/build.sh            # every language found
#   examples/ping-pong/languages/build.sh go js      # the named ones
#
# Each tool is found on PATH, or set by its variable: TINYGO, WIT_BINDGEN_GO,
# COMPONENTIZE_PY, JCO.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
OUT="$ROOT/target/ping-pong-languages"
TINYGO=${TINYGO:-tinygo}
WIT_BINDGEN_GO=${WIT_BINDGEN_GO:-wit-bindgen-go}
COMPONENTIZE_PY=${COMPONENTIZE_PY:-componentize-py}
JCO=${JCO:-jco}

missing() {
  local tool
  for tool in "$@"; do
    if ! command -v "$tool" > /dev/null 2>&1; then
      echo "$tool"
      return
    fi
  done
}

# TinyGo builds against a world that includes wasi:cli beside the app world
# (go/world.wit); its WIT comes from TinyGo's own copy. The bindings and the
# build happen in a copy under target/, so the source tree stays clean.
build_go() {
  local work="$OUT/go" tinygo_root
  tinygo_root=$("$TINYGO" env TINYGOROOT)
  rm -rf "$work"
  mkdir -p "$work/wit/deps/cli" "$work/wit/deps/paddock-runtime"
  cp -r "$tinygo_root"/lib/wasi-cli/wit/deps/* "$work/wit/deps/"
  cp "$tinygo_root"/lib/wasi-cli/wit/*.wit "$work/wit/deps/cli/"
  cp "$ROOT/wit/runtime.wit" "$work/wit/deps/paddock-runtime/"
  cp "$HERE/go/world.wit" "$work/wit/"
  cp "$HERE"/go/*.go "$HERE"/go/go.mod "$HERE"/go/go.sum "$work/"
  (
    cd "$work"
    "$WIT_BINDGEN_GO" generate --world pong:go/app --out gen wit > /dev/null
    # -scheduler=none: an app has no use for goroutines, and the default
    # (asyncify) costs garbage-collection pauses of milliseconds (spike S21).
    "$TINYGO" build -target=wasip2 --wit-package wit --wit-world app -opt=2 \
      -scheduler=none -o "$OUT/pong-go.wasm" .
  )
}

# componentize-py reads wit/runtime.wit as it is. It builds from a copy under
# target/, as it writes __pycache__ beside the source.
build_python() {
  local work="$OUT/python"
  rm -rf "$work"
  mkdir -p "$work"
  cp "$HERE/python/app.py" "$work/"
  (
    cd "$work"
    "$COMPONENTIZE_PY" -d "$ROOT/wit/runtime.wit" -w paddock:runtime/app \
      componentize app -o "$OUT/pong-python.wasm" > /dev/null
  )
}

# So does jco.
build_js() {
  "$JCO" componentize "$HERE/js/app.js" --wit "$ROOT/wit/runtime.wit" \
    --world-name paddock:runtime/app -o "$OUT/pong-js.wasm" > /dev/null
}

langs=${*:-go python js}
mkdir -p "$OUT"
for lang in $langs; do
  case $lang in
    go) tools=("$TINYGO" "$WIT_BINDGEN_GO") ;;
    python) tools=("$COMPONENTIZE_PY") ;;
    js) tools=("$JCO") ;;
    *)
      echo "unknown language: $lang (go, python or js)" >&2
      exit 2
      ;;
  esac
  absent=$(missing "${tools[@]}")
  if [ -n "$absent" ]; then
    echo "skipped $lang: $absent not found"
    rm -f "$OUT/pong-$lang.wasm"
    continue
  fi
  "build_$lang"
  echo "built   $lang: $OUT/pong-$lang.wasm ($(du -k "$OUT/pong-$lang.wasm" | cut -f1) KiB)"
done
