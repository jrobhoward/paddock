#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use tokio::sync::mpsc::error::TrySendError;

use super::*;
use crate::message::Envelope;

fn cast(bytes: usize) -> Message {
    Message::Cast(Envelope::new("application/octet-stream", vec![0; bytes]))
}

fn timer(bytes: usize) -> Message {
    Message::Info(Info::Timer {
        reference: 1,
        msg: Envelope::new("application/octet-stream", vec![0; bytes]),
    })
}

#[test]
fn try_send____count_reached____full() {
    let (tx, _rx) = channel(2, 1 << 20);
    tx.try_send(cast(1)).unwrap();
    tx.try_send(cast(1)).unwrap();

    let third = tx.try_send(cast(1));

    assert!(matches!(third, Err(TrySendError::Full(_))));
}

#[test]
fn try_send____bytes_reached____full_with_slots_left() {
    let (tx, _rx) = channel(100, 10);
    tx.try_send(cast(6)).unwrap();

    let over = tx.try_send(cast(5));
    let fits = tx.try_send(cast(4));

    assert!(matches!(over, Err(TrySendError::Full(_))));
    assert!(fits.is_ok());
    assert_eq!(tx.queued(), 2);
    assert_eq!(tx.queued_bytes(), 10);
}

#[test]
fn try_send____timer_message____counts_its_body() {
    let (tx, _rx) = channel(100, 10);
    tx.try_send(timer(8)).unwrap();

    let over = tx.try_send(cast(3));

    assert!(matches!(over, Err(TrySendError::Full(_))));
}

#[test]
fn try_send____empty_bodies____limited_by_count_only() {
    let (tx, _rx) = channel(3, 1);
    for _ in 0..3 {
        tx.try_send(cast(0)).unwrap();
    }

    let fourth = tx.try_send(cast(0));

    assert!(matches!(fourth, Err(TrySendError::Full(_))));
    assert_eq!(tx.queued_bytes(), 0);
}

#[tokio::test]
async fn recv____message_taken____gives_its_bytes_back() {
    let (tx, mut rx) = channel(100, 10);
    tx.try_send(cast(10)).unwrap();

    let taken = rx.recv().await;
    let again = tx.try_send(cast(10));

    assert!(matches!(taken, Some(Message::Cast(_))));
    assert!(again.is_ok());
}

#[test]
fn close____drained____gives_every_byte_back() {
    let (tx, mut rx) = channel(100, 10);
    tx.try_send(cast(4)).unwrap();
    tx.try_send(cast(6)).unwrap();

    rx.close();
    while rx.try_recv().is_ok() {}

    assert_eq!(tx.queued_bytes(), 0);
    assert!(matches!(tx.try_send(cast(1)), Err(TrySendError::Closed(_))));
}

#[test]
fn try_send____closed_and_over_bytes____closed() {
    let (tx, mut rx) = channel(100, 4);
    rx.close();

    let sent = tx.try_send(cast(5));

    assert!(matches!(sent, Err(TrySendError::Closed(_))));
}

#[test]
fn raise_max_bytes____higher____room_for_more() {
    let (tx, _rx) = channel(100, 4);
    tx.try_send(cast(4)).unwrap();

    tx.raise_max_bytes(10);
    let fits = tx.try_send(cast(6));
    tx.raise_max_bytes(2);
    let over = tx.try_send(cast(1));

    assert!(fits.is_ok());
    assert!(matches!(over, Err(TrySendError::Full(_))));
    assert_eq!(tx.queued_bytes(), 10);
}

#[test]
fn weak_sender____mailbox_dropped____does_not_upgrade() {
    let (tx, rx) = channel(1, 1);
    let weak = tx.downgrade();

    drop(tx);
    drop(rx);

    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn send_timeout____room_now____queued_at_once() {
    let (tx, _rx) = channel(2, 10);

    let sent = tx.send_timeout(cast(5), Duration::from_secs(5)).await;

    assert_eq!(sent, Ok(()));
    assert_eq!(tx.queued(), 1);
}

#[tokio::test(start_paused = true)]
async fn send_timeout____no_room____times_out_unqueued() {
    let (tx, _rx) = channel(1, 10);
    tx.try_send(cast(1)).unwrap();

    let sent = tx.send_timeout(cast(1), Duration::from_millis(50)).await;

    assert_eq!(sent, Err(WaitError::Timeout));
    assert_eq!(tx.queued(), 1);
    assert_eq!(tx.queued_bytes(), 1);
}

#[tokio::test]
async fn send_timeout____slot_freed____queued_after_waiting() {
    let (tx, mut rx) = channel(1, 10);
    tx.try_send(cast(1)).unwrap();
    let waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(2), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    rx.recv().await.unwrap();
    let sent = waiter.await.unwrap();

    assert_eq!(sent, Ok(()));
    assert_eq!(tx.queued_bytes(), 2);
}

#[tokio::test]
async fn send_timeout____bytes_freed____queued_after_waiting() {
    let (tx, mut rx) = channel(100, 10);
    tx.try_send(cast(8)).unwrap();
    let waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(5), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    rx.recv().await.unwrap();
    let sent = waiter.await.unwrap();

    assert_eq!(sent, Ok(()));
    assert_eq!(tx.queued_bytes(), 5);
}

#[tokio::test]
async fn send_timeout____waiter_owed_room____later_try_send_full() {
    let (tx, mut rx) = channel(100, 10);
    tx.try_send(cast(8)).unwrap();
    let waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(10), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    rx.recv().await.unwrap();
    let barging = tx.try_send(cast(1));
    let sent = waiter.await.unwrap();

    assert!(matches!(barging, Err(TrySendError::Full(_))));
    assert_eq!(sent, Ok(()));
}

#[tokio::test]
async fn send_timeout____larger_than_the_limit____full_at_once() {
    let (tx, _rx) = channel(100, 10);

    let sent = tx.send_timeout(cast(11), Duration::from_secs(5)).await;

    assert_eq!(sent, Err(WaitError::Full));
}

#[tokio::test]
async fn send_timeout____mailbox_closed_while_waiting____closed() {
    let (tx, mut rx) = channel(1, 10);
    tx.try_send(cast(1)).unwrap();
    let waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(1), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    rx.close();
    while rx.try_recv().is_ok() {}
    let sent = waiter.await.unwrap();

    assert_eq!(sent, Err(WaitError::Closed));
}

#[tokio::test]
async fn send_timeout____two_waiting_for_bytes____queued_in_arrival_order() {
    let (tx, mut rx) = channel(100, 10);
    tx.try_send(cast(10)).unwrap();
    let first = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(9), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;
    let second = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(1), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    rx.recv().await.unwrap();
    assert_eq!(first.await.unwrap(), Ok(()));
    assert_eq!(second.await.unwrap(), Ok(()));
    let a = rx.recv().await.unwrap();
    let b = rx.recv().await.unwrap();

    assert!(matches!(a, Message::Cast(m) if m.body.len() == 9));
    assert!(matches!(b, Message::Cast(m) if m.body.len() == 1));
}

#[tokio::test(start_paused = true)]
async fn send_timeout____timed_out_waiting_for_bytes____later_try_send_queued() {
    let (tx, mut rx) = channel(100, 10);
    tx.try_send(cast(8)).unwrap();

    let sent = tx.send_timeout(cast(5), Duration::from_millis(50)).await;
    rx.recv().await.unwrap();
    let after = tx.try_send(cast(10));

    assert_eq!(sent, Err(WaitError::Timeout));
    assert!(after.is_ok());
}

#[tokio::test(start_paused = true)]
async fn send_timeout____timed_out_waiting_for_a_slot____gives_its_bytes_back() {
    let (tx, _rx) = channel(1, 10);
    tx.try_send(cast(1)).unwrap();

    let sent = tx.send_timeout(cast(5), Duration::from_millis(50)).await;

    assert_eq!(sent, Err(WaitError::Timeout));
    assert_eq!(tx.queued_bytes(), 1);
}

#[tokio::test]
async fn send_timeout____closed_while_waiting_for_bytes____closed_without_draining() {
    let (tx, mut rx) = channel(100, 10);
    tx.try_send(cast(10)).unwrap();
    let waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(1), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    rx.close();
    let sent = waiter.await.unwrap();

    assert_eq!(sent, Err(WaitError::Closed));
}

#[tokio::test]
async fn receiver_dropped____messages_left____bytes_back_and_waiter_closed() {
    let (tx, rx) = channel(100, 10);
    tx.try_send(cast(10)).unwrap();
    let waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(1), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    drop(rx);
    let sent = waiter.await.unwrap();

    assert_eq!(sent, Err(WaitError::Closed));
    assert_eq!(tx.queued_bytes(), 0);
}

#[tokio::test]
async fn raise_max_bytes____waiter_for_bytes____queued() {
    let (tx, _rx) = channel(100, 10);
    tx.try_send(cast(10)).unwrap();
    let waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(5), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    tx.raise_max_bytes(15);
    let sent = waiter.await.unwrap();

    assert_eq!(sent, Ok(()));
    assert_eq!(tx.queued_bytes(), 15);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn senders_of_both_kinds____many_threads____limit_held_and_every_byte_back() {
    const MAX: u64 = 1_000;
    let (tx, mut rx) = channel(64, MAX);
    let mut senders = Vec::new();
    for s in 0..8_usize {
        let tx = tx.clone();
        senders.push(tokio::spawn(async move {
            let mut queued = 0_u64;
            for i in 0..2_000_usize {
                let bytes = 1 + (s * 31 + i * 17) % 200;
                let sent = if (s + i) % 3 == 0 {
                    // The receiver keeps taking: a waiter left asleep is a
                    // lost wakeup.
                    let waited = tx.send_timeout(cast(bytes), Duration::from_secs(10)).await;
                    assert_eq!(waited, Ok(()));
                    true
                } else {
                    tx.try_send(cast(bytes)).is_ok()
                };
                if sent {
                    queued += 1;
                }
                assert!(tx.queued_bytes() <= MAX);
                if i % 50 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            queued
        }));
    }
    let receiver = tokio::spawn({
        let weak = tx.downgrade();
        async move {
            let mut taken = 0_u64;
            while let Some(_message) = rx.recv().await {
                taken += 1;
                if let Some(tx) = weak.upgrade() {
                    assert!(tx.queued_bytes() <= MAX);
                }
            }
            taken
        }
    });
    let mut queued = 0;
    for sender in senders {
        queued += sender.await.unwrap();
    }
    drop(tx);
    let taken = receiver.await.unwrap();

    assert_eq!(taken, queued);
}

#[test]
fn reserve____more_than_half____capped_at_half_and_not_queued() {
    let (tx, _rx) = channel(10, 100);

    let reservation = tx.reserve(10, 100).unwrap();

    assert_eq!(reservation.slots(), 5);
    assert_eq!(reservation.bytes(), 50);
    assert_eq!(tx.queued(), 0);
    assert_eq!(tx.queued_bytes(), 0);
}

#[test]
fn reserve____two_reservations____share_the_cap() {
    let (tx, _rx) = channel(10, 100);

    let first = tx.reserve(3, 30).unwrap();
    let second = tx.reserve(3, 30).unwrap();
    let third = tx.reserve(1, 1);

    assert_eq!((first.slots(), first.bytes()), (3, 30));
    assert_eq!((second.slots(), second.bytes()), (2, 20));
    assert!(third.is_none());
}

#[test]
fn reservation_send____mailbox_otherwise_full____queued() {
    let (tx, _rx) = channel(4, 100);
    let mut reservation = tx.reserve(2, 20).unwrap();
    while tx.try_send(cast(1)).is_ok() {}

    let first = reservation.send(cast(10));
    let second = reservation.send(cast(10));
    let third = reservation.send(cast(0));

    assert!(first.is_ok());
    assert!(second.is_ok());
    assert!(third.is_err());
    assert_eq!(tx.queued(), 4);
    assert_eq!(tx.queued_bytes(), 22);
}

#[test]
fn reservation_send____body_over_bytes_left____handed_back() {
    let (tx, _rx) = channel(10, 100);
    let mut reservation = tx.reserve(2, 10).unwrap();

    let sent = reservation.send(cast(11));

    assert!(matches!(sent, Err(Message::Cast(m)) if m.body.len() == 11));
    assert_eq!(reservation.slots(), 2);
}

#[test]
fn reservation____dropped____room_back() {
    let (tx, _rx) = channel(4, 100);
    let mut reservation = tx.reserve(2, 50).unwrap();
    reservation.send(cast(5)).unwrap();

    drop(reservation);
    let mut queued = 0;
    while tx.try_send(cast(1)).is_ok() {
        queued += 1;
    }

    assert_eq!(queued, 3);
    assert_eq!(tx.queued_bytes(), 8);
}

#[tokio::test]
async fn reservation_send____taken____gives_its_bytes_back() {
    let (tx, mut rx) = channel(10, 100);
    let mut reservation = tx.reserve(1, 40).unwrap();
    reservation.send(cast(30)).unwrap();
    drop(reservation);

    let taken = rx.recv().await;

    assert!(matches!(taken, Some(Message::Cast(m)) if m.body.len() == 30));
    assert_eq!(tx.queued_bytes(), 0);
    assert!(tx.try_send(cast(100)).is_ok());
}

#[tokio::test]
async fn reserve____waiter_for_bytes____none() {
    let (tx, _rx) = channel(100, 10);
    tx.try_send(cast(10)).unwrap();
    let _waiter = tokio::spawn({
        let tx = tx.clone();
        async move { tx.send_timeout(cast(5), Duration::from_secs(5)).await }
    });
    tokio::task::yield_now().await;

    let reservation = tx.reserve(1, 1);

    assert!(reservation.is_none());
}

#[test]
fn top_up____after_use____adds_within_the_cap() {
    let (tx, _rx) = channel(10, 100);
    let mut reservation = tx.reserve(4, 40).unwrap();
    reservation.send(cast(10)).unwrap();
    reservation.send(cast(10)).unwrap();

    let added = tx.top_up(&mut reservation, 10, 100);

    assert_eq!(added, (3, 30));
    assert_eq!((reservation.slots(), reservation.bytes()), (5, 50));
}
