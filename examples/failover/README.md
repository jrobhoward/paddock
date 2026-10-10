# Failover demo

A singleton moving between two Paddock nodes when the primary's process is
killed, with a witness deciding the promotion (ROADMAP D10). Three
`paddock node` processes run on loopback: a witness (node 3) and a pair
(nodes 1 and 2).

```bash
examples/failover/demo.sh        # from the repository root
```

The script builds `paddock` and the counter example (needs the
`wasm32-wasip2` target), makes a cluster key per node and a peers file, and
deploys the counter to both members with `placement.mode = "singleton"`.
Then it:

1. waits for node 1 to be primary and run the counter, and counts to 3;
2. kills node 1 with `SIGKILL` and prints how long node 2 took to run the
   counter;
3. restarts node 1 and shows it rejoin as standby at the new epoch.

The nodes stop at the end; their configs, data and logs stay in
`$DEMO_DIR` (a new temporary directory unless set). UDP ports 7401 to 7403
are used; set `BASE_PORT` to move them.

## What to expect

The demo uses short timings: 200 ms heartbeats, a 1 s node timeout, a 2 s
lease and a 300 ms margin. A killed primary hands over in about node
timeout + lease + 2 margins, plus a few heartbeats: 3.4 s in three runs on
2026-10-09 (16-core Linux machine, loaded by other work). With the default
timings (1 s, 5 s, 3 s, 500 ms) the same bound is about 9 to 10 s.

The counter restarts from `init` on node 2 (`count=0`): a singleton's state
does not cross nodes. Apps recover it from their sources or from `disk`
buckets.

`examples/tutorial` is the longer version: an app with state over HTTP,
paused between steps, with the primary killed by hand.

`crates/paddock/tests/failover.rs` runs the same steps as a test, with the
pair started after the witness has stopped waiting for it.
