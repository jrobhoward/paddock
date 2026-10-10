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

### 4. A stale operator confirmation promoted a second runner (reading, R4)

**Problem.** `paddock failover confirm` set a flag that only a promotion cleared, and the node accepted it at any time. A confirmation given while nothing was wrong stayed set; on a later cut of network and serial, with both nodes still reaching the witness, the standby promoted itself without a lease once its own confirmations were out of date, while the primary kept renewing at the witness. Outside the model, which does not include the operator's confirmation in layer 2.

**Fix.** The confirmation lapses when the standby hears its partner, steps down or rejoins (`failover/node.rs`). The simulation test `operator_confirm____given_while_healthy_then_network_and_serial_cut____no_second_runner` failed before the fix (both nodes ran) and passes after; 1,800 new random failover seeds pass. Whether to honour a confirmation while the witness is reachable at all is ROADMAP Q10.

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
| R11 | Every frame a witness answers was sent at most `Delay` before (on an up link) | Over one lease, clock drift plus the time to stop a singleton once its lease ends stays below twice the margin. Message delay is not part of it, but for one bound: a lease runs from when its renewal or echoed stamp was sent, and the witness counts from when it received, so a delay only makes it wait longer. The bound: a witness's answer arrives within a lease of its request, before the requester asks again, since a grant starts the lease from the latest request sent (ROADMAP Q14 removes it) | Deployment: margin 500 ms by default, a 1 s budget; stopping takes an epoch tick of guest code (2 ms by default) and a host call in flight (A8), and clock drift over a 3 s lease is microseconds |

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
| A8 | A node's process is never paused | Low | Narrowed 2026-10-10 (ROADMAP Q8). It did not hold: a primary paused past its lease ran its singleton for about 2 ms after it continued, until its loop saw the lease had ended, while the standby already ran it (found by `scripts/soak.sh`, `SIGSTOP`). Singleton instances now check the lease against the monotonic clock themselves, before each callback and at each epoch tick, so after such a pause no callback starts. Left: a callback running when the process was stopped runs on until the next epoch tick after it continues (2 ms by default), and a host call in flight then (a keyvalue write, a cast) completes |

## Conformance with the code

Checked by reading (ROADMAP R4, 2026-10-10): each action of `FailoverL2.tla` against `failover/node.rs`, `failover/witness.rs` and `failover/mod.rs`, and the node's gate (`crates/paddock/src/node/gate.rs`), which the model does not include.

| Model | Code | Matches |
| --- | --- | --- |
| `Beat`: status and echo on every path | `Node::publish` sets the payload; the liveness loop sends it each interval | yes. The echo is a stamp, and a primary renews from when it set that stamp, which is no later than the model's send time |
| `Beat`: a primary renews at the witness | `Primary::step` sends `Renew` each step | yes |
| `RecvBeat`: renewal through the partner | `Primary::step` reads the partner's live beats (`PartnerView::confirmed`) | yes, applied at the next step rather than on arrival; the lease is computed from the stamp's time either way |
| `StepPrimary`: fenced by a partner at a higher epoch | `Primary::step` | yes |
| `StandbyHears`: cancel, stand-down after `Hold` | `Standby::step`, partner heard | yes |
| `StandbySilent`, `Ask` (with `ConfirmOnSend`) | `Standby::step`, `Ctx::ask`, `Node::publish` | yes |
| `StepDownNode`: rejoin on the link, resume after `Hold` | `Down::step`, `Ctx::waited_out` | yes |
| `RecvAtWitness`: renew, fenced, refused, not yet, granted | `Witness::handle` | yes. The code counts the requester's confirmation from when the request arrives, not when it was sent, so it waits a little longer |
| `RecvReply`: lease ok, fenced, granted, not yet, refused (standby, down) | `State::on_message` | yes. A granted lease runs from when the request left (the model: from the reply; see below) |
| `Crash`, `Restart` with `PersistEpoch` | process exit; `Node::start` at the epoch the gate kept | yes, with one difference: see "Epoch kept after it is published" |
| `WitnessCrash`, `WitnessRestart` with `ForgetRenewals` | `Witness::start` with `resume` | yes |
| `Running(n)`: primary with a lease | `Leadership::may_run`, then the gate | yes up to `may_run`; the gate's start and stop are not modelled |

Differences, and what each needs:

- **Operator confirmation (fixed 2026-10-10).** Not in layer 2 (A6 covers the arbiters; R7 is layer 1's). A confirmation never lapsed: given while nothing was wrong, it promoted the standby without a lease on a later cut of network and serial while the primary still renewed at the witness, and both ran. Found by reading; reproduced by `operator_confirm____given_while_healthy_then_network_and_serial_cut____no_second_runner` in the simulation. A confirmation now lapses when the standby hears its partner, steps down or rejoins. Whether a confirmation should count while the witness is reachable is ROADMAP Q10.
- **Stopping takes time.** The model's singleton stops the moment the lease ends. The code's instances stop running guest code at the lease's end by themselves (`failover::RunUntil`, checked before each callback and at each epoch tick); the gate kills them once the failover task wakes and publishes. What remains is a callback in progress at the end, up to one epoch tick of guest execution, and a host call in flight (A8).
- **Witness state kept before it changes (fixed 2026-10-10).** The model's witness changes its epoch and promoted node atomically and durably. The code changed them in memory under the lock, then wrote them outside it: a failed write withheld the grant but left memory ahead of disk, and a `Fenced` answer could carry an epoch not yet on disk. Now a grant is decided, written, then applied; meanwhile the witness answers nothing from the pair, as if those messages were lost, so a renewal of the node being replaced cannot extend its lease after the decision (`witness.rs`).
- **Epoch kept after it is published.** `PersistEpoch` keeps every epoch a node reaches. The code keeps it in the gate, after the leadership is published (so beats can carry it first) and before acting on it; a crash in between restarts the node at its previous epoch. Nothing found that this breaks, since the node never ran at the unkept epoch; a layer 2 variant with a crash between reaching and keeping would settle it.
- **A lost answer left a standby asking (fixed 2026-10-10).** In both the model and the code, a standby cancelled its request only with no answer due; if the answer was lost and its partner was heard again, it kept asking, and so never again confirmed the primary's lease, which then rested on the witness alone. A liveness issue, not a safety one. Now a request past its retry time counts as answered (`StandbyHears` in the model, `Standby::step` in the code); a grant that arrives after the cancel is ignored.
- **Reply ages in the model.** `Push` sets a frame's age to 0, so `granted` and `leaseOk` lose the age they were meant to carry: the model's leases run from the reply, later than the code's (from the request or renewal). The code is the more conservative, and since `lease` feeds only `Running` and `Renew`, a shorter lease cannot make `AtMostOneRunning` fail; no rerun is needed, but the model should carry the age when it next changes.
- **No serial line.** Nodes have no serial path yet (ROADMAP F5), so a deployment is the model with the serial line lost from the start: one of its faults already spent. The one-fault configurations then cover nothing beyond that loss, and the two-fault ones one fault more. ROADMAP V2 adds a configuration with the serial line lost at the start.
- **The timing assumption (one statement since 2026-10-10).** It was stated three ways: "message delay plus clock drift below the margin" in R11, "below twice the margin" in `failover/mod.rs` and `FailoverConfig::margin`. From the code's arithmetic, delays only make the witness wait longer; what the margins cover is clock drift over a lease plus the time to stop a singleton. R11, `failover/mod.rs` and `FailoverConfig::margin` now say that, with the one bound on delay the code does need: an answer within a lease of its request, since a grant that arrives after a retry starts the lease from the retry (ROADMAP Q14). The model, with one request in flight at a time, cannot express that case.

Checking conformance mechanically (trace validation) is not adopted for now. Every modelled action matched; both defects found sit outside the model (the operator's confirmation, the gate and pauses), where a trace check against the model could not have found them. Covering those in the model comes first. Revisit if `failover/` changes often (R5).

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
