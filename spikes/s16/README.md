# Spike S16: a TLA+ model of failover

This model is now layer 1 of [docs/tla](../../docs/tla/VERIFICATION.md), which adds a timed layer 2 of the code and keeps the results current.

**Question.** Can a singleton app fail over between an HA pair and a witness without ever running on both nodes, under any single failure, and when the network and the serial line fail together while both nodes live? Which rules does the implementation (ROADMAP D10) need for that?

**Answer.**
- **Yes, with a lease.** In the model, a primary runs the singleton only while it holds a lease, renewed through its partner (over the network or the serial line) or through the witness (only at the witness's epoch). The witness grants a promotion only once the old primary's lease has run out. With that, no reachable state has the app running on two nodes, with every fault the model has: crashes and restarts, the network, the serial line, each node's WAN link, and the witness crashing.
- **Without the lease, network plus serial cut while both nodes live gives two runners** (counterexample below): the witness promotes the standby while the cut-off primary keeps running at the old epoch. HAppliance's model accepts this window until the stale primary is fenced on reconnect; a singleton driving devices cannot.
- **With a serial line that fails only with a node, the lease is not needed** (`NoLeaseSafe` passes). The lease is what covers network and serial failing together, the case a witness on a third site was added for.
- **Availability.** When faults stop, some node runs the app unless neither node can renew a lease (both dead, or each cut off from its partner and the witness alike). With network and serial cut and both nodes reaching the witness, the primary keeps the app (it renews at the witness), and the standby is not promoted.

Checked on 2026-10-08 with TLC 2026.02.28 (OpenJDK 21). Built from HAppliance's `docs/tla/SyncReplication.tla` and `SyncReplicationL3_Promotion.tla` (MIT OR Apache-2.0, same author): roles, epochs, LAN and serial channels, promotion in three steps, witness crash and recovery, and a resume that cancels the partner's pending request. HAppliance's replicated data path is left out.

## How to run

```sh
# Requires Java 11+ and tla2tools.jar
# (https://github.com/tlaplus/tlaplus/releases), here at ~/tla/tla2tools.jar.
cd spikes/s16
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -config Safe.cfg Failover.tla
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -config Adversarial.cfg Failover.tla
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -config NoLease.cfg Failover.tla
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -config NoLeaseSafe.cfg Failover.tla
# Liveness configs stop injecting faults, so the model comes to rest: -deadlock.
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -deadlock -config Crashes.cfg Failover.tla
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -deadlock -config Links.cfg Failover.tla
```

## Model

- **Nodes.** Alpha and Beta run apps; the witness (on a third site, such as a disaster-recovery node) runs none. Alpha starts as primary at epoch 1.
- **Faults**, each switched on per config: a node crashes (and may restart as `Down`, keeping its epoch); the network between the pair, the serial line, or one node's WAN link to the witness fails (and may heal); the witness crashes (keeping its epoch and last promotion on disk, losing requests in flight).
- **Lease.** `Renew` when the primary reaches its partner (a standby at no higher epoch, not promoting) or the witness at the witness's epoch. `Expire` only while it cannot renew; the app stops with it.
- **Timing.** The model has no clocks. A lease runs out only while its holder cannot renew, and a grant or resume waits until the other node's lease is gone (`LeaseGone`). The implementation makes this hold with bounded clock drift (requirements R3 and R6 below).
- **Promotion.** `PromotionRequest` (the standby hears its partner on no path and reaches the witness), `WitnessGrants` (the commit point: the witness records the new epoch, after the old primary's lease is gone), `CompletePromotion` (the grant is the new primary's first lease). Stale primaries step down when they meet a higher epoch from their partner or at the witness. A restarted node rejoins a primary, or resumes through the witness if it was the last one promoted, also waiting for the partner's lease.

## Results

| Config | Faults | Checks | Distinct states | Result |
| --- | --- | --- | --- | --- |
| `Safe` | all; serial fails only with a node | safety | 2,728 | pass |
| `Adversarial` | all, serial independent | safety | 6,096 | pass |
| `NoLease` | as Adversarial, no lease | safety | 1,544 (stopped) | `AtMostOneRunning` violated |
| `NoLeaseSafe` | as Safe, no lease | safety | 2,456 | pass |
| `Crashes` | crashes, nothing heals | safety, `Available` | 12 | pass |
| `Links` | network, serial, WAN, witness; nothing heals | safety, `Available` | 95 | pass |

`Safe` and `Adversarial` also pass with `MaxEpoch = 6` (4,312 and 9,840 states). Safety is `TypeOK`, `AtMostOneRunning`, `RunningOnPrimary`, `AtMostOnePrimaryPerEpoch` and `OneGrantAtATime`.

Each lease guard is needed: removing any one of these makes `Adversarial` fail `AtMostOneRunning`:
1. the resume through the witness waiting for the partner's lease;
2. the grant waiting for it;
3. renewal at the witness only at the witness's epoch;
4. the standby refusing to renew once it has asked to be promoted.

`NoLease` counterexample, 8 states: Alpha starts the app; the network fails; the serial line fails; Beta, hearing Alpha on neither, asks the witness; the witness grants epoch 2; Beta becomes primary and starts the app while Alpha, cut off at epoch 1, still runs it.

## Requirements for D10

- **R1.** The standby's serial heartbeat carries its role, epoch and whether it is promoting, so a primary cut off from the network but not the serial line renews its lease over serial. (C5's heartbeat paths carry no data yet.)
- **R2.** The witness renews a lease only at its own epoch; a primary renewing at a lower epoch learns the higher one and steps down, stopping the app.
- **R3.** The witness grants a promotion or a resume only after the lease length plus a drift margin has passed since the last renewal it or the requester knows of. The requester reports when it last confirmed the primary's lease.
- **R4.** A standby stops confirming the primary's lease once it asks to be promoted.
- **R5.** The witness handles grants and resumes one at a time; a resume cancels the partner's pending request (HAppliance's L3 fix).
- **R6.** A primary stops its singletons a margin before its own lease would run out, so it is stopped before the witness counts the lease as gone.
- **R7.** Losing the witness stops automatic promotion (no requests are granted). An operator-confirmed promotion is the way out, and must itself wait out the lease.

## Not modeled

- More than two app nodes, or more than one witness.
- `hot-standby` roles (several instances, one leading); only a singleton.
- Clocks: the lease's timing is abstracted as above, so drift bounds are an assumption, not a result.
- C5's sessions and the transport underneath: the model's channels are up or down as a whole.
- Several singleton apps with their own leases; one lease per node is assumed to cover all of a node's singletons.
