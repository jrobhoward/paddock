#!/bin/sh
# S36 on FreeBSD, as root:
#   scripts/setup-freebsd.sh setup target/release/s36
#   scripts/setup-freebsd.sh teardown
# The shared jail root /var/s36-jail/root holds the program at its host path
# (/opt/s36/s36), its libraries and the run-time linker; each instance's root
# is a read-only nullfs view of it with a devfs (ruleset 4243: pvclock, null)
# and its app's directory mounted read-write. Users are bare uids: the node
# is 59990, apps 60000-60999.
set -eu
NODE=59990
BASE=/var/s36-jail
ROOT=$BASE/root
RS=4243

teardown() {
    pkill -KILL -x s36 2>/dev/null || true
    for u in $NODE 60000 60001; do pkill -KILL -U $u 2>/dev/null || true; done
    for j in $(jls -N name 2>/dev/null | grep '^s36-' || true); do jail -r "$j" 2>/dev/null || true; done
    sleep 0.3
    for m in $(mount -p | awk -v b="$BASE/" 'index($2, b) == 1 {print $2}' | sort -r); do umount -f "$m"; done
    rctl 2>/dev/null | grep '^jail:' | while read -r r; do rctl -r "${r%%=*}" 2>/dev/null || true; done
    if [ -d "$BASE" ]; then chflags -R noschg "$BASE"; rm -rf "$BASE"; fi
    devfs rule -s $RS delset 2>/dev/null || true
    rm -rf /opt/s36 /etc/s36 /var/lib/s36
}

case "${1:-}" in
setup)
    bin=${2:?usage: $0 setup path/to/s36}
    teardown
    install -d -m 0755 /opt/s36
    install -m 0755 "$bin" /opt/s36/s36
    mkdir -p "$ROOT/opt/s36" "$ROOT/libexec" "$ROOT/dev" "$ROOT/tmp" "$BASE/jails"
    install -m 0555 "$bin" "$ROOT/opt/s36/s36"
    install -m 0555 /libexec/ld-elf.so.1 "$ROOT/libexec/"
    for lib in $(ldd -f '%p\n' "$bin" | grep '^/'); do
        mkdir -p "$ROOT$(dirname "$lib")"
        install -m 0444 "$lib" "$ROOT$lib"
    done
    devfs rule -s $RS add hide
    devfs rule -s $RS add path pvclock unhide
    devfs rule -s $RS add path null unhide
    chmod 0755 "$BASE" "$ROOT"
    install -d -m 0700 /etc/s36
    cat > /etc/s36/helper.conf <<CONF
node_uid = $NODE
node_gid = $NODE
uids = 60000-60999
programs = /opt/s36
node_program = /opt/s36/s36
ports = 81
memory_max = 1073741824
pids_max = 256
apps = /var/lib/s36/apps
jail_root = $ROOT
jails = $BASE/jails
devfs_ruleset = $RS
CONF
    chmod 0600 /etc/s36/helper.conf
    install -d -m 0755 /var/lib/s36
    install -d -m 0700 -o $NODE -g $NODE /var/lib/s36/node
    install -d -m 0711 /var/lib/s36/apps
    echo "ready: /opt/s36/s36, /etc/s36/helper.conf, $ROOT"
    ;;
teardown)
    teardown
    echo "removed"
    ;;
*)
    echo "usage: $0 setup path/to/s36 | teardown" >&2
    exit 2
    ;;
esac
