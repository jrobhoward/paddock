#!/bin/sh
# Run S36 as root after setup: the helper starts the node, the node prints
# the report. Afterwards, no process of the node's or the apps' uids may be
# left (the node's death scenario). Extra arguments go to the node.
#   scripts/run.sh [--iters 200 --rtt 20000 --modes direct,uid,...] > report.md
set -u
/opt/s36/s36 helper /etc/s36/helper.conf -- node "$@"
sleep 0.5
echo "## Left after the node's death"
echo
left=$(ps -ax -o pid= -o uid= -o command= | awk '$2 == 59990 || ($2 >= 60000 && $2 <= 60999)')
if [ -z "$left" ]; then
    echo "No process of uid 59990 or 60000-60999 left."
else
    echo "Processes left (uid 59990 or 60000-60999; the uid modes' forked children are expected, see the node-side table):"
    echo
    echo '```'
    echo "$left"
    echo '```'
fi
# Leftovers hold this script's output open; end them now they are reported.
for u in 59990 60000 60001; do pkill -KILL -U $u 2>/dev/null; done
exit 0
