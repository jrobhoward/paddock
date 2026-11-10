//! The free-port helper the node tests and the soak share
//! (`common/ports.rs`).

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::ports::free_port;

/// Below 32768: Linux's ephemeral range starts there by default, and
/// macOS's at 49152.
const EPHEMERAL_START: u16 = 32_768;

#[test]
fn free_port____many____distinct_and_below_the_ephemeral_range() {
    let ports: Vec<u16> = (0..50).map(|_| free_port().unwrap()).collect();

    let distinct: BTreeSet<u16> = ports.iter().copied().collect();
    assert_eq!(distinct.len(), ports.len(), "{ports:?}");
    assert!(ports.iter().all(|p| *p < EPHEMERAL_START), "{ports:?}");
}

#[test]
fn free_port____picked____bindable_for_tcp_and_udp() {
    let port = free_port().unwrap();

    let tcp = std::net::TcpListener::bind(("127.0.0.1", port));
    let udp = std::net::UdpSocket::bind(("127.0.0.1", port));

    assert!(tcp.is_ok(), "{tcp:?}");
    assert!(udp.is_ok(), "{udp:?}");
}
