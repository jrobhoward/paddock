# Tutorial: state that survives failures

One small app, a counter on a bulletin board, run through the failures
Paddock handles: a crash, a node restart, and the loss of a whole node.
Each chapter is a script that starts the nodes, prints the commands to
try, and pauses between steps; it stops everything at the end or on
Ctrl-C. Run the scripts from the repository root. They need the
`wasm32-wasip2` target and `curl`, and the first build takes a few
minutes.

```bash
examples/tutorial/01-one-node.sh
examples/tutorial/02-failover.sh
examples/tutorial/03-hot-standby.sh
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
with `placement.mode = "singleton"`: it runs on the primary only. The
standby answers `503 app not running`.

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

Clients need one address that reaches whichever node is primary. With
HTTP, a load balancer in front of both nodes does it without new
features: the standby answers a singleton's routes with `503 app not
running`, so a health check on the app's own route (here `GET /board`)
marks only the primary up. For example, with HAProxy:

```text
backend board
    option httpchk GET /board
    http-check expect status 200
    server node1 127.0.0.1:8081 check inter 500ms fall 2 rise 2
    server node2 127.0.0.1:8082 check inter 500ms fall 2 rise 2
```

Cloud load balancers work the same way. A virtual IP that moves with the
primary (as VRRP would) is planned as a separate agent driven by the
node's leadership (ROADMAP D11). It stays outside `paddock node`, which
then needs no network privileges.

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

## Chapter 4 (later): replicated buckets

The board becomes a bucket replicated between the pair (`scope =
"cluster"`, Phase 3), and the app goes back to plain reads and writes, as in
chapter 1, with state that survives the loss of a node.
