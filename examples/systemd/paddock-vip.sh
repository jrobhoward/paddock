#!/bin/sh
# Move a virtual IP address to the node of an HA pair whose singletons may
# run (docs/OPERATIONS.md, "A virtual IP that follows the primary"). Runs
# beside `paddock node` on each node of the pair, as its own service with
# CAP_NET_ADMIN and CAP_NET_RAW (paddock-vip.service); the node itself gets
# neither.
#
#   paddock-vip.sh <address/prefix> <interface>
#   paddock-vip.sh 10.0.0.10/24 eth0
#
# It follows `paddock failover --watch --json`: the address is added (with
# a gratuitous ARP, so neighbours update their caches) when a line says
# "may_run":true, and removed on any other line, including
# "role":"unreachable" when the node is gone. Not tested in CI: it needs the
# capabilities above.
set -u
address=${1:?usage: paddock-vip.sh <address/prefix> <interface>}
interface=${2:?usage: paddock-vip.sh <address/prefix> <interface>}
ip=${address%/*}

release() { ip addr del "$address" dev "$interface" 2>/dev/null || true; }
trap 'release; exit 0' INT TERM
release

paddock --json failover --watch | while read -r line; do
  case "$line" in
    *'"may_run":true'*)
      echo "paddock-vip: $line: taking $address"
      ip addr replace "$address" dev "$interface"
      arping -q -U -c 3 -I "$interface" "$ip" || true
      ;;
    *)
      echo "paddock-vip: $line: releasing $address"
      release
      ;;
  esac
done
# The watch ended (the node said this node has no [failover]): hold nothing.
release
exit 1
