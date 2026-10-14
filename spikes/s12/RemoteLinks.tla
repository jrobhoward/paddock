----------------------------- MODULE RemoteLinks -----------------------------
(*
 * Spike S12 (ROADMAP C4): links and monitors between two nodes, with C5's
 * sessions, pid incarnations and node creations, over a network that
 * stalls, closes connections and (in the adversarial configs) loses frames
 * or refuses them when a send queue is full.
 *
 * The model follows the code as of 2026-10-14:
 *   - crates/paddock-host/src/delivery.rs (link, unlink, monitor,
 *     demonitor, notify_exit),
 *   - crates/paddock-host/src/transport/mod.rs (send, deliver, end_session),
 *   - crates/paddock-host/src/transport/liveness.rs (on_frame, on_heartbeat,
 *     check, update),
 *   - crates/paddock-host/src/transport/quic.rs (one connection per pair,
 *     a frame lost only with its connection, a send queue of 4,096 frames,
 *     a close seen by each side on its own, a stale close ignored once a
 *     new connection replaced it).
 *
 * Abstractions:
 *   - Two nodes, one instance slot each; an instance restarts with the next
 *     incarnation, a node with the next creation. Pids are
 *     [node, creation, incarnation]. Only links and monitors between the
 *     nodes are modeled; local ones never touch the transport.
 *   - The data stream is an ordered queue per direction. Heartbeats travel
 *     on the control stream, so they may overtake data: a Heartbeat action
 *     reads the sender's state directly.
 *   - No clocks. A node may find a peer silent when no beat can reach it
 *     (GoSilent), or spuriously, as a delayed heartbeat does (a fault).
 *   - Exit and down notices only remove table entries; whether a non-
 *     trapping instance then dies is covered by Exit being possible at any
 *     time.
 *)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS A, B, MaxInc, MaxCr, MaxRef, MaxSession, MaxChan, MaxFaults
\* Faults the environment may inject.
CONSTANTS CanCut,        \* a direction stalls (frames held), and heals
          CanClose,      \* the connection closes (frames in flight lost)
          CanCrash,      \* a node crashes and restarts with a new creation
          CanSpurious,   \* a node finds its peer silent without a cause
          CanQueueFull,  \* a send is refused while the peer is reachable
          CanDrop        \* a frame is lost without its connection closing
\* Candidate fix: a notice (LinkExit, Down, Unlink, Demonitor, a noproc
\* answer) that cannot be sent ends the session with that node.
CONSTANTS FixFailedNotice
\* Candidate fix: OTP 23's unlink protocol. Unlink carries an id and is
\* acknowledged; until the ack, the unlinking side ignores a Link from the
\* other side, whose half the Unlink will remove.
CONSTANTS FixUnlinkAck
\* Candidate fix: the close of a connection that was the pair's current one
\* ends the session even after a redial replaced it (quic.rs ignores it).
CONSTANTS FixReplacedClose
\* Candidate fix: no link or monitor frames to a node before a frame from
\* its current start was heard (as built, a fresh peer is reachable at once
\* and its session numbering is adopted on first contact without ending).
CONSTANTS FixContactFirst

Nodes == {A, B}
Other(n) == IF n = A THEN B ELSE A

Pid == [node : Nodes, cr : 1..MaxCr, inc : 1..MaxInc]
Frame == [k : {"Link", "Unlink", "UnlinkAck", "LinkExit", "Monitor", "Demonitor", "Down"},
          a : Pid, b : Pid, ref : 0..MaxRef]
Packet == [cr : 1..MaxCr, to : 0..MaxCr, s : Nat, f : Frame]

VARIABLES
    up,         \* [Nodes -> BOOLEAN]  frames from n reach Other(n) (not stalled)
    conn,       \* BOOLEAN  the pair's connection is open
    closePend,  \* [Nodes -> BOOLEAN]  n has yet to see the last close
    nodeUp,     \* [Nodes -> BOOLEAN]
    cr,         \* [Nodes -> 1..MaxCr]  creation
    inc,        \* [Nodes -> 1..MaxInc]  incarnation of the instance
    alive,      \* [Nodes -> BOOLEAN]  the instance runs
    knownCr,    \* [Nodes -> 0..MaxCr]  Other(n)'s creation as n knows it; 0: none
    sess,       \* [Nodes -> Nat]  n's session number for the pair
    heard,      \* [Nodes -> BOOLEAN]  n's link from Other(n) is not silent
    hearsMe,    \* [Nodes -> BOOLEAN]  Other(n)'s last heartbeat said it hears n
    reach,      \* [Nodes -> BOOLEAN]  n sends frames to Other(n)
    links,      \* [Nodes -> SUBSET [l : Pid, r : Pid]]  local l linked to remote r
    watching,   \* [Nodes -> SUBSET [w : Pid, t : Pid, ref]]  local w monitors remote t
    monitors,   \* [Nodes -> SUBSET [t : Pid, w : Pid, ref]]  remote w monitors local t
    unlinking,  \* [Nodes -> SUBSET [l : Pid, r : Pid, ref]]  unlinks awaiting their ack
    nextRef,    \* [Nodes -> [mon : 0..MaxRef, unl : 0..MaxRef]]
    chan,       \* [Nodes -> Seq(Packet)]  data frames from n to Other(n)
    faults      \* faults injected so far

vars == <<up, conn, closePend, nodeUp, cr, inc, alive, knownCr, sess, heard,
          hearsMe, reach, links, watching, monitors, unlinking, nextRef, chan, faults>>
peerVars == <<knownCr, sess, heard, hearsMe, reach>>
tables == <<links, watching, monitors, unlinking>>

Me(n) == [node |-> n, cr |-> cr[n], inc |-> inc[n]]
Live(p) == nodeUp[p.node] /\ alive[p.node] /\ p = Me(p.node)
\* Pids of m that exist or existed.
PidsOf(m) == {p \in Pid : p.node = m /\ (p.cr < cr[m] \/ (p.cr = cr[m] /\ p.inc <= inc[m]))}

SetToSeq(S) == CHOOSE f \in [1..Cardinality(S) -> S] :
                   \A i, j \in 1..Cardinality(S) : i # j => f[i] # f[j]

-----------------------------------------------------------------------------
(* Sending (transport::send). A send is refused while the peer is not
   reachable; with the connection closed but the close not yet seen, the
   frame is queued and lost with the connection. *)

CanSendW(n, r) ==
    /\ nodeUp[n] /\ r /\ (conn \/ closePend[n])
    /\ ~FixContactFirst \/ knownCr[n] # 0

\* How many of len frames are queued: all, none if refused, or (queue full)
\* a prefix.
SendKW(n, r, len) ==
    IF len = 0 \/ ~CanSendW(n, r) THEN {0}
    ELSE IF CanQueueFull /\ faults < MaxFaults THEN 0..len
    ELSE {len}
SendK(n, len) == SendKW(n, reach[n], len)
QueueFullCost(n, r, k, len) == IF CanSendW(n, r) /\ k < len THEN 1 ELSE 0

PktW(n, s, to, f) == [cr |-> cr[n], to |-> to, s |-> s, f |-> f]
Pkt(n, f) == PktW(n, sess[n], knownCr[n], f)
\* Queue the first k frames of seq (nothing reaches a closed connection).
Queued(n, s, to, seq, k) ==
    IF conn THEN chan[n] \o [i \in 1..k |-> PktW(n, s, to, seq[i])] ELSE chan[n]

\* A notice could not be sent: with the fix, end the session.
EndOnFail(len, k) == FixFailedNotice /\ k < len

-----------------------------------------------------------------------------
(* Receiving (liveness::on_frame): how a packet's creation and session
   compare with what the receiver n knows of its sender. *)

Classify(n, pcr, ps) ==
    IF knownCr[n] # 0 /\ pcr < knownCr[n] THEN "Stale"
    ELSE IF knownCr[n] # pcr
         THEN IF knownCr[n] # 0 THEN "NewerFresh" ELSE "CurrentFresh"
    ELSE IF ps < sess[n] THEN "Stale"
    ELSE IF ps > sess[n] THEN "Newer"
    ELSE "Current"

IsFresh(c) == c \in {"NewerFresh", "CurrentFresh"}
Clears(c) == c \in {"NewerFresh", "Newer"}

\* The frame f at node m, on tables L, W, M: new tables, and the answer to
\* send back, if any (a set of at most one frame).
Handle(m, f, L, W, M, U) ==
    CASE f.k = "Link" ->
            IF \E u \in U : u.l = f.b /\ u.r = f.a
            THEN [L |-> L, W |-> W, M |-> M, U |-> U, ans |-> {}]
            ELSE IF Live(f.b)
            THEN [L |-> L \cup {[l |-> f.b, r |-> f.a]}, W |-> W, M |-> M, U |-> U, ans |-> {}]
            ELSE [L |-> L, W |-> W, M |-> M, U |-> U,
                  ans |-> {[k |-> "LinkExit", a |-> f.b, b |-> f.a, ref |-> 0]}]
      [] f.k = "Unlink" ->
            [L |-> L \ {[l |-> f.b, r |-> f.a]}, W |-> W, M |-> M, U |-> U,
             ans |-> IF FixUnlinkAck
                     THEN {[k |-> "UnlinkAck", a |-> f.b, b |-> f.a, ref |-> f.ref]}
                     ELSE {}]
      [] f.k = "UnlinkAck" ->
            [L |-> L, W |-> W, M |-> M, U |-> U \ {[l |-> f.b, r |-> f.a, ref |-> f.ref]},
             ans |-> {}]
      [] f.k = "LinkExit" ->
            [L |-> L \ {[l |-> f.b, r |-> f.a]}, W |-> W, M |-> M, U |-> U, ans |-> {}]
      [] f.k = "Monitor" ->
            IF Live(f.b)
            THEN [L |-> L, W |-> W, M |-> M \cup {[t |-> f.b, w |-> f.a, ref |-> f.ref]},
                  U |-> U, ans |-> {}]
            ELSE [L |-> L, W |-> W, M |-> M, U |-> U,
                  ans |-> {[k |-> "Down", a |-> f.b, b |-> f.a, ref |-> f.ref]}]
      [] f.k = "Demonitor" ->
            [L |-> L, W |-> W, M |-> {x \in M : ~(x.w = f.a /\ x.ref = f.ref)}, U |-> U,
             ans |-> {}]
      [] f.k = "Down" ->
            [L |-> L, W |-> W \ {[w |-> f.b, t |-> f.a, ref |-> f.ref]}, M |-> M, U |-> U,
             ans |-> {}]

-----------------------------------------------------------------------------

Init ==
    /\ up = [n \in Nodes |-> TRUE]
    /\ conn = TRUE
    /\ closePend = [n \in Nodes |-> FALSE]
    /\ nodeUp = [n \in Nodes |-> TRUE]
    /\ cr = [n \in Nodes |-> 1]
    /\ inc = [n \in Nodes |-> 1]
    /\ alive = [n \in Nodes |-> TRUE]
    /\ knownCr = [n \in Nodes |-> 1]
    /\ sess = [n \in Nodes |-> 0]
    /\ heard = [n \in Nodes |-> TRUE]
    /\ hearsMe = [n \in Nodes |-> TRUE]
    /\ reach = [n \in Nodes |-> TRUE]
    /\ links = [n \in Nodes |-> {}]
    /\ watching = [n \in Nodes |-> {}]
    /\ monitors = [n \in Nodes |-> {}]
    /\ unlinking = [n \in Nodes |-> {}]
    /\ nextRef = [n \in Nodes |-> [mon |-> 0, unl |-> 0]]
    /\ chan = [n \in Nodes |-> <<>>]
    /\ faults = 0

ClearTables(n) ==
    /\ links' = [links EXCEPT ![n] = {}]
    /\ watching' = [watching EXCEPT ![n] = {}]
    /\ monitors' = [monitors EXCEPT ![n] = {}]
    /\ unlinking' = [unlinking EXCEPT ![n] = {}]

-----------------------------------------------------------------------------
(* The instance on n (delivery.rs). *)

LinkTo(n, r) ==
    /\ nodeUp[n] /\ alive[n]
    /\ r \in PidsOf(Other(n))
    /\ ~\E h \in links[n] : h.r = r
    /\ \E k \in SendK(n, 1) :
        /\ links' = [links EXCEPT ![n] = IF k = 1 THEN @ \cup {[l |-> Me(n), r |-> r]} ELSE @]
        /\ chan' = [chan EXCEPT ![n] =
                       Queued(n, sess[n], knownCr[n],
                              <<[k |-> "Link", a |-> Me(n), b |-> r, ref |-> 0]>>, k)]
        /\ faults' = faults + QueueFullCost(n, reach[n], k, 1)
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive, peerVars,
                   watching, monitors, unlinking, nextRef>>

Unlink(n, h) ==
    /\ nodeUp[n] /\ alive[n]
    /\ h \in links[n]
    /\ ~FixUnlinkAck \/ nextRef[n].unl < MaxRef
    /\ LET id == IF FixUnlinkAck THEN nextRef[n].unl + 1 ELSE 0 IN
       \E k \in SendK(n, 1) :
        LET endNow == EndOnFail(1, k) IN
        /\ IF endNow
           THEN /\ ClearTables(n)
                /\ sess' = [sess EXCEPT ![n] = @ + 1]
           ELSE /\ links' = [links EXCEPT ![n] = @ \ {h}]
                /\ unlinking' = [unlinking EXCEPT ![n] =
                       IF FixUnlinkAck /\ k = 1 THEN @ \cup {[l |-> h.l, r |-> h.r, ref |-> id]}
                       ELSE @]
                /\ UNCHANGED <<watching, monitors, sess>>
        /\ chan' = [chan EXCEPT ![n] =
                       Queued(n, sess[n], knownCr[n],
                              <<[k |-> "Unlink", a |-> h.l, b |-> h.r, ref |-> id]>>, k)]
        /\ nextRef' = [nextRef EXCEPT ![n].unl = id]
        /\ faults' = faults + QueueFullCost(n, reach[n], k, 1)
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive,
                   knownCr, heard, hearsMe, reach>>

MonitorTo(n, t) ==
    /\ nodeUp[n] /\ alive[n]
    /\ t \in PidsOf(Other(n))
    /\ nextRef[n].mon < MaxRef
    /\ LET ref == nextRef[n].mon + 1 IN
       \E k \in SendK(n, 1) :
        /\ watching' = [watching EXCEPT ![n] =
                           IF k = 1 THEN @ \cup {[w |-> Me(n), t |-> t, ref |-> ref]} ELSE @]
        /\ chan' = [chan EXCEPT ![n] =
                       Queued(n, sess[n], knownCr[n],
                              <<[k |-> "Monitor", a |-> Me(n), b |-> t, ref |-> ref]>>, k)]
        /\ nextRef' = [nextRef EXCEPT ![n].mon = ref]
        /\ faults' = faults + QueueFullCost(n, reach[n], k, 1)
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive, peerVars,
                   links, monitors, unlinking>>

Demonitor(n, x) ==
    /\ nodeUp[n] /\ alive[n]
    /\ x \in watching[n]
    /\ \E k \in SendK(n, 1) :
        LET endNow == EndOnFail(1, k) IN
        /\ IF endNow
           THEN /\ ClearTables(n)
                /\ sess' = [sess EXCEPT ![n] = @ + 1]
           ELSE /\ watching' = [watching EXCEPT ![n] = @ \ {x}]
                /\ UNCHANGED <<links, monitors, unlinking, sess>>
        /\ chan' = [chan EXCEPT ![n] =
                       Queued(n, sess[n], knownCr[n],
                              <<[k |-> "Demonitor", a |-> x.w, b |-> x.t, ref |-> x.ref]>>, k)]
        /\ faults' = faults + QueueFullCost(n, reach[n], k, 1)
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive,
                   knownCr, heard, hearsMe, reach, nextRef>>

\* notify_exit: a LinkExit per link, a Down per monitor on it, a Demonitor
\* per monitor it held.
Exit(n) ==
    /\ nodeUp[n] /\ alive[n]
    /\ LET batch == SetToSeq(
                  {[k |-> "LinkExit", a |-> h.l, b |-> h.r, ref |-> 0] : h \in links[n]}
           \cup {[k |-> "Down", a |-> x.t, b |-> x.w, ref |-> x.ref] : x \in monitors[n]}
           \cup {[k |-> "Demonitor", a |-> x.w, b |-> x.t, ref |-> x.ref] : x \in watching[n]})
           len == Len(batch)
       IN \E k \in SendK(n, len) :
        /\ alive' = [alive EXCEPT ![n] = FALSE]
        /\ ClearTables(n)
        /\ sess' = [sess EXCEPT ![n] = IF EndOnFail(len, k) THEN @ + 1 ELSE @]
        /\ chan' = [chan EXCEPT ![n] = Queued(n, sess[n], knownCr[n], batch, k)]
        /\ faults' = faults + QueueFullCost(n, reach[n], k, len)
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc,
                   knownCr, heard, hearsMe, reach, nextRef>>

\* A supervisor restarts the instance with the next incarnation.
Spawn(n) ==
    /\ nodeUp[n] /\ ~alive[n] /\ inc[n] < MaxInc
    /\ inc' = [inc EXCEPT ![n] = @ + 1]
    /\ alive' = [alive EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, peerVars, tables,
                   nextRef, chan, faults>>

-----------------------------------------------------------------------------
(* Frames and heartbeats arriving (transport::deliver, liveness). *)

\* The next data frame from n arrives at m.
Deliver(n) ==
    LET m == Other(n)
        pk == Head(chan[n])
    IN
    /\ conn /\ up[n] /\ nodeUp[m] /\ chan[n] # <<>>
    /\ IF pk.to # 0 /\ pk.to # cr[m]
       THEN \* Meant for an earlier start of m.
            /\ chan' = [chan EXCEPT ![n] = Tail(@)]
            /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive,
                           peerVars, tables, nextRef, faults>>
       ELSE
       LET c == Classify(m, pk.cr, pk.s)
           s1 == IF IsFresh(c) \/ c = "Newer" THEN pk.s ELSE sess[m]
           r1 == IF IsFresh(c) THEN TRUE ELSE reach[m]
           kc1 == IF IsFresh(c) THEN pk.cr ELSE knownCr[m]
           L0 == IF Clears(c) THEN {} ELSE links[m]
           W0 == IF Clears(c) THEN {} ELSE watching[m]
           M0 == IF Clears(c) THEN {} ELSE monitors[m]
           U0 == IF Clears(c) THEN {} ELSE unlinking[m]
           h == IF c = "Stale"
                THEN [L |-> L0, W |-> W0, M |-> M0, U |-> U0, ans |-> {}]
                ELSE Handle(m, pk.f, L0, W0, M0, U0)
           alen == Cardinality(h.ans)
           ans == SetToSeq(h.ans)
       IN \E k \in SendKW(m, r1, alen) :
            LET endNow == EndOnFail(alen, k) IN
            /\ links' = [links EXCEPT ![m] = IF endNow THEN {} ELSE h.L]
            /\ watching' = [watching EXCEPT ![m] = IF endNow THEN {} ELSE h.W]
            /\ monitors' = [monitors EXCEPT ![m] = IF endNow THEN {} ELSE h.M]
            /\ unlinking' = [unlinking EXCEPT ![m] = IF endNow THEN {} ELSE h.U]
            /\ sess' = [sess EXCEPT ![m] = IF endNow THEN s1 + 1 ELSE s1]
            /\ knownCr' = [knownCr EXCEPT ![m] = kc1]
            /\ heard' = [heard EXCEPT ![m] = TRUE]
            /\ hearsMe' = [hearsMe EXCEPT ![m] = IF IsFresh(c) THEN TRUE ELSE @]
            /\ reach' = [reach EXCEPT ![m] = r1]
            /\ chan' = [chan EXCEPT ![n] = Tail(@),
                                    ![m] = Queued(m, s1, kc1, ans, k)]
            /\ faults' = faults + QueueFullCost(m, r1, k, alen)
            /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive, nextRef>>

\* A heartbeat from n arrives at m on the control stream. Accepted only if
\* meant for m's creation; read even from an ended session.
Heartbeat(n) ==
    LET m == Other(n)
        c == Classify(m, cr[n], sess[n])
        s1 == IF IsFresh(c) \/ c = "Newer" THEN sess[n] ELSE sess[m]
        r1 == IF IsFresh(c) THEN TRUE ELSE reach[m]
        hy == heard[n]
        lost == r1 /\ ~hy
    IN
    /\ conn /\ up[n] /\ nodeUp[n] /\ nodeUp[m]
    /\ knownCr[n] = 0 \/ knownCr[n] = cr[m]
    /\ IF Clears(c) \/ lost THEN ClearTables(m) ELSE UNCHANGED tables
    /\ sess' = [sess EXCEPT ![m] = IF lost THEN s1 + 1 ELSE s1]
    /\ knownCr' = [knownCr EXCEPT ![m] = IF IsFresh(c) THEN cr[n] ELSE @]
    /\ heard' = [heard EXCEPT ![m] = TRUE]
    /\ hearsMe' = [hearsMe EXCEPT ![m] = hy]
    /\ reach' = [reach EXCEPT ![m] = hy]
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive, nextRef, chan, faults>>

\* No beat from Other(n) can reach n.
NoBeat(n) ==
    LET m == Other(n) IN
    ~conn \/ ~up[m] \/ ~nodeUp[m] \/ (knownCr[m] # 0 /\ knownCr[m] # cr[n])

\* liveness::check: the link is silent for node_timeout; if the peer was
\* reachable, the session ends.
Silent(n) ==
    /\ heard' = [heard EXCEPT ![n] = FALSE]
    /\ reach' = [reach EXCEPT ![n] = FALSE]
    /\ IF reach[n]
       THEN ClearTables(n) /\ sess' = [sess EXCEPT ![n] = @ + 1]
       ELSE UNCHANGED <<tables, sess>>
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive, knownCr,
                   hearsMe, nextRef, chan>>

GoSilent(n) ==
    /\ nodeUp[n] /\ heard[n] /\ NoBeat(n)
    /\ Silent(n)
    /\ UNCHANGED faults

GoSilentSpurious(n) ==
    /\ CanSpurious /\ faults < MaxFaults
    /\ nodeUp[n] /\ heard[n] /\ ~NoBeat(n)
    /\ Silent(n)
    /\ faults' = faults + 1

\* liveness::check recomputes reachability every round.
Recheck(n) ==
    /\ nodeUp[n] /\ ~reach[n] /\ heard[n] /\ hearsMe[n]
    /\ reach' = [reach EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive, knownCr, sess,
                   heard, hearsMe, tables, nextRef, chan, faults>>

-----------------------------------------------------------------------------
(* The connection (quic.rs). *)

CloseConn ==
    /\ conn' = FALSE
    /\ chan' = [n \in Nodes |-> <<>>]
    /\ closePend' = [n \in Nodes |-> nodeUp[n]]
    /\ UNCHANGED <<up, nodeUp, cr, inc, alive, peerVars, tables, nextRef>>

\* A close for any reason: a bad frame, a restart of QUIC's path.
Close ==
    /\ CanClose /\ faults < MaxFaults /\ conn
    /\ CloseConn
    /\ faults' = faults + 1

\* QUIC's idle timeout closes a stalled connection.
IdleClose ==
    /\ conn /\ (~up[A] \/ ~up[B])
    /\ CloseConn
    /\ UNCHANGED faults

\* watch_close: n sees the close and ends the session.
NoticeClose(n) ==
    /\ nodeUp[n] /\ closePend[n]
    /\ closePend' = [closePend EXCEPT ![n] = FALSE]
    /\ ClearTables(n)
    /\ sess' = [sess EXCEPT ![n] = @ + 1]
    /\ UNCHANGED <<up, conn, nodeUp, cr, inc, alive, knownCr, heard, hearsMe,
                   reach, nextRef, chan, faults>>

\* d, having seen the close, dials again. As built, the other side's close,
\* if not yet seen, is then ignored: its connection is no longer the
\* current one.
Redial(d) ==
    /\ ~conn /\ nodeUp[d] /\ nodeUp[Other(d)] /\ ~closePend[d]
    /\ conn' = TRUE
    /\ closePend' = IF FixReplacedClose THEN closePend
                    ELSE [closePend EXCEPT ![Other(d)] = FALSE]
    /\ UNCHANGED <<up, nodeUp, cr, inc, alive, peerVars, tables, nextRef, chan, faults>>

Cut(n) ==
    /\ CanCut /\ faults < MaxFaults /\ up[n]
    /\ up' = [up EXCEPT ![n] = FALSE]
    /\ faults' = faults + 1
    /\ UNCHANGED <<conn, closePend, nodeUp, cr, inc, alive, peerVars, tables,
                   nextRef, chan>>

Heal(n) ==
    /\ ~up[n]
    /\ up' = [up EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<conn, closePend, nodeUp, cr, inc, alive, peerVars, tables,
                   nextRef, chan, faults>>

\* The mesh's random drop: a frame lost while the session goes on.
Drop(n) ==
    /\ CanDrop /\ faults < MaxFaults /\ chan[n] # <<>>
    /\ chan' = [chan EXCEPT ![n] = Tail(@)]
    /\ faults' = faults + 1
    /\ UNCHANGED <<up, conn, closePend, nodeUp, cr, inc, alive, peerVars, tables, nextRef>>

-----------------------------------------------------------------------------
(* Node crash and restart. *)

Crash(n) ==
    /\ CanCrash /\ faults < MaxFaults /\ nodeUp[n] /\ cr[n] < MaxCr
    /\ nodeUp' = [nodeUp EXCEPT ![n] = FALSE]
    /\ alive' = [alive EXCEPT ![n] = FALSE]
    /\ ClearTables(n)
    /\ conn' = FALSE
    /\ chan' = [x \in Nodes |-> <<>>]
    /\ closePend' = [x \in Nodes |-> x = Other(n) /\ nodeUp[x] /\ (conn \/ closePend[x])]
    /\ faults' = faults + 1
    /\ UNCHANGED <<up, cr, inc, peerVars, nextRef>>

\* A new start: a higher creation, no memory of the peer.
Restart(n) ==
    /\ ~nodeUp[n]
    /\ nodeUp' = [nodeUp EXCEPT ![n] = TRUE]
    /\ cr' = [cr EXCEPT ![n] = @ + 1]
    /\ inc' = [inc EXCEPT ![n] = 1]
    /\ alive' = [alive EXCEPT ![n] = TRUE]
    /\ knownCr' = [knownCr EXCEPT ![n] = 0]
    /\ sess' = [sess EXCEPT ![n] = 0]
    /\ heard' = [heard EXCEPT ![n] = TRUE]
    /\ hearsMe' = [hearsMe EXCEPT ![n] = TRUE]
    /\ reach' = [reach EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<up, conn, closePend, tables, nextRef, chan, faults>>

-----------------------------------------------------------------------------

Progress(n) ==
    \/ Deliver(n) \/ Heartbeat(n) \/ GoSilent(n) \/ Recheck(n)
    \/ NoticeClose(n) \/ Redial(n) \/ Heal(n) \/ Restart(n)

Next ==
    \/ \E n \in Nodes :
        \/ \E r \in Pid : LinkTo(n, r) \/ MonitorTo(n, r)
        \/ \E h \in links[n] : Unlink(n, h)
        \/ \E x \in watching[n] : Demonitor(n, x)
        \/ Exit(n) \/ Spawn(n)
        \/ Progress(n)
        \/ GoSilentSpurious(n) \/ Cut(n) \/ Drop(n) \/ Crash(n)
    \/ Close \/ IdleClose

\* Delivery, heartbeats, timeouts, closes and redials happen; faults and
\* the instances' own actions need not.
Fairness ==
    /\ \A n \in Nodes :
        /\ WF_vars(Deliver(n)) /\ WF_vars(Heartbeat(n)) /\ WF_vars(GoSilent(n))
        /\ WF_vars(Recheck(n)) /\ WF_vars(NoticeClose(n)) /\ WF_vars(Redial(n))
        /\ WF_vars(Heal(n)) /\ WF_vars(Restart(n))
    /\ WF_vars(IdleClose)

Spec == Init /\ [][Next]_vars
LiveSpec == Spec /\ Fairness

-----------------------------------------------------------------------------
(* Properties. *)

TypeOK ==
    /\ up \in [Nodes -> BOOLEAN]
    /\ conn \in BOOLEAN
    /\ closePend \in [Nodes -> BOOLEAN]
    /\ nodeUp \in [Nodes -> BOOLEAN]
    /\ cr \in [Nodes -> 1..MaxCr]
    /\ inc \in [Nodes -> 1..MaxInc]
    /\ alive \in [Nodes -> BOOLEAN]
    /\ knownCr \in [Nodes -> 0..MaxCr]
    /\ sess \in [Nodes -> Nat]
    /\ heard \in [Nodes -> BOOLEAN]
    /\ hearsMe \in [Nodes -> BOOLEAN]
    /\ reach \in [Nodes -> BOOLEAN]
    /\ \A n \in Nodes :
        /\ links[n] \subseteq [l : Pid, r : Pid]
        /\ watching[n] \subseteq [w : Pid, t : Pid, ref : 1..MaxRef]
        /\ monitors[n] \subseteq [t : Pid, w : Pid, ref : 1..MaxRef]
        /\ unlinking[n] \subseteq [l : Pid, r : Pid, ref : 1..MaxRef]
        /\ \A i \in 1..Len(chan[n]) : chan[n][i] \in Packet
    /\ nextRef \in [Nodes -> [mon : 0..MaxRef, unl : 0..MaxRef]]
    /\ faults \in 0..MaxFaults

\* Every entry names the live local instance and a pid of the other node.
LocalLive ==
    \A n \in Nodes :
        /\ \A h \in links[n] : Live(h.l) /\ h.l.node = n /\ h.r.node = Other(n)
        /\ \A x \in watching[n] : Live(x.w) /\ x.w.node = n /\ x.t.node = Other(n)
        /\ \A x \in monitors[n] : Live(x.t) /\ x.t.node = n /\ x.w.node = Other(n)

\* Nothing is in flight or pending: both nodes up and connected, every frame
\* delivered, each knows the other's creation and session, each reaches the
\* other.
Quiet ==
    /\ \A n \in Nodes :
        /\ nodeUp[n] /\ ~closePend[n] /\ chan[n] = <<>>
        /\ knownCr[n] = cr[Other(n)] /\ heard[n] /\ hearsMe[n] /\ reach[n]
    /\ conn
    /\ sess[A] = sess[B]

\* Each side of a link or monitor has its counterpart on the other node.
Symmetric ==
    \A n \in Nodes :
        /\ \A h \in links[n] : [l |-> h.r, r |-> h.l] \in links[Other(n)]
        /\ \A x \in watching[n] : [t |-> x.t, w |-> x.w, ref |-> x.ref] \in monitors[Other(n)]
        /\ \A x \in monitors[n] : [w |-> x.w, t |-> x.t, ref |-> x.ref] \in watching[Other(n)]

\* Once the network is quiet, both nodes agree on every link and monitor:
\* no exit or down notice was lost, and no entry leaked.
Agreement == Quiet => Symmetric

\* Liveness: a link or monitor whose other end is gone (its instance exited,
\* its node crashed) is eventually removed: the local instance was told,
\* with the exit reason or noconnection. And an entry kept for a remote
\* watcher that is gone is eventually dropped.
LinkEnds ==
    \A n \in Nodes, l, r \in Pid :
        ([l |-> l, r |-> r] \in links[n] /\ ~Live(r)) ~> ([l |-> l, r |-> r] \notin links[n])
DownArrives ==
    \A n \in Nodes, w, t \in Pid, ref \in 1..MaxRef :
        ([w |-> w, t |-> t, ref |-> ref] \in watching[n] /\ ~Live(t))
            ~> ([w |-> w, t |-> t, ref |-> ref] \notin watching[n])
NoMonitorLeak ==
    \A n \in Nodes, w, t \in Pid, ref \in 1..MaxRef :
        ([t |-> t, w |-> w, ref |-> ref] \in monitors[n] /\ ~Live(w))
            ~> ([t |-> t, w |-> w, ref |-> ref] \notin monitors[n])

Bounds ==
    \A n \in Nodes : Len(chan[n]) <= MaxChan /\ sess[n] <= MaxSession

Symmetry == Permutations({A, B})
=============================================================================
