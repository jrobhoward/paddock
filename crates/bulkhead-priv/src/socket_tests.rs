#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::os::fd::AsFd;

use super::*;

#[test]
fn send____two_messages_queued____received_one_at_a_time() {
    let (a, b) = pair().unwrap();

    send(a.as_fd(), b"first", &[]).unwrap();
    send(a.as_fd(), b"second!", &[]).unwrap();
    let one = recv(b.as_fd()).unwrap();
    let two = recv(b.as_fd()).unwrap();

    let Received::Message(one, _) = one else {
        panic!("not a message")
    };
    let Received::Message(two, _) = two else {
        panic!("not a message")
    };
    assert_eq!(one, b"first");
    assert_eq!(two, b"second!");
}

#[test]
fn send____with_a_descriptor_then_without____each_its_own() {
    let (a, b) = pair().unwrap();
    let (r, _w) = rustix::pipe::pipe().unwrap();

    send(a.as_fd(), b"with", &[r.as_fd()]).unwrap();
    send(a.as_fd(), b"without", &[]).unwrap();
    let one = recv(b.as_fd()).unwrap();
    let two = recv(b.as_fd()).unwrap();

    let Received::Message(one, fds) = one else {
        panic!("not a message")
    };
    assert_eq!((one.as_slice(), fds.len()), (&b"with"[..], 1));
    let Received::Message(two, fds) = two else {
        panic!("not a message")
    };
    assert_eq!((two.as_slice(), fds.len()), (&b"without"[..], 0));
}
