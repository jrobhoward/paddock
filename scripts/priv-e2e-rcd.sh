#!/bin/sh
# The privileged helper's rc.d script on a FreeBSD test machine (ROADMAP
# PS1), as root, never on a development machine: rc starts bulkhead-priv
# (examples/freebsd/bulkhead), which starts the node; status, reload and
# stop go through it. No app is deployed: bundles carry no FreeBSD program
# yet (ROADMAP FB6). It installs a `bulkhead` user, binaries under
# /usr/local and the rc script, and removes them all at the end unless
# KEEP=1.
#
#   cargo build -p bulkhead -p bulkhead-priv                   # as a user
#   cargo build -p bulkhead-native --bin bulkhead-runner
#   sh scripts/priv-e2e-rcd.sh "$PWD"                           # as root
set -eu
SRC=${1:?checkout}
T=$SRC/target/debug
id bulkhead >/dev/null 2>&1 || pw useradd bulkhead -d /nonexistent -s /usr/sbin/nologin -c "Bulkhead node"
NUID=$(id -u bulkhead); NGID=$(id -g bulkhead)
install -m 0755 $T/bulkhead-priv /usr/local/sbin/bulkhead-priv
install -m 0755 $T/bulkhead /usr/local/bin/bulkhead
install -d -m 0755 /usr/local/libexec/bulkhead
install -m 0755 $T/bulkhead-runner /usr/local/libexec/bulkhead/bulkhead-runner
install -d -m 0755 /usr/local/etc/bulkhead
service bulkhead onestop 2>/dev/null || true
rm -rf /var/db/bulkhead; install -d -m 0700 -o bulkhead -g bulkhead /var/db/bulkhead
cat > /usr/local/etc/bulkhead/node.toml <<TOML
node = 1
data_dir = "/var/db/bulkhead"

[native]
landlock_optional = true

[priv]
TOML
cat > /usr/local/etc/bulkhead/priv.conf <<CONF
node = /usr/local/bin/bulkhead
node_args = node --config /usr/local/etc/bulkhead/node.toml
node_uid = $NUID
node_gid = $NGID
runner = /usr/local/libexec/bulkhead/bulkhead-runner
uids = 60000-60999
state = /var/db/bulkhead-priv
binds = /var/db/bulkhead
memory_max = 2GiB
pids_max = 1024
CONF
chmod 0600 /usr/local/etc/bulkhead/priv.conf
install -m 0555 $SRC/examples/freebsd/bulkhead /usr/local/etc/rc.d/bulkhead
sysrc bulkhead_enable=YES >/dev/null
service bulkhead start
for i in $(seq 100); do [ -S /var/db/bulkhead/admin.sock ] && break; sleep 0.1; done
echo "== processes"
ps -ax -o user= -o pid= -o ppid= -o command= | grep -E "bulkhead" | grep -v -E "grep|ps1-rcd"
echo "== status"; service bulkhead status
echo "== what an app's user reaches of the node's (ROADMAP PS2; no app is deployed here)"
D=/var/db/bulkhead
for p in $D $D/state.json $D/kv $D/bundles $D/compiled $D/priv; do
    [ -e "$p" ] && stat -f '%Lp %Su:%Sg %N' "$p"
done
as_app() { /usr/sbin/chroot -u 60000 -g 60000 / "$@"; }
for p in $D/state.json $D/kv $D/bundles $D/creation; do
    [ -e "$p" ] || continue
    if as_app test -r "$p"; then echo "READABLE by the app: $p"; else echo "refused to the app: $p"; fi
done
echo "== admin as the node's user"
su -m bulkhead -c "/usr/local/bin/bulkhead --socket /var/db/bulkhead/admin.sock list" || true
echo "== reload"; service bulkhead reload; sleep 1
echo "== stop"; service bulkhead stop; sleep 1
echo "left: $(pgrep -x bulkhead | wc -l | tr -d ' ') nodes, $(pgrep -x bulkhead-priv | wc -l | tr -d ' ') helpers"
echo "== log"; tail -12 /var/log/bulkhead.log
sysrc -x bulkhead_enable
if [ "${KEEP:-0}" != 1 ]; then
    rm -f /usr/local/etc/rc.d/bulkhead /usr/local/sbin/bulkhead-priv /usr/local/bin/bulkhead /var/log/bulkhead.log
    rm -rf /usr/local/libexec/bulkhead /usr/local/etc/bulkhead /var/db/bulkhead /var/db/bulkhead-priv
    pw userdel bulkhead
    echo "== removed"
fi
