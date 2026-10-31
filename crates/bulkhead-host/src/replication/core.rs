//! One node's replica as a state machine: inputs (leadership, client
//! writes, messages from the partner, the witness's answers, ticks) change
//! it and return effects (messages to send, records to ask of the witness,
//! answers to clients, watch notices), which the task in `mod.rs` carries
//! out. No I/O but the log's disk.
//!
//! Names follow spike S20's actions (`spikes/s20/PairReplication.tla`):
//! `take_over`, `append_own` (`ClientWrite`, `NoOp`), `commit`,
//! `go_alone`, `go_sync`, `report_synced`, `compact`, `send`,
//! `retransmit`, `recv_append` (`RecvApp`), `recv_snapshot` (`RecvSnap`)
//! and `recv_reply` (`RecvReply`), with the rules R1 to R7 of its README
//! and `PromoteAlone = FALSE`: a primary starts in sync mode and asks the
//! witness to record its partner out of date before it commits alone.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::log::{Command, Entry, Log, Snapshot};
use crate::failover::Standing;
use crate::services::keyvalue::storage::{BucketDump, Op, Outcome};

/// Tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Config {
    /// Most entries in one `Append`.
    pub(crate) batch: usize,
    /// About the most bytes of entries in one `Append`.
    pub(crate) batch_bytes: usize,
    /// Most entries sent and not yet acknowledged.
    pub(crate) window: u64,
    /// The most bytes of entries the log keeps, and the oldest entry it
    /// keeps: within both, a partner back from an outage catches up from
    /// the log rather than a snapshot. Only committed entries are dropped.
    pub(crate) log_bytes: u64,
    pub(crate) log_age: Duration,
    /// How long an entry waits for the partner before the primary asks to
    /// commit without it; at once when the partner is heard on no path.
    pub(crate) alone_after: Duration,
    /// How long without progress before resending (R6).
    pub(crate) resend_after: Duration,
    /// About the most bytes of a snapshot chunk.
    pub(crate) chunk_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            batch: 256,
            batch_bytes: 256 << 10,
            window: 4096,
            log_bytes: 64 << 20,
            log_age: Duration::from_secs(3600),
            alone_after: Duration::from_secs(1),
            resend_after: Duration::from_millis(500),
            chunk_bytes: 256 << 10,
        }
    }
}

/// Names whoever waits for a write's or read's answer; the task maps it.
pub(crate) type Waiter = u64;

/// Why an operation has no outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Refusal {
    /// Not taken: no primary with a lease, or the partner cannot be
    /// reached. Nothing was written.
    Unavailable,
    /// Taken, but not committed in time, or the primary stepped down
    /// first: it may yet commit, or not.
    Unknown,
    /// Storage failed.
    Failed(String),
}

/// Messages between the replicas of the pair, and operations a standby
/// forwards to the primary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Msg {
    /// Entries from `prev + 1`, after the entry at `prev` of epoch
    /// `prev_epoch`, and the sender's commit index. Without entries, the
    /// commit index alone, and a probe.
    Append {
        epoch: u64,
        prev: u64,
        prev_epoch: u64,
        entries: Vec<Entry>,
        commit: u64,
    },
    /// The receiver holds every entry up to `matched` on disk.
    Ack { epoch: u64, matched: u64 },
    /// `prev` did not match; resend from after `hint`, the receiver's
    /// commit index.
    Nack { epoch: u64, hint: u64 },
    /// Chunk `chunk` of the buckets at commit index `index`.
    Snapshot {
        epoch: u64,
        index: u64,
        index_epoch: u64,
        chunk: u32,
        last: bool,
        buckets: Vec<BucketDump>,
    },
    /// Chunk `chunk` arrived; with `done`, the receiver holds everything
    /// up to `index`.
    SnapshotAck {
        epoch: u64,
        index: u64,
        chunk: u32,
        done: bool,
    },
    /// A write forwarded from the standby; answered with `Done`.
    Write {
        id: u64,
        bucket: String,
        quota: Option<u64>,
        op: Op,
    },
    /// A linearizable read forwarded from the standby.
    Read { id: u64, bucket: String, op: Op },
    /// The answer to a forwarded `Write` or `Read`.
    Done {
        id: u64,
        result: Result<Outcome, Refusal>,
    },
}

/// What the task does after an input.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Send to the partner.
    Send(Msg),
    /// Ask the witness to set or clear the record (`go_alone`,
    /// `report_synced`); answer with [`Replica::recorded`].
    Record { epoch: u64, outdated: bool },
    /// Answer a write.
    Reply {
        waiter: Waiter,
        result: Result<Outcome, Refusal>,
    },
    /// Tell the bucket's watches.
    Notify {
        bucket: String,
        changed: Vec<String>,
        revision: u64,
    },
}

/// A write a client asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Write {
    pub(crate) waiter: Waiter,
    pub(crate) bucket: String,
    pub(crate) quota: Option<u64>,
    pub(crate) op: Op,
}

/// The leadership the replica follows: failover's, at this moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lead {
    pub(crate) standing: Standing,
    pub(crate) epoch: u64,
    /// The primary holds its lease.
    pub(crate) may_run: bool,
    /// The witness has the partner recorded out of date.
    pub(crate) partner_outdated: bool,
}

#[derive(Debug)]
enum Role {
    Idle,
    Standby(Standby),
    Primary(Primary),
}

#[derive(Debug, Default)]
struct Standby {
    /// A snapshot arriving.
    receiving: Option<Receiving>,
}

#[derive(Debug)]
struct Receiving {
    index: u64,
    epoch: u64,
    next_chunk: u32,
    buckets: Vec<BucketDump>,
}

#[derive(Debug)]
struct Primary {
    may_run: bool,
    /// Sync mode: commit only what the partner has acknowledged.
    sync: bool,
    partner_outdated: bool,
    /// The next entry to send, and the last the partner acknowledged.
    next: u64,
    matched: u64,
    /// This epoch's no-op: a read is served once it commits (R5).
    noop: u64,
    /// Writes waiting to commit, by index.
    waiting: BTreeMap<u64, Waiter>,
    /// Since when an entry has waited for the partner.
    behind_since: Option<Instant>,
    /// When the partner last made progress, or a resend began.
    progress_at: Instant,
    /// When the partner last acknowledged something new, since this
    /// primary last went alone.
    acked_at: Option<Instant>,
    /// A record asked of the witness and not yet answered.
    recording: bool,
    /// A snapshot being sent.
    sending: Option<Sending>,
    /// Where the last nack sent `next`; a second nack to the same place
    /// waits for the resend timer instead of resending at once, so a
    /// standby that cannot match never spins the pair.
    last_nack: Option<u64>,
    /// Entries wait for the resend timer, or an ack that moves `matched`.
    held: bool,
}

impl Primary {
    /// The first entry the partner is not known to hold: past the last
    /// ack; with none yet in this epoch, where the last nack put `next`,
    /// or this epoch's no-op. A resend starts here, and the window counts
    /// from here.
    fn unacked(&self) -> u64 {
        if self.matched > 0 {
            self.matched + 1
        } else {
            self.last_nack.unwrap_or(self.noop)
        }
    }
}

#[derive(Debug)]
struct Sending {
    index: u64,
    epoch: u64,
    chunks: Arc<Vec<Vec<BucketDump>>>,
    chunk: u32,
    sent_at: Option<Instant>,
}

/// A node's replica.
#[derive(Debug)]
pub(crate) struct Replica {
    cfg: Config,
    log: Log,
    epoch: u64,
    role: Role,
}

type Out = Result<Vec<Effect>, String>;

impl Replica {
    pub(crate) fn new(cfg: Config, log: Log) -> Self {
        Self {
            cfg,
            log,
            epoch: 0,
            role: Role::Idle,
        }
    }

    pub(crate) fn log(&self) -> &Log {
        &self.log
    }

    /// Whether this replica is the primary (it takes writes, or refuses
    /// them while it has no lease); otherwise the task forwards them.
    pub(crate) fn is_primary(&self) -> bool {
        matches!(self.role, Role::Primary(_))
    }

    /// Whether this replica is the standby, which forwards writes.
    pub(crate) fn is_standby(&self) -> bool {
        matches!(self.role, Role::Standby(_))
    }

    /// Whether a linearizable read may be served now (R5): the primary
    /// with its lease, once it committed an entry of its own epoch. Every
    /// committed entry is applied, so the local copy is then up to date.
    pub(crate) fn read_ready(&self) -> bool {
        matches!(&self.role, Role::Primary(p) if p.may_run && self.log.commit() >= p.noop)
    }

    /// Whether the primary is committing without its partner.
    pub(crate) fn alone(&self) -> bool {
        matches!(&self.role, Role::Primary(p) if !p.sync)
    }

    /// The partner's acknowledged index, on the primary.
    pub(crate) fn matched(&self) -> Option<u64> {
        match &self.role {
            Role::Primary(p) => Some(p.matched),
            Role::Standby(_) | Role::Idle => None,
        }
    }

    /// Failover's leadership changed, or may have.
    pub(crate) fn leadership(&mut self, lead: Lead, now: Instant) -> Out {
        let mut out = Vec::new();
        match lead.standing {
            Standing::Primary => {
                if let Role::Primary(p) = &mut self.role
                    && self.epoch == lead.epoch
                {
                    p.may_run = lead.may_run;
                    p.partner_outdated = lead.partner_outdated;
                } else {
                    self.leave(&mut out);
                    self.epoch = lead.epoch;
                    self.take_over(lead, now)?;
                }
            }
            Standing::Standby => {
                if !matches!(self.role, Role::Standby(_)) || self.epoch != lead.epoch {
                    self.leave(&mut out);
                    self.epoch = lead.epoch;
                    self.role = Role::Standby(Standby::default());
                }
            }
            Standing::Down => {
                self.leave(&mut out);
                self.epoch = lead.epoch;
            }
        }
        self.progress(now, &mut out)?;
        Ok(out)
    }

    /// Stop being whatever this replica was: writes waiting on a primary
    /// may yet commit under the next one, or not.
    fn leave(&mut self, out: &mut Vec<Effect>) {
        if let Role::Primary(p) = std::mem::replace(&mut self.role, Role::Idle) {
            for waiter in p.waiting.into_values() {
                out.push(Effect::Reply {
                    waiter,
                    result: Err(Refusal::Unknown),
                });
            }
        }
    }

    /// `TakeOver`: a new primary resends from its end, in sync mode, and
    /// appends a no-op of its epoch (R4).
    fn take_over(&mut self, lead: Lead, now: Instant) -> Result<(), String> {
        let at = self.log.last() + 1;
        self.log.append(
            at,
            &[Entry {
                epoch: lead.epoch,
                cmd: Command::Noop,
            }],
        )?;
        self.role = Role::Primary(Primary {
            may_run: lead.may_run,
            sync: true,
            partner_outdated: lead.partner_outdated,
            next: at,
            matched: 0,
            noop: at,
            waiting: BTreeMap::new(),
            behind_since: Some(now),
            progress_at: now,
            acked_at: None,
            recording: false,
            sending: None,
            last_nack: None,
            held: false,
        });
        Ok(())
    }

    /// `ClientWrite`: append writes as one transaction, on a primary with
    /// its lease; refuse them otherwise.
    pub(crate) fn write(&mut self, writes: Vec<Write>, now: Instant) -> Out {
        let mut out = Vec::new();
        let Role::Primary(p) = &mut self.role else {
            refuse(writes, &mut out);
            return Ok(out);
        };
        if !p.may_run {
            refuse(writes, &mut out);
            return Ok(out);
        }
        let at = self.log.last() + 1;
        let entries: Vec<Entry> = writes
            .iter()
            .map(|w| Entry {
                epoch: self.epoch,
                cmd: Command::Write {
                    bucket: w.bucket.clone(),
                    quota: w.quota,
                    op: w.op.clone(),
                },
            })
            .collect();
        if let Err(e) = self.log.append(at, &entries) {
            for w in writes {
                out.push(Effect::Reply {
                    waiter: w.waiter,
                    result: Err(Refusal::Failed(e.clone())),
                });
            }
            return Err(e);
        }
        for (i, w) in writes.into_iter().enumerate() {
            p.waiting.insert(at + i as u64, w.waiter);
        }
        p.behind_since.get_or_insert(now);
        self.progress(now, &mut out)?;
        Ok(out)
    }

    /// The witness answered a record asked by an `Effect::Record`; `ok` if
    /// it keeps it.
    pub(crate) fn recorded(&mut self, epoch: u64, outdated: bool, ok: bool, now: Instant) -> Out {
        let mut out = Vec::new();
        if let Role::Primary(p) = &mut self.role
            && epoch == self.epoch
        {
            p.recording = false;
            if ok {
                p.partner_outdated = outdated;
                if outdated {
                    // `GoAlone`.
                    p.sync = false;
                    p.acked_at = None;
                }
            }
        }
        self.progress(now, &mut out)?;
        Ok(out)
    }

    /// Periodic work: resend, the commit index to the partner, going
    /// alone, compaction.
    pub(crate) fn tick(&mut self, now: Instant, partner_heard: bool) -> Out {
        let mut out = Vec::new();
        let last = self.log.last();
        let commit = self.log.commit();
        let epoch = self.epoch;
        if let Role::Primary(p) = &mut self.role {
            // `Retransmit` (R6), from the first entry not known to be held,
            // not from the start of the log.
            if now.duration_since(p.progress_at) >= self.cfg.resend_after {
                let from = p.unacked();
                if p.next > from {
                    p.next = from;
                }
                if let Some(s) = &mut p.sending {
                    s.sent_at = None;
                }
                p.held = false;
                p.progress_at = now;
            }
            // The commit index alone, and a probe of where the partner is.
            if p.next > last && p.sending.is_none() {
                out.push(Effect::Send(Msg::Append {
                    epoch,
                    prev: last,
                    prev_epoch: self.log.last_epoch(),
                    entries: Vec::new(),
                    commit,
                }));
            }
            // `GoAlone`: an entry has waited too long for the partner.
            let waited = p
                .behind_since
                .is_some_and(|t| now.duration_since(t) >= self.cfg.alone_after);
            if p.sync && p.may_run && commit < last && !p.recording && (waited || !partner_heard) {
                p.recording = true;
                out.push(Effect::Record {
                    epoch,
                    outdated: true,
                });
            }
        }
        self.compact(now)?;
        self.progress(now, &mut out)?;
        Ok(out)
    }

    /// `Compact`: past the oldest entries while the log is over its bounds.
    fn compact(&mut self, now: Instant) -> Result<(), String> {
        let to = self
            .log
            .compact_target(now, self.cfg.log_bytes, self.cfg.log_age);
        if to > self.log.base() {
            self.log.compact(to)?;
        }
        Ok(())
    }

    /// A message from the partner.
    pub(crate) fn message(&mut self, msg: Msg, now: Instant) -> Out {
        let mut out = Vec::new();
        match (&self.role, msg) {
            (
                Role::Standby(_),
                Msg::Append {
                    epoch,
                    prev,
                    prev_epoch,
                    entries,
                    commit,
                },
            ) if epoch == self.epoch => {
                self.recv_append(prev, prev_epoch, &entries, commit, &mut out)?;
            }
            (
                Role::Standby(_),
                Msg::Snapshot {
                    epoch,
                    index,
                    index_epoch,
                    chunk,
                    last,
                    buckets,
                },
            ) if epoch == self.epoch => {
                self.recv_snapshot(index, index_epoch, chunk, last, buckets, &mut out)?;
            }
            (Role::Primary(_), Msg::Ack { epoch, matched }) if epoch == self.epoch => {
                self.recv_ack(matched, now);
            }
            (Role::Primary(_), Msg::Nack { epoch, hint }) if epoch == self.epoch => {
                self.recv_nack(hint, now);
            }
            (
                Role::Primary(_),
                Msg::SnapshotAck {
                    epoch,
                    index,
                    chunk,
                    done,
                },
            ) if epoch == self.epoch => {
                self.recv_snapshot_ack(index, chunk, done, now);
            }
            // Another epoch's, or not for this role: dropped.
            _ => {}
        }
        self.progress(now, &mut out)?;
        Ok(out)
    }

    /// `RecvApp`: take entries after a matching one, drop a conflicting
    /// suffix, raise the commit index, and acknowledge; or name the commit
    /// index to resend from (R6).
    fn recv_append(
        &mut self,
        prev: u64,
        prev_epoch: u64,
        entries: &[Entry],
        commit: u64,
        out: &mut Vec<Effect>,
    ) -> Result<(), String> {
        let epoch = self.epoch;
        let base = self.log.base();
        // Entries up to `base` are committed, so they match.
        let matches = prev <= base || self.log.epoch_at(prev) == Some(prev_epoch);
        if prev <= self.log.commit() && !matches {
            tracing::error!(
                index = prev,
                "replication: a committed entry differs from the primary's"
            );
        }
        if prev > self.log.last() || !matches {
            out.push(Effect::Send(Msg::Nack {
                epoch,
                hint: self.log.commit(),
            }));
            return Ok(());
        }
        let mut index = prev + 1;
        let mut held = 0;
        while held < entries.len()
            && (index <= base || self.log.epoch_at(index) == Some(entries[held].epoch))
        {
            index += 1;
            held += 1;
        }
        if held < entries.len() {
            if index <= self.log.commit() {
                return Err(format!(
                    "replicated log: entry {index} conflicts with a committed one"
                ));
            }
            self.log.append(index, &entries[held..])?;
        }
        let last_sent = prev + entries.len() as u64;
        let to = commit.min(last_sent);
        self.commit(to, out)?;
        out.push(Effect::Send(Msg::Ack {
            epoch,
            matched: last_sent,
        }));
        Ok(())
    }

    /// `RecvSnap`: a standby that already holds the snapshot's last entry
    /// keeps its log and only commits up to it (R7); one that committed
    /// past it needs nothing; any other installs it once every chunk has
    /// arrived.
    fn recv_snapshot(
        &mut self,
        index: u64,
        index_epoch: u64,
        chunk: u32,
        last: bool,
        buckets: Vec<BucketDump>,
        out: &mut Vec<Effect>,
    ) -> Result<(), String> {
        let epoch = self.epoch;
        let ack = |chunk, done| {
            Effect::Send(Msg::SnapshotAck {
                epoch,
                index,
                chunk,
                done,
            })
        };
        let holds = index <= self.log.last() && self.log.epoch_at(index) == Some(index_epoch);
        if holds || index <= self.log.commit() {
            if let Role::Standby(s) = &mut self.role {
                s.receiving = None;
            }
            if holds {
                self.commit(index, out)?;
            }
            out.push(ack(chunk, true));
            return Ok(());
        }
        let Role::Standby(s) = &mut self.role else {
            return Ok(());
        };
        if chunk == 0 {
            s.receiving = Some(Receiving {
                index,
                epoch: index_epoch,
                next_chunk: 0,
                buckets: Vec::new(),
            });
        }
        let Some(r) = s.receiving.as_mut().filter(|r| r.index == index) else {
            return Ok(());
        };
        if chunk < r.next_chunk {
            // Its acknowledgement was lost.
            out.push(ack(chunk, false));
            return Ok(());
        }
        if chunk > r.next_chunk {
            return Ok(());
        }
        merge(&mut r.buckets, buckets);
        r.next_chunk += 1;
        if !last {
            out.push(ack(chunk, false));
            return Ok(());
        }
        let Some(r) = s.receiving.take() else {
            return Ok(());
        };
        let before = self.log.snapshot()?;
        let snap = Snapshot {
            index: r.index,
            epoch: r.epoch,
            buckets: r.buckets,
        };
        self.log.install(&snap)?;
        notify_differences(&before.buckets, &snap.buckets, out);
        out.push(ack(chunk, true));
        Ok(())
    }

    /// `RecvReply`, an ack.
    fn recv_ack(&mut self, matched: u64, now: Instant) {
        if let Role::Primary(p) = &mut self.role {
            if matched > p.matched {
                p.progress_at = now;
                p.acked_at = Some(now);
                // The partner matches again: the nacks of appends that
                // were in flight behind a gap no longer hold the next.
                p.last_nack = None;
                p.held = false;
            }
            p.matched = p.matched.max(matched);
            p.next = p.next.max(p.matched + 1);
        }
    }

    /// `RecvReply`, a nack: resend from after the partner's commit index.
    fn recv_nack(&mut self, hint: u64, now: Instant) {
        let last = self.log.last();
        if let Role::Primary(p) = &mut self.role {
            p.next = (hint + 1).min(last + 1);
            p.held = p.last_nack == Some(p.next);
            p.last_nack = Some(p.next);
            p.progress_at = now;
        }
    }

    fn recv_snapshot_ack(&mut self, index: u64, chunk: u32, done: bool, now: Instant) {
        let Role::Primary(p) = &mut self.role else {
            return;
        };
        let Some(s) = p.sending.as_mut().filter(|s| s.index == index) else {
            return;
        };
        p.progress_at = now;
        if done {
            p.acked_at = Some(now);
            p.matched = p.matched.max(index);
            p.next = p.next.max(index + 1);
            p.sending = None;
        } else if chunk == s.chunk {
            s.chunk += 1;
            s.sent_at = None;
        }
    }

    /// Apply entries up to `to` (if past the commit index), answer the
    /// writes among them, and tell their watches.
    fn commit(&mut self, to: u64, out: &mut Vec<Effect>) -> Result<(), String> {
        if to <= self.log.commit() {
            return Ok(());
        }
        let committed = self.log.commit_to(to)?;
        let mut waiting = match &mut self.role {
            Role::Primary(p) => Some(&mut p.waiting),
            Role::Standby(_) | Role::Idle => None,
        };
        for c in committed {
            let result = match c.write {
                None => continue,
                Some((bucket, Ok(applied))) => {
                    if let Some(revision) = applied.revision {
                        out.push(Effect::Notify {
                            bucket,
                            changed: applied.changed,
                            revision,
                        });
                    }
                    Ok(applied.outcome)
                }
                Some((_, Err(e))) => Err(Refusal::Failed(e)),
            };
            if let Some(waiter) = waiting.as_mut().and_then(|w| w.remove(&c.index)) {
                out.push(Effect::Reply { waiter, result });
            }
        }
        Ok(())
    }

    /// Everything a primary does as soon as it can: `Commit`, `GoSync`,
    /// `ReportSynced` and `Send`.
    fn progress(&mut self, now: Instant, out: &mut Vec<Effect>) -> Result<(), String> {
        let Role::Primary(p) = &self.role else {
            return Ok(());
        };
        // `Commit` (R1, R4): an entry of this epoch, acknowledged unless
        // alone, while the lease holds.
        let last = self.log.last();
        let to = if p.sync { p.matched.min(last) } else { last };
        if p.may_run && to > self.log.commit() && self.log.epoch_at(to) == Some(self.epoch) {
            self.commit(to, out)?;
        }
        let commit = self.log.commit();
        let epoch = self.epoch;
        let Role::Primary(p) = &mut self.role else {
            return Ok(());
        };
        if commit >= last {
            p.behind_since = None;
        }
        // `GoSync`: the partner acknowledges again and is close behind.
        // The model allows it at any time (`EagerSync`); waiting for the
        // whole log, as R2 first had it, never happens under steady
        // writes. Clearing the record still needs the partner to hold
        // every committed entry.
        let acking = p
            .acked_at
            .is_some_and(|t| now.duration_since(t) < self.cfg.resend_after);
        if !p.sync && acking && last.saturating_sub(p.matched) <= self.cfg.batch as u64 {
            p.sync = true;
        }
        // `ReportSynced` (R2): only in sync mode.
        if p.sync && p.partner_outdated && p.matched >= commit && !p.recording {
            p.recording = true;
            out.push(Effect::Record {
                epoch,
                outdated: false,
            });
        }
        self.send(now, out)
    }

    /// `Send`: the next entries, or a snapshot when they are compacted.
    fn send(&mut self, now: Instant, out: &mut Vec<Effect>) -> Result<(), String> {
        let epoch = self.epoch;
        let base = self.log.base();
        let last = self.log.last();
        let commit = self.log.commit();
        let Role::Primary(p) = &mut self.role else {
            return Ok(());
        };
        if p.sending.is_none() && !p.held && p.next <= base {
            let snap = self.log.snapshot()?;
            p.sending = Some(Sending {
                index: snap.index,
                epoch: snap.epoch,
                chunks: Arc::new(chunks(snap.buckets, self.cfg.chunk_bytes)),
                chunk: 0,
                sent_at: None,
            });
        }
        if let Some(s) = &mut p.sending {
            if s.sent_at.is_none() {
                let i = usize::try_from(s.chunk).unwrap_or(usize::MAX);
                let buckets = s.chunks.get(i).cloned().unwrap_or_default();
                out.push(Effect::Send(Msg::Snapshot {
                    epoch,
                    index: s.index,
                    index_epoch: s.epoch,
                    chunk: s.chunk,
                    last: i + 1 >= s.chunks.len(),
                    buckets,
                }));
                s.sent_at = Some(now);
            }
            return Ok(());
        }
        while !p.held && p.next <= last && p.next < p.unacked() + self.cfg.window {
            let prev = p.next - 1;
            let entries = self
                .log
                .entries(p.next, self.cfg.batch, self.cfg.batch_bytes)?;
            let n = entries.len() as u64;
            out.push(Effect::Send(Msg::Append {
                epoch,
                prev,
                prev_epoch: self
                    .log
                    .epoch_at(prev)
                    .ok_or("replicated log: sending after a compacted entry")?,
                entries,
                commit,
            }));
            p.next += n;
        }
        Ok(())
    }
}

fn refuse(writes: Vec<Write>, out: &mut Vec<Effect>) {
    for w in writes {
        out.push(Effect::Reply {
            waiter: w.waiter,
            result: Err(Refusal::Unavailable),
        });
    }
}

/// Split `buckets` into chunks of about `bytes` each; a bucket larger than
/// that spans chunks, as consecutive dumps of the same name.
fn chunks(buckets: Vec<BucketDump>, bytes: usize) -> Vec<Vec<BucketDump>> {
    let mut out: Vec<Vec<BucketDump>> = vec![Vec::new()];
    let mut size = 0;
    for bucket in buckets {
        let mut part = BucketDump {
            entries: Vec::new(),
            ..bucket.clone()
        };
        for entry in bucket.entries {
            let len = entry.key.len() + entry.value.len() + 16;
            if size + len > bytes && size > 0 {
                let rest = BucketDump {
                    entries: Vec::new(),
                    ..part.clone()
                };
                let full = std::mem::replace(&mut part, rest);
                if let Some(current) = out.last_mut() {
                    current.push(full);
                }
                out.push(Vec::new());
                size = 0;
            }
            size += len;
            part.entries.push(entry);
        }
        size += 32;
        if let Some(current) = out.last_mut() {
            current.push(part);
        }
    }
    out
}

/// Add a chunk's dumps, joining a bucket continued from the last chunk.
fn merge(into: &mut Vec<BucketDump>, buckets: Vec<BucketDump>) {
    for b in buckets {
        match into.last_mut() {
            Some(last) if last.name == b.name => last.entries.extend(b.entries),
            _ => into.push(b),
        }
    }
}

/// Watch notices for what a snapshot changed: each key whose value or
/// revision differs, or that is gone (at its bucket's new revision).
fn notify_differences(before: &[BucketDump], after: &[BucketDump], out: &mut Vec<Effect>) {
    let old: HashMap<&str, &BucketDump> = before.iter().map(|b| (b.name.as_str(), b)).collect();
    let new: HashMap<&str, &BucketDump> = after.iter().map(|b| (b.name.as_str(), b)).collect();
    let mut names: Vec<&str> = old.keys().chain(new.keys()).copied().collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let revision = new.get(name).map_or(0, |b| b.revision);
        let was: HashMap<&str, (&[u8], u64)> = old
            .get(name)
            .map(|b| {
                b.entries
                    .iter()
                    .map(|e| (e.key.as_str(), (e.value.as_slice(), e.revision)))
                    .collect()
            })
            .unwrap_or_default();
        let now: HashMap<&str, (&[u8], u64)> = new
            .get(name)
            .map(|b| {
                b.entries
                    .iter()
                    .map(|e| (e.key.as_str(), (e.value.as_slice(), e.revision)))
                    .collect()
            })
            .unwrap_or_default();
        let mut by_revision: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for (key, (value, rev)) in &now {
            if was.get(key) != Some(&(*value, *rev)) {
                by_revision
                    .entry(*rev)
                    .or_default()
                    .push((*key).to_string());
            }
        }
        for key in was.keys() {
            if !now.contains_key(key) {
                by_revision
                    .entry(revision)
                    .or_default()
                    .push((*key).to_string());
            }
        }
        for (rev, mut changed) in by_revision {
            changed.sort();
            out.push(Effect::Notify {
                bucket: name.to_string(),
                changed,
                revision: rev,
            });
        }
    }
}

#[cfg(test)]
#[path = "core_tests.rs"]
mod core_tests;
