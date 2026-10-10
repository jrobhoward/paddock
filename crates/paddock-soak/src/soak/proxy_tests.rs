#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use super::*;

/// Node 2 behind a proxy, and node 1 sending to it from its own socket.
struct Pair {
    node1: UdpSocket,
    node2: UdpSocket,
    faults: Arc<Faults>,
}

async fn pair() -> Pair {
    let node1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let node2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let faults = Arc::new(Faults::new(1));
    let peers = HashMap::from([
        (node1.local_addr().unwrap(), 1),
        (node2.local_addr().unwrap(), 2),
    ]);
    let proxy = start(2, node2.local_addr().unwrap(), peers, faults.clone())
        .await
        .unwrap();
    node1.connect(proxy).await.unwrap();
    Pair {
        node1,
        node2,
        faults,
    }
}

/// The next datagram on `socket` within 200 ms, and where it came from.
async fn receive(socket: &UdpSocket) -> Option<(Vec<u8>, SocketAddr)> {
    let mut buf = [0u8; 64];
    let (n, from) = tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf))
        .await
        .ok()?
        .ok()?;
    Some((buf[..n].to_vec(), from))
}

#[tokio::test]
async fn proxy____pass____delivers_both_ways() {
    let p = pair().await;

    p.node1.send(b"ping").await.unwrap();
    let (ping, via) = receive(&p.node2).await.unwrap();
    p.node2.send_to(b"pong", via).await.unwrap();
    let (pong, _) = receive(&p.node1).await.unwrap();

    assert_eq!(
        (ping.as_slice(), pong.as_slice()),
        (&b"ping"[..], &b"pong"[..])
    );
}

#[tokio::test]
async fn proxy____cut_one_way____only_that_direction_dropped() {
    let p = pair().await;
    p.node1.send(b"hello").await.unwrap();
    let (_, via) = receive(&p.node2).await.unwrap();

    p.faults.set(2, 1, Mode::Cut).await;
    p.node1.send(b"in").await.unwrap();
    let inbound = receive(&p.node2).await;
    p.node2.send_to(b"out", via).await.unwrap();
    let outbound = receive(&p.node1).await;

    assert_eq!(inbound.map(|(d, _)| d), Some(b"in".to_vec()));
    assert_eq!(outbound, None);
}

#[tokio::test]
async fn proxy____hold_then_pass____held_delivered_in_order() {
    let p = pair().await;

    p.faults.set(1, 2, Mode::Hold).await;
    for d in [b"a", b"b", b"c"] {
        p.node1.send(d).await.unwrap();
    }
    let while_held = receive(&p.node2).await;
    p.faults.heal_all().await;
    let mut after = Vec::new();
    while let Some((d, _)) = receive(&p.node2).await {
        after.push(d);
    }

    assert_eq!(while_held, None);
    assert_eq!(after, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
}

#[tokio::test]
async fn proxy____drop_all____nothing_delivered() {
    let p = pair().await;

    p.faults.set(1, 2, Mode::Drop(1000)).await;
    p.node1.send(b"lost").await.unwrap();
    let got = receive(&p.node2).await;

    assert_eq!(got, None);
}
