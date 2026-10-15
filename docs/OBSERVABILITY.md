# Observability

What an operator can see on a running node: instance stats, direct calls to one instance, metrics for Prometheus, and probes a tracer attaches to without a restart.

## Instances

`paddock ps` (or `GET /v1/instances`) lists every instance:

| Column | Meaning |
| --- | --- |
| `PID` | `node.serial.incarnation@creation`. The serial stays across restarts of the instance; the incarnation grows by one each time; the creation counts the node's starts, so a pid from before a node restart never names a later instance. The instance id is the creation in its high 24 bits and the serial in its low 40. |
| `APP`, `VERSION`, `STATE` | What it runs, and its lifecycle state (`active`, `upgrading`, `passive`, ...). |
| `QUEUED` | Messages waiting in its mailbox. |
| `DROPPED` | Casts and timer messages dropped because its mailbox was full. |
| `MEMORY` | Linear memory it has grown to. Wasm memory does not shrink, so this is its high-water mark. |
| `RESTARTS` | Times its supervisor restarted it (the incarnation minus one). |
| `LAST EXIT` | Why the previous incarnation exited, as its supervisor saw it. |

`paddock --json ps` also lists each instance's links and monitors as its node records them: `links` (pids, on any node), `monitoring` (monitors it holds: `reference` and target `pid`) and `monitored_by` (monitors others hold on it: `reference` and watcher `pid`). For a link or monitor between nodes, each node lists its own side, so once the cluster is quiet the two sides match (spike S12).

`paddock state <pid>` shows an instance's state through its `save-snapshot`.

Connection handlers do not appear in `ps` (an instance per connection, with no mailbox). `paddock status <app>` lists their listeners instead: address, connections open now, and totals accepted, refused (over `max_connections`), crashed and closed by the idle timeout.

## Calling one instance

`paddock call` and `paddock cast` address an app (through its pool's routing), one instance by pid, or the instance holding a registered name:

```sh
paddock call orders 'ping'                     # an app
paddock call --pid 0.12.3@1 'ping'             # one instance
paddock call --name order-router 'ping'        # a registered name
paddock cast --name order-router 'refresh'
paddock call orders --cbor '{"id": 42}'        # JSON sent as CBOR; a CBOR reply prints as JSON
```

The admin API routes are `POST /v1/apps/{app}/call`, `/v1/instances/{pid}/call` and `/v1/names/{name}/call` (and `cast`), so `curl` works too:

```sh
curl --unix-socket /var/lib/paddock/admin.sock -H 'content-type: text/plain' \
  --data 'ping' 'http://paddock/v1/instances/0.12.3/call?timeout_ms=1000'
```

A call through the admin API costs about 65 µs against about 4 µs inside the node ([BASELINE.md](BASELINE.md)), so it suits operators and tests, not a data path.

## Metrics

The node renders every app's metrics and its own in the Prometheus text format (version 0.0.4): `paddock metrics`, `GET /v1/metrics` on the admin API, and, with `[metrics] address = "127.0.0.1:9100"` in node config, `GET /metrics` on that address for a Prometheus scraper (unauthenticated; bind it to loopback or a management network). OpenTelemetry collectors can scrape the same endpoint.

Apps' own metrics (`paddock:metrics`) are named `<prefix>_<name>` and carry an `app` label plus their own labels. They are per app: every instance of an app updating the same name and labels updates one series, so counters and histograms add up across a pool and survive restarts.

The host's metrics, every one with an `app` label. The names and labels are an interface, like the probes below:

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `paddock_instances` | gauge | | Live instances. |
| `paddock_memory_bytes` | gauge | | Linear memory of live instances. |
| `paddock_mailbox_depth` | gauge | | Messages waiting in live instances' mailboxes. |
| `paddock_callbacks_total` | counter | `kind` (`call`, `cast`, `info`) | Callbacks run. |
| `paddock_callback_guest_seconds` | histogram | | Guest execution time per callback (10 µs to 1 s buckets). |
| `paddock_mailbox_dropped_total` | counter | | Messages dropped at a full mailbox. |
| `paddock_instance_exits_total` | counter | `reason` (`normal`, `shutdown`, `killed`, `noproc`, `noconnection`, `timeout`, `crashed`) | Instance exits. |

With an HTTP ingress, also (no labels): `paddock_http_requests_total`, `paddock_http_server_errors_total`, `paddock_http_not_found_total`, `paddock_http_refused_connections_total`.

With a cluster transport, also, labelled `node` (the other node): `paddock_cluster_send_queue_bytes` (gauge: bytes of casts, calls and replies queued for it) and `paddock_cluster_send_refused_total` (counter: messages refused past `[cluster] send_buffer_bytes`, and notices refused past the frame cap, since the node started).

Gauges are read when the metrics are rendered; counters are kept per app from the node's start, and a counter's series starts once the app's first instance starts.

## Probes

The `paddock` binary carries USDT probes on the message path (provider `paddock`, from `paddock`'s `probes` cargo feature, on by default; without it the probe calls compile to nothing). They use anyprobe: SystemTap SDT on Linux (bpftrace, perf), DTrace USDT on macOS and FreeBSD, ETW on Windows. With no tracer attached a probe costs one enabled check and its arguments are not computed; `baseline` shows no difference beyond run-to-run noise ([BASELINE.md](BASELINE.md)).

An instance is identified by its pid's whole `id` (and `incarnation` where shown): the node's creation shifted left 40 bits, plus the serial `ps` shows. In bpftrace, `arg0 & 0xffffffffff` is the serial and `arg0 >> 40` the creation. A sender of 0 is a message from outside any instance (the admin API). A `&str` argument is two values, a pointer and a length.

| Probe | Arguments |
| --- | --- |
| `call__start` | call id, sender id, target id, body bytes |
| `call__done` | call id, target id, outcome (`ok`, `timeout`, `noproc`, `overloaded`, `too-large`, `access-denied`, `exited`, `noconnection`) |
| `cast` | sender id, target id, body bytes, outcome |
| `callback__start` | instance id, kind (`call`, `cast`, `info`) |
| `callback__done` | instance id, kind, guest execution in nanoseconds (host time excluded), succeeded |
| `instance__exit` | instance id, incarnation, exit reason |
| `supervisor__restart` | new instance id, incarnation, child id |
| `upgrade__start` | app, target version |
| `upgrade__done` | app, target version, completed |
| `mailbox__drop` | instance id, kind (`cast`, `timer`) |
| `connection__open` | handler instance id, listener name; its end is `instance__exit` |
| `node__down` | node, the session number that follows; the session with that node ended (links and monitors with it got `noconnection`) |

The call id pairs `call__start` with `call__done`; it is 0 if no tracer was attached when the call started. Probe names and arguments are an interface: scripts depend on them, so they change only with a CHANGELOG entry.

List them in a binary with `readelf -n target/release/paddock | grep -A3 stapsdt`, or `cargo anyprobe` (from the anyprobe repository), which also writes tracer scripts that decode every argument.

[`examples/tracing`](../examples/tracing/README.md) runs a node with traffic worth tracing (calls between apps, kills and restarts, an overflowing mailbox) and has ready-made bpftrace scripts.

bpftrace one-liners, attached to a running node (root; `buf(ptr, len)` reads a `&str`):

```sh
PID=$(pgrep -f 'paddock node')

# Call latency per target instance, in microseconds.
sudo bpftrace -p $PID -e '
usdt:*:paddock:call__start { @t[arg0] = nsecs; }
usdt:*:paddock:call__done /@t[arg0]/ { @us[arg1] = hist((nsecs - @t[arg0]) / 1000); delete(@t[arg0]); }'

# Guest execution per callback, by instance.
sudo bpftrace -p $PID -e 'usdt:*:paddock:callback__done { @guest_us[arg0] = hist(arg3 / 1000); }'

# Who casts to whom.
sudo bpftrace -p $PID -e 'usdt:*:paddock:cast { @casts[arg0, arg1] = count(); }'

# Exits and restarts as they happen.
sudo bpftrace -p $PID -e '
usdt:*:paddock:instance__exit { printf("exit %d.%d %r\n", arg0, arg1, buf(arg2, arg3)); }
usdt:*:paddock:supervisor__restart { printf("restart %d.%d %r\n", arg0, arg1, buf(arg2, arg3)); }'

# Messages dropped by full mailboxes.
sudo bpftrace -p $PID -e 'usdt:*:paddock:mailbox__drop { @drops[arg0] = count(); }'
```

Probes see what the host sees: pids, sizes, kinds, timings and outcomes. They do not see message bodies or guest internals.

Verified so far: the probe notes are in the Linux x86-64 binary (`readelf`), and the cost with no tracer attached. Attaching bpftrace to a live node has not been run in this repository's tests; it needs root.
