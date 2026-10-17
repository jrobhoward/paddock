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
control's routes there (`standby = "forward"`).

`crates/paddock/tests/scada.rs` runs the three on one node against a
fake device: polling, a long poll woken by a change, an alarm raised by a
write and acknowledged, the upgrade to version 2 and the rollback, and a
device going offline.

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

1. `control` upgraded to version 2 on the standby, then on the primary;
2. a planned switchover;
3. a rollback on both nodes;
4. an upgrade of the standby, then the primary killed before its turn,
   restarted, and upgraded.

A probe per node reads the point table and the alarms ten times a second.
The report gives, per step and node, the failed requests out of those made
and the longest time between point updates while the node answered, and
the alarm as the primary sees it after each step. Configs, data, logs,
the probes' lines and the report stay in `$DRILL_DIR` (a new temporary
directory unless set). Ports: `HTTP_PORT` (8091, 8092), `BASE_PORT`
(UDP 7501 to 7503) and `DEVICE_PORT` (5021, 5022).

Until replicated buckets (K2, K3) each node polls the devices into its own
`points` bucket, and `control`'s buckets are on the node it runs on.

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

X2 turns these into pass or fail criteria.
