#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

//! Two replicas driven by hand, messages passed between them on request
//! (or lost), on a paused clock: the cases spike S20's model checks.

use crate::services::keyvalue::storage::Disk;

use super::*;

const A: usize = 0;
const B: usize = 1;

struct Pair {
    nodes: [Replica; 2],
    /// Messages in flight to each node.
    inbox: [Vec<Msg>; 2],
    replies: Vec<(Waiter, Result<Outcome, Refusal>)>,
    /// Records asked by each node: (epoch, outdated).
    records: [Vec<(u64, bool)>; 2],
    notices: [Vec<(String, Vec<String>, u64)>; 2],
}

fn config() -> Config {
    Config {
        keep: 100,
        ..Config::default()
    }
}

fn replica(cfg: Config) -> Replica {
    Replica::new(cfg, Log::open(Arc::new(Disk::in_memory())).unwrap())
}

fn lead(standing: Standing, epoch: u64, may_run: bool) -> Lead {
    Lead {
        standing,
        epoch,
        may_run,
        partner_outdated: false,
    }
}

fn set(waiter: Waiter, key: &str, value: &str) -> Write {
    Write {
        waiter,
        bucket: "points".into(),
        quota: None,
        op: Op::Set(key.into(), value.as_bytes().to_vec()),
    }
}

impl Pair {
    fn new(cfg: Config) -> Self {
        Self {
            nodes: [replica(cfg), replica(cfg)],
            inbox: [Vec::new(), Vec::new()],
            replies: Vec::new(),
            records: [Vec::new(), Vec::new()],
            notices: [Vec::new(), Vec::new()],
        }
    }

    /// A primary at epoch 1 with its lease, B its standby, the no-op
    /// committed.
    fn running() -> Self {
        let mut pair = Self::new(config());
        pair.lead(B, lead(Standing::Standby, 1, false));
        pair.lead(A, lead(Standing::Primary, 1, true));
        pair.pump();
        pair
    }

    fn route(&mut self, from: usize, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Send(m) => self.inbox[1 - from].push(m),
                Effect::Record { epoch, outdated } => self.records[from].push((epoch, outdated)),
                Effect::Reply { waiter, result } => self.replies.push((waiter, result)),
                Effect::Notify {
                    bucket,
                    changed,
                    revision,
                } => self.notices[from].push((bucket, changed, revision)),
            }
        }
    }

    fn lead(&mut self, node: usize, lead: Lead) {
        let out = self.nodes[node].leadership(lead, Instant::now()).unwrap();
        self.route(node, out);
    }

    fn write(&mut self, node: usize, writes: Vec<Write>) {
        let out = self.nodes[node].write(writes, Instant::now()).unwrap();
        self.route(node, out);
    }

    fn tick(&mut self, node: usize, partner_heard: bool) {
        let out = self.nodes[node]
            .tick(Instant::now(), partner_heard)
            .unwrap();
        self.route(node, out);
    }

    fn recorded(&mut self, node: usize, epoch: u64, outdated: bool) {
        let out = self.nodes[node]
            .recorded(epoch, outdated, true, Instant::now())
            .unwrap();
        self.route(node, out);
    }

    /// Deliver what is in flight, both ways, until nothing is.
    fn pump(&mut self) {
        while self.inbox.iter().any(|q| !q.is_empty()) {
            for node in [A, B] {
                for m in std::mem::take(&mut self.inbox[node]) {
                    let out = self.nodes[node].message(m, Instant::now()).unwrap();
                    self.route(node, out);
                }
            }
        }
    }

    /// Lose what is in flight.
    fn drop_all(&mut self) {
        self.inbox = [Vec::new(), Vec::new()];
    }

    fn value(&self, node: usize, key: &str) -> Option<Vec<u8>> {
        let snap = self.nodes[node].log().snapshot().unwrap();
        snap.buckets
            .iter()
            .flat_map(|b| b.entries.iter())
            .find(|e| e.key == key)
            .map(|e| e.value.clone())
    }

    fn buckets(&self, node: usize) -> Vec<BucketDump> {
        self.nodes[node].log().snapshot().unwrap().buckets
    }
}

#[tokio::test(start_paused = true)]
async fn write____in_sync____committed_only_once_the_standby_acknowledges() {
    let mut pair = Pair::running();

    pair.write(A, vec![set(7, "a", "1")]);
    let before_ack = pair.replies.clone();
    pair.pump();

    assert!(before_ack.is_empty());
    assert_eq!(pair.replies, [(7, Ok(Outcome::Done))]);
    assert_eq!(pair.value(A, "a"), Some(b"1".to_vec()));
    assert_eq!(pair.notices[A], [("points".into(), vec!["a".into()], 1)]);
}

#[tokio::test(start_paused = true)]
async fn standby____commit_index_arrives____applies_and_notifies() {
    let mut pair = Pair::running();
    pair.write(A, vec![set(7, "a", "1")]);
    pair.pump();

    pair.tick(A, true);
    pair.pump();

    assert_eq!(pair.value(B, "a"), Some(b"1".to_vec()));
    assert_eq!(pair.notices[B], [("points".into(), vec!["a".into()], 1)]);
    assert_eq!(pair.nodes[B].log().commit(), pair.nodes[A].log().commit());
}

#[tokio::test(start_paused = true)]
async fn write____without_a_lease_or_on_the_standby____unavailable() {
    let mut pair = Pair::running();
    pair.lead(A, lead(Standing::Primary, 1, false));

    pair.write(A, vec![set(1, "a", "1")]);
    pair.write(B, vec![set(2, "b", "2")]);

    assert_eq!(
        pair.replies,
        [
            (1, Err(Refusal::Unavailable)),
            (2, Err(Refusal::Unavailable))
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn standby_gone____primary_records_it_out_of_date_then_commits_alone() {
    let mut pair = Pair::running();

    pair.write(A, vec![set(7, "a", "1")]);
    pair.drop_all();
    pair.tick(A, true);
    let too_soon = pair.records[A].clone();
    tokio::time::advance(Duration::from_millis(1_100)).await;
    pair.tick(A, true);
    let asked = pair.records[A].clone();
    let before_answer = pair.replies.clone();
    pair.recorded(A, 1, true);

    assert!(too_soon.is_empty());
    assert_eq!(asked, [(1, true)]);
    assert!(before_answer.is_empty(), "nothing commits alone unrecorded");
    assert_eq!(pair.replies, [(7, Ok(Outcome::Done))]);
    assert!(pair.nodes[A].alone());
}

#[tokio::test(start_paused = true)]
async fn standby_silent_on_every_path____asks_at_once() {
    let mut pair = Pair::running();

    pair.write(A, vec![set(7, "a", "1")]);
    pair.drop_all();
    pair.tick(A, false);

    assert_eq!(pair.records[A], [(1, true)]);
}

#[tokio::test(start_paused = true)]
async fn standby_back____caught_up_then_sync_and_the_record_cleared() {
    let mut pair = Pair::running();
    pair.write(A, vec![set(7, "a", "1")]);
    pair.drop_all();
    pair.tick(A, false);
    pair.recorded(A, 1, true);
    pair.write(A, vec![set(8, "b", "2")]);
    pair.drop_all();

    tokio::time::advance(Duration::from_millis(600)).await;
    pair.tick(A, true);
    pair.pump();
    let report = pair.records[A].clone();
    pair.recorded(A, 1, false);
    pair.tick(A, true);
    pair.pump();

    assert!(!pair.nodes[A].alone());
    assert_eq!(report, [(1, true), (1, false)]);
    assert_eq!(pair.value(B, "b"), Some(b"2".to_vec()));
    assert_eq!(pair.buckets(A), pair.buckets(B));
}

#[tokio::test(start_paused = true)]
async fn lost_append____resent_after_the_timeout() {
    let mut pair = Pair::running();

    pair.write(A, vec![set(7, "a", "1")]);
    pair.drop_all();
    pair.tick(A, true);
    pair.drop_all();
    tokio::time::advance(Duration::from_millis(600)).await;
    pair.tick(A, true);
    pair.pump();

    assert_eq!(pair.replies, [(7, Ok(Outcome::Done))]);
}

#[tokio::test(start_paused = true)]
async fn primary_steps_down____waiting_writes_answered_unknown() {
    let mut pair = Pair::running();
    pair.write(A, vec![set(7, "a", "1")]);
    pair.drop_all();

    pair.lead(A, lead(Standing::Down, 1, false));

    assert_eq!(pair.replies, [(7, Err(Refusal::Unknown))]);
}

#[tokio::test(start_paused = true)]
async fn new_primary____stale_suffix_on_the_standby____dropped_and_replaced() {
    let mut pair = Pair::running();
    // A appends at epoch 1 but B never sees it.
    pair.write(A, vec![set(7, "stale", "x")]);
    pair.drop_all();
    // B is promoted at epoch 2 and writes; A rejoins as its standby.
    pair.lead(A, lead(Standing::Standby, 2, false));
    pair.lead(B, lead(Standing::Primary, 2, true));
    pair.write(B, vec![set(8, "fresh", "y")]);
    pair.pump();
    pair.tick(B, true);
    pair.pump();

    assert_eq!(
        pair.replies,
        [(7, Err(Refusal::Unknown)), (8, Ok(Outcome::Done))]
    );
    assert_eq!(pair.value(A, "stale"), None);
    assert_eq!(pair.value(A, "fresh"), Some(b"y".to_vec()));
    assert_eq!(pair.nodes[A].log().last(), pair.nodes[B].log().last());
}

#[tokio::test(start_paused = true)]
async fn standby_far_behind____catches_up_from_a_snapshot_in_chunks() {
    let mut pair = Pair::new(Config {
        keep: 0,
        chunk_bytes: 64,
        ..Config::default()
    });
    pair.lead(B, lead(Standing::Standby, 1, false));
    pair.lead(A, lead(Standing::Primary, 1, true));
    pair.drop_all();
    tokio::time::advance(Duration::from_millis(1_100)).await;
    pair.tick(A, false);
    pair.recorded(A, 1, true);
    let writes = (0..20)
        .map(|i| set(i, &format!("key{i:02}"), "some value"))
        .collect();
    pair.write(A, writes);
    pair.drop_all();
    pair.tick(A, false);
    pair.drop_all();

    tokio::time::advance(Duration::from_millis(600)).await;
    pair.tick(A, true);
    pair.pump();

    assert_eq!(pair.nodes[A].log().base(), pair.nodes[A].log().commit());
    assert_eq!(pair.buckets(A), pair.buckets(B));
    assert_eq!(pair.buckets(B)[0].entries.len(), 20);
    assert!(
        !pair.nodes[A].alone(),
        "back in sync once the snapshot is in"
    );
    assert_eq!(pair.notices[B].iter().map(|n| n.1.len()).sum::<usize>(), 20);
}

#[tokio::test(start_paused = true)]
async fn snapshot____standby_holds_its_last_entry____keeps_its_log_and_commits() {
    let mut pair = Pair::running();
    pair.write(A, vec![set(7, "a", "1")]);
    pair.pump();
    let index = pair.nodes[A].log().commit();
    let last_before = pair.nodes[B].log().last();

    let out = pair.nodes[B]
        .message(
            Msg::Snapshot {
                epoch: 1,
                index,
                index_epoch: 1,
                chunk: 0,
                last: true,
                buckets: Vec::new(),
            },
            Instant::now(),
        )
        .unwrap();

    assert!(out.contains(&Effect::Send(Msg::SnapshotAck {
        epoch: 1,
        index,
        chunk: 0,
        done: true
    })));
    assert_eq!(pair.nodes[B].log().last(), last_before);
    assert_eq!(pair.nodes[B].log().commit(), index);
    assert_eq!(pair.value(B, "a"), Some(b"1".to_vec()), "not replaced");
}

#[tokio::test(start_paused = true)]
async fn read_ready____only_once_the_epochs_noop_commits_with_the_lease() {
    let mut pair = Pair::new(config());
    pair.lead(B, lead(Standing::Standby, 1, false));
    pair.lead(A, lead(Standing::Primary, 1, true));

    let before = pair.nodes[A].read_ready();
    pair.pump();
    let after = pair.nodes[A].read_ready();
    pair.lead(A, lead(Standing::Primary, 1, false));
    let without_lease = pair.nodes[A].read_ready();

    assert!(!before);
    assert!(after);
    assert!(!without_lease);
    assert!(!pair.nodes[B].read_ready());
}

#[tokio::test(start_paused = true)]
async fn message____from_another_epoch____dropped() {
    let mut pair = Pair::running();

    let out = pair.nodes[B]
        .message(
            Msg::Append {
                epoch: 9,
                prev: 0,
                prev_epoch: 0,
                entries: vec![Entry {
                    epoch: 9,
                    cmd: Command::Noop,
                }],
                commit: 1,
            },
            Instant::now(),
        )
        .unwrap();

    assert!(out.is_empty());
    assert_eq!(pair.nodes[B].log().last(), 1);
}

#[test]
fn chunks____bucket_larger_than_a_chunk____split_and_merged_back() {
    use crate::services::keyvalue::storage::DumpEntry;
    let bucket = |name: &str, n: usize| BucketDump {
        name: name.into(),
        revision: 5,
        deleted: 2,
        entries: (0..n)
            .map(|i| DumpEntry {
                key: format!("k{i}"),
                value: vec![0; 10],
                revision: 1,
            })
            .collect(),
    };
    let buckets = vec![bucket("a", 7), bucket("b", 0), bucket("c", 3)];

    let parts = chunks(buckets.clone(), 60);
    let mut merged = Vec::new();
    for part in parts.clone() {
        merge(&mut merged, part);
    }

    assert!(parts.len() > 2);
    assert_eq!(merged, buckets);
}

#[tokio::test(start_paused = true)]
async fn steady_writes____standby_back_but_always_a_write_behind____sync_again_and_record_cleared()
{
    let mut pair = Pair::running();
    pair.write(A, vec![set(1, "a", "1")]);
    pair.drop_all();
    pair.tick(A, false);
    pair.recorded(A, 1, true);
    tokio::time::advance(Duration::from_millis(600)).await;
    pair.tick(A, true);

    // Each round: the standby takes what is in flight and acks, but a new
    // write is appended before its acks arrive.
    for i in 2..12 {
        let to_b = std::mem::take(&mut pair.inbox[B]);
        for m in to_b {
            let out = pair.nodes[B].message(m, Instant::now()).unwrap();
            pair.route(B, out);
        }
        pair.write(A, vec![set(i, &format!("k{i}"), "v")]);
        let to_a = std::mem::take(&mut pair.inbox[A]);
        for m in to_a {
            let out = pair.nodes[A].message(m, Instant::now()).unwrap();
            pair.route(A, out);
        }
        tokio::time::advance(Duration::from_millis(10)).await;
    }

    assert!(
        !pair.nodes[A].alone(),
        "back in sync with a write in flight"
    );
    assert_eq!(pair.records[A], [(1, true), (1, false)]);
}
