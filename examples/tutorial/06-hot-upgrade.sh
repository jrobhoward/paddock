#!/usr/bin/env bash
# Tutorial chapter 6: a hot upgrade. The counter example in three versions
# on one node: v1 to v2 with the count kept and the pid unchanged, a broken
# v3 reverted, a rollback refused, and `rollback --restart`. See
# examples/tutorial/README.md.
#
#   examples/tutorial/06-hot-upgrade.sh          # pauses between steps
#   AUTO=1 examples/tutorial/06-hot-upgrade.sh   # runs straight through
source "$(dirname "$0")/common.sh"

# One crate, three versions by cargo feature; each built into its own
# target directory so the three components sit side by side.
for v in 1 2 3; do
  features=""
  manifest="$ROOT/examples/counter/paddock.toml"
  if [ "$v" != 1 ]; then
    features="--features v$v"
    manifest="$ROOT/examples/counter/paddock-v$v.toml"
  fi
  # shellcheck disable=SC2086
  cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
    --target wasm32-wasip2 -p example-counter $features \
    --target-dir "$ROOT/target/tutorial-counter-v$v"
  "$PADDOCK" bundle "$manifest" \
    "$ROOT/target/tutorial-counter-v$v/wasm32-wasip2/release/example_counter.wasm" \
    --key signer.key --out "counter-$v.rbp" >/dev/null
done

cat > node-1.toml <<TOML
node = 1
data_dir = "$DEMO_DIR/data-1"
admin_socket = "$(sock 1)"
trusted_keys = ["$SIGNER"]
TOML

start_node 1
p 1 deploy counter-1.rbp >/dev/null
pid() { p 1 --json ps | grep -o '"pid": *"[^"]*"' | head -1 | cut -d'"' -f4; }

cat <<EOF

== Chapter 6: a hot upgrade
A node is running (process ${NODE_PIDS[1]}) with the counter example, version 1:
an instance that keeps a count in its own memory. Bundles of three
versions are in $DEMO_DIR:
  counter-1.rbp  keeps a count
  counter-2.rbp  also keeps the last three values; reads version 1's state
  counter-3.rbp  version 2, but refuses every snapshot: an upgrade that fails

From another terminal:
  export PATH="$(dirname "$PADDOCK"):\$PATH" PADDOCK_SOCKET=$(sock 1)
  cd $DEMO_DIR
  paddock call counter inc
  paddock call counter get
  paddock ps
  paddock state <pid>         # the instance's state, through save-snapshot
  paddock upgrade counter counter-2.rbp
EOF

pause "count to 3"
for _ in 1 2 3; do showp 1 call counter inc; done
showp 1 call counter get
showp 1 ps
showp 1 state "$(pid)"

pause "upgrade to version 2"
before=$(pid)
showp 1 upgrade counter counter-2.rbp
showp 1 call counter get
showp 1 call counter inc
showp 1 call counter get
echo "The count carried over, and the instance is the same: $before before,"
echo "$(pid) now. The node paused its mailbox between two messages, took"
echo "version 1's save-snapshot, started version 2 with load-snapshot, and"
echo "resumed; the pid, its registered name and anything queued stayed."

pause "upgrade to version 3, which refuses the snapshot"
showp 1 upgrade counter counter-3.rbp || true
showp 1 call counter get
echo "Version 3's load-snapshot failed, so the node kept version 2 running,"
echo "state and all. Nothing was lost; the upgrade just did not happen."

pause "roll back to version 1"
showp 1 rollback counter || true
showp 1 call counter get
cat <<'EOF'
A rollback is an upgrade to the previous version, so version 1 would have
to read version 2's snapshot, the downgrade half of OTP's code_change.
It cannot (it writes format 1, version 2 format 2), so the node refused
and kept version 2.
EOF

pause "roll back anyway, by restart"
showp 1 rollback counter --restart
showp 1 call counter get
showp 1 status counter
echo "Version 1 again, from init: the count is gone. Losing state is always"
echo "the operator's explicit choice (--restart), and the audit log says so:"
show grep -o '"event":"rollback"[^}]*"restart_requested":true' "$DEMO_DIR/data-1/audit.log"

if [ "$AUTO" != 1 ]; then
  echo
  echo "The node keeps running until Ctrl-C. Try the commands above."
  wait "${NODE_PIDS[1]}"
fi
