# Tracing a live node with bpftrace

A walkthrough of Paddock's USDT probes (anyprobe, provider `paddock`) on a running node: a script that starts a node with steady, eventful traffic, and bpftrace scripts that watch it from a second terminal. Linux only; bpftrace needs root. The probes and their arguments are listed in [docs/OBSERVABILITY.md](../../docs/OBSERVABILITY.md).

## 1. Start the demo

From the repository root:

```sh
examples/tracing/demo.sh
```

It builds `paddock` and the `ping-pong` and `chaos` examples, starts a node in a temporary directory, deploys three apps, and prints the node's process id and the commands to run next. Then it loops until Ctrl-C:

| Every | What happens | Probes it fires |
| --- | --- | --- |
| 0.25 s | `paddock call ping ping:<n>`: the admin API calls `ping`, which calls `pong` by its registered name | `call__start`/`call__done` twice (sender 0, then sender `ping`), `callback__*` |
| 10 s | `paddock kill` on `pong`: it restarts, and `ping`, which monitors it, gets `down` | `instance__exit` (`killed`), `supervisor__restart`, `callback__*` (`info`) |
| 15 s | `chaos` spins in a call with a 4-message mailbox while 8 casts arrive: 4 queue, 4 are dropped, and the call times out | `cast` (`ok`, then `overloaded`), `mailbox__drop`, `callback__done` (failed), `instance__exit` (`timeout`), `supervisor__restart` |

`paddock ps` in another terminal (with the `PADDOCK_SOCKET` the script prints) shows the restarts and last exits as they happen.

## 2. Attach a tracer

In a second terminal, from the repository root, with the process id the demo printed:

```sh
sudo bpftrace -p <node pid> examples/tracing/bt/lifecycle.bt
```

| Script | Shows |
| --- | --- |
| `bt/lifecycle.bt` | Exits, restarts and rollouts as they happen; dropped messages per instance every 5 s. |
| `bt/calls.bt` | Calls between instances every 5 s (sender id, target id; 0 is the admin API), and on Ctrl-C a latency histogram per target. |
| `bt/callbacks.bt` | Failed callbacks as they happen, and on Ctrl-C guest execution time per instance (host time excluded). |
| `bt/everything.bt` | Every probe, one line each. |

Instance ids are the middle number of a pid (`0.<id>.<incarnation>`); the demo deploys `pong` as id 1, `ping` as 2 and `chaos` as 3.

Attaching needs no restart and no change to the node: with no tracer attached each probe costs one enabled check. The scripts use `-p`, which lets bpftrace find the probes in the running process and switch them on.

## Notes

- These bpftrace scripts have not been run in this repository's automated checks: bpftrace needs root even to parse a script. They follow anyprobe's documented argument layout (a `&str` is a pointer and a length, read with `buf(ptr, len)` and printed with `%r`). Tested so far: that the probes are in the binary (`readelf -n target/release/paddock | grep -A3 stapsdt`) and that the demo's traffic produces the events above.
- Without bpftrace, `perf` reads the same probes: `sudo perf probe -x target/release/paddock sdt_paddock:instance__exit`, then `sudo perf record -e sdt_paddock:instance__exit -p <node pid>`.
- `DEMO_DIR=<dir> examples/tracing/demo.sh` keeps the node's files (data, `node.log`, bundles) in `<dir>` instead of a temporary directory.
