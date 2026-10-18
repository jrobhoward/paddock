#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use crate::services::keyvalue::storage::Outcome;

fn set(epoch: u64, key: &str, value: &str) -> Entry {
    Entry {
        epoch,
        cmd: Command::Write {
            bucket: "points".into(),
            quota: None,
            op: Op::Set(key.into(), value.as_bytes().to_vec()),
        },
    }
}

fn noop(epoch: u64) -> Entry {
    Entry {
        epoch,
        cmd: Command::Noop,
    }
}

fn log() -> Log {
    Log::open(Arc::new(Disk::in_memory())).unwrap()
}

fn get(log: &Log, key: &str) -> Option<Vec<u8>> {
    let txn = log.disk().db().begin_read().unwrap();
    let dumps = log.disk().export(&txn, SPACE).unwrap();
    dumps
        .iter()
        .flat_map(|d| d.entries.iter())
        .find(|e| e.key == key)
        .map(|e| e.value.clone())
}

#[test]
fn append____over_an_uncommitted_suffix____replaces_it() {
    let mut log = log();
    log.append(1, &[noop(1), set(1, "a", "1"), set(1, "b", "2")])
        .unwrap();

    log.append(2, &[set(2, "c", "3")]).unwrap();

    assert_eq!(log.last(), 2);
    assert_eq!(log.epoch_at(2), Some(2));
    assert_eq!(
        log.entries(1, 10, 1 << 20).unwrap(),
        [noop(1), set(2, "c", "3")]
    );
}

#[test]
fn append____at_or_below_the_commit_index____refused() {
    let mut log = log();
    log.append(1, &[noop(1), set(1, "a", "1")]).unwrap();
    log.commit_to(2).unwrap();

    let result = log.append(2, &[set(2, "b", "2")]);

    assert!(result.is_err());
    assert_eq!(log.last(), 2);
}

#[test]
fn commit_to____writes____applied_with_outcomes_and_revisions() {
    let mut log = log();
    let quota = Entry {
        epoch: 1,
        cmd: Command::Write {
            bucket: "points".into(),
            quota: Some(4),
            op: Op::Set("big".into(), b"too large".to_vec()),
        },
    };
    log.append(1, &[noop(1), set(1, "a", "1"), quota]).unwrap();

    let committed = log.commit_to(3).unwrap();

    assert_eq!(log.commit(), 3);
    assert!(committed[0].write.is_none());
    let (bucket, applied) = committed[1].write.as_ref().unwrap();
    assert_eq!(bucket, "points");
    let applied = applied.as_ref().unwrap();
    assert_eq!(applied.outcome, Outcome::Done);
    assert_eq!(applied.revision, Some(1));
    assert!(committed[2].write.as_ref().unwrap().1.is_err());
    assert_eq!(get(&log, "a"), Some(b"1".to_vec()));
    assert_eq!(get(&log, "big"), None);
}

#[test]
fn open____after_appends_commits_and_compaction____same_state() {
    let disk = Arc::new(Disk::in_memory());
    let mut log = Log::open(disk.clone()).unwrap();
    log.append(1, &[noop(1), set(1, "a", "1"), set(2, "b", "2")])
        .unwrap();
    log.commit_to(2).unwrap();
    log.compact(1).unwrap();

    let reopened = Log::open(disk).unwrap();

    assert_eq!(
        (reopened.base(), reopened.commit(), reopened.last()),
        (1, 2, 3)
    );
    assert_eq!(reopened.epoch_at(1), Some(1));
    assert_eq!(reopened.epoch_at(3), Some(2));
    assert_eq!(reopened.epoch_at(0), Some(0));
}

#[test]
fn compact____past_the_commit_index____refused() {
    let mut log = log();
    log.append(1, &[noop(1), set(1, "a", "1")]).unwrap();
    log.commit_to(1).unwrap();

    let result = log.compact(2);

    assert!(result.is_err());
    assert_eq!(log.base(), 0);
}

#[test]
fn entries____compacted____refused_and_a_batch_holds_at_least_one() {
    let mut log = log();
    log.append(1, &[noop(1), set(1, "a", "1"), set(1, "b", "2")])
        .unwrap();
    log.commit_to(2).unwrap();
    log.compact(2).unwrap();

    let compacted = log.entries(2, 10, 1 << 20);
    let tiny = log.entries(3, 10, 1).unwrap();

    assert!(compacted.is_err());
    assert_eq!(tiny, [set(1, "b", "2")]);
}

#[test]
fn install____snapshot_of_another_log____buckets_and_indexes_replaced() {
    let mut source = log();
    source
        .append(1, &[noop(1), set(1, "a", "1"), set(1, "b", "2")])
        .unwrap();
    source.commit_to(3).unwrap();
    let snap = source.snapshot().unwrap();
    let mut target = log();
    target.append(1, &[noop(1), set(1, "stale", "x")]).unwrap();

    target.install(&snap).unwrap();

    assert_eq!((snap.index, snap.epoch), (3, 1));
    assert_eq!((target.base(), target.commit(), target.last()), (3, 3, 3));
    assert_eq!(target.epoch_at(3), Some(1));
    assert_eq!(get(&target, "b"), Some(b"2".to_vec()));
    assert_eq!(get(&target, "stale"), None);
    assert_eq!(target.snapshot().unwrap().buckets, snap.buckets);
}
