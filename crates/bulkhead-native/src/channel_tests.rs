#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use rstest::rstest;
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// A channel and the app's end of it, with the bell's read end and the
/// door's write end.
fn app_end(spin: Duration) -> (Arc<AppEnd>, OwnedFd, OwnedFd) {
    let (channel, _memfd) = Channel::create().unwrap();
    let (bell_read, bell_write) = bell().unwrap();
    let (door_read, door_write) = door().unwrap();
    (
        Arc::new(AppEnd::new(channel, bell_write, door_read, Spin(spin))),
        bell_read,
        door_write,
    )
}

#[test]
fn ring____more_than_its_size_in_turns____bytes_in_order() {
    let (channel, _fd) = Channel::create().unwrap();
    let ring = channel.to_app();
    let data = pattern(3 * RING_CAP + 123);
    let mut got = Vec::new();
    let mut buf = vec![0; 10_000];

    let mut sent = 0;
    while got.len() < data.len() {
        sent += ring.write(&data[sent..]).unwrap();
        let n = ring.read(&mut buf).unwrap();
        got.extend_from_slice(&buf[..n]);
    }

    assert_eq!(got, data);
}

#[test]
fn ring____full____writes_nothing_more_until_read() {
    let (channel, _fd) = Channel::create().unwrap();
    let ring = channel.to_node();

    let first = ring.write(&pattern(RING_CAP + 10)).unwrap();
    let more = ring.write(b"x").unwrap();
    let room = ring.has_room();
    let n = ring.read(&mut [0; 4]).unwrap();
    let after = ring.write(b"abcd").unwrap();

    assert_eq!((first, more, room), (RING_CAP, 0, false));
    assert_eq!((n, after), (4, 4));
}

#[test]
fn ring____writer_claims_more_than_the_ring_holds____read_corrupt() {
    let (channel, _fd) = Channel::create().unwrap();
    let ring = channel.to_node();
    ring.word64(TAIL)
        .store(RING_CAP as u64 + 1, Ordering::Release);

    let read = ring.read(&mut [0; 16]);

    assert_eq!(read, Err(Corrupt));
    assert!(ring.is_readable());
}

#[test]
fn ring____reader_position_past_the_writer____write_corrupt() {
    let (channel, _fd) = Channel::create().unwrap();
    let ring = channel.to_app();
    ring.word64(HEAD).store(5, Ordering::Release);

    let write = ring.write(b"hello");

    assert_eq!(write, Err(Corrupt));
    assert!(ring.has_room());
}

#[rstest]
#[case::spinning(Duration::from_micros(20))]
#[case::sleeping(Duration::ZERO)]
fn app_end____node_writes_more_than_the_ring____app_reads_it_all(#[case] spin: Duration) {
    let (app, _bell, door) = app_end(spin);
    let data = pattern(5 * RING_CAP + 999);
    let reader = {
        let app = app.clone();
        let len = data.len();
        std::thread::spawn(move || {
            let mut got = vec![0; len];
            FromNode(&app).read_exact(&mut got).unwrap();
            got
        })
    };

    // The node's side: write what fits, wake the app, wait for room.
    let ring = app.channel.to_app();
    let mut sent = 0;
    while sent < data.len() {
        let n = ring.write(&data[sent..]).unwrap();
        sent += n;
        wake_reader(ring.reader_waiting(), door.as_fd());
        if n == 0 {
            std::thread::yield_now();
        }
    }
    let got = reader.join().unwrap();

    assert_eq!(got, data);
}

#[rstest]
#[case::spinning(Duration::from_micros(20))]
#[case::sleeping(Duration::ZERO)]
fn app_end____app_writes_more_than_the_ring____node_reads_it_all(#[case] spin: Duration) {
    let (app, _bell, _door) = app_end(spin);
    let data = pattern(4 * RING_CAP + 17);
    let writer = {
        let (app, data) = (app.clone(), data.clone());
        std::thread::spawn(move || ToNode(&app).write_all(&data).unwrap())
    };

    // The node's side: read what is there, wake the app if it waits for
    // room.
    let ring = app.channel.to_node();
    let mut got = Vec::new();
    let mut buf = vec![0; 8192];
    while got.len() < data.len() {
        let n = ring.read(&mut buf).unwrap();
        got.extend_from_slice(&buf[..n]);
        wake_writer(ring.writer_waiting());
        if n == 0 {
            std::thread::yield_now();
        }
    }
    writer.join().unwrap();

    assert_eq!(got, data);
}

#[test]
fn app_end____node_closed_the_ring____rest_then_end() {
    let (app, _bell, _door) = app_end(DEFAULT_SPIN);
    let ring = app.channel.to_app();
    ring.write(b"last").unwrap();
    ring.close();
    let mut buf = [0; 16];

    let n = app.read_from_node(&mut buf).unwrap();
    let end = app.read_from_node(&mut buf).unwrap();

    assert_eq!(&buf[..n], b"last");
    assert_eq!(end, 0);
}

#[test]
fn app_end____node_waits_for_a_frame____write_rings_the_bell_once() {
    let (app, bell_read, _door) = app_end(DEFAULT_SPIN);
    let ring = app.channel.to_node();
    ring.reader_waiting().store(1, Ordering::SeqCst);

    app.write_to_node(b"one").unwrap();
    app.write_to_node(b"two").unwrap();
    let mut rung = [0; 8];
    let n = rustix::io::read(&bell_read, &mut rung).unwrap();

    assert_eq!(n, 1);
    assert_eq!(ring.reader_waiting().load(Ordering::SeqCst), 0);
}

#[test]
fn app_end____node_waits_for_room____read_rings_the_bell() {
    let (app, bell_read, _door) = app_end(DEFAULT_SPIN);
    let ring = app.channel.to_app();
    ring.write(b"frame").unwrap();
    ring.writer_waiting().store(1, Ordering::SeqCst);

    app.read_from_node(&mut [0; 5]).unwrap();
    let n = rustix::io::read(&bell_read, &mut [0; 8]).unwrap();

    assert_eq!(n, 1);
    assert_eq!(ring.writer_waiting().load(Ordering::SeqCst), 0);
}

#[test]
fn open____not_a_channel_sized_file____refused() {
    let fd = memfd_create("small", MemfdFlags::CLOEXEC).unwrap();
    rustix::fs::ftruncate(&fd, 100).unwrap();

    let opened = Channel::open(fd.as_fd());

    assert!(opened.is_err());
}

#[test]
fn spin____from_env_without_the_variable____default() {
    let spin = if std::env::var_os(SPIN_ENV).is_none() {
        Spin::from_env().0
    } else {
        DEFAULT_SPIN
    };

    assert_eq!(spin, DEFAULT_SPIN);
}
