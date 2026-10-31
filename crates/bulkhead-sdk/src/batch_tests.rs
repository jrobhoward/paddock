#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

/// Timers that only count.
#[derive(Default)]
struct Fake {
    next: u64,
    armed: Vec<TimerRef>,
    cancelled: Vec<TimerRef>,
}

impl Timers for Fake {
    fn arm(&mut self, _delay: Duration) -> TimerRef {
        self.next += 1;
        let t = TimerRef(self.next);
        self.armed.push(t);
        t
    }

    fn cancel(&mut self, timer: TimerRef) {
        self.cancelled.push(timer);
    }
}

fn limits(items: usize, bytes: usize, delay: bool) -> Limits {
    Limits {
        max_items: items,
        max_bytes: bytes,
        max_delay: delay.then(|| Duration::from_millis(10)),
    }
}

fn decoded(msg: &Envelope) -> Vec<u32> {
    items::<u32>(msg).unwrap().unwrap()
}

#[test]
fn push____up_to_max_items____one_batch_in_order() {
    let (mut fake, mut b) = (Fake::default(), Batcher::new(limits(3, 1 << 20, false)));

    let first = b.push_with(&mut fake, "a", &1u32).unwrap();
    let second = b.push_with(&mut fake, "a", &2u32).unwrap();
    let third = b.push_with(&mut fake, "a", &3u32).unwrap();

    assert!(first.is_empty() && second.is_empty());
    assert_eq!(third.len(), 1);
    assert_eq!(third[0].0, "a");
    assert_eq!(decoded(&third[0].1), [1, 2, 3]);
    assert_eq!(third[0].1.schema.as_deref(), Some(SCHEMA));
    assert_eq!(b.waiting(&"a"), 0);
}

#[test]
fn push____two_destinations____batched_apart() {
    let (mut fake, mut b) = (Fake::default(), Batcher::new(limits(2, 1 << 20, false)));

    b.push_with(&mut fake, "a", &1u32).unwrap();
    b.push_with(&mut fake, "b", &10u32).unwrap();
    let a = b.push_with(&mut fake, "a", &2u32).unwrap();

    assert_eq!(decoded(&a[0].1), [1, 2]);
    assert_eq!(b.waiting(&"b"), 1);
}

#[test]
fn push____next_item_past_max_bytes____what_is_there_goes_first() {
    // Each u32 below 24 encodes in one byte; 1000 in three.
    let (mut fake, mut b) = (Fake::default(), Batcher::new(limits(100, 3, false)));

    b.push_with(&mut fake, "a", &1u32).unwrap();
    b.push_with(&mut fake, "a", &2u32).unwrap();
    let sent = b.push_with(&mut fake, "a", &1000u32).unwrap();

    assert_eq!(
        sent.len(),
        2,
        "the early batch, then the item that fills one alone"
    );
    assert_eq!(decoded(&sent[0].1), [1, 2]);
    assert_eq!(decoded(&sent[1].1), [1000]);
}

#[test]
fn push____first_item_with_delay____arms_one_timer_cancelled_when_full() {
    let (mut fake, mut b) = (Fake::default(), Batcher::new(limits(2, 1 << 20, true)));

    b.push_with(&mut fake, "a", &1u32).unwrap();
    let full = b.push_with(&mut fake, "a", &2u32).unwrap();

    assert_eq!(fake.armed, [TimerRef(1)]);
    assert_eq!(fake.cancelled, [TimerRef(1)]);
    assert_eq!(full.len(), 1);
    assert!(
        b.on_timer(TimerRef(1)).is_none(),
        "cancelled timer flushes nothing"
    );
}

#[test]
fn on_timer____partial_batch____flushed_and_next_item_arms_again() {
    let (mut fake, mut b) = (Fake::default(), Batcher::new(limits(10, 1 << 20, true)));
    b.push_with(&mut fake, "a", &7u32).unwrap();
    b.push_with(&mut fake, "a", &8u32).unwrap();

    let flushed = b.on_timer(TimerRef(1)).unwrap();
    let other = b.on_timer(TimerRef(99));
    b.push_with(&mut fake, "a", &9u32).unwrap();

    assert_eq!(flushed.0, "a");
    assert_eq!(decoded(&flushed.1), [7, 8]);
    assert!(other.is_none());
    assert_eq!(fake.armed, [TimerRef(1), TimerRef(2)]);
}

#[rstest::rstest]
#[case(0)]
#[case(23)]
#[case(24)]
#[case(255)]
#[case(256)]
#[case(70_000)]
fn array_header____any_count____decodes_as_that_many_items(#[case] n: usize) {
    let mut body = array_header(n);
    for _ in 0..n {
        ciborium::into_writer(&5u8, &mut body).unwrap();
    }

    let back: Vec<u8> = ciborium::from_reader(body.as_slice()).unwrap();

    assert_eq!(back.len(), n);
}

#[test]
fn items____not_a_batch____none() {
    let plain = Envelope::cbor(&[1u32, 2]).unwrap();
    let batch = envelope(&[1u32, 2]).unwrap();

    assert!(items::<u32>(&plain).is_none());
    assert_eq!(items::<u32>(&batch).unwrap().unwrap(), [1, 2]);
}
