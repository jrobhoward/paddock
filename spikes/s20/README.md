# Spike S20: a TLA+ model of replication within the HA pair

**Question (ROADMAP K1).** Replicated buckets replicate synchronously between the pair, with the failover primary as leader at its epoch and the witness breaking ties (decided 2026-10-16). What commit rule, catch-up path and log length keep every committed write over any single failure or planned switchover, with no two nodes committing different entries at one index, and linearizable reads on request?

**Answer: a log in the Raft shape, plus a record at the witness.** With the rules below, no reachable state loses or replaces a committed write, and a linearizable read always returns exactly what has been committed, with one failure at a time (any number over time, a new one possibly while a node is still catching up from the last) and planned switchovers (results below). Each rule, switched off, gives a counterexample under the same faults.

- **R1. Commit.** In sync mode the primary commits an entry once its partner has acked it. It commits alone only after the witness has recorded, at the primary's epoch, that the partner is out of date. A grant is such a record: a new primary starts alone. While the record stands, the witness refuses to promote the partner, so a promotion never picks a node that misses a committed write. Without the record, one failure is enough: a planned switchover promotes B, which commits a write alone; A rejoins but has not caught up when B crashes, and A is promoted without the write (`NoOutdated`).
- **R2. Back to sync before the report.** Once the partner has acked the whole log, the primary returns to sync mode, and only then tells the witness the partner is in sync again, which clears the record. A report while still committing alone lets a planned switchover promote a standby that lacks the last write (`NoReportInSync`, with no failure at all).
- **R3. Disk before ack.** A node writes an entry to disk before it acks it, and the primary before it commits it. Otherwise a primary that crashes after committing alone resumes without the write (`NoDurableAck`).
- **R4. Only the primary's own epoch commits directly.** A primary commits an entry of its own epoch (and every entry before it with it). A new primary therefore appends a no-op entry first.
- **R5. Read barrier.** A linearizable read is served by a primary that holds its lease and has committed an entry of its own epoch. A newly promoted primary may not yet know that its predecessor committed the entries it inherited; without the barrier its first read misses a committed write (`NoReadBarrier`, after a planned switchover with no failure).
- **R6. Catch-up by log, else by snapshot.** A standby takes an Append at its own epoch only, if the entry before it matches (index and epoch); it drops a conflicting suffix, and answers with its match index, or with its commit index on a mismatch, from which the primary resends. A primary resends from the first index not acked when no ack comes within a timeout: without it, an Append or snapshot lost with the LAN is never resent and the standby never catches up (found as a deadlock). A standby that needs entries the primary has compacted gets a snapshot.
- **R7. A snapshot keeps what follows it.** A standby that already holds a snapshot's last entry (same index and epoch) keeps its log and only moves its snapshot point; otherwise the snapshot replaces its log. Replacing it anyway throws away entries the standby has already acked, which the primary may have committed (`NoKeepSuffix`: the primary then crashes and the standby is promoted without the write).

**Corrections for K2 (2026-10-17).** Two rules as first written cost availability, and K2 changed them; both are design switches in the model, checked below. `PromoteAlone = FALSE`: a promotion clears the record and the new primary (or a resumed one) starts in sync mode, going alone through the witness like any primary. As first written, every promotion recorded the partner out of date, so a pair with nothing to replicate could not fail back if the new primary crashed before reporting. `EagerSync = TRUE`: an alone primary may return to sync mode at any time (R2's report still needs the partner to hold every committed entry). As first written, it returned only once the partner had acked the whole log, which under steady writes never happens, so the record never cleared and the standby could never be promoted.

**The log's length is a performance choice, not a safety one.** Any node may compact up to its commit index at any time: with R6 and R7 the model loses nothing. The log only needs to reach back far enough that a standby back from a typical outage catches up by entries rather than by a snapshot. K2 sets that bound by bytes and age, and keeps entries a standby has not acked while the bound allows.

**What the lease does here.** Reads need the lease (R5): a primary cut off from its partner on the LAN and the serial line and from the witness loses its lease while it still leads; the witness promotes the standby, which commits, and without the lease rule the old primary then serves a stale read (`Links3NoLeaseReads`). That takes three failures at once, beyond the single-failure guarantee, but nothing else stops it. Writes do not need the lease: a stale primary gets no acks (a standby acks only at its own epoch) and cannot commit alone (the witness records only at its epoch), so with commits allowed without the lease no committed write is lost or replaced under the same three link failures (`Links3NoCommitRunning`, exhaustive). The design keeps "commit only while holding the lease" anyway, as DESIGN.md has it ("writes refused as `unavailable` without a lease"): it costs nothing, and a client is not told a write committed by a primary that is about to learn it was replaced. Random simulation of three failures with crashes as well (`Sim3NoLeaseReads`, 15 minutes, about 240,000 traces) did not find the stale read; the exhaustive run on link failures alone found it in 12 steps.

## How to run

```sh
# Requires Java 11+ and tla2tools.jar
# (https://github.com/tlaplus/tlaplus/releases), here at ~/tla/tla2tools.jar.
cd spikes/s20
bash mkcfg.sh                       # writes every config
./run.sh DesignSeq Design1         # exhaustive, checkpoints, into ~/tla-runs/s20-<date>
./run.sh EagerSeq Eager1           # K2's rules (PromoteAlone = FALSE, EagerSync = TRUE)
# One config by hand; each rule-off and vacuity config ends in seconds.
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -deadlock -config NoOutdated.cfg PairReplication.tla
# Random traces for the three-fault configs.
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -simulate num=2000000 -depth 60 -deadlock -config Sim2Design.cfg PairReplication.tla
```

`-deadlock` because the model stops once its bounds are reached (every write made, the log full).

## Model

- **Failover** is layer 1's (`docs/tla/FailoverL1.tla`): atomic steps, links up or down, and the lease abstracted as "it runs out only while its holder cannot renew" and "a grant or resume waits until the other node's lease is gone". Layer 2 (`FailoverL2.tla`) checks the timing behind that against the code. Promotion is one step at the witness. A planned switchover makes the primary step down and yield; the standby asks the witness at once.
- **The data path** has messages: Append (one entry, the one before it, the commit index), a commit-only Append once the partner has everything, Ack, Nack and snapshots, on a FIFO queue per direction over the LAN; a LAN fault loses what is queued. The serial line carries heartbeats only.
- **Nodes** append client writes and no-ops, write to disk, commit, compact, enter and leave alone mode, report to the witness, send and resend. A crash loses what is not on disk; the commit index is kept with the data.
- **The witness** keeps its epoch, last promotion and the out-of-date record on disk.
- **Faults**: a crash, the LAN, the serial line or one WAN link lost, the witness crashing. Restarts and heals are not faults; planned switchovers neither. `MaxDown` bounds how many components are failed at once, `MaxFaults` the faults over a whole run (`NoBound` for none). The guarantee is `MaxDown = 1` with no bound on the total (`DesignSeq`): a single point of failure, any number of times, a new failure possibly arriving while a node still catches up from the last. `Design1` (one fault in a whole run, any overlap) is a subset of it. Two failures at once are beyond the guarantee and checked by random simulation only.
- **Bounds**: `MaxEpoch = 3`, two client writes, a log of three entries, one frame per direction.

**Properties.** `NoLostWrite`: what clients were told was committed only grows. `CommittedAgree`: every node's committed entries are a prefix of that history, so no two nodes commit different entries at one index. `CommittedStable`: a live node never drops a committed entry. `LeaderComplete`: a primary that runs at the witness's epoch holds every committed write. `ReadLinearizable`: a read served under R5 returns exactly the committed history. `AtMostOneRunning`, from failover.

**Vacuity.** Each of these must be violated, to show the model reaches what the properties are about: a write committed after a failover, a commit alone, a catch-up from a snapshot, a standby dropping an uncommitted suffix, the witness refusing an out-of-date standby, a report clearing the record, a read after a failover, and a planned switchover handing over.

## Results

Checked on 2026-10-17 with TLC 2026.02.28 (OpenJDK 21) on the 16-core development machine.

| Config | Faults | Distinct states | Result |
| --- | --- | --- | --- |
| `DesignSeq` | one at a time, any number, plus a planned switchover | 2,158,274,739 | pass (6 h 27 min, 12 workers, depth 75)* |
| `Design1` | one in the whole run, plus a planned switchover | 22,942,055 | pass (3 min 33 s, 12 workers) |
| `NoOutdated` | one at a time | 10,830 (stopped) | `LeaderComplete` violated (R1): switchover, B commits alone, B crashes |
| `NoReportInSync` | none: a planned switchover | 2,871 (stopped) | `LeaderComplete` violated (R2) |
| `NoDurableAck` | one at a time | 4,288 (stopped) | `LeaderComplete` violated (R3) |
| `NoReadBarrier` | none: a planned switchover | 8,674 (stopped) | `ReadLinearizable` violated (R5) |
| `NoKeepSuffix` | one at a time | 6,730,293 (stopped) | `LeaderComplete` violated (R7) |
| `Links3NoLeaseReads` | three link failures at once, no crashes | 77,516 (stopped) | `ReadLinearizable` violated (R5's lease) |
| `Links3NoCommitRunning` | three link failures at once, no crashes | 5,775,685 | pass: commits need no lease |
| `Sim2Design` | two at a time, two in the run, plus a planned switchover | random: about 9.4 million traces, 575 million states | no violation (30 min time box, 12 workers) |
| `Vac_*` (eight) | one at a time | 124 to 12,543 (stopped) | each violated, as required |
| `Sync1` | as `Design1`, with `PromoteAlone = FALSE` | 36,602,522 | pass (6 min 9 s) |
| `SyncNoOutdated`, `SyncVac_*` (three) | one at a time, `PromoteAlone = FALSE` | under 13,000 each (stopped) | each violated, as required |
| `EagerSeq` | as `DesignSeq`, with `PromoteAlone = FALSE` and `EagerSync = TRUE` (K2's rules) | 4,300,097,334 | pass (14 h 34 min, 12 workers, depth 70, contended)* |
| `Eager1` | as `Design1`, with both | 59,429,764 | pass (14 min 32 s, 8 workers, contended) |
| `EagerNoReportInSync` | one at a time, both, R2's report allowed while alone | stopped early | `LeaderComplete` violated, as required |
| `EagerVac_Report` | one at a time, both | stopped early | `NoReport` violated, as required |

\* At two billion states and more, TLC's 64-bit fingerprints probably collided once or a few times (its estimates: 1.3 optimistic and 0.5 from the actual fingerprints for `DesignSeq`, 5.7 and 0.5 for `EagerSeq`), so a handful of states may have gone unexplored. `Design1`, a subset with no collision risk to speak of, passed too, and every rule-off config finds its counterexample within 7 million states. A second run with another fingerprint seed (`-fp 2`) would close that gap at the cost of another six and a half hours.

**Retransmit widened (2026-10-21).** `Retransmit` first rewound the next index to just past the partner's match. A new primary has matched nothing, so on a resend it sent its whole log again, which made a switchover's hand-over grow with the log (ROADMAP X2: from about 390 ms to 765 ms over an hour). It now rewinds to any index not yet acknowledged, the first or a later one; the implementation, with nothing acknowledged in the epoch, resends from where the last nack put it or from its no-op (the send window, which the model does not have, counts from there too). Rerun that day on the same machine (12 workers, idle at the start): `Design1` 29,630,926 distinct states, pass (5 min 10 s, depth 47); `Eager1` 82,040,501, pass (14 min 6 s, depth 49). The `Seq` configs were not rerun.
