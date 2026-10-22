---------------------------- MODULE PairReplication ----------------------------
(*
 * Spike S20 (ROADMAP K1): synchronous replication of a log between the HA
 * pair (A, B), with the witness breaking ties, on top of failover.
 *
 * Failover is as in layer 1 (docs/tla/FailoverL1.tla): atomic steps, links
 * up or down, the lease abstracted as "it runs out only while its holder
 * cannot renew" and "a grant or resume waits until the other node's lease
 * is gone". Layer 2 (FailoverL2.tla) checks the timing behind that against
 * the code. Here promotion is one step at the witness.
 *
 * The data path is what this model adds, with messages:
 *
 *   - The leader is the failover primary at its epoch. It appends client
 *     writes (and one no-op entry per epoch) to its log, each entry tagged
 *     with its epoch, and ships them to its partner over the LAN, one entry
 *     per Append, pipelined. The LAN is a FIFO queue per direction; a LAN
 *     fault loses what is queued.
 *   - A standby takes an Append only at its own epoch and only if the entry
 *     before it matches (index and epoch); it drops a conflicting suffix,
 *     appends, and answers Ack(match). A mismatch answers Nack with its
 *     commit index, from which the primary resends. The Append carries the
 *     primary's commit index.
 *   - A node compacts its log up to its commit index (a snapshot of the
 *     state at that index). A standby that needs entries the primary has
 *     compacted gets the snapshot instead, and acks its index. A standby
 *     that already holds the snapshot's last entry keeps its log
 *     (KeepSuffix). A primary that has no ack within a timeout sends again
 *     from the first index not acked.
 *   - In "sync" mode the primary commits an entry once its partner has
 *     acked it. In "alone" mode it commits on its own. A primary enters
 *     alone mode only through the witness (Outdated): the witness records
 *     that the partner of the node it promoted is out of date, and then
 *     refuses to promote that partner. A grant itself puts the new primary
 *     in alone mode (its partner is gone). Once the partner has caught up,
 *     the primary returns to sync mode and only then tells the witness its
 *     partner is in sync again (ReportOnlyInSync).
 *   - A primary commits only while it runs, that is, holds its lease
 *     (CommitWhileRunning), and only an entry of its own epoch (earlier ones
 *     commit with it, as in Raft).
 *   - A node writes an entry to disk before it acks it, and the primary
 *     before it commits it (DurableAck). A crash loses what is not on disk.
 *   - A read on request (linearizable) is served by a primary that runs
 *     (holds its lease) and has committed an entry of its own epoch
 *     (ReadBarrier), from its local copy at its commit index.
 *
 * Each design switch is TRUE for the design, FALSE to show what its rule is
 * for.
 *
 * Faults: a crash, the LAN, the serial line or a WAN link lost, the witness
 * crashing; restarts and heals are not faults. See MaxFaults and MaxDown.
 *
 * The properties: no committed write is ever lost or replaced (NoLostWrite,
 * CommittedAgree, CommittedStable); a primary that runs has every committed
 * write (LeaderComplete); a linearizable read sees every committed write
 * and nothing uncommitted (ReadLinearizable); the singleton never runs on
 * both nodes (AtMostOneRunning, from failover).
 *)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS A, B
CONSTANTS MaxEpoch, MaxWrites, MaxLog, QueueLen, MaxSwitchovers
\* MaxFaults bounds the faults over a whole run (-1: no bound); MaxDown how
\* many components may be failed at once (a node, a link, the witness). A
\* single point of failure is MaxFaults = -1, MaxDown = 1: one failure at a
\* time, any number over time, a new one possibly while a node still
\* catches up from the last.
CONSTANTS MaxFaults, MaxDown
\* Which faults the environment may inject, and whether nodes compact.
CONSTANTS Crashes, LanFaults, SerialFaults, WanFaults, WitnessCrashes, Compaction
\* Design switches.
CONSTANTS Outdated, ReportOnlyInSync, CommitWhileRunning, DurableAck, ReadBarrier,
          LeaseReads, KeepSuffix
\* PromoteAlone: a grant records the partner out of date and the new primary
\* starts alone, and a resumed one alone if the record stands. Otherwise
\* both start in sync and go alone like any primary, so a pair that never
\* commits alone never has a record standing.
CONSTANTS PromoteAlone
\* EagerSync: an alone primary may return to sync mode at any time, not only
\* once the partner holds the whole log (under steady writes it never does).
CONSTANTS EagerSync

Nodes == {A, B}
\* MaxFaults <- NoBound in a config (TLC configs take no negative numbers).
NoBound == -1
Partner(n) == IF n = A THEN B ELSE A
Max(x, y) == IF x >= y THEN x ELSE y
Min(x, y) == IF x <= y THEN x ELSE y

NoEntry == [e |-> 0, v |-> -1]
LastE(lg) == IF lg = <<>> THEN 0 ELSE lg[Len(lg)].e
EntE(lg, i) == IF i = 0 THEN 0 ELSE lg[i].e
Prefix(lg, i) == SubSeq(lg, 1, i)
IsPrefix(s, t) == Len(s) <= Len(t) /\ SubSeq(t, 1, Len(s)) = s

Chans == {<<A, B>>, <<B, A>>}

VARIABLES
    \* Failover, as layer 1.
    role,         \* "Primary", "Standby" or "Down"
    epoch,
    alive,
    lease,        \* holds a current lease
    yielding,     \* stepped down for a planned switchover, until it rejoins
    lanUp, serUp, wanUp, wAlive,
    wEpoch, wPromoted,
    \* Replication.
    wAlone,       \* the witness's record: the node it promoted commits alone,
                  \* its partner is out of date (on disk)
    log,          \* entries [e, v]; v = 0 for a no-op, else a write's id
    dur,          \* how much of the log is on disk
    commit,       \* commit index
    base,         \* the log is compacted up to here (a snapshot)
    mode,         \* a primary's commit rule: "sync" or "alone"
    next,         \* a primary's next index to send to its partner
    match,        \* a primary's highest index its partner acked at its epoch
    q,            \* LAN queue per direction
    \* Bookkeeping.
    acked,        \* every committed write in order (what clients were told)
    writes, faults, switchovers

foVars == <<role, epoch, alive, lease, yielding>>
netVars == <<lanUp, serUp, wanUp, wAlive>>
witVars == <<wEpoch, wPromoted, wAlone>>
dataVars == <<log, dur, commit, base>>
primVars == <<mode, next, match>>
vars == <<foVars, netVars, witVars, dataVars, primVars, q, acked, writes, faults,
          switchovers>>

Init ==
    /\ role = [n \in Nodes |-> IF n = A THEN "Primary" ELSE "Standby"]
    /\ epoch = [n \in Nodes |-> 1]
    /\ alive = [n \in Nodes |-> TRUE]
    /\ lease = [n \in Nodes |-> n = A]
    /\ yielding = [n \in Nodes |-> FALSE]
    /\ lanUp = TRUE /\ serUp = TRUE /\ wanUp = [n \in Nodes |-> TRUE] /\ wAlive = TRUE
    /\ wEpoch = 1 /\ wPromoted = A /\ wAlone = FALSE
    /\ log = [n \in Nodes |-> <<>>]
    /\ dur = [n \in Nodes |-> 0]
    /\ commit = [n \in Nodes |-> 0]
    /\ base = [n \in Nodes |-> 0]
    /\ mode = [n \in Nodes |-> "sync"]
    /\ next = [n \in Nodes |-> 1]
    /\ match = [n \in Nodes |-> 0]
    /\ q = [c \in Chans |-> <<>>]
    /\ acked = <<>>
    /\ writes = 0 /\ faults = 0 /\ switchovers = 0

\* ------------------------------------------------------------------ failover

PeerLan(n) == alive[n] /\ alive[Partner(n)] /\ lanUp
PeerSer(n) == alive[n] /\ alive[Partner(n)] /\ serUp
ReachW(n) == alive[n] /\ wanUp[n] /\ wAlive
SeesPartnerDown(n) == alive[n] /\ ~PeerLan(n) /\ ~PeerSer(n)
PartnerYields(n) == PeerLan(n) /\ role[Partner(n)] = "Down" /\ yielding[Partner(n)]
Running(n) == alive[n] /\ role[n] = "Primary" /\ lease[n]
LeaseGone(n) == ~alive[n] \/ ~lease[n]

CanRenew(n) ==
    /\ alive[n] /\ role[n] = "Primary"
    /\ \/ ReachW(n) /\ epoch[n] = wEpoch
       \/ /\ PeerLan(n) \/ PeerSer(n)
          /\ role[Partner(n)] = "Standby" /\ epoch[Partner(n)] <= epoch[n]

Renew(n) ==
    /\ ~lease[n] /\ CanRenew(n)
    /\ lease' = [lease EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<role, epoch, alive, yielding, netVars, witVars, dataVars, primVars, q,
                   acked, writes, faults, switchovers>>

\* A lease runs out only while its holder cannot renew.
Expire(n) ==
    /\ lease[n] /\ ~CanRenew(n)
    /\ lease' = [lease EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<role, epoch, alive, yielding, netVars, witVars, dataVars, primVars, q,
                   acked, writes, faults, switchovers>>

\* A primary starts sending its partner what it has from the end of its log.
TakeOver(n) ==
    /\ next' = [next EXCEPT ![n] = Len(log[n]) + 1]
    /\ match' = [match EXCEPT ![n] = 0]

\* The witness grants, once the old primary's lease is gone, and (Outdated)
\* not to a node it has recorded out of date. The grant is the new primary's
\* first lease, and it commits alone until its partner catches up.
Promote(n) ==
    /\ alive[n] /\ role[n] = "Standby" /\ ReachW(n)
    /\ SeesPartnerDown(n) \/ PartnerYields(n)
    /\ epoch[n] + 1 > wEpoch /\ epoch[n] + 1 <= MaxEpoch
    /\ LeaseGone(Partner(n))
    /\ Outdated => ~(wAlone /\ wPromoted # n)
    /\ role' = [role EXCEPT ![n] = "Primary"]
    /\ epoch' = [epoch EXCEPT ![n] = @ + 1]
    /\ lease' = [lease EXCEPT ![n] = TRUE]
    /\ wEpoch' = epoch[n] + 1
    /\ wPromoted' = n
    /\ wAlone' = (Outdated /\ PromoteAlone)
    /\ mode' = [mode EXCEPT ![n] = IF Outdated /\ ~PromoteAlone THEN "sync" ELSE "alone"]
    /\ TakeOver(n)
    /\ UNCHANGED <<alive, yielding, netVars, dataVars, q, acked, writes, faults, switchovers>>

StepDown(n, e) ==
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ epoch' = [epoch EXCEPT ![n] = e]
    /\ lease' = [lease EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<alive, yielding, netVars, witVars, dataVars, primVars, q, acked, writes,
                   faults, switchovers>>

FencedByPartner(n) ==
    /\ alive[n] /\ role[n] = "Primary" /\ (PeerLan(n) \/ PeerSer(n))
    /\ epoch[n] < epoch[Partner(n)]
    /\ StepDown(n, epoch[Partner(n)])

FencedByWitness(n) ==
    /\ alive[n] /\ role[n] = "Primary" /\ ReachW(n) /\ epoch[n] < wEpoch
    /\ StepDown(n, wEpoch)

Rejoin(n) ==
    /\ alive[n] /\ role[n] = "Down" /\ PeerLan(n)
    /\ role[Partner(n)] = "Primary" /\ epoch[Partner(n)] >= epoch[n]
    /\ role' = [role EXCEPT ![n] = "Standby"]
    /\ epoch' = [epoch EXCEPT ![n] = epoch[Partner(n)]]
    /\ yielding' = [yielding EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<alive, lease, netVars, witVars, dataVars, primVars, q, acked, writes,
                   faults, switchovers>>

\* The node the witness last promoted resumes when it cannot rejoin a
\* primary, once its partner's lease is gone. It commits alone if the
\* witness still records its partner out of date.
Resume(n) ==
    /\ alive[n] /\ role[n] = "Down" /\ ReachW(n) /\ wPromoted = n
    /\ ~(PeerLan(n) /\ role[Partner(n)] = "Primary")
    /\ LeaseGone(Partner(n))
    /\ role' = [role EXCEPT ![n] = "Primary"]
    /\ epoch' = [epoch EXCEPT ![n] = wEpoch]
    /\ lease' = [lease EXCEPT ![n] = TRUE]
    /\ yielding' = [yielding EXCEPT ![n] = FALSE]
    /\ mode' = [mode EXCEPT ![n] = IF wAlone /\ PromoteAlone THEN "alone" ELSE "sync"]
    /\ TakeOver(n)
    /\ UNCHANGED <<alive, netVars, witVars, dataVars, q, acked, writes, faults, switchovers>>

\* A planned switchover: a running primary that hears its partner as a
\* standby at its epoch steps down and yields. Not a fault.
Switchover(n) ==
    /\ switchovers < MaxSwitchovers
    /\ Running(n) /\ PeerLan(n)
    /\ role[Partner(n)] = "Standby" /\ epoch[Partner(n)] = epoch[n]
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ lease' = [lease EXCEPT ![n] = FALSE]
    /\ yielding' = [yielding EXCEPT ![n] = TRUE]
    /\ switchovers' = switchovers + 1
    /\ UNCHANGED <<epoch, alive, netVars, witVars, dataVars, primVars, q, acked, writes,
                   faults>>

\* ------------------------------------------------------------------ faults

DownNow == Cardinality({n \in Nodes : ~alive[n]}) + Cardinality({n \in Nodes : ~wanUp[n]})
           + (IF lanUp THEN 0 ELSE 1) + (IF serUp THEN 0 ELSE 1) + (IF wAlive THEN 0 ELSE 1)

Fault ==
    /\ DownNow < MaxDown
    /\ IF MaxFaults < 0 THEN UNCHANGED faults
       ELSE faults < MaxFaults /\ faults' = faults + 1

\* A crash loses what is not on disk; the commit index is kept with the data.
Crash(n) ==
    /\ Crashes /\ alive[n] /\ Fault
    /\ alive' = [alive EXCEPT ![n] = FALSE]
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ lease' = [lease EXCEPT ![n] = FALSE]
    /\ yielding' = [yielding EXCEPT ![n] = FALSE]
    /\ log' = [log EXCEPT ![n] = Prefix(@, dur[n])]
    /\ commit' = [commit EXCEPT ![n] = Min(@, dur[n])]
    /\ UNCHANGED <<epoch, netVars, witVars, dur, base, primVars, q, acked, writes,
                   switchovers>>

Restart(n) ==
    /\ ~alive[n]
    /\ alive' = [alive EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<role, epoch, lease, yielding, netVars, witVars, dataVars, primVars, q,
                   acked, writes, faults, switchovers>>

Others == <<foVars, witVars, dataVars, primVars, acked, writes, switchovers>>

LanFail == LanFaults /\ lanUp /\ Fault /\ lanUp' = FALSE /\ q' = [c \in Chans |-> <<>>]
           /\ UNCHANGED <<serUp, wanUp, wAlive, Others>>
LanHeal == ~lanUp /\ lanUp' = TRUE /\ UNCHANGED <<serUp, wanUp, wAlive, q, faults, Others>>
SerFail == SerialFaults /\ serUp /\ Fault /\ serUp' = FALSE
           /\ UNCHANGED <<lanUp, wanUp, wAlive, q, Others>>
SerHeal == ~serUp /\ serUp' = TRUE /\ UNCHANGED <<lanUp, wanUp, wAlive, q, faults, Others>>
WanFail(n) == WanFaults /\ wanUp[n] /\ Fault /\ wanUp' = [wanUp EXCEPT ![n] = FALSE]
              /\ UNCHANGED <<lanUp, serUp, wAlive, q, Others>>
WanHeal(n) == ~wanUp[n] /\ wanUp' = [wanUp EXCEPT ![n] = TRUE]
              /\ UNCHANGED <<lanUp, serUp, wAlive, q, faults, Others>>
\* The witness keeps its epoch, last promotion and record on disk.
WitnessCrash == WitnessCrashes /\ wAlive /\ Fault /\ wAlive' = FALSE
                /\ UNCHANGED <<lanUp, serUp, wanUp, q, Others>>
WitnessRestart == ~wAlive /\ wAlive' = TRUE
                  /\ UNCHANGED <<lanUp, serUp, wanUp, q, faults, Others>>

\* ------------------------------------------------------------------ writes

Leads(n) == alive[n] /\ role[n] = "Primary"

AppendOwn(n, v) ==
    /\ Len(log[n]) < MaxLog
    /\ log' = [log EXCEPT ![n] = Append(@, [e |-> epoch[n], v |-> v])]
    /\ dur' = IF DurableAck THEN [dur EXCEPT ![n] = Len(log[n]) + 1] ELSE dur
    /\ UNCHANGED <<commit, base>>

ClientWrite(n) ==
    /\ Leads(n) /\ writes < MaxWrites
    /\ AppendOwn(n, writes + 1)
    /\ writes' = writes + 1
    /\ UNCHANGED <<foVars, netVars, witVars, primVars, q, acked, faults, switchovers>>

\* A new primary's first entry, so that it can commit what it inherited.
NoOp(n) ==
    /\ Leads(n) /\ LastE(log[n]) # epoch[n]
    /\ AppendOwn(n, 0)
    /\ UNCHANGED <<foVars, netVars, witVars, primVars, q, acked, writes, faults, switchovers>>

\* Without DurableAck the disk catches up on its own time.
Persist(n) ==
    /\ alive[n] /\ dur[n] < Len(log[n])
    /\ dur' = [dur EXCEPT ![n] = Len(log[n])]
    /\ UNCHANGED <<foVars, netVars, witVars, log, commit, base, primVars, q, acked, writes,
                   faults, switchovers>>

Commit(n) ==
    /\ Leads(n)
    /\ CommitWhileRunning => lease[n]
    /\ \E c \in (commit[n] + 1)..Len(log[n]) :
          /\ log[n][c].e = epoch[n]
          /\ DurableAck => dur[n] >= c
          /\ mode[n] = "sync" => match[n] >= c
          /\ commit' = [commit EXCEPT ![n] = c]
          \* A commit that neither extends nor repeats what clients were
          \* told replaces a committed write: NoLostWrite fails.
          /\ acked' = IF IsPrefix(Prefix(log[n], c), acked) THEN acked ELSE Prefix(log[n], c)
    /\ UNCHANGED <<foVars, netVars, witVars, log, dur, base, primVars, q, writes, faults,
                   switchovers>>

\* No ack within its timeout: commit alone. With Outdated only through the
\* witness, at its epoch, which records the partner out of date; without, on
\* the primary's own judgement that its partner is gone.
GoAlone(n) ==
    /\ Leads(n) /\ mode[n] = "sync"
    /\ IF Outdated
       THEN /\ ReachW(n) /\ epoch[n] = wEpoch /\ wPromoted = n
            /\ wAlone' = TRUE
       ELSE /\ ~PeerLan(n)
            /\ UNCHANGED wAlone
    /\ mode' = [mode EXCEPT ![n] = "alone"]
    /\ UNCHANGED <<foVars, netVars, wEpoch, wPromoted, dataVars, next, match, q,
                   acked, writes, faults, switchovers>>

\* The partner has acked the whole log: commit in sync again.
GoSync(n) ==
    /\ Leads(n) /\ mode[n] = "alone" /\ (EagerSync \/ match[n] = Len(log[n]))
    /\ mode' = [mode EXCEPT ![n] = "sync"]
    /\ UNCHANGED <<foVars, netVars, witVars, dataVars, next, match, q, acked,
                   writes, faults, switchovers>>

\* Tell the witness the partner is in sync again: it may be promoted.
ReportSynced(n) ==
    /\ Outdated /\ Leads(n) /\ ReachW(n)
    /\ wAlone /\ wPromoted = n /\ wEpoch = epoch[n]
    /\ ReportOnlyInSync => mode[n] = "sync"
    /\ match[n] >= commit[n]
    /\ wAlone' = FALSE
    /\ UNCHANGED <<foVars, netVars, wEpoch, wPromoted, dataVars, primVars, q, acked, writes,
                   faults, switchovers>>

Compact(n) ==
    /\ Compaction /\ alive[n] /\ base[n] < commit[n] /\ commit[n] <= dur[n]
    /\ base' = [base EXCEPT ![n] = commit[n]]
    /\ UNCHANGED <<foVars, netVars, witVars, log, dur, commit, primVars, q, acked, writes,
                   faults, switchovers>>

\* ------------------------------------------------------------------ messages

Push(qq, c, m) == IF ~lanUp \/ Len(qq[c]) >= QueueLen THEN qq
                  ELSE [qq EXCEPT ![c] = Append(@, m)]

\* The next entry, the snapshot if that entry is compacted, or, once the
\* partner has been sent everything, the commit index alone (a heartbeat
\* on the data stream).
Send(n) ==
    LET p == Partner(n)
        c == <<n, p>>
        i == next[n]
        app == [k |-> "app", e |-> epoch[n], prev |-> i - 1, prevE |-> EntE(log[n], i - 1),
                ent |-> IF i <= Len(log[n]) THEN log[n][i] ELSE NoEntry, commit |-> commit[n]]
        snap == [k |-> "snap", e |-> epoch[n], idx |-> base[n], ents |-> Prefix(log[n], base[n])]
    IN /\ Leads(n) /\ lanUp /\ Len(q[c]) < QueueLen
       /\ \/ /\ i <= base[n]
             /\ q' = Push(q, c, snap)
             /\ next' = [next EXCEPT ![n] = base[n] + 1]
          \/ /\ base[n] < i /\ i <= Len(log[n])
             /\ q' = Push(q, c, app)
             /\ next' = [next EXCEPT ![n] = i + 1]
          \/ /\ i > Len(log[n]) /\ commit[n] > 0
             /\ q' = Push(q, c, app)
             /\ UNCHANGED next
       /\ UNCHANGED <<foVars, netVars, witVars, dataVars, mode, match, acked, writes, faults,
                      switchovers>>

\* No ack within its timeout: send again from an index not yet acked, the
\* first or a later one (a new primary with nothing acked resends from its
\* no-op, not from 1; a nack moves it back further). Without it, an Append
\* or snapshot lost with the LAN is never resent.
Retransmit(n) ==
    /\ Leads(n) /\ next[n] > match[n] + 1
    /\ \E i \in match[n] + 1 .. next[n] - 1 : next' = [next EXCEPT ![n] = i]
    /\ UNCHANGED <<foVars, netVars, witVars, dataVars, mode, match, q, acked, writes,
                   faults, switchovers>>

Reply(qq, p, m) == Push(qq, <<p, Partner(p)>>, m)

RecvApp(p, m, qq) ==
    LET ok == m.prev <= Len(log[p]) /\ EntE(log[p], m.prev) = m.prevE
        hasEnt == m.ent # NoEntry
        have == hasEnt /\ Len(log[p]) > m.prev /\ log[p][m.prev + 1] = m.ent
        newLog == IF ~hasEnt \/ have THEN log[p] ELSE Append(Prefix(log[p], m.prev), m.ent)
        last == IF hasEnt THEN m.prev + 1 ELSE m.prev
    IN \/ /\ ~ok
          /\ q' = Reply(qq, p, [k |-> "nack", e |-> epoch[p], hint |-> commit[p]])
          /\ UNCHANGED dataVars
       \/ /\ ok
          /\ log' = [log EXCEPT ![p] = newLog]
          /\ dur' = [dur EXCEPT ![p] = IF DurableAck THEN Len(newLog)
                                       ELSE IF newLog = log[p] THEN @ ELSE Min(@, m.prev)]
          /\ commit' = [commit EXCEPT ![p] = Max(@, Min(m.commit, last))]
          /\ q' = Reply(qq, p, [k |-> "ack", e |-> epoch[p], match |-> last])
          /\ UNCHANGED base

\* A snapshot replaces the standby's log, unless the standby already holds
\* its last entry (same index and epoch): then the entries after it may be
\* ones the standby has acked, and it keeps them (KeepSuffix).
RecvSnap(p, m, qq) ==
    LET holds == Len(log[p]) >= m.idx /\ EntE(log[p], m.idx) = EntE(m.ents, m.idx)
    IN /\ \/ /\ KeepSuffix /\ holds
             /\ commit' = [commit EXCEPT ![p] = Max(@, m.idx)]
             /\ UNCHANGED <<log, dur, base>>
          \/ /\ ~(KeepSuffix /\ holds) /\ m.idx > commit[p]
             /\ log' = [log EXCEPT ![p] = m.ents]
             /\ dur' = [dur EXCEPT ![p] = m.idx]
             /\ commit' = [commit EXCEPT ![p] = m.idx]
             /\ base' = [base EXCEPT ![p] = m.idx]
          \/ /\ ~(KeepSuffix /\ holds) /\ m.idx <= commit[p]
             /\ UNCHANGED dataVars
    /\ q' = Reply(qq, p, [k |-> "ack", e |-> epoch[p], match |-> m.idx])

RecvReply(n, m) ==
    \/ /\ m.k = "ack"
       /\ match' = [match EXCEPT ![n] = Max(@, m.match)]
       /\ next' = [next EXCEPT ![n] = Max(@, m.match + 1)]
    \/ /\ m.k = "nack"
       /\ next' = [next EXCEPT ![n] = Min(m.hint + 1, Len(log[n]) + 1)]
       /\ UNCHANGED match

\* Deliver the frame at the head of a direction. A standby takes Appends
\* and snapshots at its own epoch only; a primary takes replies at its own.
Deliver(c) ==
    LET to == c[2]
        m == Head(q[c])
        qq == [q EXCEPT ![c] = Tail(@)]
    IN /\ lanUp /\ q[c] # <<>>
       /\ \/ /\ alive[to] /\ role[to] = "Standby" /\ m.e = epoch[to] /\ m.k = "app"
             /\ RecvApp(to, m, qq)
             /\ UNCHANGED primVars
          \/ /\ alive[to] /\ role[to] = "Standby" /\ m.e = epoch[to] /\ m.k = "snap"
             /\ RecvSnap(to, m, qq)
             /\ UNCHANGED primVars
          \/ /\ alive[to] /\ role[to] = "Primary" /\ m.e = epoch[to]
             /\ m.k \in {"ack", "nack"}
             /\ q' = qq
             /\ RecvReply(to, m)
             /\ UNCHANGED <<dataVars, mode>>
          \/ /\ ~(alive[to] /\ role[to] = "Standby" /\ m.e = epoch[to] /\ m.k \in {"app", "snap"})
             /\ ~(alive[to] /\ role[to] = "Primary" /\ m.e = epoch[to] /\ m.k \in {"ack", "nack"})
             /\ q' = qq
             /\ UNCHANGED <<dataVars, primVars>>
       /\ UNCHANGED <<foVars, netVars, witVars, acked, writes, faults, switchovers>>

\* ------------------------------------------------------------------ spec

Node(n) ==
    \/ Renew(n) \/ Expire(n) \/ Promote(n) \/ FencedByPartner(n) \/ FencedByWitness(n)
    \/ Rejoin(n) \/ Resume(n)
    \/ ClientWrite(n) \/ NoOp(n) \/ Persist(n) \/ Commit(n) \/ GoAlone(n) \/ GoSync(n)
    \/ ReportSynced(n) \/ Compact(n) \/ Send(n) \/ Retransmit(n)

Env ==
    \/ \E n \in Nodes : Switchover(n) \/ Crash(n) \/ Restart(n) \/ WanFail(n) \/ WanHeal(n)
    \/ LanFail \/ LanHeal \/ SerFail \/ SerHeal \/ WitnessCrash \/ WitnessRestart

Next == (\E n \in Nodes : Node(n)) \/ (\E c \in Chans : Deliver(c)) \/ Env

Spec == Init /\ [][Next]_vars

\* ------------------------------------------------------------------ properties

AtMostOneRunning == ~(Running(A) /\ Running(B))

\* Every node's committed entries are a prefix of what clients were told:
\* no two nodes commit different entries at one index.
CommittedAgree == \A n \in Nodes : IsPrefix(Prefix(log[n], commit[n]), acked)

\* A primary that runs at the witness's epoch holds every committed write.
LeaderComplete ==
    \A n \in Nodes : Running(n) /\ epoch[n] = wEpoch => IsPrefix(acked, log[n])

CanRead(n) ==
    /\ alive[n] /\ role[n] = "Primary"
    /\ LeaseReads => lease[n]
    /\ ReadBarrier => commit[n] > 0 /\ log[n][commit[n]].e = epoch[n]

\* A linearizable read returns exactly what has been committed.
ReadLinearizable == \A n \in Nodes : CanRead(n) => Prefix(log[n], commit[n]) = acked

\* What clients were told only grows.
NoLostWrite == [][IsPrefix(acked, acked')]_vars

\* A live node never drops a committed entry.
CommittedStable ==
    [][\A n \in Nodes : alive[n] /\ alive'[n] => IsPrefix(Prefix(log[n], commit[n]), log'[n])]_vars

TypeOK ==
    /\ \A n \in Nodes : base[n] <= commit[n] /\ commit[n] <= Len(log[n]) /\ dur[n] <= Len(log[n])
    /\ \A n \in Nodes : Leads(n) => next[n] >= 1 /\ next[n] <= Len(log[n]) + 1

\* ------------------------------------------------------------------ vacuity
\* Each must be violated by its config, to show the model reaches what the
\* properties are about.

\* A write is committed after a failover (by a node at epoch 2 or later).
NoWriteAfterFailover == ~\E i \in 1..Len(acked) : acked[i].v > 0 /\ acked[i].e >= 2
\* A primary commits alone.
NoAloneCommit == [][~\E n \in Nodes : mode[n] = "alone" /\ commit'[n] > commit[n]
                                      /\ role[n] = "Primary"]_vars
\* A standby catches up from a snapshot.
NoSnapshotCatchUp == [][~\E n \in Nodes : role[n] = "Standby" /\ base'[n] > base[n]
                                          /\ commit'[n] > commit[n]]_vars
\* A standby drops an uncommitted suffix it had from an earlier epoch.
NoTruncate == [][~\E n \in Nodes : role[n] = "Standby" /\ alive[n] /\ alive'[n]
                                   /\ ~IsPrefix(log[n], log'[n])]_vars
\* The witness refuses an out-of-date standby that would otherwise be promoted.
NoRefusal == ~\E n \in Nodes :
    /\ alive[n] /\ role[n] = "Standby" /\ ReachW(n) /\ SeesPartnerDown(n)
    /\ epoch[n] + 1 > wEpoch /\ epoch[n] + 1 <= MaxEpoch /\ LeaseGone(Partner(n))
    /\ wAlone /\ wPromoted # n
\* A primary tells the witness its partner is in sync again.
NoReport == [][~(wAlone /\ ~wAlone')]_vars
\* A read is served after a failover.
NoReadAfterFailover == ~\E n \in Nodes : CanRead(n) /\ epoch[n] >= 2 /\ acked # <<>>
\* A planned switchover hands over: the old standby commits at a new epoch.
NoHandover == ~(switchovers > 0 /\ \E i \in 1..Len(acked) : acked[i].e >= 2)

=============================================================================
