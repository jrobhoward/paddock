# Shared by the tutorial chapters; sourced, not run.
#
# Sets ROOT, DEMO_DIR, PADDOCK and BOARD_WASM, builds paddock and the board
# example, and makes a signing key. Defines:
#   sock N           the admin socket of node N
#   p N ARGS...      run the paddock CLI against node N
#   start_node N     start `paddock node` on $DEMO_DIR/node-N.toml
#   until_ok SECS CMD...   retry CMD until it succeeds
#   pause TEXT       wait for Enter (skipped with AUTO=1)
#   show CMD...      print a command, then run it
#   showp N ARGS...  the same for `paddock ARGS` against node N
#   deploy_board N MANIFEST [EXTRA]   sign and deploy a board manifest
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
DEMO_DIR=${DEMO_DIR:-$(mktemp -d -t paddock-tutorial-XXXXXX)}
PADDOCK="$ROOT/target/release/paddock"
BOARD_WASM="$ROOT/target/wasm32-wasip2/release/example_board.wasm"
AUTO=${AUTO:-0}

echo "building (release; the first build takes a few minutes)..."
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p paddock
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
  --target wasm32-wasip2 -p example-board

cd "$DEMO_DIR"
[ -f signer.key ] || "$PADDOCK" keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)

# Unix socket paths are limited to about 107 bytes; keep these short.
sock() { echo "/tmp/paddock-tut-$$-$1.sock"; }
p() {
  local n=$1
  shift
  "$PADDOCK" --socket "$(sock "$n")" "$@"
}

declare -A NODE_PIDS
start_node() {
  "$PADDOCK" node --config "$DEMO_DIR/node-$1.toml" >> "$DEMO_DIR/node-$1.log" 2>&1 &
  NODE_PIDS[$1]=$!
  for _ in $(seq 200); do [ -S "$(sock "$1")" ] && return; sleep 0.05; done
  echo "node $1 did not start; see $DEMO_DIR/node-$1.log" >&2
  exit 1
}
cleanup() {
  # Also nodes the reader started by hand from these configs.
  pkill -f "paddock node --config $DEMO_DIR/" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -f /tmp/paddock-tut-$$-*.sock
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

# Show a paddock command against node N, then run it.
showp() {
  local n=$1
  shift
  echo "\$ paddock $*        # node $n"
  p "$n" "$@"
  echo
}

# Sign a board manifest (with extra TOML appended) and deploy it to node N.
deploy_board() {
  local n=$1 manifest=$2 extra=${3:-}
  local name
  name=$(basename "$manifest" .toml)
  cp "$manifest" "$name.toml"
  [ -n "$extra" ] && printf '\n%s\n' "$extra" >> "$name.toml"
  "$PADDOCK" bundle "$name.toml" "$BOARD_WASM" --key signer.key --out "$name.rbp" >/dev/null
  p "$n" deploy "$name.rbp" >/dev/null
}
