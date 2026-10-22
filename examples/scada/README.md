# SCADA reference app

Phase 3's workload (ROADMAP X1): a small SCADA system on Paddock, fed by
the built-in Modbus TCP extension (`paddock node` built with the `modbus`
feature). Three apps:

| App | Placement | What it does |
| --- | --- | --- |
| `points` | a pool of 4, `hash:device` | Takes each device's updates from the Modbus extension and writes them into the node's shared `points` bucket: `<device>/<table>/<address>` and `<device>/status`. |
| `hmi` | a pool of 2 | Serves the table over HTTP, `GET /scada/points`, and holds a long poll (`?after=<revision>&wait_ms=<n>`) until the bucket changes. |
| `control` | singleton | Watches the table and raises alarms from it (rules in its `rules` config, and `<device>-offline`), takes acknowledgements and device writes, and keeps an event log, in disk buckets of its own. Version 2 (`paddock-v2.toml`, `--features v2`) adds a field and migrates version 1's snapshot; version 1 reads version 2's, so a rollback keeps the alarms. |

Each module's doc comment lists its routes and replies. On an HA pair the
apps are deployed to both nodes. `points` and `hmi` run on both;
`control` runs on the primary, and the standby's HTTP listener forwards
control's routes there (`standby = "forward"`). The `points` bucket is a
`cluster` bucket (`scope = "cluster"`, on disk) and each device has
`primary_only = true`, so the primary alone polls and writes the table and
the standby's copy comes from it. `control`'s buckets are `replicated`, so
it needs `[failover] replication = true` on both nodes; its alarms and
event log are on both, and a new primary's `control` starts with them.

`crates/paddock/tests/scada.rs` runs the three on one node against a
fake device (`control`'s buckets on disk there, since one node has no
replication): polling, a long poll woken by a change, an alarm raised by a
write and acknowledged, the upgrade to version 2 and the rollback, and a
device going offline. A second test runs them on a pair and a witness: the
standby does not poll, its table comes from the primary, and an
acknowledged alarm is still acknowledged after a planned switchover.

## The release drill

```bash
examples/scada/drill.sh        # from the repository root
```

The script builds `paddock` with `modbus`, the apps (both versions of
`control`) and `fake_device` (`crates/paddock-modbus/examples`: two
devices whose register 0 counts up every 100 ms), and starts a witness and
a pair as three `paddock node` processes, with the timings of
`examples/failover` (200 ms heartbeats, 1 s node timeout, 2 s lease). It
deploys the apps to both nodes, raises and acknowledges an alarm, and then
runs a release cycle, `STEP_SECS` (5) apart:

1. `control` released at version 2 across the pair (`paddock release
   upgrade --pair pair.toml`: the standby, then the primary in place);
2. a planned switchover;
3. a release rollback across the pair;
4. a release of the standby alone (`--standby-only`), then the primary
   killed before its step, restarted as the standby, and the release run
   again to finish.

A probe per node reads the point table and the alarms ten times a second.
The report gives, per step and node, the failed requests out of those made
and the longest time between point updates while the node answered, and
the alarm as the primary sees it after each step. Configs, data, logs,
the probes' lines and the report stay in `$DRILL_DIR` (a new temporary
directory unless set). Ports: `HTTP_PORT` (8091, 8092), `BASE_PORT`
(UDP 7501 to 7503) and `DEVICE_PORT` (5021, 5022).

## First run

On 2026-10-16 (16-core Linux machine, idle), the drill showed:

- Point updates continued through every step on the node that was up: the
  longest gap was 121 ms, about the poll period.
- Control's routes failed for about 2.5 s on both nodes during the
  switchover, and on the surviving node when the primary was killed: the
  singleton stops on one node and starts from `init` on the other (M1
  moves it with its state).
- The acknowledged alarm came back `active` after the switchover, from the
  new primary's own buckets, and `acked` again after the failover back,
  from that node's older copy (K3 replicates them).
- Snapshot upgrades and rollbacks of `control` failed no requests.

## Second run: the table and the alarms on replicated buckets

On 2026-10-20 (same machine, idle), with the point table in a `cluster`
bucket polled by the primary alone and `control`'s buckets `replicated`:

- The acknowledged alarm stayed `acked` after every step, the switchover
  and the failover included.
- Point updates stopped for about 3 s at the switchover (3,064 ms on node
  1, 3,059 ms on node 2) and 3.8 s at the failover, on both nodes: no node
  polls, and no write to the table commits, until the new primary holds
  its lease. The switchover's hand-over took 2.7 s in the node logs (the
  old primary stops at once; the new one is granted after the 2 s lease).
  The first run, with each node polling into its own table, showed 121 ms.
- Control's routes failed for the same window as before, about 2.8 s at
  the switchover.
- In steady steps the longest gap was 115 to 231 ms on either node, the
  standby's table trailing the primary's by about a poll.
- Run again the same day with singletons moved with their state (ROADMAP
  M1): at the switchover `control` handed its state over (263 bytes) and
  started on node 2 from it, not from `init`; the gaps and failures were
  as above (3,069 and 3,058 ms; 28 alarm requests failed on each node).
- With the lease released at the witness in a switchover (ROADMAP L1),
  run the same night: the switchover's longest point gap fell to 572 ms
  on node 1 and 571 ms on node 2, and 6 and 5 alarm requests failed,
  against about 3 s and 28. The failover (the primary killed) is
  unchanged at 3.6 s: it still waits the node timeout and the lease.

## Third run: the cycle through `paddock release`

On 2026-10-21 (same machine, load average about 1.4 from other work), with
the upgrades and the rollback run by `paddock release` across the pair
(ROADMAP U1) instead of a command per node:

- Each release did the standby (a bundle swap of the held singleton),
  then the primary in place by snapshot, and failed no requests; the
  longest point gap in those steps was 116 to 232 ms.
- The switchover: 684 ms on node 1 and 572 ms on node 2, 6 alarm requests
  failed on each, as in the second run's last pass.
- The primary killed after the standby's step: node 2's probes failed
  throughout, and node 1's point table stopped for 3,776 ms and its alarm
  requests failed 24 times, until it held the lease. The rerun after node
  2 came back upgraded node 2 alone, now the standby, and was done.
- When node 2 rejoined, the point table stopped for about 570 ms on both
  nodes, before the release ran: the primary held its replication sends
  for the 500 ms resend timer after the standby caught up (CHANGELOG,
  Fixed). With the fix, a standby killed and restarted on the same setup
  left a longest gap of 115 ms on the primary at its return (685 ms
  before). Killing the standby still stops the table for about 1.1 s, the
  wait before the primary commits without it.
- The acknowledged alarm stayed `acked` after every step.

## Under faults: `paddock-soak scada`

`paddock-soak scada` (`scripts/soak.sh --only scada`, ROADMAP X2) runs
the same apps on the failover soak's pair and witness, behind proxies
that cut, hold and drop the links, with nodes killed and stopped, and
between episodes of faults releases `control` across the pair and
switches over. Its pass rule: on planned steps no failed point read, no
point gap of 1 s or more, and failed `control` requests only within a
switchover's hand-over (1 s); after every heal one node runs `control`,
the acknowledged alarm is `acked`, and the release converges. Under
faults it records failed requests and gaps without judging them.

On 2026-10-22 (same machine, idle), an hour with seed 2026 passed: 136
windows, 79 of them planned, and 58 heals, with one node running
`control` after each and no overlap.

- The 45 switchovers left point gaps of 505 to 813 ms (627 ms on
  average, taking the longer node of each), and `control`'s requests
  failed for at most 713 ms.
- The 34 releases failed no request; their longest point gap was 102 to
  204 ms.
- The primary lost in the middle of a release (ten times) stopped the
  surviving node's point table for about 3.5 to 3.8 s and failed about 25
  of its alarm requests, until it held the lease, as in X1's third run.
- Under faults, 23,158 of 119,781 requests failed, and the longest point
  gap was 31 s.

The first hour, the day before, failed on switchovers: the hand-over grew
from about 400 ms to past 1 s over the hour, with the replicated log.
A new primary sent nothing while its log was longer than its send window
and then resent the whole log (CHANGELOG, Fixed).
