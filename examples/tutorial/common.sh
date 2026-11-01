# Shared by the tutorial chapters; sourced, not run.
#
# Sets ROOT, DEMO_DIR, BULKHEAD, BOARD_DIR and BOARD_PROGRAM, builds bulkhead
# and the board example, and makes a signing key. BOARD=native builds the
# same Rust source as a native program (Linux) with bulkhead-runner beside
# bulkhead, and bundles it with `kind = "native-process"`; BOARD=go uses the
# Go build (examples/board-go, on the Go SDK; needs TinyGo, on PATH or in
# TINYGO). Defines:
#   sock N           the admin socket of node N
#   p N ARGS...      run the bulkhead CLI against node N
#   start_node N     start `bulkhead node` on $DEMO_DIR/node-N.toml
#   until_ok SECS CMD...   retry CMD until it succeeds
#   pause TEXT       wait for Enter (skipped with AUTO=1)
#   show CMD...      print a command, then run it
#   showp N ARGS...  the same for `bulkhead ARGS` against node N
#   deploy_board N MANIFEST [EXTRA]   sign and deploy a board manifest
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
DEMO_DIR=${DEMO_DIR:-$(mktemp -d -t bulkhead-tutorial-XXXXXX)}
BULKHEAD="$ROOT/target/release/bulkhead"
AUTO=${AUTO:-0}

echo "building (release; the first build takes a few minutes)..."
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p bulkhead
BOARD_KIND=
case "${BOARD:-rust}" in
  rust)
    BOARD_DIR="$ROOT/examples/board"
    BOARD_PROGRAM="$ROOT/target/wasm32-wasip2/release/example_board.wasm"
    cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
      --target wasm32-wasip2 -p example-board
    ;;
  native)
    BOARD_DIR="$ROOT/examples/board"
    BOARD_PROGRAM="$ROOT/target/release/example-board"
    BOARD_KIND='[appserver]
kind = "native-process"
native = { program = "executable" }'
    # The node runs native apps through the bulkhead-runner beside it.
    cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
      -p bulkhead-native --bin bulkhead-runner
    cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
      -p example-board --features native --bin example-board
    ;;
  go)
    BOARD_DIR="$ROOT/examples/board-go"
    BOARD_PROGRAM="$ROOT/target/tutorial-board-go/board.wasm"
    mkdir -p "$(dirname "$BOARD_PROGRAM")"
    "$ROOT/sdks/go/build.sh" "$BOARD_DIR" "$BOARD_PROGRAM"
    ;;
  *)
    echo "BOARD is rust, native or go, not $BOARD" >&2
    exit 2
    ;;
esac

cd "$DEMO_DIR"
[ -f signer.key ] || "$BULKHEAD" keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)

# Unix socket paths are limited to about 107 bytes; keep these short.
sock() { echo "/tmp/bulkhead-tut-$$-$1.sock"; }
p() {
  local n=$1
  shift
  "$BULKHEAD" --socket "$(sock "$n")" "$@"
}

declare -A NODE_PIDS
start_node() {
  "$BULKHEAD" node --config "$DEMO_DIR/node-$1.toml" >> "$DEMO_DIR/node-$1.log" 2>&1 &
  NODE_PIDS[$1]=$!
  for _ in $(seq 200); do [ -S "$(sock "$1")" ] && return; sleep 0.05; done
  echo "node $1 did not start; see $DEMO_DIR/node-$1.log" >&2
  exit 1
}
cleanup() {
  # Also nodes the reader started by hand from these configs.
  pkill -f "bulkhead node --config $DEMO_DIR/" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -f /tmp/bulkhead-tut-$$-*.sock
  echo
  echo "nodes stopped; configs, data and logs are in $DEMO_DIR"
}
trap cleanup EXIT

until_ok() {
  local deadline=$(($(date +%s) + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "timed out waiting for: $*" >&2
      exit 1
    fi
    sleep 0.05
  done
}

pause() {
  echo
  if [ "$AUTO" = 1 ]; then
    echo "-- $1"
  else
    read -r -p "-- $1 [Enter] "
  fi
}

# Show a command, then run it.
show() {
  echo "\$ $*"
  "$@"
  echo
}

# Show a bulkhead command against node N, then run it.
showp() {
  local n=$1
  shift
  echo "\$ bulkhead $*        # node $n"
  p "$n" "$@"
  echo
}

# Sign a board manifest (with extra TOML appended, and the native build's
# kind) and deploy it to node N.
deploy_board() {
  local n=$1 manifest=$2 extra=${3:-}
  local name
  name=$(basename "$manifest" .toml)
  cp "$manifest" "$name.toml"
  [ -n "$extra" ] && printf '\n%s\n' "$extra" >> "$name.toml"
  [ -n "$BOARD_KIND" ] && printf '\n%s\n' "$BOARD_KIND" >> "$name.toml"
  "$BULKHEAD" bundle "$name.toml" "$BOARD_PROGRAM" --key signer.key --out "$name.rbp" >/dev/null
  p "$n" deploy "$name.rbp" >/dev/null
}
