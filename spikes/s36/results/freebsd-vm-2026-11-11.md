10:33AM  up 43 mins, 0 users, load averages: 0.50, 0.26, 0.19
helper: node started as pid 2191 (uid 59990)
# S36 run: FreeBSD 15.0-RELEASE amd64

Node: pid 2191, uid 59990, gid 59990, helper pid 2190; 200 spawns and 20000 round trips per mode.

## The helper's refusals

| request | answer |
| --- | --- |
| spawn with uid 100 (outside the range) | refused: uid 100 is outside 60000-60999 |
| spawn with uid 0 | refused: uid 0 is outside 60000-60999 |
| spawn with the node's uid | refused: uid 59990 is outside 60000-60999 |
| spawn /bin/sh (outside the programs directory) | refused: program /bin/sh is outside /opt/s36 |
| spawn through `..` out of the programs directory | refused: program /bin/sh is outside /opt/s36 |
| spawn with memory_max above the ceiling | refused: limits above the helper's ceilings |
| spawn app `../etc` | refused: app name "../etc" is not [a-z0-9-]{1,32} |
| signal instance 999999 (not started) | refused: no instance 999999 |
| bind port 8080 (not in its ports) | refused: port 8080 is not in the helper's ports |
| spawn app beta under alpha's uid | refused: uid 60000 belongs to app alpha |
| a malformed request | refused: malformed request |
| signal SIGSTOP (not one the node may send) | refused: signal 17 is not one the node may send |
| bind port 81 (allowed) | bound 81 0.0.0.0:81 |

## Starting and stopping an instance

Spawn: from the node's request (or fork, for `direct`) to the app's hello. Exit notice: from asking the app to exit to the node hearing of it (its own `waitpid` for `direct`, the helper's `Exited` otherwise). Round trip: a 1-byte message and its answer over the app's seqpacket socket, both sides blocking. Times in µs.

| mode | spawn p50 | spawn p90 | spawn max | exit notice p50 | exit notice p90 | round trip p50 | round trip p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| direct | 1369 | 1406 | 1629 | 142 | 159 | 10.6 | 18.4 |
| uid | 1427 | 1472 | 1747 | 175 | 199 | 11.8 | 20.9 |
| uid+rctl | 1456 | 1508 | 1712 | 182 | 203 | 10.6 | 19.1 |
| uid+jail+rctl | 1271 | 1645 | 10016 | 143 | 173 | 10.0 | 16.5 |

## What a compromised app reaches

Instance alpha-1 probes, with no Landlock and no seccomp. Beta is another app (another uid in the helper's modes); alpha-2 is alpha's other instance (the same uid). `direct`: every app under the node's uid, as today. The node is not dumpable in every mode (ROADMAP PS3).

| probe | direct | uid | uid+rctl | uid+jail+rctl |
| --- | --- | --- | --- | --- |
| app's view: pid uid gid host | 2998 59990 59990 <host> | 3001 60000 60000 <host> | 3004 60000 60000 <host> | 3007 60000 60000 s36-app |
| write own dir | ok | ok | ok | ok |
| read other app's file | ok | EACCES | EACCES | ENOENT |
| list other app's dir | ok | EACCES | EACCES | ok |
| read node's data | ok | EACCES | EACCES | ENOENT |
| read helper's config | EACCES | EACCES | EACCES | ENOENT |
| connect node's admin socket | ok | EACCES | EACCES | ENOENT |
| connect node's TCP port | ok | ok | ok | EPROTONOSUPPORT |
| signal node | ok | EPERM | EPERM | ESRCH |
| signal other app | ok | EPERM | EPERM | ESRCH |
| signal own sibling instance | ok | ok | ok | ESRCH |
| signal helper | EPERM | EPERM | EPERM | ESRCH |
| read node's /proc environ | ENOENT | ENOENT | ENOENT | ENOENT |
| open node's /proc mem | ENOENT | ENOENT | ENOENT | ENOENT |
| read node's /proc cmdline | ENOENT | ENOENT | ENOENT | ENOENT |
| read sibling's /proc environ | ENOENT | ENOENT | ENOENT | ENOENT |
| processes visible in /proc | 0 | 0 | 0 | ENOENT |
| setuid(0) | EPERM | EPERM | EPERM | EPERM |
| read /etc/master.passwd | EACCES | EACCES | EACCES | ENOENT |
| read /etc/passwd | ok | ok | ok | ENOENT |
| create in /tmp | ok | ok | ok | EROFS |
| named POSIX shm | ok | ok | ok | ok |
| new IPv4 socket | ok | ok | ok | EPROTONOSUPPORT |
| connect to 1.1.1.1:53 | ECONNREFUSED | ECONNREFUSED | ECONNREFUSED | EPROTONOSUPPORT |

## What the unprivileged node can still do

| | uid | uid+rctl | uid+jail+rctl |
| --- | --- | --- | --- |
| kill(pid, 0) | EPERM | EPERM | EPERM |
| signal 0 through the process handle | EPERM | EPERM | EPERM |
| read the app's directory | EACCES | EACCES | EACCES |
| read the cgroup's memory.current | - | - | - |
| end the instance (as the node may) | helper SIGKILL: done; exit code -1 signal 9 | helper SIGKILL: done; exit code -1 signal 9 | helper SIGKILL: done; exit code -1 signal 9 |
| handle polls ready at exit / helper's notice (µs) | 169 / 200 | 185 / 212 | 293 / 340 |
| the app's forked child afterwards | alive (EPERM) | alive (EPERM) | pid not visible to the node |
| helper SIGTERM (no handler in the app) | done; exit code -1 signal 15 | done; exit code -1 signal 15 | done; exit code -1 signal 15 |

## The node's death

Forked child 3010 of the `uid` table row, now: alive (EPERM).

Forked child 3013 of the `uid+rctl` table row, now: alive (EPERM).

The node exits now, leaving alpha (pid 3018) and beta (pid 3019) running in mode `uid+jail+rctl`; the helper should kill both. `scripts/run.sh` checks that no process of either app's uid is left.

helper: node's socket closed; killing 2 instances
helper: node exited (exit 0); helper exits
## Left after the node's death

Processes left (uid 59990 or 60000-60999; the uid modes' forked children are expected, see the node-side table):

```
3010 60000 /opt/s36/s36 app
3013 60000 /opt/s36/s36 app
```
removed
