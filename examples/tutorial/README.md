# Tutorial: state that survives failures

One small app, a counter on a bulletin board, run through the failures
Paddock handles: a crash, a node restart, and the loss of a whole node,
with its state kept in four places; then a counter that keeps its state in
memory, carried through a hot upgrade.
Each chapter is a script that starts the nodes, prints the commands to
try, and pauses between steps; it stops everything at the end or on
Ctrl-C. Run the scripts from the repository root. They need the
`wasm32-wasip2` target and `curl`, and the first build takes a few
minutes.

```bash
examples/tutorial/01-one-node.sh
examples/tutorial/02-failover.sh
examples/tutorial/03-hot-standby.sh
examples/tutorial/04-database.sh             # needs Postgres's server programs
examples/tutorial/05-replicated.sh
examples/tutorial/06-hot-upgrade.sh
AUTO=1 examples/tutorial/02-failover.sh    # no pauses: kills node 1 itself
```

The app is [`examples/board`](../board): one component deployed as two
apps. `board` serves `GET`, `POST` and `DELETE /board` over the node's HTTP
ingress and keeps the count in the node's `board` bucket, not in its own
memory; `POST /board/crash` makes it trap. Every reply names the instance
that answered (`"pid"`, and the `x-paddock-pid` header). `board-watch`
watches the count on the board and logs every change.

## Chapter 1: one node

`01-one-node.sh` starts one node with HTTP ingress on port 8080
(`HTTP_PORT`) and a `board` bucket on disk, and deploys both apps.

```text
$ curl -s -X POST http://127.0.0.1:8080/board
{"count":3,"pid":"<1.1.1@1>"}
$ curl -s -X POST http://127.0.0.1:8080/board/crash
$ curl -s http://127.0.0.1:8080/board
{"count":3,"pid":"<1.1.2@1>"}
```

A pid reads `<node.instance.incarnation@creation>`. The crash fails its
request (502); the app's supervisor restarts the instance from `init`, as
the next incarnation of the same instance (`1.1.1` to `1.1.2`). The new
instance starts with nothing in memory and reads the count from the board,
so nothing is lost. `paddock logs board-watch` shows each change the
watcher saw, and `paddock ps` the instances.

The script then restarts the whole node. The new node process has a new
creation (`@2`), and the count is still 3: the bucket is on disk.

What this shows: an instance is disposable. State an app needs after a
crash lives outside it, in a bucket (or a database, or the devices it
reads), and the supervisor's restart is the recovery.

## Chapter 2: two nodes and a witness

`02-failover.sh` starts three nodes on loopback: a witness (node 3) and an
HA pair (nodes 1 and 2), each member with its own HTTP ingress (ports 8081
and 8082) and its own `board` bucket. The board app is deployed to both,
with `placement.mode = "singleton"`: it runs on the primary only. Each
node's listener names its partner's (`partner_url` under `[http.main]`),
so the standby answers the board's requests with `307 Temporary Redirect`
to the same path on the primary, and `curl -L` follows it.

The script prints each node's process id. Kill the primary as a power cut
would, `kill -9 <node 1's process>`, and read node 2 a few seconds later:

```text
$ curl -s http://127.0.0.1:8082/board
{"count":0,"pid":"<2.1.1@1>"}
$ paddock --socket ... failover
role:       primary
epoch:      2
```

Node 2 was promoted, with the witness's grant, once node 1's lease had run
out: about 3.4 s with the demo's short timings (200 ms heartbeats, 1 s node
timeout, 2 s lease), about 9 to 10 s with the defaults. It started the
board app from `init`. The count is 0: the app moved, but its board did
not. A `disk` bucket lives on one node, and node 1's copy went down with
it. The script then restarts node 1, which rejoins as the standby; its own
board still says 3, a stale copy that would come back if node 1 were
promoted again.

Kill node 1 only once node 2 is the standby (`paddock failover` on node 2
says so; the script waits for it). A node still starting is `down`, and a
`down` node does not ask to be promoted: if the primary dies in that
window, nobody takes over until it returns. This is an open question for
the failover protocol (DESIGN.md, "Open questions").

Clients that follow redirects can use either node's address, as the
script's `curl -L` does: a request to the standby goes on to the primary.
307 keeps the method and body, so a `POST` is repeated there. The
standby redirects only while it hears the primary; during a takeover it
answers `503 app not running`, as it does without `partner_url`.

For clients that do not follow redirects, a listener can forward instead:
with `standby = "forward"` under `[http.main]`, the standby sends the
request over the cluster to the board on the primary and answers with its
reply, so either address works for any client.

For one address that reaches whichever node is primary, put a load
balancer in front of both nodes. A health check on the app's own route
(here `GET /board`) expecting 200 marks only the primary up, since the
standby answers 307 or 503. For example, with HAProxy:

```text
backend board
    option httpchk GET /board
    http-check expect status 200
    server node1 127.0.0.1:8081 check inter 500ms fall 2 rise 2
    server node2 127.0.0.1:8082 check inter 500ms fall 2 rise 2
```

Cloud load balancers work the same way. For a virtual IP that moves with
the primary (as VRRP would), [`examples/systemd/paddock-vip.sh`](../systemd/paddock-vip.sh)
is an agent driven by the node's leadership (`paddock failover --watch`;
OPERATIONS.md). It stays outside `paddock node`, which then needs no
network privileges.

What this shows: failover moves where an app runs, not its state. The next
chapters move the state too.

## Chapter 3: a hot standby

`03-hot-standby.sh` starts the same three nodes as chapter 2 and deploys
the board as a hot standby ([`hot-standby.toml`](../board/hot-standby.toml)):
`placement.mode = "hot-standby"` runs it on both nodes of the pair, and
each instance learns its role from the `failover-role` host event. Every
instance joins the `board` process group, which spans the cluster. The
primary writes each new count to its own board, then calls every other
member of the group with it, and answers the client only after:

```text
$ curl -s -X POST http://127.0.0.1:8081/board
{"count":3,"pid":"<1.1.1@1>","replicas":1}
$ curl -s http://127.0.0.1:8082/board
{"count":3,"pid":"<2.1.1@1>"}
$ curl -s -X POST http://127.0.0.1:8082/board
standby: writes go to the primary
```

The standby serves reads from its own board, which the primary keeps
current, and refuses writes (503). Kill the primary with `kill -9`, and a
few seconds later node 2 is the primary with the count where node 1 left
it, since every write node 1 answered was already on node 2's board:

```text
$ curl -s -X POST http://127.0.0.1:8082/board
{"count":4,"pid":"<2.1.1@1>","replicas":0}
```

The app did not have to start on node 2; it was running all along and was
told its new role. When node 1 comes back it rejoins as the standby, its
instance joins the group again, and the next write brings its board up to
date.

What this shows: a hot standby keeps state on two nodes by sending every
write to the standby before answering, at the cost of a call between the
nodes per write. If the standby cannot be reached, the primary still
answers, with `"replicas":0`, and the standby's board falls behind until
the next write after it is back. An app that cannot accept that waits for
the standby, or refuses the write.

## Chapter 4: state outside the pair

`04-database.sh` starts chapter 2's pair and witness, and a Postgres server
of its own in the demo directory (as you, on a Unix socket only; it needs
Postgres's server programs, on Ubuntu `apt install postgresql`). The app is
[`examples/tally`](../tally), a counter that keeps its count in Postgres
through `paddock:sql`, deployed to both nodes as a singleton, as the board
was in chapter 2. Each node's config names the database once,
`[datasources.main]`; the app's manifest names `main` with how many
connections it may hold, and policy grants it (`datasources = ["main"]`).
The app never sees where the database is or how to log in.

Each `POST /tally` runs in one transaction: it adds to the count with
`UPDATE ... RETURNING` and logs the change, with the instance that made it,
in `tally_log`; if any step fails, nothing is kept. Kill the primary with
`kill -9`, and node 2 serves the tally a few seconds later, at the same
count:

```text
$ curl -s http://127.0.0.1:8082/tally
{"count":3,"pid":"<2.1.1@1>","recent":[{"by":"<1.1.1@1>","n":3,...},...]}
$ curl -s -X POST http://127.0.0.1:8082/tally
{"count":4,"pid":"<2.1.1@1>","recent":[{"by":"<2.1.1@1>","n":4},...]}
```

As in chapter 2, node 2 started the app from `init`; unlike chapter 2, its
state was never on node 1. Every change was committed in the database
before the node answered, so nothing that a client saw is lost.

What this shows: state an app keeps in a database outside the pair moves
with it for free. The database is then the part to keep available, with
its own replication or a managed service, which Paddock does not do. Its
calls cost a round trip to the database each, and a transaction holds one
of the app's connections until it commits.

## Chapter 5: replicated buckets

`05-replicated.sh` is chapter 2 with one change in node config: the
`board` bucket is a cluster bucket, kept whole on both nodes of the pair
through a replicated log (`scope = "cluster"` under `[buckets.board]`, and
`replication = true` under `[failover]` on both nodes). The app is the same
singleton as in chapter 2, unchanged: it reads and writes its board as in
chapter 1. Each write now returns once the standby has it on disk too.

Kill the primary with `kill -9`, and node 2 serves the board a few seconds
later, at the same count:

```text
$ curl -s http://127.0.0.1:8082/board
{"count":3,"pid":"<2.1.1@1>"}
$ paddock --socket ... failover
role:       primary
outdated:   the partner lacks replicated writes; it is not promoted until it catches up
replicated: board (commit 5, last 5, log 1 KiB, partner at 0, 5 behind, committing alone)
```

Node 2 started the app from `init`, as in chapter 2, but the board was
already there. With node 1 gone, node 2 commits alone, once the witness
has recorded node 1 as out of date: the witness would not promote node 1
now, since it lacks writes node 2 answered. When node 1 comes back it
rejoins as the standby, catches up from node 2's log, and the record
clears (`partner at 6, 0 behind`). Unlike chapter 2, node 1 holds no stale
board.

What this shows: a replicated bucket moves state with the app and keeps
both copies current, with no code in the app and no database to run. Each
write costs a round trip to the standby and a sync to its disk (about 2.2
to 2.8 ms on NVMe; BASELINE.md), so it suits state, such as counts, point
tables and settings, more than streams. A singleton in `snapshot` upgrade
mode can also take its in-memory state along in a planned switchover
(`paddock failover switchover`; OPERATIONS.md).

## Chapter 6: a hot upgrade

`06-hot-upgrade.sh` starts one node and builds [`examples/counter`](../counter)
in three versions: v1 keeps a count in its own memory, v2 also keeps the
last three values and reads v1's state, and v3 is v2 refusing every
snapshot. All three use `upgrade.mode = "snapshot"`.

```text
$ paddock call counter get
1.0.0 count=3
$ paddock upgrade counter counter-2.rbp
1.0.0 -> 2.0.0 (snapshot), 1 instance(s), longest pause 344 µs
$ paddock call counter get
2.0.0 count=3 history=
$ paddock upgrade counter counter-3.rbp
Error: rollout stopped and reverted: instance-1: load-snapshot failed: v3 refuses snapshots from 2.0.0; ...
$ paddock rollback counter
Error: rollout stopped and reverted: instance-1: load-snapshot failed: cannot read snapshot format 2 ...
$ paddock rollback counter --restart
2.0.0 -> 1.0.0 (restart), 1 instance(s)
$ paddock call counter get
1.0.0 count=0
```

The upgrade to v2 paused the instance's mailbox between two messages, took
v1's `save-snapshot`, started v2 with `load-snapshot` and resumed it: the
same pid, registered name and queued messages, and the count. v3's
`load-snapshot` failed, so the node kept v2 running with its state. A
rollback is an upgrade to the previous version, in that version's mode, so
v1 would have to read v2's snapshot, the downgrade half of OTP's
`code_change`; it cannot, so the rollback is refused too. `--restart`
moves the instances by restart instead, from `init`, discarding the state,
and the audit log records `"restart_requested": true`.

What this shows: code changes without stopping the app or losing what it
holds in memory, and a version that cannot take the state over is refused
rather than run without it. Writing the snapshot functions is the app's
part: `paddock-sdk`'s `#[derive(Snapshot)]` versions the format and hands
older ones to a `migrate` function (WRITING_APPS.md, "State across
upgrades"). An app whose state lives outside the instance, as the board's
does, needs none of this and upgrades in `restart` or `blue-green` mode.
