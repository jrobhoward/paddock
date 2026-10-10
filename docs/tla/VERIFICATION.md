# TLA+ verification of failover

2026-10-08 · Covers `paddock_host::failover` (ROADMAP D10) and the rules in [spikes/s16](../../spikes/s16/README.md)

Failover of a singleton app between an HA pair (two nodes that run apps) with a witness (a third node on another site, such as a disaster-recovery node) and a serial line between the pair. The property checked throughout: the singleton never runs on both nodes at once.

The method follows HAppliance's (`HAppliance/docs/tla/VERIFICATION.md`): layered specifications, each giving up one simplification of the one before; configurations per fault class; every counterexample turned into a fix in both the specification and the code; and tables mapping each assumption of the model to a requirement on the code. HAppliance's eight configurations were re-run on 2026-10-08 with TLC 2026.02.28 and reproduce its published state counts exactly.

## Specifications

| File | Layer | What it adds |
| --- | --- | --- |
| `FailoverL1.tla` | 1 | Roles, epochs, the witness, the lease. Atomic steps; channels up or down as a whole; the lease abstracted as "it runs out only while its holder cannot renew". Spike S16's model, unchanged. |
| `FailoverL2.tla` | 2 | The code with time: ticks; a FIFO queue per link direction that can be up, held or lost; heartbeats carrying status and an echo, read as they arrive and going stale; the witness answering each request at once with no pending state; the cancel and stand-down rules; crashes, witness crashes, lost replies; epochs not kept across restarts. |

Layer 2 keeps every time as an age or a countdown, capped where larger values change nothing, so its state space is finite. It runs in rounds: each tick has phases (faults, delivery of every frame due in a fixed link order, one node's turn and then the other's, either node first), and in its turn a node may beat, step, both in either order, or neither. A frame sent in one round arrives in the next unless its link holds or loses it. The rounds keep the interleavings that change the outcome (which node acts first, beat before or after step, skipped beats) and drop those that only reorder independent deliveries within a tick; the Rust simulation (`crates/paddock-host/tests/simulation/failover.rs`) runs on real interleavings and covers what rounds drop.

Layer 2 has four design switches, so that each fix or change can be checked on its own:

| Switch | `FALSE` (the code before the fix) | `TRUE` (the code now) |
| --- | --- | --- |
| `EchoLease` | the primary renews from the arrival time of a confirming beat | it renews from when it set the stamp the standby echoes |
| `ConfirmOnSend` | the standby refreshes its "last confirmed" time on each step that hears its partner | the time is when its beats stop confirming, recorded before any request leaves |
| `PersistEpoch` | a restarted node comes back at epoch 0 | it keeps its epoch |
| `ForgetRenewals` | the witness's renewal record survives its crashes | a restarted witness counts the node it promoted as renewed at the restart, and has no record of the other |

## Results

Run from 2026-10-08 to 2026-10-10: TLC 2026.02.28, OpenJDK 21, 16 cores, 62 GB. `Lease = 3`, `Margin = 1`, `Timeout = 2`, `Delay = 1` ticks, `MaxEpoch = 3`, one frame per link direction.

### Layer 1

| Config | Faults | Distinct states | Result |
| --- | --- | --- | --- |
| `L1_Safe` | all; serial fails only with a node | 2,728 | pass |
| `L1_Adversarial` | all, serial independent | 6,096 | pass |
| `L1_NoLease` | as Adversarial, no lease | 841 (stopped) | `AtMostOneRunning` violated |
| `L1_NoLeaseSafe` | as Safe, no lease | 2,456 | pass |
| `L1_Crashes` | crashes, nothing heals (liveness) | 12 | pass, `Available` holds |
| `L1_Links` | link and witness faults, nothing heals (liveness) | 95 | pass, `Available` holds |

### Layer 2

Exhaustive runs (breadth first, every reachable state), from 2026-10-08 to 2026-10-10, with every switch as in the code now except where noted. A fault is a crash, a witness crash, or a link lost or held; restarts and heals are free, so one fault is a whole failure and recovery.

| Config | Faults | Distinct states | Depth | Time | Result |
| --- | --- | --- | --- | --- | --- |
| `L2_Crashes_Fixed1` | one crash, witness crash or lost link | 149,712,184 | 310 | 17 min 30 s (12 workers) | pass |
| `L2_Crashes_Fixed` | up to two of the same | 8,367,682,206 | 364 | 19 h 40 min | pass, before the last two switches (see below) |
| `L2_Links_Fixed_loss2` | up to two links lost, nothing held; includes LAN and serial both cut | 4,675,991,758 | 362 | 9 h 54 min (8 workers) | pass (see below) |
| `L2_Links_Fixed_holds1` | one link held or lost | 908,291,071 | 349 | 2 h 18 min (8 workers) | pass |

`PersistEpoch` and `ForgetRenewals` act only when a node or the witness restarts, so the Links configurations, which have neither, are the same with either setting. The crash configurations were run again with both on (2026-10-10), except the two-fault `L2_Crashes_Fixed`: its exhaustive result above is with both off (epochs lost on restart, the renewal record kept), and with both on it has passed random simulation only.

`L2_Links_Fixed` (up to three faults, holds allowed) is too large to finish: with two faults it had 335 million states at depth 53 and a growing queue when stopped. Holds are what make it large, so it is split into the two configurations above.

TLC stores each state as a 64-bit fingerprint, and two states with the same fingerprint count as one. For `L2_Crashes_Fixed1` it puts the chance of such a collision at 0.006, for `L2_Links_Fixed_holds1` at 0.08; for `L2_Links_Fixed_loss2` (4.7 billion states) and `L2_Crashes_Fixed` (8.4 billion), at about 1, so in each run a state or two (and any reached only through them) were most likely not checked. A second run with another fingerprint seed (`-fp`) would close this; the random simulations below cover the same configurations independently.

Random simulation (TLC `-simulate`, traces up to 400 steps):

| Config | Traces | Result |
| --- | --- | --- |
| `L2_Code` | 11 | `AtMostOneRunning` violated (bug 1) |
| `L2_Crashes_Code` | 10 | `AtMostOneRunning` violated |
| `L2_Links_Code` | 8 | `AtMostOneRunning` violated |
| `L2_Fixed` | 243,045 | pass |
| `L2_Crashes_Fixed` | 270,012 | pass |
| `L2_Links_Fixed` | 19,947 | pass |

## Bugs found

### 1. A standby's confirmation time could precede its last confirming beat (L2)

**Problem.** The standby recorded "last confirmed the primary's lease" on its step, while its heartbeats went out on their own schedule. A confirming beat could leave after the last step that recorded the time. When the standby then asked to be promoted, it reported a confirmation older than its last confirming beat, and the witness granted the promotion while the primary still held a lease renewed from that beat.

**Counterexample** (`L2_Code`, TLC simulation, found in under 50 random traces): B rejoins as standby at t=7 and records its confirmation at 7. Its beats at t=8 to t=11 still confirm A's lease, and A renews from each as it arrives (lease to t=14). A skips beats (a slow node), so B hears it on no path at t=11 and asks. The witness computes that A's lease ran out at t=12 and grants; from t=12 both run.

**Fix.** The confirmation time is the moment the standby's beats stop confirming (`ConfirmOnSend`), set before the request leaves (`Ctx::ask` in `failover/node.rs`). The first version of the fix set it only when the new payload was published, after the request had left; the Rust simulation caught that (9 of 100 random seeds), since the model treats "start asking and send the request" as one step.

### 2. Renewal by arrival time trusts the network's delay (L2)

**Problem.** The primary renewed its lease from the moment a confirming beat arrived. A beat held by the network (QUIC keeps unacknowledged frames, so a healed link delivers old ones) could arrive long after the standby stopped confirming, and renew a lease the witness already counted as gone. In the code, sessions (C5) discarded such frames until a change made heartbeat payloads readable in any session; the payload then decided.

**Fix.** Renewal by echo (`EchoLease`): the primary stamps its beats with a counter and remembers when it set each stamp; a confirming standby echoes the highest stamp it has seen; the primary renews from when it set that stamp, which precedes the confirmation however long the beat took. In the model each fix alone removes the violation in the configurations here; the code has both, so neither rests on the other.

### 3. A stale payload counted as fresh (simulation, before L2)

The random failover simulation found that a heartbeat discarded as belonging to an ended session still refreshed the link's "heard" time while the kept payload stayed the old "standby, confirming" one, and the primary renewed from it. Payloads now carry their own arrival time. Layer 2 models arrival-timed payloads, so this class is covered there.

## Implementation requirements

From layer 1 (S16), with where the code meets each:

| # | Assumption in the model | Requirement on the code | Where |
| --- | --- | --- | --- |
| R1 | The primary renews through its partner over LAN or serial | The standby's beats on every path carry its standing, epoch and whether it is promoting | `failover::Status`, `liveness` payloads |
| R2 | Renewal at the witness only at its epoch | The witness renews only `epoch == its epoch` from the node it last promoted; anything else gets `Fenced` | `failover/witness.rs` |
| R3 | A grant waits until the other node's lease is gone | The witness waits the lease plus the margin after the later of its last renewal of that node and the requester's last confirmation | `failover/witness.rs` |
| R4 | A promoting standby confirms nothing | A standby asking to be promoted stops echoing | `node::State::confirming` |
| R5 | Grants and resumes one at a time | The witness handles one message at a time, under its lock | `failover::Shared::deliver` |
| R6 | A lease ends exactly when it runs out | A primary stops `margin` before its lease would end, and its loop wakes at that moment | `node::Ctx::promote`, `node::run` |
| R7 | Without the witness nothing is promoted | An operator's confirmation promotes only after the partner's lease has run out | `node::Standby::step` |

From layer 2:

| # | Assumption in the model | Requirement on the code | Where |
| --- | --- | --- | --- |
| R8 | `EchoLease` | Renewal through the partner is timed by the primary's own stamp, echoed | `node::Primary::step` |
| R9 | `ConfirmOnSend` | The confirmation time is when the payload stops confirming, recorded before a request leaves | `node::Ctx::ask`, `node::Node::publish` |
| R10 | The witness's epoch and last promotion survive its crashes; its renewal record does not (`ForgetRenewals`) | Kept on disk and synced before a grant is answered | The epoch and promoted node: `data_dir/witness-state`, synced before a grant (`serve_witness_kept`). The renewal record is not kept: a restarted witness counts its promoted node as renewed at the restart (`witness::Witness::start`), which is never earlier than the true record |
| R11 | Every frame a witness answers was sent at most `Delay` before (on an up link) | Message delay plus clock drift stays below the margin | Deployment: margin 500 ms by default |

## Assumptions not tested

| # | Assumption | Risk | Mitigation |
| --- | --- | --- | --- |
| A1 | Clocks do not drift | Medium | The margin covers drift and delay together (R11); the simulation cannot test drift either |
| A2 | One pair and one witness | Low | The design has exactly these |
| A3 | Sessions (C5) are not modelled; every delivered frame is acted on | Low | Worst case for stale frames; sessions only discard more |
| A4 | Within a round, deliveries follow a fixed link order and arrive one round after sending | Medium | The Rust simulation runs real interleavings and delays; 200 random fault schedules pass |
| A5 | Bounded faults per run (two, exhaustively; one where frames are held), epochs up to 3, one frame per link direction | Medium | Typical of model checking; random simulation allows three faults with holds |
| A6 | Only the witness arbiter is modelled in layer 2 | Medium | `Paths` and `Operator` run without leases and rest on layer 1's `L1_NoLeaseSafe` or the operator's judgement |
| A7 | A node steps and beats at most once per tick | Low | The code steps and beats once per heartbeat interval |
| A8 | A node's process is never paused | High | Not held: a primary paused past its lease runs its singleton for about 2 ms after it continues, until its loop sees the lease has ended, while the standby already runs it. Found by `scripts/soak.sh` (`SIGSTOP`, 2026-10-10); ROADMAP Q8 |

## Running the specifications

```sh
cd docs/tla
# Layer 1 (seconds each); the liveness configs need -deadlock.
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -config L1_Safe.cfg FailoverL1.tla
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -deadlock -config L1_Crashes.cfg FailoverL1.tla
# Layer 2, exhaustive: minutes for the one-fault configs, about 20 hours and
# 250 GB of disk (in states/) for L2_Crashes_Fixed. -checkpoint 30 and
# -recover states/<run> let a long run survive a restart.
java -XX:+UseParallelGC -Xmx12g -jar ~/tla/tla2tools.jar -workers auto -config L2_Crashes_Fixed1.cfg FailoverL2.tla
java -XX:+UseParallelGC -Xmx40g -jar ~/tla/tla2tools.jar -workers auto -checkpoint 30 -config L2_Crashes_Fixed.cfg FailoverL2.tla
# Layer 2, random simulation (minutes): the Code configs find bug 1 in seconds.
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -simulate num=100000000 -depth 400 -deadlock \
    -workers auto -config L2_Links_Code.cfg FailoverL2.tla
```

TLC 1.8 or later (`tla2tools.jar` from https://github.com/tlaplus/tlaplus/releases), Java 11 or later. None of this runs in CI.
