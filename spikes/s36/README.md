# S36: the privileged helper

Spike S36 of `docs/ROADMAP.md` (Phase 6a). It asks whether a small root helper can start every app instance under a user of its own, fenced by cgroups and namespaces on Linux and by jails and `rctl` on FreeBSD, while the node runs unprivileged. It also asks what that costs. It decides four things:

- the helper's API;
- the user layout;
- which Linux namespaces are on by default;
- how the node learns of an instance's exit.

Run 2026-11-11:
- **Linux:** a Raspberry Pi 5 (aarch64, Ubuntu 26.04, Linux 7.0), idle.
- **FreeBSD:** a FreeBSD 15.0 VM (amd64, 4 vCPUs) on the 16-core machine. Its times are function checks only: the VM shares the host's cores, and the host had desktop load (about 0.5).
- **x86-64 Linux:** an Ubuntu 26.04 VM (Linux 7.0, 4 vCPUs) on the same 16-core machine, whose load average was 1.3 to 2.3. Function check only, as for the FreeBSD VM.

Results:
- `results/pi5-linux-2026-11-11.md`;
- `results/freebsd-vm-2026-11-11.md`;
- `results/x86-vm-linux-2026-11-11.md`.

The row "a TCP stream the node made" came from a short rerun after both full runs, on the same machines; it is quoted under "Findings".

## What was built

One binary, `s36`, in three roles:

- **`helper`** runs as root. It reads its own configuration (`/etc/s36/helper.conf`, 0600) and starts the node under the node's uid, with a `SOCK_SEQPACKET` socket on fd 3. After that it does only what the node asks on that socket. It is synchronous and single-threaded, with one `poll`:
  - over the node's socket and the node's process handle;
  - over each instance's handle (pidfds on Linux; one kqueue of process descriptors on FreeBSD).

  Its only dependency is `libc`. It is about 1,000 lines with both systems' code: `helper.rs`, `wire.rs` and `os.rs`, plus `linux.rs` or `freebsd.rs`.
- **`node`** runs unprivileged (uid 59990, not dumpable as PS3 plans) and runs the scenarios below.
- **`app`** runs under its app's uid (60000 and up). Its control socket is fd 3.

### The API

| request (node to helper) | answer | the helper checks, against its own config |
| --- | --- | --- |
| `Spawn { id, app, uid, flags, memory_max, pids_max, program, args }`, descriptors as `SCM_RIGHTS` (fds 3, 4, ... in the app) | `Spawned { id, pid, cgroup }` and the process handle (pidfd or process descriptor), or `Refused { msg }` | uid in the range, not 0, not the node's; a uid serves one app only (the first that used it), and an app one uid; program inside the programs directory after `..` and symbolic links are resolved; app name `[a-z0-9-]{1,32}`; limits under its ceilings; at most 8 descriptors; flags valid for the system |
| `Signal { id, sig }` | `Done` or `Refused` | an instance it started; `SIGTERM`, `SIGKILL` or `SIGCONT` only |
| `Bind { port }` | `Bound { port }` and the listener | a port in its list |
| (unsolicited) | `Exited { id, pid, code, signal }` | sent before the helper's cleanup |

Messages are a tag byte and little-endian fields, with strings as a length and bytes, at most 8 KiB, one per seqpacket. The parser is about 60 lines.

The helper refused everything it should on both systems (the "refusals" table in each result):
- uids out of range, 0, or the node's;
- `/bin/sh`, directly or through `..`;
- limits above its ceilings;
- an app named `../etc`;
- the other system's flags;
- an unknown instance or signal, an unlisted port, or a malformed message;
- another app under a uid already in use.

It bound port 81 for the node.

### Modes

| mode | Linux | FreeBSD |
| --- | --- | --- |
| `direct` | today's path: the node forks and execs the app under its own uid | the same |
| `uid` | the helper `clone3`s with `CLONE_PIDFD`, then `setgroups`, `setresgid` and `setresuid` into the app's uid, then the parent-death signal (it must come after `setresuid`, which clears it) | `pdfork`, then the same uid drop |
| `uid+cgroup` / `uid+rctl` | plus a cgroup per instance under `<cgroup>/<app>/i<id>` (`memory.max`, `pids.max`), entered by `CLONE_INTO_CGROUP`; its `cgroup.kill` is chowned to the node | plus `rctl` rules on the process, added by the child on itself before it drops to the app's uid |
| `uid+cgroup+ns` | plus new mount, PID, network, IPC and UTS namespaces, with a private `/proc` | (none) |
| `uid+cgroup+ns+root` / `uid+jail+rctl` | plus a minimal root (below) | a jail per instance (below), with `rctl` rules on the jail |

**Linux minimal root.** The child builds it in its own mount namespace, then `pivot_root`s into it and remounts it read-only. It is a tmpfs holding:
- read-only binds of `/usr`, the `lib` and `bin` directories (as symbolic links where the host has them), `/etc/ld.so.cache` and the programs directory;
- the app's own directory, read-write;
- `/dev/null`, `/dev/zero` and `/dev/urandom`;
- a private tmpfs `/tmp`;
- `/proc`.

**FreeBSD jail.** The helper does this before `pdfork`, as PB5 did:
- mounts a read-only nullfs view of a shared jail root, a devfs (ruleset 4243: `pvclock`, `null`), and the app's directory read-write at its own path;
- creates a persistent jail on that root, with no IPv4 or IPv6;
- adds the `rctl` rules `jail:<jid>:memoryuse` and `maxproc`.

The child `jail_attach`es and drops to the app's uid. The helper then makes the jail non-persistent, so the jail goes with its last process.

## Results

### Start, exit notice and round trip

Times are in µs: 200 starts and 20,000 round trips per mode.
- **Spawn:** from the node's request (fork, for `direct`) to the app's hello.
- **Exit notice:** from asking the app to exit to the node hearing of it.
- **Round trip:** a 1-byte message and its answer over a seqpacket socket, both sides blocking.

Raspberry Pi 5:

| mode | spawn p50 | spawn p90 | exit notice p50 | round trip p50 | round trip p99 |
| --- | ---: | ---: | ---: | ---: | ---: |
| direct | 760 | 809 | 102 | 9.3 | 11.7 |
| uid | 850 | 890 | 117 | 9.1 | 11.2 |
| uid+cgroup | 1160 | 1297 | 129 | 10.2 | 12.7 |
| uid+cgroup+ns | 2037 | 2276 | 214 | 10.2 | 13.0 |
| uid+cgroup+ns+root | 2568 | 2785 | 234 | 10.2 | 12.9 |

FreeBSD VM (function check; the host was not idle):

| mode | spawn p50 | spawn p90 | spawn max | exit notice p50 | round trip p50 |
| --- | ---: | ---: | ---: | ---: | ---: |
| direct | 1369 | 1406 | 1629 | 142 | 10.6 |
| uid | 1427 | 1472 | 1747 | 175 | 11.8 |
| uid+rctl | 1456 | 1508 | 1712 | 182 | 10.6 |
| uid+jail+rctl | 1271 | 1645 | 10016 | 143 | 10.0 |

Ubuntu 26.04 VM, x86-64 (function check; the host was not idle):

| mode | spawn p50 | spawn p90 | exit notice p50 | round trip p50 | round trip p99 |
| --- | ---: | ---: | ---: | ---: | ---: |
| direct | 1060 | 1107 | 187 | 17.5 | 28.2 |
| uid | 1107 | 1191 | 209 | 19.6 | 30.3 |
| uid+cgroup | 1276 | 1383 | 259 | 20.6 | 31.6 |
| uid+cgroup+ns | 2107 | 2367 | 352 | 20.7 | 31.9 |
| uid+cgroup+ns+root | 2315 | 2834 | 300 | 20.9 | 30.7 |

The x86-64 run agrees with the Pi's in every refusal, probe and node-side row; only its times differ. The fences cost about the same there (0.05 ms for the request, 0.2 ms for the cgroup, 0.8 ms for the namespaces, 0.2 ms for the root), and a round trip in a VM is about twice the Pi's. One difference: `uid` took 2.1 µs longer than `direct` per round trip, where the Pi showed none; with the host loaded and one run, that is not attributed (a rerun on idle bare metal would settle it, PS1's `native_cost` run). The cgroup added 1.0 µs, as on the Pi.

- **Start through the helper.** The request itself adds about 0.1 ms on the Pi (`direct` to `uid`). Each fence adds to that:
  - the cgroup, 0.3 ms;
  - the five namespaces, 0.9 ms;
  - the minimal root, 0.5 ms.

  On FreeBSD the jail costs nothing measurable at p50, with outliers up to 10 ms. None of this matters for long-lived instances. Today's start through `bulkhead-runner` (4 to 14 ms, S30) was not measured here; the helper's cost adds to it.
- **Round trips do not change** with the helper, the uid, the namespaces or the jail. A cgroup adds 0.7 to 1.0 µs to a blocking round trip on the Pi, with or without its cpu controller: two rounds each way, `+memory +pids +cpu` against `+memory +pids`, gave 10.2 and 10.2 against 10.1 and 9.9 µs. A node with `[native] cgroup` pays that today. Bulkhead's rings, which spin before they sleep, were not measured here; PS1 reruns `native_cost` under the helper.
- **Exit notice.** The helper's `Exited` follows the instance's own exit by 20 to 30 µs on the Pi. A first version sent it after cleanup. On FreeBSD that took about 1 s, because unmounting a dying jail's root waits for the kernel to free the jail (PB5). The helper now notices first and retries busy unmounts from its loop.

### What a compromised app reaches

These probes ran with no Landlock and no seccomp, so the user, the namespaces, the root or the jail stand alone. Instance alpha-1 probes the other app, beta, and its own app's other instance, alpha-2. Selected rows:

| probe | direct (Linux, FreeBSD) | uid, uid+cgroup (Linux, FreeBSD) | uid+cgroup+ns | uid+cgroup+ns+root | uid+jail+rctl |
| --- | --- | --- | --- | --- | --- |
| read the other app's file | ok | EACCES | EACCES | ENOENT | ENOENT |
| read the node's data, connect its admin socket | ok | EACCES | EACCES | ENOENT | ENOENT |
| connect the node's TCP port on loopback | ok | ok | ENETUNREACH | ENETUNREACH | EPROTONOSUPPORT |
| signal the node, the other app | ok | EPERM | ESRCH | ESRCH | ESRCH |
| signal its own sibling instance | ok | ok | ESRCH | ESRCH | ESRCH |
| read the node's `/proc/<pid>/environ` (Linux) | EACCES (not dumpable) | EACCES | ENOENT | ENOENT | (no procfs) |
| read the node's `/proc/<pid>/cmdline` (Linux) | ok | ok | ENOENT | ENOENT | (no procfs) |
| processes visible in `/proc` (Linux) | 180 | 180 | 1 | 1 | (no procfs) |
| create in `/tmp` | ok | ok | ok (the host's) | ok (its own) | EROFS |
| named POSIX shm | ok | ok | ok (the host's `/dev/shm`) | ENOENT | ok (its own root's namespace, PB5) |
| read `/etc/passwd` | ok | ok | ok | ENOENT | ENOENT |
| list the other app's directory | ok | EACCES | EACCES | ENOENT | ok: an empty mount point (below) |
| a new IPv4 socket | ok | ok | ok | ok | EPROTONOSUPPORT |

- **The uid alone** keeps an app out of other apps' and the node's files, sockets, signals and `/proc` secrets: the check S36 asked for, with Landlock off. It does not hide two things:
  - **Processes:** their command lines, and the loopback ports of the node and other apps. Today's seccomp filter refuses `socket` and `connect`, which covers the ports.
  - **Shared writable places:** `/tmp` and `/dev/shm`.
- **An app's instances share a uid.** In the uid modes they can signal each other and read each other's `/proc/<pid>/environ`. Ptrace between them was refused on Ubuntu by Yama (`ptrace_scope` 1), not by the uid. FreeBSD's `security.bsd.unprivileged_proc_debug` is 1, so siblings there could debug each other (not probed). A PID namespace or a jail per instance removes all of it.
- **Namespaces plus a minimal root** leave an app its own directory, the system's libraries and its descriptors, and nothing else visible. Passed descriptors keep working: in every mode the app wrote to a TCP stream the node had connected and passed, including in a network namespace with only loopback down and in a jail with no IP.
- **The jail's shared root leaks other apps' names.** Each app's directory is mounted at its host path, so its mount point sits in the shared jail root, and every jailed app sees the empty directories of the others. It also leaves no writable `/tmp`. FB2 should mount the app's directory at a fixed path in the jail, and give each instance its own `/tmp`.

### What the unprivileged node can still do

| | Linux uid | Linux with cgroup (any) | FreeBSD uid, rctl | FreeBSD jail |
| --- | --- | --- | --- | --- |
| `kill(pid, 0)` | EPERM | EPERM | EPERM | EPERM |
| signal through its copy of the process handle (`pidfd_send_signal`, `pdkill`) | EPERM | EPERM | EPERM | EPERM |
| ptrace the app, read its directory | EPERM, EACCES | EPERM, EACCES | (ptrace not probed), EACCES | EACCES |
| read the instance's memory | (none) | `memory.current` | (none) | (not tried: `rctl -u` needs root) |
| end the instance | through the helper | write `cgroup.kill`: no capability, no round trip | through the helper | through the helper (`jail_remove`) |
| the instance's forked child afterwards | alive | gone | alive | gone (with the jail) |
| its handle polls ready at exit, against the helper's notice (µs) | 100 / 119 | 154 / 178 | 169 / 200 | 293 / 340 |
| helper `SIGTERM` to an app with no handler | exits | exits; ignored with a PID namespace | exits | exits |

When the node died, the helper killed the two instances it left, with their cgroups or jails, and exited. Only the uid modes' forked children were left behind.

## Findings

1. **A process handle does not get round the uid check.** A pidfd or process descriptor lets the node wait for an exit. It cannot signal an instance under another uid: both `pidfd_send_signal` and `pdkill` return `EPERM`. Neither can `kill` or `ptrace`. Without root, the node's ways to end an instance are:
   - a request to the helper;
   - on Linux, the instance's `cgroup.kill`, chowned to the node, which ends the instance and everything it forked, without a capability.
2. **Only a cgroup or a jail ends what an app forked.** In the uid modes a forked child that ignores `SIGTERM` outlives its instance. It even outlived the node's death, since the helper knows only the processes it started. With a cgroup (`cgroup.kill`) or a jail (`jail_remove`), it went with the instance. That is PS7's answer: under the helper, an instance always has one or the other.
3. **The helper must own the uid-to-app binding.** If the node chose an app's uid freely, a compromised node could start code under another app's uid and read that app's volumes. The helper refuses a uid already used by another app. PS1 has to keep that binding in the helper's own state, not in the node's deploy store as Phase 6a proposed.
4. **In a PID namespace the app is pid 1.** The kernel drops signals to a namespace's init that it has no handler for, so the helper's `SIGTERM` was ignored. `SIGKILL` and `cgroup.kill` work, and Bulkhead already stops instances with a frame and kills with `SIGKILL`, so nothing breaks. The app also inherits init's duty to reap orphaned grandchildren, but only if it forks.
5. **Exits are the helper's to report.** Only a parent can `waitid` a process (or read its `NOTE_EXIT` status), and the helper is the parent. The node's copy of the handle polls ready 20 to 50 µs sooner, which is not worth a second path.
6. **Order of operations in the child (Linux):**
   1. `mount --make-rprivate /`;
   2. hostname;
   3. `/proc` or the minimal root;
   4. descriptors to 3, 4, ...;
   5. `setgroups`, `setresgid`, `setresuid`;
   6. the parent-death signal (cleared by a credential change, so after the drop);
   7. `execve` with an empty environment.

   An error pipe (close-on-exec) carries a failing step and its errno back to the helper. On FreeBSD, `rctl` rules go on the jail from the helper before the child attaches: a jailed root cannot add rules, and per-process rules would have to come from the child before its uid drop.

## Decisions

- **The helper's API** is the four messages above, extended later by S35's ZFS requests and S40's network rules:
  - `Spawn`, `Signal` and `Bind`;
  - the helper's `Exited` notice.

  Transport and code:
  - a seqpacket socket, descriptors as `SCM_RIGHTS`;
  - a tagged binary format with a fuzzed parser;
  - synchronous, `libc` only, its own crate.
- **User layout:** a user per app, not per instance, from the helper's configured range. Group: the user's own gid, with no supplementary groups. The helper:
  - binds each uid to the first app that uses it and keeps that binding;
  - makes `<apps>/<app>` owned by the app's uid, mode 0700.

  An app's instances share the uid, so they are not fenced from each other by it; namespaces or a jail per instance do that. Open for PS2: the node cannot read or write an app's 0700 directory, so files it installs for the app (`priv/`) need the helper or a group the node may set.
- **Linux namespaces:** all five (mount, PID, network, IPC, UTS), the minimal root, and a cgroup per instance, on by default under the helper.
  - **Costs:** about 1.8 ms more per start on the Pi and nothing per message.
  - **Hides:** other processes, loopback ports, `/tmp`, `/dev/shm`, `/etc` and other apps' directories.
  - **Opt-out per profile:** S40's own-sockets profile needs a network, so it opts out of the network namespace (or gets its cgroup BPF rules instead).
  - **Not used:** user namespaces, since the helper is root.
- **Exit notices:** the helper reports each exit with its status, before its cleanup. The node may hold the process handle, but needs it for nothing. Ending an instance:
  - **Linux:** the node writes `cgroup.kill` itself;
  - **FreeBSD:** the helper removes the jail on `Signal(SIGKILL)`.

## Not done

- **Combined fences:** Landlock and the seccomp filter on top of the helper. The real `bulkhead-runner` under the helper is PS1's work.
- **Limits enforced:** `memory.max`, `pids.max` and `rctl`'s rules were set but not driven to their limits. FB2 tests `rctl` by jail.
- **Capability mode** (FB2), ZFS (S35), and the service units (PS1).

## Running

Linux, as root, from this directory after `cargo build --release`:

```sh
sh scripts/setup-linux.sh setup target/release/s36   # /opt/s36, /etc/s36/helper.conf, /var/lib/s36; no users added
sh scripts/run.sh > report.md                        # options after it go to the node: --iters, --rtt, --modes
sh scripts/setup-linux.sh teardown                   # removes all of it, cgroups included
```

FreeBSD: the same with `scripts/setup-freebsd.sh`, which also builds the shared jail root under `/var/s36-jail` and devfs ruleset 4243.

`CONTROLLERS="+memory +pids" sh scripts/setup-linux.sh setup ...` leaves out the cpu controller.

`scripts/run.sh` ends by listing any process left under the node's or the apps' uids, then kills them: a forked child left by a uid mode keeps the output pipe open otherwise.
