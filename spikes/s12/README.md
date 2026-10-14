# Spike S12: a TLA+ model of remote links and monitors

**Question (ROADMAP C4).** With C5's sessions (numbers on frames, `hears_you`, ending on either side), pid incarnations and node creations, do both nodes always agree on every link and monitor between them, so that no exit or down notice is lost and no entry leaks? What must D3 add for that?

**Answer: not as built; four changes make it hold.** The model finds four ways for the two sides to disagree. Each ends with an instance that keeps a link or monitor that the other node has no record of, so the exit of its partner never reaches it:

1. **Crossed link and unlink, with no fault at all.** A links B's instance and unlinks it again while B links A's. B applies A's `Link` and `Unlink` and drops its half; A then applies B's `Link` and keeps a half that B no longer has. This is the race OTP 23 fixed with `UNLINK_ID` and `UNLINK_ID_ACK`.
2. **A notice refused while the session goes on.** A node that ended a session and now counts the peer unreachable still applies the peer's frames, since `deliver` does not check reachability. The `noproc` answer to a `Link`, or a `LinkExit` when an instance exits, is then refused by `transport::send` and dropped silently, and the node becomes reachable again without a new session. A full QUIC send queue (4,096 frames) drops a notice the same way.
3. **A lost connection whose close one side ignores.** A ends the session (a timeout) and becomes reachable again; B, which had not ended it, does so when the connection closes, so both sides are at the same session number. A's `Link`, sent before the close, is lost with the connection. B redials before A sees the close, and `quic.rs` then ignores the old connection's close, since a new connection has replaced it. A keeps a link B never received.
4. **A link sent by a restarted node before it has heard its peer.** A restarts and links B's instance before any frame from B arrives. B takes the `Link` as the first frame of A's new start and records its half, then ends the session (the old connection's close, with R2) and drops the half. A takes B's next frame as the first from B and adopts its session number without ending anything, as `liveness::on_frame` does for a node never heard, so A keeps its half.

With four changes, no reachable state of the configs checked so far has the two nodes disagree once the network is quiet:

- **R1. A notice that cannot be sent ends the session.** `LinkExit`, `Down`, `Unlink`, `Demonitor`, the unlink acknowledgement and the `noproc` answers: when `transport::send` refuses one (peer unreachable, no connection, queue full), the sender ends the session with that node, so the other side clears its entries with `noconnection`. Linking or monitoring a remote pid already fails with `noconnection` when the send is refused, and needs nothing new.
- **R2. Every lost connection ends the session on both sides.** `watch_close` ends the session for any connection that was the pair's current one, even when a redial replaced it before its close was seen.
- **R3. Unlink is acknowledged, as in OTP 23.** `Unlink` carries an id; the receiver removes its half and answers `UnlinkAck` with the id; until the acknowledgement arrives, the unlinking side ignores a `Link` from the other side for the same pair, because its `Unlink` will remove the half that `Link` came from.
- **R4. No links or monitors with a node before a frame from its current start has been heard.** Until then, linking or monitoring one of its pids fails at once with `noconnection`. Heartbeats, joins and their answers still go out, so a restarted node is still reachable at once for membership. Adopting the peer's session number on first contact is then safe: the node has nothing with that peer yet to clear.

**The guarantee needs a transport that loses frames only with the connection.** QUIC does: each stream is ordered and reliable until the connection closes. The in-memory mesh's random `drop` and `lose_next` lose a frame while the session continues, and then no rule can keep both sides in agreement (`Drop` below). `tests/simulation/random.rs` already stops requiring notices in its runs with drops.

Not yet finished (see "Results"): the exhaustive runs of the fixed model, with and without crashes, and the liveness properties `LinkEnds`, `DownArrives` and `NoMonitorLeak`.

## How to run

```sh
# Requires Java 11+ and tla2tools.jar
# (https://github.com/tlaplus/tlaplus/releases), here at ~/tla/tla2tools.jar.
cd spikes/s12
bash mkcfg.sh     # writes the safety configs; Liveness.cfg is kept by hand
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -config Quic.cfg RemoteLinks.tla
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -workers auto -config QuicFixed.cfg RemoteLinks.tla
# Random traces, deeper than the exhaustive search reaches; -deadlock
# because a trace stops at the bounds.
java -XX:+UseParallelGC -jar ~/tla/tla2tools.jar -simulate num=3000000 -depth 80 -deadlock -config QuicFixed.cfg RemoteLinks.tla
```

## Model

- **Nodes.** Two, each with one instance slot. An instance exits at any time and restarts with the next incarnation; a node crashes and restarts with the next creation (`Crashes` only). Pids are `[node, creation, incarnation]`. Only links and monitors between the two nodes are modeled; local ones never touch the transport.
- **Tables.** Per node, as in `links.rs`: links (local, remote), monitors this node's instance holds on the other (`watching`), monitors the other node's instances hold on this one (`monitors`), and, with R3, unlinks awaiting their acknowledgement.
- **Frames.** `Link`, `Unlink` (and `UnlinkAck` with R3), `LinkExit`, `Monitor`, `Demonitor`, `Down`, each carrying the sender's creation, the receiver's creation as the sender knows it, and the sender's session number. The data stream is an ordered queue per direction. Heartbeats travel on QUIC's control stream and may overtake data, so a heartbeat reads the sender's state at the moment it arrives.
- **Sessions** follow `liveness.rs`: a frame for an earlier start of the receiver is discarded; one from an older session is discarded, except that a heartbeat still says whether its sender hears the receiver; one from a newer session, or the first from a new creation, ends the old session first. A node that finds its peer silent, or learns from a heartbeat that the peer does not hear it, counts it unreachable and ends the session; it counts it reachable again once it hears it and the peer hears it.
- **Network faults**, each switched on per config and counted against `MaxFaults`: a direction stalls (frames held) and heals; the connection closes (frames in flight lost), each side seeing the close on its own, and either side redials; a node finds its peer silent without cause (a delayed heartbeat); a send is refused with the peer reachable (a full queue); a frame is lost while the connection stays up (the mesh's `drop`). A stalled connection closes on QUIC's idle timeout without counting as a fault.
- **No clocks.** A peer may be found silent when no beat can reach the node; nothing else is timed.

**Properties.** `Agreement`: once the network is quiet (both nodes up and connected, nothing in flight, each knowing the other's creation and the same session number, each reaching the other), every link, monitor and watched monitor on one node has its counterpart on the other. A lost exit or down notice, and a leaked entry, both show up as an entry without a counterpart. `LocalLive`: every entry names the live local instance. Liveness, under fairness of delivery, heartbeats, timeouts, closes and redials: `LinkEnds` (a link whose remote end is gone is eventually removed: the instance was told), `DownArrives` (likewise for a monitor), `NoMonitorLeak` (an entry kept for a remote watcher that is gone is eventually dropped).

## Results

Checked on 2026-10-14 with TLC 2026.02.28 (OpenJDK 21) on the 16-core development machine, while the D10 soak ran. Bounds: `MaxInc = 2`, `MaxRef = 1`, `MaxSession = 3`, `MaxChan = 2`, `MaxFaults = 2`, `MaxCr = 1` unless stated; symmetry between the two nodes for safety configs.

| Config | Faults | Fixes | Distinct states | Result |
| --- | --- | --- | --- | --- |
| `Quic` | stalls, closes, spurious timeouts | none | 6,243 (stopped) | `Agreement` violated: crossed link and unlink (1) |
| `QuicUnlinkAck` | as `Quic` | R3 | 27,097 (stopped) | `Agreement` violated: ignored close (3) |
| `NoR1` | as `Quic` | R2, R3 | 59,922 (stopped) | `Agreement` violated: refused `noproc` answer (2) |
| `NoR2` | as `Quic` | R1, R3 | 16,051 (stopped) | `Agreement` violated: ignored close (3) |
| `NoR3` | as `Quic` | R1, R2 | 6,370 (stopped) | `Agreement` violated: crossed link and unlink (1) |
| `QuicFixed` | as `Quic` | R1, R2, R3 | | |
| `QueueFull` | full send queues only | R3 | 83 (stopped) | `Agreement` violated: refused `Unlink` (2) |
| `QueueFullFixed` | spurious timeouts, full queues | R1, R2, R3 | | |
| `Drop` | frames lost in a live session | R1, R2, R3 | 9 (stopped) | `Agreement` violated, as expected |
| `CrashesR1R3` | as `Quic`, plus crashes (`MaxCr = 2`) | R1, R2, R3 | 126,330 (stopped) | `Agreement` violated: link before first contact (4) |
| `Crashes` | as `CrashesR1R3` | R1 to R4 | | |
| `Liveness` | as `Quic`, `MaxFaults = 1` | R1 to R4 | | |

Each fix is needed: without any one of them (`NoR1`, `NoR2`, `NoR3`, `CrashesR1R3`), `Agreement` fails again with the counterexample that fix addresses. R4 changes nothing in configs with one creation per node, where every node has heard its peer from the start.

Counterexamples, shortest found:

- `Quic`, 7 states, no fault: A links B's instance and unlinks it (`Link`, `Unlink` queued); B links A's (`Link` queued, B's half recorded); B applies A's `Link` (it has the half already) and `Unlink` (its half goes); A applies B's `Link`. A holds a link B does not.
- `QuicUnlinkAck`, 7 states: A finds B silent without cause and ends session 0 (now 1); B's next heartbeat, from session 0, makes A count B reachable; A links B's instance (`Link` at session 1 queued); the connection closes, losing it; B sees the close and ends session 0 (now 1); B redials, and A's close is ignored. Both at session 1; A holds a link B never received.
- `CrashesR1R3`, 8 states: A crashes and restarts at creation 2, knowing nothing of B; A redials; A links B's instance (`Link` at A's session 0); B takes it as the first frame of A's new start, adopts session 0 and records its half; B sees the old connection's close (R2) and ends session 0 (now 1), dropping the half; B's heartbeat at session 1 is the first frame A hears from B, so A adopts session 1 without ending anything. A holds a link B dropped.
- `NoR1`, 8 states: A's instance exits; B and A each find the other silent once and end session 0 (both now 1); B links A's exited instance; A applies the `Link` at session 1, but its `noproc` answer is refused since A counts B unreachable; A finds B reachable again. B holds a link to an instance that is gone and is never told.

## What this changes

- **D3** builds on R1 to R4 before remote links and monitors are offered across nodes. R1, R2 and R4 change code that exists today (`delivery.rs`, `transport/mod.rs`, `transport/quic.rs`, `transport/liveness.rs`); R3 adds a frame, so the wire format's version goes up.
- **The mesh's `drop` fault** is outside the guarantee. Tests that drop frames check only what survives loss, as `random.rs` does.
- **Calls** are not modeled. A `Reply` refused for the same reasons is lost too; the caller then waits for its timeout, and with R1 it fails with `noconnection` instead, when the session ends.

## Not modeled

- More than two nodes, or more than one instance per node; links and monitors between pairs of nodes are independent, each pair with its own session.
- Calls, casts and exit signals sent with `process.exit` (best effort, no table entry).
- Membership: joins, leaves and the `Leave` frame. A clean leave ends the session on both sides in the code; the model has no leave.
- Exit reasons: a notice either arrives, with its reason or `noconnection`, or it does not.
- Timing: how soon a lost peer is noticed (`node_timeout`) is outside the model.
