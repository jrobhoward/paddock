------------------------------ MODULE FailoverL1 ------------------------------
(*
 * Spike S16: failover of a singleton app between an HA pair (Alpha, Beta)
 * with a witness on a third site (DESIGN.md "Two nodes cannot
 * break a tie alone").
 *
 * The base: roles, epochs, LAN and serial channels, witness promotion in
 * three steps, witness crash and recovery, and a resume that cancels
 * pending promotions. Replicated data is left out.
 *
 * What Paddock adds:
 *   - The property is about apps, not roles: at most one node RUNS the
 *     singleton at any moment (AtMostOneRunning). A stale primary at a
 *     lower epoch may not keep going until it is fenced: a SCADA singleton
 *     cannot.
 *   - A lease. A primary runs the singleton only while it holds a lease,
 *     renewed through its partner (over LAN or serial: the standby's
 *     heartbeat says "standby, not promoting, epoch e") or through the
 *     witness (only at the witness's epoch). It stops the app when the
 *     lease runs out.
 *   - A WAN link per node, so one node can be cut off from the witness
 *     while the other is not.
 *
 * Timing assumption. TLA+ here has no clocks. The lease is abstracted as:
 * a holder's lease can run out only while it cannot renew (Expire), and a
 * promotion or resume is granted only once the other node's lease is gone
 * (LeaseGone). The implementation makes this true with bounded clock
 * drift: renewals go only through the partner or the witness, each knows
 * when it last renewed, and the witness waits the lease length plus a
 * drift margin after the last renewal either reports before it grants,
 * while the holder stops its app a margin before its own expiry.
 *
 * UseLease = FALSE checks the same protocol without the lease, to show
 * what the lease is for.
 *)
EXTENDS Naturals

CONSTANTS Alpha, Beta, MaxEpoch
\* Which faults the environment may inject, and whether the lease is used.
CONSTANTS UseLease, CanCrash, CanRecover, CanFailLan, CanFailSerial,
          CanFailWan, CanFailWitness, CanHeal

Nodes == {Alpha, Beta}

VARIABLES
    role,          \* [Nodes -> {"Primary", "Secondary", "Down"}]
    epoch,         \* [Nodes -> 1..MaxEpoch]
    alive,         \* [Nodes -> BOOLEAN]
    lease,         \* [Nodes -> BOOLEAN]  holds a current lease
    running,       \* [Nodes -> BOOLEAN]  runs the singleton app
    promState,     \* [Nodes -> {"Idle", "Requested", "Granted"}]
    lanUp,         \* BOOLEAN  cluster link between the pair
    serialUp,      \* BOOLEAN  serial line between the pair
    wanUp,         \* [Nodes -> BOOLEAN]  each node's link to the witness
    witnessAlive,  \* BOOLEAN
    wEpoch,        \* epoch recorded at the witness (persistent)
    wPromoted      \* node the witness last promoted (persistent)

nodeVars == <<role, epoch, alive, lease, running, promState>>
envVars == <<lanUp, serialUp, wanUp, witnessAlive>>
witnessVars == <<wEpoch, wPromoted>>
vars == <<nodeVars, envVars, witnessVars>>

Partner(n) == IF n = Alpha THEN Beta ELSE Alpha

CanReachPeer(n) == alive[n] /\ alive[Partner(n)] /\ lanUp
CanReachPeerSerial(n) == alive[n] /\ alive[Partner(n)] /\ serialUp
CanReachWitness(n) == alive[n] /\ wanUp[n] /\ witnessAlive

\* Every heartbeat path to the partner is silent.
SeesPartnerDown(n) == alive[n] /\ ~CanReachPeer(n) /\ ~CanReachPeerSerial(n)

\* Whether n may run the app: with the lease, only while holding one.
Holds(n) == ~UseLease \/ lease[n]

\* The timing assumption: n's lease has run out, or n is dead.
LeaseGone(n) == ~UseLease \/ ~alive[n] \/ ~lease[n]

\* n, a primary, can renew its lease now.
CanRenew(n) ==
    /\ alive[n]
    /\ role[n] = "Primary"
    /\ \/ CanReachWitness(n) /\ epoch[n] = wEpoch
       \/ /\ CanReachPeer(n) \/ CanReachPeerSerial(n)
          /\ role[Partner(n)] = "Secondary"
          /\ promState[Partner(n)] = "Idle"
          /\ epoch[Partner(n)] <= epoch[n]

Init ==
    /\ role = [n \in Nodes |-> IF n = Alpha THEN "Primary" ELSE "Secondary"]
    /\ epoch = [n \in Nodes |-> 1]
    /\ alive = [n \in Nodes |-> TRUE]
    /\ lease = [n \in Nodes |-> n = Alpha]
    /\ running = [n \in Nodes |-> FALSE]
    /\ promState = [n \in Nodes |-> "Idle"]
    /\ lanUp = TRUE
    /\ serialUp = TRUE
    /\ wanUp = [n \in Nodes |-> TRUE]
    /\ witnessAlive = TRUE
    /\ wEpoch = 1
    /\ wPromoted = Alpha

\* ---------------------------------------------------------------- faults

NodeCrash(n) ==
    /\ CanCrash /\ alive[n]
    /\ alive' = [alive EXCEPT ![n] = FALSE]
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ lease' = [lease EXCEPT ![n] = FALSE]
    /\ running' = [running EXCEPT ![n] = FALSE]
    /\ promState' = [promState EXCEPT ![n] = "Idle"]
    /\ UNCHANGED <<epoch, envVars, witnessVars>>

\* A node restarts as Down, keeping its last epoch.
NodeRecover(n) ==
    /\ CanRecover /\ ~alive[n]
    /\ alive' = [alive EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<role, epoch, lease, running, promState, envVars, witnessVars>>

LanFail == CanFailLan /\ lanUp /\ lanUp' = FALSE
           /\ UNCHANGED <<nodeVars, serialUp, wanUp, witnessAlive, witnessVars>>
LanHeal == CanHeal /\ ~lanUp /\ lanUp' = TRUE
           /\ UNCHANGED <<nodeVars, serialUp, wanUp, witnessAlive, witnessVars>>
SerialFail == CanFailSerial /\ serialUp /\ serialUp' = FALSE
              /\ UNCHANGED <<nodeVars, lanUp, wanUp, witnessAlive, witnessVars>>
SerialHeal == CanHeal /\ ~serialUp /\ serialUp' = TRUE
              /\ UNCHANGED <<nodeVars, lanUp, wanUp, witnessAlive, witnessVars>>
WanFail(n) == CanFailWan /\ wanUp[n] /\ wanUp' = [wanUp EXCEPT ![n] = FALSE]
              /\ UNCHANGED <<nodeVars, lanUp, serialUp, witnessAlive, witnessVars>>
WanHeal(n) == CanHeal /\ ~wanUp[n] /\ wanUp' = [wanUp EXCEPT ![n] = TRUE]
              /\ UNCHANGED <<nodeVars, lanUp, serialUp, witnessAlive, witnessVars>>

\* The witness keeps wEpoch and wPromoted on disk; requests in flight are lost.
WitnessCrash ==
    /\ CanFailWitness /\ witnessAlive
    /\ witnessAlive' = FALSE
    /\ promState' = [n \in Nodes |-> IF promState[n] = "Requested" THEN "Idle"
                                     ELSE promState[n]]
    /\ UNCHANGED <<role, epoch, alive, lease, running, lanUp, serialUp, wanUp, witnessVars>>
WitnessRecover ==
    /\ CanHeal /\ ~witnessAlive
    /\ witnessAlive' = TRUE
    /\ UNCHANGED <<nodeVars, lanUp, serialUp, wanUp, witnessVars>>

\* ---------------------------------------------------------------- lease and app

Renew(n) ==
    /\ UseLease /\ ~lease[n] /\ CanRenew(n)
    /\ lease' = [lease EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<role, epoch, alive, running, promState, envVars, witnessVars>>

\* A lease runs out only while its holder cannot renew; the app stops first.
Expire(n) ==
    /\ UseLease /\ lease[n] /\ ~CanRenew(n)
    /\ lease' = [lease EXCEPT ![n] = FALSE]
    /\ running' = [running EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<role, epoch, alive, promState, envVars, witnessVars>>

StartApp(n) ==
    /\ alive[n] /\ role[n] = "Primary" /\ Holds(n) /\ ~running[n]
    /\ running' = [running EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<role, epoch, alive, lease, promState, envVars, witnessVars>>

\* ---------------------------------------------------------------- promotion

PromotionRequest(n) ==
    /\ role[n] = "Secondary"
    /\ SeesPartnerDown(n)
    /\ CanReachWitness(n)
    /\ promState[n] = "Idle"
    /\ epoch[n] + 1 > wEpoch
    /\ epoch[n] + 1 <= MaxEpoch
    /\ promState' = [promState EXCEPT ![n] = "Requested"]
    /\ UNCHANGED <<role, epoch, alive, lease, running, envVars, witnessVars>>

\* The witness's commit point. It grants only once the partner's lease is gone.
WitnessGrants(n) ==
    /\ witnessAlive
    /\ promState[n] = "Requested"
    /\ alive[n] /\ wanUp[n]
    /\ role[n] = "Secondary"
    /\ epoch[n] + 1 > wEpoch
    /\ epoch[n] + 1 <= MaxEpoch
    /\ LeaseGone(Partner(n))
    /\ wEpoch' = epoch[n] + 1
    /\ wPromoted' = n
    /\ promState' = [promState EXCEPT ![n] = "Granted"]
    /\ UNCHANGED <<role, epoch, alive, lease, running, envVars>>

\* The grant is also the new primary's first lease.
CompletePromotion(n) ==
    /\ alive[n]
    /\ promState[n] = "Granted"
    /\ role[n] = "Secondary"
    /\ role' = [role EXCEPT ![n] = "Primary"]
    /\ epoch' = [epoch EXCEPT ![n] = wEpoch]
    /\ lease' = [lease EXCEPT ![n] = UseLease]
    /\ promState' = [promState EXCEPT ![n] = "Idle"]
    /\ UNCHANGED <<alive, running, envVars, witnessVars>>

\* ---------------------------------------------------------------- fencing and rejoin

StepDown(n) ==
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ lease' = [lease EXCEPT ![n] = FALSE]
    /\ running' = [running EXCEPT ![n] = FALSE]
    /\ promState' = [promState EXCEPT ![n] = "Idle"]

\* A stale primary meets its partner's higher epoch.
FencedByPartner(n) ==
    /\ alive[n] /\ role[n] = "Primary"
    /\ CanReachPeer(n) \/ CanReachPeerSerial(n)
    /\ epoch[n] < epoch[Partner(n)]
    /\ StepDown(n)
    /\ epoch' = [epoch EXCEPT ![n] = epoch[Partner(n)]]
    /\ UNCHANGED <<alive, envVars, witnessVars>>

\* A stale primary tries to renew at the witness and learns the higher epoch.
FencedByWitness(n) ==
    /\ alive[n] /\ role[n] = "Primary"
    /\ CanReachWitness(n)
    /\ epoch[n] < wEpoch
    /\ StepDown(n)
    /\ epoch' = [epoch EXCEPT ![n] = wEpoch]
    /\ UNCHANGED <<alive, envVars, witnessVars>>

Rejoin(n) ==
    /\ alive[n] /\ role[n] = "Down"
    /\ CanReachPeer(n)
    /\ role[Partner(n)] = "Primary"
    /\ epoch[Partner(n)] >= epoch[n]
    /\ role' = [role EXCEPT ![n] = "Secondary"]
    /\ epoch' = [epoch EXCEPT ![n] = epoch[Partner(n)]]
    /\ promState' = [promState EXCEPT ![n] = "Idle"]
    /\ UNCHANGED <<alive, lease, running, envVars, witnessVars>>

\* The node the witness last promoted resumes as primary when it cannot
\* rejoin a primary. Like a grant, it waits for the partner's lease, and it
\* cancels the partner's pending request.
ResumeViaWitness(n) ==
    /\ alive[n] /\ role[n] = "Down"
    /\ CanReachWitness(n)
    /\ wPromoted = n
    /\ ~(CanReachPeer(n) /\ role[Partner(n)] = "Primary")
    /\ LeaseGone(Partner(n))
    /\ role' = [role EXCEPT ![n] = "Primary"]
    /\ epoch' = [epoch EXCEPT ![n] = wEpoch]
    /\ lease' = [lease EXCEPT ![n] = UseLease]
    /\ promState' = [m \in Nodes |-> "Idle"]
    /\ UNCHANGED <<alive, running, envVars, witnessVars>>

\* ---------------------------------------------------------------- spec

Protocol(n) ==
    \/ Renew(n) \/ Expire(n) \/ StartApp(n)
    \/ PromotionRequest(n) \/ WitnessGrants(n) \/ CompletePromotion(n)
    \/ FencedByPartner(n) \/ FencedByWitness(n)
    \/ Rejoin(n) \/ ResumeViaWitness(n)

Environment ==
    \/ \E n \in Nodes : NodeCrash(n) \/ NodeRecover(n) \/ WanFail(n) \/ WanHeal(n)
    \/ LanFail \/ LanHeal \/ SerialFail \/ SerialHeal
    \/ WitnessCrash \/ WitnessRecover

Next == (\E n \in Nodes : Protocol(n)) \/ Environment

\* Every protocol step that stays enabled is eventually taken; faults are not.
Fairness == \A n \in Nodes :
    /\ WF_vars(Renew(n)) /\ WF_vars(Expire(n)) /\ WF_vars(StartApp(n))
    /\ WF_vars(PromotionRequest(n)) /\ WF_vars(WitnessGrants(n))
    /\ WF_vars(CompletePromotion(n))
    /\ WF_vars(FencedByPartner(n)) /\ WF_vars(FencedByWitness(n))
    /\ WF_vars(Rejoin(n)) /\ WF_vars(ResumeViaWitness(n))

Spec == Init /\ [][Next]_vars /\ Fairness

\* ---------------------------------------------------------------- properties

TypeOK ==
    /\ role \in [Nodes -> {"Primary", "Secondary", "Down"}]
    /\ epoch \in [Nodes -> 1..MaxEpoch]
    /\ alive \in [Nodes -> BOOLEAN]
    /\ lease \in [Nodes -> BOOLEAN]
    /\ running \in [Nodes -> BOOLEAN]
    /\ promState \in [Nodes -> {"Idle", "Requested", "Granted"}]
    /\ wanUp \in [Nodes -> BOOLEAN]
    /\ wEpoch \in 1..MaxEpoch
    /\ wPromoted \in Nodes

\* The property Paddock needs: the singleton never runs on both nodes.
AtMostOneRunning == ~(running[Alpha] /\ running[Beta])

\* An app runs only on a live primary.
RunningOnPrimary == \A n \in Nodes : running[n] => alive[n] /\ role[n] = "Primary"

\* Two live primaries never share an epoch.
AtMostOnePrimaryPerEpoch ==
    \A m, n \in Nodes :
        (m # n /\ alive[m] /\ alive[n] /\ role[m] = "Primary" /\ role[n] = "Primary")
        => epoch[m] # epoch[n]

\* The witness never grants two promotions at once.
OneGrantAtATime == ~(promState[Alpha] = "Granted" /\ promState[Beta] = "Granted")

\* Availability when faults stop: some node runs the app, unless no node
\* can renew a lease (both nodes dead, or cut off from the partner and the
\* witness alike).
NobodyCanRun ==
    \A n \in Nodes :
        ~alive[n] \/ (~CanReachPeer(n) /\ ~CanReachPeerSerial(n) /\ ~CanReachWitness(n))
Available == <>[](\E n \in Nodes : running[n]) \/ <>[]NobodyCanRun

\* ---------------------------------------------------------------- bounds

EpochBound == \A n \in Nodes : epoch[n] <= MaxEpoch

\* The serial line fails only with a node (the safe model).
SerialReliable == (alive[Alpha] /\ alive[Beta]) => serialUp

=============================================================================
