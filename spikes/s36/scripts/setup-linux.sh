#!/bin/sh
# S36 on Linux, as root:
#   scripts/setup-linux.sh setup target/release/s36   # files, dirs, config
#   scripts/setup-linux.sh teardown                   # removes all of it
# Users are bare uids: the node is 59990, apps 60000-60999. Nothing is added
# to /etc/passwd.
set -eu
NODE=59990
CG=/sys/fs/cgroup/s36

teardown() {
    pkill -KILL -x s36 2>/dev/null || true
    for u in $NODE 60000 60001; do pkill -KILL -U $u 2>/dev/null || true; done
    sleep 0.2
    if [ -d $CG ]; then
        find $CG -mindepth 1 -depth -type d -exec rmdir {} \; 2>/dev/null || true
        rmdir $CG 2>/dev/null || true
    fi
    rm -rf /opt/s36 /etc/s36 /var/lib/s36
}

case "${1:-}" in
setup)
    bin=${2:?usage: $0 setup path/to/s36}
    teardown
    install -d -m 0755 /opt/s36
    install -m 0755 "$bin" /opt/s36/s36
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
cgroup = $CG
cgroup_controllers = ${CONTROLLERS:-+memory +pids +cpu}
roots = /var/lib/s36/roots
CONF
    chmod 0600 /etc/s36/helper.conf
    install -d -m 0755 /var/lib/s36
    install -d -m 0700 -o $NODE -g $NODE /var/lib/s36/node
    install -d -m 0711 /var/lib/s36/apps
    install -d -m 0700 /var/lib/s36/roots
    echo "ready: /opt/s36/s36, /etc/s36/helper.conf, /var/lib/s36"
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
