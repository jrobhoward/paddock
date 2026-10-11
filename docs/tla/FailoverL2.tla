------------------------------ MODULE FailoverL2 ------------------------------
(*
 * Layer 2: failover of a singleton between an HA pair (A, B) with a witness
 * (W), as `paddock_host::failover` implements it, with time.
 *
 * Layer 1 (FailoverL1.tla, from spike S16) has atomic steps and abstracts
 * the lease as "it runs out only while its holder cannot renew". Here time
 * is discrete ticks, and every time the code keeps is an age or a countdown,
 * capped where larger values change nothing, so the state space is finite:
 *
 *   - Every link is a FIFO queue per direction. LAN and WAN can be up, held
 *     (frames kept until healed, as QUIC keeps unacknowledged data) or lost
 *     (frames dropped); the serial line is up or lost. On an up link a frame
 *     arrives within Delay ticks. A fault takes one direction or both.
 *   - Each tick a node may beat once and step once, in either order, or
 *     skip either (a slow node). A beat carries the node's status (role,
 *     promoting, epoch) and, from a confirming standby, an echo: the age of
 *     the newest beat it has seen from its partner.
 *   - A node keeps the last status per path and its age; a path is live
 *     while that age is at most Timeout.
 *   - The primary renews through its partner's confirming beats (by echo
 *     with EchoLease, else by arrival) and at the witness (by the renewal's
 *     send time). Its singleton runs while its lease countdown is positive.
 *   - The witness answers each request at once (Granted, NotYet with a wait,
 *     Refused) and keeps no pending request. Its epoch and last promotion
 *     survive its crashes; its renewal record too, unless ForgetRenewals: then
 *     it restarts counting the node it promoted as just renewed (the code).
 *   - A standby asks when it hears its partner on no path; it stops asking
 *     if it hears a primary again with no request in flight, or one past
 *     its retry time, whose answer was lost (cancel); it
 *     steps down after hearing its partner Down for a lease (stand-down).
 *   - Nodes crash and restart Down; their epoch survives only with
 *     PersistEpoch. Replies can be lost with their link.
 *   - A reply carries its request's age, so a lease (from a renewal or a
 *     grant) runs from when the request it answers was sent.
 *   - With OperatorConfirms, an operator may confirm that a standby's
 *     partner is gone while the standby hears it on no path and cannot
 *     reach the witness. The operator is right: the partner is down. Once
 *     the partner's lease is waited out the standby promotes itself without
 *     a lease (an override), and runs until its partner confirms it as
 *     standby; the confirmation lapses if it hears its partner first.
 *
 * Rounds. Each tick runs in phases, which keeps the interleavings to those
 * that matter: faults, then delivery of every frame due (in a fixed link
 * order), then one node's turn and the other's (either node first; in a
 * turn the node may beat, step, both in either order, or neither), then
 * the tick. A frame sent in a round arrives in the next (Delay = 1) unless
 * its link is held or lost.
 *
 * Sessions (C5) are not modelled: every delivered frame is acted on, which is
 * the worst case for stale beats. Clocks do not drift.
 *
 * The property: the singleton never runs on both nodes at once.
 *)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS A, B, W
CONSTANTS Lease, Margin, Timeout, Delay, MaxEpoch, QueueLen, MaxFaults
\* Design switches: TRUE is the current code, FALSE the code before the fix.
CONSTANTS EchoLease, ConfirmOnSend, PersistEpoch
\* Whether the witness's renewal record is lost when it crashes (FALSE before
\* the record was dropped from what it keeps on disk).
CONSTANT ForgetRenewals
\* Which faults the environment may inject.
CONSTANTS LanFaults, SerialFaults, WanFaults, Crashes, WitnessCrashes
\* Whether links can hold frames (else only lose them).
CONSTANT Holds
\* Whether an operator may confirm a standby's partner gone (R7 at layer 2).
CONSTANT OperatorConfirms

Pair == {A, B}
Partner(n) == IF n = A THEN B ELSE A
Paths == {"lan", "ser"}
Hold == Lease + Margin
\* Ages saturate here: beyond it, no comparison changes.
Cap == IF Timeout > Hold THEN Timeout + 1 ELSE Hold + 1
None == [role |-> "None", promoting |-> FALSE, epoch |-> 0]

Max(x, y) == IF x >= y THEN x ELSE y
Min(x, y) == IF x <= y THEN x ELSE y
Older(a) == Min(a + 1, Cap)

Lan(f, t) == <<"lan", f, t>>
Ser(f, t) == <<"ser", f, t>>
Wan(f, t) == <<"wan", f, t>>
Channels == {Lan(A, B), Lan(B, A), Ser(A, B), Ser(B, A)}
            \cup {Wan(n, W) : n \in Pair} \cup {Wan(W, n) : n \in Pair}

VARIABLES
    role, epoch, alive, lease,
    asking,       \* a standby asking to be promoted (its beats then confirm nothing)
    asked,        \* a request was sent in this episode (a grant would be taken)
    inflight,     \* a request's answer is due
    retry,        \* ticks until it may ask again
    confAge,      \* ticks since its beats last confirmed the partner's lease
    downAge,      \* ticks the partner has said Down, or -1
    view,         \* [path -> [st, age]]: the partner's last status per path
    seenAge,      \* age of the newest beat seen from the partner, or -1
    opConfirmed,  \* an operator confirmed the partner gone, not yet acted on
    overridden,   \* a primary running without a lease by that confirmation
    beaten, stepped,
    wEpoch, wPromoted, wRenewedAge, wAlive,
    chan, q, faults,
    phase,        \* "env", "deliver", "first", "second", "tick"
    first         \* the node whose turn comes first this round

nodeVars == <<role, epoch, alive, lease, asking, asked, inflight, retry, confAge,
              downAge, view, seenAge, opConfirmed, overridden, beaten, stepped>>
witnessVars == <<wEpoch, wPromoted, wRenewedAge, wAlive>>
vars == <<nodeVars, witnessVars, chan, q, faults, phase, first>>

Running(n) == alive[n] /\ role[n] = "Primary" /\ (lease[n] > 0 \/ overridden[n])
Confirming(n) == role[n] = "Standby" /\ ~asking[n]

Live(n, p) == view[n][p].age <= Timeout
AnyLive(n) == \E p \in Paths : Live(n, p)
LiveSt(n, lanOnly) == {view[n][p].st : p \in {x \in Paths : Live(n, x) /\ (~lanOnly \/ x = "lan")}}
PrimaryEpochs(n, lanOnly) == {s.epoch : s \in {t \in LiveSt(n, lanOnly) : t.role = "Primary"}}
PartnerDown(n) == \E s \in LiveSt(n, FALSE) : s.role = "Down"

Push(qq, c, m) ==
    IF chan[c] = "lost" \/ Len(qq[c]) >= QueueLen
    THEN qq
    ELSE [qq EXCEPT ![c] = Append(@, [m EXCEPT !.due = Delay])]

Init ==
    /\ role = [n \in Pair |-> "Down"]
    /\ epoch = [n \in Pair |-> 0]
    /\ alive = [n \in Pair |-> TRUE]
    /\ lease = [n \in Pair |-> 0]
    /\ asking = [n \in Pair |-> FALSE]
    /\ asked = [n \in Pair |-> FALSE]
    /\ inflight = [n \in Pair |-> FALSE]
    /\ retry = [n \in Pair |-> 0]
    /\ confAge = [n \in Pair |-> 0]
    /\ downAge = [n \in Pair |-> -1]
    /\ view = [n \in Pair |-> [p \in Paths |-> [st |-> None, age |-> 0]]]
    /\ seenAge = [n \in Pair |-> -1]
    /\ opConfirmed = [n \in Pair |-> FALSE]
    /\ overridden = [n \in Pair |-> FALSE]
    /\ beaten = [n \in Pair |-> FALSE]
    /\ stepped = [n \in Pair |-> FALSE]
    /\ wEpoch = 1
    /\ wPromoted = A
    /\ wRenewedAge = [n \in Pair |-> Cap]
    /\ wAlive = TRUE
    /\ chan = [c \in Channels |-> "up"]
    /\ q = [c \in Channels |-> <<>>]
    /\ faults = 0
    /\ phase = "env"
    /\ first = A

\* ------------------------------------------------------------------ time

\* A tick: once every frame due on an up link has arrived, everything ages.
Tick ==
    /\ phase = "tick"
    /\ phase' = "env"
    /\ UNCHANGED first
    /\ q' = [c \in Channels |->
               [i \in 1..Len(q[c]) |->
                  [q[c][i] EXCEPT !.age = Older(@),
                                  !.due = IF chan[c] = "up" THEN @ - 1 ELSE @]]]
    /\ lease' = [n \in Pair |-> Max(lease[n] - 1, 0)]
    /\ retry' = [n \in Pair |-> Max(retry[n] - 1, 0)]
    /\ confAge' = [n \in Pair |-> Older(confAge[n])]
    /\ downAge' = [n \in Pair |-> IF downAge[n] < 0 THEN -1 ELSE Older(downAge[n])]
    /\ seenAge' = [n \in Pair |-> IF seenAge[n] < 0 THEN -1 ELSE Older(seenAge[n])]
    /\ view' = [n \in Pair |-> [p \in Paths |-> [view[n][p] EXCEPT !.age = Older(@)]]]
    /\ wRenewedAge' = [n \in Pair |-> Older(wRenewedAge[n])]
    /\ beaten' = [n \in Pair |-> FALSE]
    /\ stepped' = [n \in Pair |-> FALSE]
    /\ UNCHANGED <<role, epoch, alive, asking, asked, inflight, opConfirmed, overridden,
                   wEpoch, wPromoted, wAlive, chan, faults>>

\* ------------------------------------------------------------------ faults

Faulty(c) == \/ c[1] = "lan" /\ LanFaults
             \/ c[1] = "ser" /\ SerialFaults
             \/ c[1] = "wan" /\ WanFaults
Rev(c) == <<c[1], c[3], c[2]>>
\* A fault takes one direction of a link, or both (one fault either way).
Dirs(c, both) == IF both THEN {c, Rev(c)} ELSE {c}

Hold1(c, both) ==
    /\ Holds /\ faults < MaxFaults /\ Faulty(c) /\ c[1] # "ser"
    /\ \A d \in Dirs(c, both) : chan[d] = "up"
    /\ chan' = [d \in Channels |-> IF d \in Dirs(c, both) THEN "held" ELSE chan[d]]
    /\ faults' = faults + 1
    /\ UNCHANGED <<nodeVars, witnessVars, q>>

Lose(c, both) ==
    /\ faults < MaxFaults /\ Faulty(c)
    /\ \E d \in Dirs(c, both) : chan[d] # "lost"
    /\ chan' = [d \in Channels |-> IF d \in Dirs(c, both) THEN "lost" ELSE chan[d]]
    /\ q' = [d \in Channels |-> IF d \in Dirs(c, both) THEN <<>> ELSE q[d]]
    /\ faults' = faults + 1
    /\ UNCHANGED <<nodeVars, witnessVars>>

\* Held frames move on, due within Delay of the heal.
Heal(c) ==
    /\ chan[c] # "up"
    /\ chan' = [chan EXCEPT ![c] = "up"]
    /\ q' = [q EXCEPT ![c] = [i \in 1..Len(@) |-> [@[i] EXCEPT !.due = Delay]]]
    /\ UNCHANGED <<nodeVars, witnessVars, faults>>

Crash(n) ==
    /\ Crashes /\ faults < MaxFaults /\ alive[n]
    /\ alive' = [alive EXCEPT ![n] = FALSE]
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ lease' = [lease EXCEPT ![n] = 0]
    /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
    /\ overridden' = [overridden EXCEPT ![n] = FALSE]
    /\ faults' = faults + 1
    /\ UNCHANGED <<epoch, asking, asked, inflight, retry, confAge, downAge, view,
                   seenAge, beaten, stepped, witnessVars, chan, q>>

\* A restart knows nothing but, with PersistEpoch, its epoch; its start
\* counts as its last confirmation of the partner's lease.
Restart(n) ==
    /\ ~alive[n]
    /\ alive' = [alive EXCEPT ![n] = TRUE]
    /\ epoch' = [epoch EXCEPT ![n] = IF PersistEpoch THEN @ ELSE 0]
    /\ asking' = [asking EXCEPT ![n] = FALSE]
    /\ asked' = [asked EXCEPT ![n] = FALSE]
    /\ inflight' = [inflight EXCEPT ![n] = FALSE]
    /\ retry' = [retry EXCEPT ![n] = 0]
    /\ confAge' = [confAge EXCEPT ![n] = 0]
    /\ downAge' = [downAge EXCEPT ![n] = -1]
    /\ view' = [view EXCEPT ![n] = [p \in Paths |-> [st |-> None, age |-> 0]]]
    /\ seenAge' = [seenAge EXCEPT ![n] = -1]
    /\ UNCHANGED <<role, lease, opConfirmed, overridden, beaten, stepped, witnessVars, chan,
                   q, faults>>

WitnessCrash ==
    /\ WitnessCrashes /\ faults < MaxFaults /\ wAlive
    /\ wAlive' = FALSE
    /\ faults' = faults + 1
    /\ UNCHANGED <<nodeVars, wEpoch, wPromoted, wRenewedAge, chan, q>>

\* With ForgetRenewals the record is lost: the node last promoted counts as
\* renewed at the restart, never earlier than its true last renewal; the
\* other has no record.
WitnessRestart ==
    /\ ~wAlive
    /\ wAlive' = TRUE
    /\ wRenewedAge' = IF ForgetRenewals
                       THEN [n \in Pair |-> IF n = wPromoted THEN 0 ELSE Cap]
                       ELSE wRenewedAge
    /\ UNCHANGED <<nodeVars, wEpoch, wPromoted, chan, q, faults>>

\* ------------------------------------------------------------------ beats

Status(n) == [role |-> role[n], promoting |-> asking[n], epoch |-> epoch[n]]

\* On LAN and serial; a primary also renews at the witness.
Beat(n) ==
    /\ alive[n] /\ ~beaten[n]
    /\ LET b == [k |-> "beat", st |-> Status(n),
                 echo |-> IF Confirming(n) THEN seenAge[n] ELSE -1, age |-> 0, due |-> 0]
           r == [k |-> "renew", epoch |-> epoch[n], age |-> 0, due |-> 0]
           q1 == Push(Push(q, Lan(n, Partner(n)), b), Ser(n, Partner(n)), b)
       IN q' = IF role[n] = "Primary" THEN Push(q1, Wan(n, W), r) ELSE q1
    /\ beaten' = [beaten EXCEPT ![n] = TRUE]
    /\ confAge' = IF ConfirmOnSend /\ Confirming(n) THEN [confAge EXCEPT ![n] = 0] ELSE confAge
    /\ UNCHANGED <<role, epoch, alive, lease, asking, asked, inflight, retry, downAge,
                   view, seenAge, opConfirmed, overridden, stepped, witnessVars, chan, faults>>

\* ------------------------------------------------------------------ steps

StepDown(n, e) ==
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ epoch' = [epoch EXCEPT ![n] = Max(@, e)]
    /\ lease' = [lease EXCEPT ![n] = 0]
    /\ asking' = [asking EXCEPT ![n] = FALSE]
    /\ asked' = [asked EXCEPT ![n] = FALSE]
    /\ inflight' = [inflight EXCEPT ![n] = FALSE]
    /\ confAge' = [confAge EXCEPT ![n] = 0]
    /\ downAge' = [downAge EXCEPT ![n] = -1]
    /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
    /\ overridden' = [overridden EXCEPT ![n] = FALSE]

\* Send a request; with ConfirmOnSend, a standby that was confirming until
\* this step counts now as its last confirmation.
Ask(n, resume) ==
    LET ago == IF ConfirmOnSend /\ Confirming(n) THEN 0 ELSE confAge[n]
    IN /\ asked' = [asked EXCEPT ![n] = TRUE]
       /\ inflight' = [inflight EXCEPT ![n] = TRUE]
       /\ retry' = [retry EXCEPT ![n] = Lease]
       /\ confAge' = [confAge EXCEPT ![n] = ago]
       /\ q' = Push(q, Wan(n, W), [k |-> "request", resume |-> resume, epoch |-> epoch[n],
                                   ago |-> ago, age |-> 0, due |-> 0])

Done(n) == stepped' = [stepped EXCEPT ![n] = TRUE]

\* `Fenced` by the partner, a primary at a higher epoch.
StepPrimary(n) ==
    /\ role[n] = "Primary"
    /\ \/ \E e \in PrimaryEpochs(n, FALSE) :
             /\ e > epoch[n]
             /\ StepDown(n, e)
             /\ UNCHANGED <<retry, q>>
       \/ /\ ~\E e \in PrimaryEpochs(n, FALSE) : e > epoch[n]
          /\ UNCHANGED <<role, epoch, lease, asking, asked, inflight, retry, confAge,
                         downAge, opConfirmed, overridden, q>>

TopEpoch(n) == Max(epoch[n], IF PrimaryEpochs(n, FALSE) = {} THEN 0
                             ELSE CHOOSE x \in PrimaryEpochs(n, FALSE) :
                                      \A y \in PrimaryEpochs(n, FALSE) : x >= y)

\* The partner is heard: maybe cancel; watch for it saying Down.
StandbyHears(n) ==
    LET e == TopEpoch(n)
        \* A request past its retry time counts as answered (its answer was
        \* lost); a grant after the cancel finds asked FALSE and is ignored.
        cancel == asking[n] /\ (~inflight[n] \/ retry[n] = 0) /\ PrimaryEpochs(n, FALSE) # {}
        newAsking == asking[n] /\ ~cancel
        \* The code before ConfirmOnSend refreshed the confirmation on every
        \* step that hears the partner.
        conf == IF ~ConfirmOnSend /\ ~newAsking THEN 0 ELSE confAge[n]
        down == IF ~PartnerDown(n) THEN -1 ELSE IF downAge[n] < 0 THEN 0 ELSE downAge[n]
    IN /\ AnyLive(n)
       /\ \/ /\ down > Hold
             /\ StepDown(n, e)
             /\ UNCHANGED <<retry, q>>
          \/ /\ down <= Hold
             /\ epoch' = [epoch EXCEPT ![n] = e]
             /\ asking' = [asking EXCEPT ![n] = newAsking]
             /\ asked' = [asked EXCEPT ![n] = IF cancel THEN FALSE ELSE @]
             /\ retry' = [retry EXCEPT ![n] = IF cancel THEN 0 ELSE @]
             /\ inflight' = [inflight EXCEPT ![n] = IF cancel THEN FALSE ELSE @]
             /\ confAge' = [confAge EXCEPT ![n] = conf]
             /\ downAge' = [downAge EXCEPT ![n] = down]
             \* An operator's confirmation is about this silence only.
             /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
             /\ UNCHANGED <<role, lease, overridden, q>>

\* Silent on every path: `Ask`, or with an operator's confirmation and the
\* partner's lease waited out, `Override`.
StandbySilent(n) ==
    /\ ~AnyLive(n)
    /\ \/ /\ opConfirmed[n] /\ confAge[n] >= Hold
          /\ role' = [role EXCEPT ![n] = "Primary"]
          /\ epoch' = [epoch EXCEPT ![n] = TopEpoch(n) + 1]
          /\ overridden' = [overridden EXCEPT ![n] = TRUE]
          /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
          /\ asking' = [asking EXCEPT ![n] = FALSE]
          /\ asked' = [asked EXCEPT ![n] = FALSE]
          /\ inflight' = [inflight EXCEPT ![n] = FALSE]
          /\ retry' = [retry EXCEPT ![n] = 0]
          /\ UNCHANGED <<lease, confAge, downAge, q>>
       \/ /\ ~(opConfirmed[n] /\ confAge[n] >= Hold)
          /\ epoch' = [epoch EXCEPT ![n] = TopEpoch(n)]
          /\ asking' = [asking EXCEPT ![n] = TRUE]
          /\ \/ /\ retry[n] = 0
                /\ Ask(n, FALSE)
             \/ /\ retry[n] > 0
                /\ UNCHANGED <<asked, inflight, retry, confAge, q>>
          /\ UNCHANGED <<role, lease, downAge, opConfirmed, overridden>>

StepStandby(n) ==
    /\ role[n] = "Standby"
    /\ StandbyHears(n) \/ StandbySilent(n)

RejoinOk(n) == \E e \in PrimaryEpochs(n, TRUE) : e >= epoch[n]

StepDownNode(n) ==
    /\ role[n] = "Down"
    /\ \/ \* `Rejoin` a primary heard on the LAN.
          \E e \in PrimaryEpochs(n, TRUE) :
             /\ e >= epoch[n]
             /\ role' = [role EXCEPT ![n] = "Standby"]
             /\ epoch' = [epoch EXCEPT ![n] = e]
             /\ asked' = [asked EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = 0]
             /\ confAge' = [confAge EXCEPT ![n] = 0]
             /\ UNCHANGED <<lease, asking, inflight, downAge, opConfirmed, overridden, q>>
       \/ \* `Resume`, once its own confirmations are out of date.
          /\ ~RejoinOk(n) /\ confAge[n] >= Hold /\ retry[n] = 0
          /\ Ask(n, TRUE)
          /\ UNCHANGED <<role, epoch, lease, asking, downAge, opConfirmed, overridden>>
       \/ /\ ~RejoinOk(n) /\ ~(confAge[n] >= Hold /\ retry[n] = 0)
          /\ UNCHANGED <<role, epoch, lease, asking, asked, inflight, retry, confAge,
                         downAge, opConfirmed, overridden, q>>

Step(n) ==
    /\ alive[n] /\ ~stepped[n]
    /\ \/ StepPrimary(n) \/ StepStandby(n) \/ StepDownNode(n)
    /\ Done(n)
    /\ UNCHANGED <<alive, view, seenAge, beaten, witnessVars, chan, faults>>

\* ------------------------------------------------------------------ delivery

Renew(n, age) == lease' = [lease EXCEPT ![n] = Max(@, Lease - Margin - age)]

RecvBeat(c, m) ==
    LET n == c[3]
        confirms == role[n] = "Primary" /\ m.st.role = "Standby" /\ ~m.st.promoting
                    /\ m.st.epoch <= epoch[n]
        age == IF EchoLease THEN m.echo + m.age ELSE 0
    IN /\ view' = [view EXCEPT ![n][c[1]] = [st |-> m.st, age |-> 0]]
       /\ seenAge' = [seenAge EXCEPT ![n] = IF @ < 0 THEN m.age ELSE Min(@, m.age)]
       /\ \/ /\ confirms /\ (~EchoLease \/ m.echo >= 0)
             /\ Renew(n, age)
             \* Confirmed as primary by its partner: an override ends.
             /\ overridden' = [overridden EXCEPT ![n] = FALSE]
          \/ /\ ~(confirms /\ (~EchoLease \/ m.echo >= 0))
             /\ UNCHANGED <<lease, overridden>>
       /\ q' = [q EXCEPT ![c] = Tail(@)]
       /\ UNCHANGED <<role, epoch, asking, asked, inflight, retry, confAge, downAge,
                      opConfirmed, beaten, stepped, witnessVars>>

Reply(qq, n, m) == Push(qq, Wan(W, n), m)

RecvAtWitness(c, m) ==
    LET n == c[2]
        p == Partner(n)
        qq == [q EXCEPT ![c] = Tail(@)]
        goneIn == Max(Hold - (m.ago + m.age), Hold - wRenewedAge[p])
        ok == IF m.resume THEN n = wPromoted ELSE m.epoch + 1 > wEpoch
        newEpoch == IF m.resume THEN wEpoch ELSE m.epoch + 1
    IN \/ /\ m.k = "renew" /\ m.epoch = wEpoch /\ n = wPromoted
          /\ wRenewedAge' = [wRenewedAge EXCEPT ![n] = 0]
          /\ q' = Reply(qq, n, [k |-> "leaseOk", age |-> m.age, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted>>
       \/ /\ m.k = "renew" /\ ~(m.epoch = wEpoch /\ n = wPromoted)
          /\ q' = Reply(qq, n, [k |-> "fenced", epoch |-> wEpoch, age |-> 0, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge>>
       \/ /\ m.k = "request" /\ ~ok
          /\ q' = Reply(qq, n, [k |-> "refused", epoch |-> wEpoch, age |-> 0, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge>>
       \/ /\ m.k = "request" /\ ok /\ goneIn > 0
          /\ q' = Reply(qq, n, [k |-> "notYet", wait |-> goneIn, age |-> 0, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge>>
       \/ /\ m.k = "request" /\ ok /\ goneIn <= 0
          /\ wEpoch' = newEpoch
          /\ wPromoted' = IF m.resume THEN wPromoted ELSE n
          /\ wRenewedAge' = [wRenewedAge EXCEPT ![n] = 0, ![p] = Cap]
          /\ q' = Reply(qq, n, [k |-> "granted", epoch |-> newEpoch, age |-> m.age, due |-> 0])

Keep(n) == UNCHANGED <<role, epoch, lease, asking, asked, inflight, retry, confAge, downAge,
                       opConfirmed, overridden>>

RecvReply(c, m) ==
    LET n == c[3]
        leaseOk == m.k = "leaseOk" /\ role[n] = "Primary"
        fenced == m.k = "fenced" /\ role[n] = "Primary" /\ m.epoch > epoch[n]
        granted == m.k = "granted" /\ role[n] # "Primary" /\ asked[n]
        refusedS == m.k = "refused" /\ role[n] = "Standby"
        refusedD == m.k = "refused" /\ role[n] = "Down"
    IN /\ q' = [q EXCEPT ![c] = Tail(@)]
       /\ \/ /\ leaseOk
             /\ Renew(n, m.age)
             /\ UNCHANGED <<role, epoch, asking, asked, inflight, retry, confAge, downAge,
                            opConfirmed, overridden>>
          \/ /\ fenced
             /\ StepDown(n, m.epoch)
             /\ UNCHANGED retry
          \/ /\ granted
             /\ role' = [role EXCEPT ![n] = "Primary"]
             /\ epoch' = [epoch EXCEPT ![n] = m.epoch]
             /\ lease' = [lease EXCEPT ![n] = Max(Lease - Margin - m.age, 0)]
             /\ asking' = [asking EXCEPT ![n] = FALSE]
             /\ asked' = [asked EXCEPT ![n] = FALSE]
             /\ inflight' = [inflight EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = 0]
             /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
             /\ overridden' = [overridden EXCEPT ![n] = FALSE]
             /\ UNCHANGED <<confAge, downAge>>
          \/ /\ m.k = "notYet"
             /\ inflight' = [inflight EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = m.wait]
             /\ UNCHANGED <<role, epoch, lease, asking, asked, confAge, downAge, opConfirmed,
                            overridden>>
          \/ /\ refusedS
             /\ StepDown(n, m.epoch)
             /\ retry' = [retry EXCEPT ![n] = Lease]
          \/ /\ refusedD
             /\ epoch' = [epoch EXCEPT ![n] = Max(@, m.epoch)]
             /\ asked' = [asked EXCEPT ![n] = FALSE]
             /\ inflight' = [inflight EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = Lease]
             /\ UNCHANGED <<role, lease, asking, confAge, downAge, opConfirmed, overridden>>
          \/ \* Anything else is no news to this standing.
             /\ ~leaseOk /\ ~fenced /\ ~granted /\ m.k # "notYet" /\ ~refusedS /\ ~refusedD
             /\ Keep(n)

\* Deliver the frame at the head of an up link; one to a dead receiver is lost.
Deliver(c) ==
    /\ chan[c] = "up" /\ q[c] # <<>>
    /\ LET m == Head(q[c])
           to == c[3]
       IN \/ /\ to = W /\ wAlive
             /\ RecvAtWitness(c, m)
             /\ UNCHANGED <<nodeVars, wAlive>>
          \/ /\ (to = W /\ ~wAlive) \/ (to # W /\ ~alive[to])
             /\ q' = [q EXCEPT ![c] = Tail(@)]
             /\ UNCHANGED <<nodeVars, witnessVars>>
          \/ /\ to # W /\ alive[to] /\ m.k = "beat"
             /\ RecvBeat(c, m)
             /\ UNCHANGED alive
          \/ /\ to # W /\ alive[to] /\ m.k # "beat"
             /\ RecvReply(c, m)
             /\ UNCHANGED <<alive, view, seenAge, beaten, stepped, witnessVars>>
    /\ UNCHANGED <<chan, faults>>

\* ------------------------------------------------------------------ spec

\* Links in the order frames are delivered within a round.
Order == <<Lan(A, B), Lan(B, A), Ser(A, B), Ser(B, A),
           Wan(A, W), Wan(B, W), Wan(W, A), Wan(W, B)>>
Deliverable(c) == chan[c] = "up" /\ q[c] # <<>> /\ Head(q[c]).due = 0
NextDeliverable(c) ==
    /\ Deliverable(c)
    /\ \A i \in 1..Len(Order) : Order[i] = c => \A j \in 1..(i - 1) : ~Deliverable(Order[j])

\* The witness cannot be reached from n: down, or its WAN not up either way.
WitnessUnreachable(n) == ~wAlive \/ chan[Wan(n, W)] # "up" \/ chan[Wan(W, n)] # "up"

\* An operator, asked by a standby that hears its partner on no path and
\* cannot reach the witness, confirms the partner gone. The operator is
\* right: the partner is down. Not a fault.
OperatorConfirm(n) ==
    /\ OperatorConfirms
    /\ alive[n] /\ role[n] = "Standby" /\ ~opConfirmed[n]
    /\ ~AnyLive(n) /\ WitnessUnreachable(n)
    /\ ~alive[Partner(n)]
    /\ opConfirmed' = [opConfirmed EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<role, epoch, alive, lease, asking, asked, inflight, retry, confAge,
                   downAge, view, seenAge, overridden, beaten, stepped, witnessVars,
                   chan, q, faults>>

Env ==
    \/ \E n \in Pair : OperatorConfirm(n)
    \/ \E c \in Channels : \E both \in BOOLEAN : Hold1(c, both) \/ Lose(c, both)
    \/ \E c \in Channels : Heal(c)
    \/ \E n \in Pair : Crash(n) \/ Restart(n)
    \/ WitnessCrash \/ WitnessRestart

Turn(n) == (phase = "first" /\ n = first) \/ (phase = "second" /\ n # first)

Next ==
    \/ phase = "env" /\ Env /\ UNCHANGED <<phase, first>>
    \/ phase = "env" /\ phase' = "deliver" /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, first>>
    \/ phase = "deliver" /\ \E c \in Channels : NextDeliverable(c) /\ Deliver(c)
       /\ UNCHANGED <<phase, first>>
    \/ /\ phase = "deliver" /\ ~\E c \in Channels : Deliverable(c)
       /\ \E f \in Pair : first' = f
       /\ phase' = "first"
       /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults>>
    \/ \E n \in Pair : Turn(n) /\ (Beat(n) \/ Step(n)) /\ UNCHANGED <<phase, first>>
    \/ /\ phase \in {"first", "second"}
       /\ phase' = IF phase = "first" THEN "second" ELSE "tick"
       /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, first>>
    \/ Tick

Spec == Init /\ [][Next]_vars

\* ------------------------------------------------------------------ properties

AtMostOneRunning == ~(Running(A) /\ Running(B))

AtMostOnePrimaryPerEpoch ==
    ~(alive[A] /\ alive[B] /\ role[A] = "Primary" /\ role[B] = "Primary"
      /\ epoch[A] = epoch[B])

EpochBound == wEpoch <= MaxEpoch /\ \A n \in Pair : epoch[n] <= MaxEpoch

=============================================================================
