#!/bin/sh
# The privileged helper end to end on a Linux test machine (ROADMAP PS1),
# as root, never on a development machine: bulkhead-priv under systemd
# (examples/systemd/bulkhead-priv.service) starts the node, which deploys
# the board example; the app must run under a user of its own, in its own
# cgroup, PID namespace and minimal root. It installs a `bulkhead` user,
# binaries under /usr/local, /etc/bulkhead and the unit, and removes them
# all at the end unless KEEP=1.
#
#   cargo build -p bulkhead -p bulkhead-priv -p example-board   # as a user
#   cargo build -p bulkhead-native --bin bulkhead-runner
#   sudo sh scripts/priv-e2e-systemd.sh "$PWD"
set -eu
SRC=${1:?checkout}
T=$SRC/target/debug
id bulkhead >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin bulkhead
NUID=$(id -u bulkhead); NGID=$(id -g bulkhead)
install -m 0755 $T/bulkhead-priv /usr/local/sbin/bulkhead-priv
install -m 0755 $T/bulkhead /usr/local/bin/bulkhead
install -d -m 0755 /usr/local/libexec/bulkhead
install -m 0755 $T/bulkhead-runner /usr/local/libexec/bulkhead/bulkhead-runner
install -d -m 0755 /etc/bulkhead
systemctl stop bulkhead 2>/dev/null || true
rm -rf /var/lib/bulkhead
install -d -m 0700 -o bulkhead -g bulkhead /var/lib/bulkhead
W=/var/lib/bulkhead-e2e; rm -rf $W; install -d -m 0755 $W
cd $W
/usr/local/bin/bulkhead keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)
cp $SRC/examples/board/bulkhead.toml board.toml
/usr/local/bin/bulkhead bundle board.toml $T/example-board --key signer.key --out board.rbp >/dev/null
chmod 0644 board.rbp
cat > /etc/bulkhead/policy.toml <<'TOML'
[apps.board]
grant = ["bulkhead:http-routes", "bulkhead:keyvalue", "bulkhead:logging"]
buckets = ["board"]
TOML
cat > /etc/bulkhead/node.toml <<TOML
node = 1
data_dir = "/var/lib/bulkhead"
trusted_keys = ["$SIGNER"]
policy = "/etc/bulkhead/policy.toml"

[http.main]
address = "127.0.0.1:8080"

[buckets.board]
durability = "disk"

[priv]
TOML
cat > /etc/bulkhead/priv.conf <<CONF
node = /usr/local/bin/bulkhead
node_args = node --config /etc/bulkhead/node.toml
node_uid = $NUID
node_gid = $NGID
runner = /usr/local/libexec/bulkhead/bulkhead-runner
uids = 60000-60999
state = /var/lib/bulkhead-priv
binds = /var/lib/bulkhead
memory_max = 2GiB
pids_max = 1024
cgroup = /sys/fs/cgroup/system.slice/bulkhead.service
CONF
chmod 0600 /etc/bulkhead/priv.conf
install -m 0644 $SRC/examples/systemd/bulkhead-priv.service /etc/systemd/system/bulkhead.service
systemctl daemon-reload
systemctl restart bulkhead
for i in $(seq 100); do [ -S /var/lib/bulkhead/admin.sock ] && break; sleep 0.1; done
echo "== processes"
ps -eo user:12,uid,pid,ppid,args --forest | grep -E "bulkhead|example-board" | grep -v grep
cli() { runuser -u bulkhead -- /usr/local/bin/bulkhead --socket /var/lib/bulkhead/admin.sock "$@"; }
echo "== deploy"
cli deploy $W/board.rbp
for i in $(seq 100); do curl -sf http://127.0.0.1:8080/board >/dev/null && break; sleep 0.1; done
echo "== board"; curl -s http://127.0.0.1:8080/board; echo
curl -s -X POST http://127.0.0.1:8080/board >/dev/null
curl -s http://127.0.0.1:8080/board; echo
echo "== status"; cli status board
echo "== processes with the app"
ps -eo user:12,uid,gid,pid,args | grep -E "bulkhead-priv|bulkhead node|^60000" | grep -v grep
APP=$(pgrep -u 60000 | head -1)
echo "== the app's view"
echo "uid/gid: $(awk '/^Uid|^Gid/ {print $2}' /proc/$APP/status | tr '\n' ' ')"
echo "cgroup:  $(cat /proc/$APP/cgroup)"
echo "pid ns:  $(readlink /proc/$APP/ns/pid) (node: $(readlink /proc/$(pgrep -x bulkhead | head -1)/ns/pid))"
echo "root:    $(ls /proc/$APP/root/)"
echo "== what the app's user reaches (ROADMAP PS2)"
D=/var/lib/bulkhead
stat -c '%a %U:%G %n' $D $D/state.json $D/kv $D/bundles $D/compiled $D/compiled/board $D/priv
as_app() { setpriv --reuid=60000 --regid=60000 --clear-groups "$@"; }
for p in $D/state.json $D/kv $D/bundles/board; do
    if as_app test -r "$p"; then echo "READABLE by the app: $p"; else echo "refused to the app: $p"; fi
done
if as_app test -r $D/compiled/board -a -x $D/compiled/board; then echo "the app reaches its program's directory"; else echo "NOT REACHABLE by the app: its program's directory"; fi
echo "helper caps: $(grep -E 'CapEff|CapBnd' /proc/$(pgrep -x bulkhead-priv)/status | tr '\n' ' ')"
echo "node caps:   $(grep -E 'CapEff' /proc/$(pgrep -x bulkhead | head -1)/status)"
echo "== stop"
systemctl stop bulkhead
sleep 1
systemctl status bulkhead --no-pager | head -3 || true
echo "left: $(ps -eo uid= | awk '$1>=60000 && $1<=60999' | wc -l) app processes, $(pgrep -x bulkhead | wc -l) nodes"
journalctl -u bulkhead --no-pager -n 40 | grep -E "bulkhead-priv|error|ERROR|WARN" | tail -15
if [ "${KEEP:-0}" != 1 ]; then
    rm -f /etc/systemd/system/bulkhead.service && systemctl daemon-reload
    rm -f /usr/local/sbin/bulkhead-priv /usr/local/bin/bulkhead
    rm -rf /usr/local/libexec/bulkhead /etc/bulkhead /var/lib/bulkhead /var/lib/bulkhead-priv $W
    userdel bulkhead
    echo "== removed"
fi
