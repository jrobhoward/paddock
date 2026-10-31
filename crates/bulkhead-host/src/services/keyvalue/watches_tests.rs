#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use crate::instance::prepare;
use crate::spec::InstanceSpec;

fn handle(id: u64) -> InstanceHandle {
    let pid = Pid {
        node: 1,
        id,
        incarnation: 1,
    };
    let (handle, task) = prepare(pid, &InstanceSpec::new("app", "1.0.0"), Arc::default());
    // Keep the receivers open, so notices queue rather than vanish.
    std::mem::forget(task);
    handle
}

fn bucket(name: &str) -> Key {
    ("app".to_string(), name.to_string())
}

impl Watches {
    /// Watches alive, those `owner` holds, and buckets with any.
    fn counts(&self, owner: Pid) -> (usize, usize, usize) {
        let t = self.table.lock().unwrap();
        (
            self.active.load(Ordering::Relaxed),
            t.by_owner.get(&owner).map_or(0, BTreeMap::len),
            t.by_bucket.len(),
        )
    }
}

#[test]
fn exited____instance_held_exact_and_prefix_watches____all_removed_and_ended() {
    let watches = Watches::default();
    let me = handle(1);
    let other = handle(2);
    let a = watches.add(bucket("b1"), "b1", "k", false, &me, 8).unwrap();
    let b = watches.add(bucket("b2"), "b2", "p/", true, &me, 8).unwrap();
    watches
        .add(bucket("b1"), "b1", "k", false, &other, 8)
        .unwrap();
    let before = watches.counts(me.pid());

    watches.exited(me.pid());

    assert_eq!(before, (3, 2, 2));
    assert_eq!(watches.counts(me.pid()), (1, 0, 1));
    assert!(a.ended.load(Ordering::Relaxed) && b.ended.load(Ordering::Relaxed));
    assert_eq!(watches.counts(other.pid()).1, 1);
}

#[test]
fn notify____after_the_watcher_exited____nothing_queued_for_it() {
    let watches = Watches::default();
    let me = handle(3);
    let watch = watches.add(bucket("b"), "b", "k", false, &me, 8).unwrap();
    watches.exited(me.pid());

    watches.notify(&bucket("b"), &["k".to_string()], 7);

    assert!(watch.pending.lock().unwrap().is_empty());
}

#[test]
fn add____after_exit____the_limit_counts_from_zero_again() {
    let watches = Watches::default();
    let me = handle(4);
    watches.add(bucket("b"), "b", "k", false, &me, 1).unwrap();
    let full = watches.add(bucket("b"), "b", "j", false, &me, 1);
    watches.exited(me.pid());

    let again = watches.add(bucket("b"), "b", "j", false, &me, 1);

    assert_eq!(full.unwrap_err(), LimitReached);
    assert!(again.is_ok());
}
