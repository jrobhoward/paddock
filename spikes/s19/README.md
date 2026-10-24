# Spike S19: ports or nodes for native processes

**Question.** Native code (a rustbridge `.so`) cannot be sandboxed or unloaded safely inside the node. Should it run as an Erlang-style port, a process the node spawns and talks to over stdin and stdout, owned and supervised like an instance? Or should it join as a cluster node through a client library? What does each cost, and which limits can the node put on it?

**Answer: a port.** Only a process the node spawns can be limited and stopped by the node, and only a port keeps Paddock's grants as the boundary. A cluster member is trusted with its stamps on every frame; a node also restricts what a port can do on the machine itself.

A port is cheap enough:
- **Calls:** a round trip through the pipe takes about 37 µs at p50. That is about a third of a call between nodes (106 µs over QUIC on loopback) and about ten times a call within a node (3.5 µs).
- **Throughput:** 64 calls in flight reach about 400,000 calls per second.
- **Start:** a port starts in about 1.9 ms with its plugin loaded.
- **Memory:** an idle runner holds about 3.8 MiB.
- **Crashes:** the node sees one through the pipe closing within about 0.3 ms.

Linux gives the node most of the limits it needs:
- Landlock for files, execution and TCP ports.
- A cgroup for memory and processes.
- Non-dumpable processes and `no_new_privs`.

Two limits are missing:
- **Address rules:** Landlock matches TCP ports, not addresses, so the manifest's `wasi:sockets` address rules cannot be enforced on native code by Landlock alone.
- **Signals:** before Linux 6.12, a sandboxed process can still signal any process of the node's user, the node included.

Run on 2026-10-23: AMD Ryzen Threadripper 1950X (16 cores), Linux 6.8 (Landlock ABI 4), Ubuntu's core dumps through apport. The machine was idle apart from a disc being read in the background (about 1% of one core). Two runs, consistent; the first is shown. Linux x86-64 only; the Pi and other systems were not run.

## How to run

```sh
cd spikes
cargo build --release -p s19-plugin -p s19-runner -p s19-bench
./target/release/s19-bench all        # start, latency, throughput, failures, sandbox
# The cgroup test needs a delegated cgroup subtree:
systemd-run --user --scope -p Delegate=yes ./target/release/s19-bench cgroup
```

## Method

- **`plugin/`** is a rustbridge plugin (`cdylib`). It echoes a payload, and on request it misbehaves:
  - loops forever or aborts;
  - allocates `n` MiB;
  - reads or writes a file;
  - opens a TCP connection;
  - runs `/bin/true`.
- **`runner/`** is the native-process runner DESIGN.md proposes for rustbridge. It loads one plugin with `rustbridge-consumer` and serves it over stdin and stdout:
  - **Frames:** a 4-byte length, a kind, a 64-bit id and a body. The kinds are call, reply, error, ping, pong and ready.
  - **Threads:** calls run on `--workers` threads. Pings are answered by the reading thread at once.
  - **Self-restriction:** the runner first marks itself non-dumpable. With `--sandbox`, it then restricts itself with Landlock before it loads the plugin and before it starts a thread. It may read the system library directories and the plugin's directory. It may not read anywhere else, write, execute or bind. It may connect over TCP only to `--allow-connect` ports.
- **`bench/`** is the node's side, on tokio, like a node owning a port:
  - It spawns the runner with stdin and stdout piped.
  - Between fork and exec it sets the parent-death signal (`SIGKILL`), `RLIMIT_CORE` 0, an optional `RLIMIT_AS`, and an optional cgroup (by writing to `cgroup.procs`).
  - It keeps a table of pending calls by id, and gives each call a deadline.

`rustbridge-erlang/port-driver` in rustbridge is a similar runner for Erlang. It uses JSON frames with base64 for binary bodies, and serves one request at a time. `runner/` differs in three ways: binary frames, calls on threads, and the sandbox.

## Results

### Start and size

| | p50 | p99 |
| --- | --- | --- |
| Spawn to ready (plugin loaded) | 1.92 ms | 2.18 ms |
| The same, sandboxed (Landlock before the load) | 1.92 ms | 2.23 ms |
| First call after ready | 125 µs | 148 µs |
| stdin closed to exit (the plugin's `on_stop` and its runtime) | 11.5 ms | 11.6 ms |

An idle runner with one worker thread uses 3.8 MiB RSS and runs three threads (main, the worker and the plugin's tokio runtime). After 45,000 calls it was 4.0 MiB.

### Calls

Sequential calls, one worker thread:

| Payload | p50 | p99 |
| --- | --- | --- |
| ping (the pipes and the frame code only) | 31 µs | 39 µs |
| 64 B echo | 39 µs | 47 µs |
| 4 KiB echo | 64 µs | 83 µs |
| 64 KiB echo | 273 µs | 396 µs |

The plugin adds about 8 µs to a ping, through rustbridge's FFI call, JSON and a UTF-8 check. The other 31 µs is two pipe crossings and the wake-ups on each side: a tokio task, the runner's reading thread, its worker, and back.

64-byte calls kept in flight, for 2 s:

| In flight | 1 worker | 4 workers |
| --- | --- | --- |
| 1 | 26,511/s | 29,368/s |
| 4 | 82,534/s | 91,338/s |
| 16 | 379,566/s | 231,384/s |
| 64 | 400,056/s | 299,246/s |

With calls in flight, frames batch in the pipes and one worker does best. More workers contend on the runner's job queue and on its stdout. A port therefore needs no threads of its own for throughput. One worker also keeps calls in order, as an instance handles one callback at a time.

### Failures

| Case | What the node sees |
| --- | --- |
| The plugin aborts | The pending call ends (pipe closed) 276 µs after it was sent; exit by signal 6 |
| `SIGKILL` from outside | stdout closed after 232 µs, reaped after 279 µs |
| A call that never returns | The call's 100 ms deadline ends it. A ping during it is answered, and so is a second call on another worker, so only a deadline per call catches it. Killed and reaped 0.3–0.4 ms after the deadline. |
| The runner stopped (`SIGSTOP`) | Only a ping with a deadline notices (100 ms here) |
| `RLIMIT_AS` 512 MiB, 1 GiB allocated | Rust's allocation failure aborts the process (signal 6); 64 MiB was fine |
| cgroup `memory.max` 128M (swap 0), 256 MiB allocated | The OOM killer kills the runner (signal 9); `memory.events` counts `oom_kill 1`; 64 MiB was fine |

**Core dumps delay the notice by about a second.** Ubuntu pipes core dumps to apport (`core_pattern` `|/usr/share/apport/apport ...`). The kernel holds the dying process until the helper has read the dump, so the first abort took 1.12 s to close the pipe. `RLIMIT_CORE` 0 did not help, because the kernel runs a pipe helper whatever the limit is. Marking the runner non-dumpable (`PR_SET_DUMPABLE` 0) did, bringing it to 276 µs. That also stops other processes of the node's user from attaching to the runner with ptrace.

### Sandbox

Landlock in the runner, with TCP connect allowed to one port:

| Plugin does | Unsandboxed | Sandboxed |
| --- | --- | --- |
| Read `/etc/hostname` | ok | `EACCES` |
| Read a file in the working directory | ok | `EACCES` |
| Write a file in `/tmp` | ok | `EACCES` |
| TCP connect to the allowed port | ok | ok |
| TCP connect to another port | ok | `EACCES` |
| Run `/bin/true` | ok | `EACCES` |

- **Status:** Landlock reported `PartiallyEnforced` on Linux 6.8. ABI 4 has no scopes, which need 6.12, so signals to processes outside the sandbox and abstract Unix sockets were not restricted. `no_new_privs` was set.
- **Execution:** the first version granted `AccessFs::from_read` on the library directories, which includes execute, and `/bin/true` ran. Granting `ReadFile` and `ReadDir` only stops execution and still loads the plugin, because mapping a library needs read, not execute.
- **Cost:** none measurable at start.

### cgroup

cgroup limits need a delegated subtree, and that delegation is the node's job, not the runner's:
- **Without delegation:** run from a login session (`session-c2.scope`), the bench could not create a child cgroup (`EACCES`).
- **With delegation:** under `systemd-run --user --scope -p Delegate=yes` it moved itself into a leaf `host/`, enabled `memory` and `pids` for children, created `plugin/` with `memory.max` 128M, `memory.swap.max` 0 and `pids.max` 16, and spawned the runner into it.

A node needs `Delegate=yes` in its systemd unit and the same layout. cgroup v2 allows no processes in a cgroup whose controllers are enabled for its children, so the node itself must live in a leaf.

## Reading the results

**Trust decides it.** SECURITY.md trusts a cluster member's stamps: "its stamps are trusted, so a member that lies about its senders' apps passes ACLs it should not". A plugin joined as a node would therefore:
- be trusted to say which app sent each call, cast, link and exit signal;
- take part in failover;
- hold a key in the peers file.

A port talks only to its own node, and the node stamps its frames as its own instance's. The plugin then gets what its grant allows and nothing more, and the messaging ACLs and every service check apply as they do to an instance.

**Only the node can limit what it starts.** A port is the node's child:
- **Limits:** the node chooses its cgroup, its Landlock rules, its rlimits and its environment.
- **Stopping:** the node kills it on a deadline, and the parent-death signal takes it down with the node.

A process that joins as a node is started by someone else: the node can refuse its frames, but cannot limit or stop it.

**Supervision maps onto what exists.**
- A port has a pid, a mailbox in the host, links and monitors.
- Its failures arrive as exit reasons:
  - **Pipe closed:** `crashed` with the signal or exit code.
  - **Call past its deadline:** `timeout`, as an epoch deadline is for a guest.
  - **No pong:** `timeout` too.
  - **OOM kill:** `crashed("memory_limit")`, read from `memory.events`.
- Supervisors restart it under the same intensity rules.

A node would instead arrive through membership and leave as `noconnection`, and a supervisor cannot restart it.

**A node is far more to build.** A client library would need:
- QUIC with mutual TLS on pinned keys;
- the version handshake (two wire versions at once during a release);
- 22 frame kinds (heartbeats, membership, links and monitors with the acknowledged unlink and the ordering rules S12 found, groups, failover, replication, calls by app name);
- liveness.

All of that would have to be kept in step for every language. A port needs about six frame kinds over stdin and stdout, in any language. rustbridge's runner covers its plugins, and any executable that speaks the frames can be a port.

**What a node gives that a port does not:**
- running on another machine;
- a lifecycle of its own (an operator or systemd restarts it);
- reaching every node directly.

None is needed for the pair and a witness. A port can be reached from the other node through the cluster like any instance.

## What it changes

- **`kind = "native-process"`** runs a bundle's native library (the variant for the node's platform) as a port through a runner shipped with the node. Each instance of the app is one process, and `instances.count` and pools apply as for components. A port gets a pid like an instance.
- **The protocol** extends the spike's frames:
  - **From the node:** the callbacks (`init`, `handle-call`, `handle-cast`, `handle-info`, `terminate`), each with an envelope (content type and bytes), plus stop and ping.
  - **From the runner:** replies, logs, and requests to the node (send, call, the granted services). Those are checked like a guest's imports.
  - **Ordering:** one callback at a time, as for an instance.
  - **Library:** rustbridge's `Server` trait (DESIGN.md, "rustbridge integration") is the plugin side.
- **Limits**, from the manifest and node config:
  - **Calls:** `callback_deadline_ms` as a deadline per call, followed by a kill.
  - **Liveness:** a ping interval with a deadline.
  - **Memory:** `memory_max` as the cgroup's `memory.max` when the node has a delegated cgroup, else as `RLIMIT_AS` (cruder: it counts virtual memory, and a failed allocation aborts).
  - **Processes:** `pids.max` from node config.
  - **Always:** no core dumps and non-dumpable, `no_new_privs`, and the parent-death signal.
- **The sandbox** is Landlock in the runner, set before the plugin loads:
  - **Files:** read the bundle's directory and the system libraries; nothing else, and no execution.
  - **Preopens:** `wasi:filesystem` preopens map to Landlock rules on their directories.
  - **Network:** none unless granted, since Landlock rules are TCP ports only. A plugin with `wasi:sockets` rules either gets the ports of its `connect` rules (addresses unchecked, said so in SECURITY.md) or is refused at deploy. The implementation refuses it at deploy.
- **Kernel versions:**
  - **Before Linux 6.12:** no Landlock scopes, so a plugin can signal the node's other processes, the node included. A seccomp filter on `kill`, `tkill`, `tgkill` and `pidfd_send_signal`, or a separate user for plugins, would close that. The implementation adds the filter.
  - **Before Linux 7.1:** Landlock applies only to the thread that sets it and the threads it starts. A runner must restrict itself before starting any thread or loading the plugin, as here.
- **Upgrades** are `restart` or `blue-green` only (DESIGN.md), and a port is never passivated.
- **Linux only to begin with.** The parent-death signal, Landlock and cgroups are Linux interfaces. Another system would need its own controls (FreeBSD: `procctl`, Capsicum, jails); nothing else was run.
- **Not measured:**
  - many ports at once (fork and exec from a large node, file descriptors);
  - the parent-death signal under tokio. That signal fires when the spawning thread exits, not the process, so ports must be spawned from a thread that lives as long as the node (a runtime worker, not a `spawn_blocking` thread, which exits after 10 s idle).
