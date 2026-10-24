//! Guest sockets (`wasi:sockets`) through the net fixture, against local
//! peers: the manifest's rules and the address guard, `max_sockets` across
//! an app's instances, listening, and readiness delivered as host events
//! (`paddock:io/ready`).

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{ask, ask_until, fixture};
use paddock_bundle::manifest::{Capability, SocketRule, SocketsScope};
use paddock_host::{InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn runtime() -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_net")).unwrap();
    (rt, app)
}

fn rule(cidr: &str) -> SocketRule {
    SocketRule {
        cidr: cidr.to_string(),
        ports: None,
    }
}

fn scope(connect: &[&str], bind: &[&str], listen: bool, max: u32) -> SocketsScope {
    SocketsScope {
        connect: connect.iter().map(|c| rule(c)).collect(),
        bind: bind.iter().map(|c| rule(c)).collect(),
        listen,
        max_sockets: max,
    }
}

fn spec(app: &str, scope: SocketsScope, granted: bool) -> InstanceSpec {
    let mut s = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: if granted {
            [Capability::Sockets].into_iter().collect()
        } else {
            Default::default()
        },
        ..InstanceSpec::new(app, "1.0.0")
    };
    s.services.sockets.scope = Some(scope);
    s
}

fn up(rt: &(Runtime, LoadedApp), spec: InstanceSpec) -> InstanceHandle {
    rt.0.spawn(&rt.1, spec).unwrap()
}

/// A local peer that accepts connections and keeps them, so they stay open.
async fn peer() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            if tx.send(stream).is_err() {
                return;
            }
        }
    });
    (address, rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect____allowed_rule____bytes_both_ways_as_ready_events() {
    let (address, mut accepted) = peer().await;
    let rt = runtime();
    let h = up(
        &rt,
        spec("net-ok", scope(&["127.0.0.1/32"], &[], false, 4), true),
    );

    let id = ask(&h, &format!("connect {address}")).await;
    let mut server = accepted.recv().await.unwrap();
    let watched = ask(&h, &format!("watch {id}")).await;
    let idle = ask(&h, "received").await;
    server.write_all(b"hello").await.unwrap();
    let received = ask_until(&h, "received", |r| !r.is_empty()).await;
    let sent = ask(&h, &format!("send {id} back")).await;
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).await.unwrap();

    assert_eq!(id, "1");
    assert_eq!(
        (watched.as_str(), idle.as_str()),
        ("ok", ""),
        "calls answered while waiting"
    );
    assert_eq!(received, "1:hello");
    assert_eq!(sent, "ok");
    assert_eq!(&buf, b"back");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready____peer_closes_and_rearmed____each_notice_once() {
    let (address, mut accepted) = peer().await;
    let rt = runtime();
    let h = up(
        &rt,
        spec("net-close", scope(&["127.0.0.1/32"], &[], false, 4), true),
    );
    let id = ask(&h, &format!("connect {address}")).await;
    let mut server = accepted.recv().await.unwrap();
    ask(&h, &format!("watch {id}")).await;

    server.write_all(b"a").await.unwrap();
    ask_until(&h, "received", |r| r == "1:a").await;
    server.write_all(b"b").await.unwrap();
    ask_until(&h, "received", |r| r == "1:a,1:b").await;
    drop(server);
    let received = ask_until(&h, "received", |r| r.ends_with("closed")).await;
    let open = ask(&h, "open").await;

    assert_eq!(received, "1:a,1:b,1:closed");
    assert_eq!(open, "0");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect____ungranted_outside_rules_or_guarded____denied() {
    let (address, _accepted) = peer().await;
    let rt = runtime();
    let ungranted = up(
        &rt,
        spec(
            "net-ungranted",
            scope(&["127.0.0.1/32"], &[], false, 4),
            false,
        ),
    );
    let elsewhere = up(
        &rt,
        spec(
            "net-elsewhere",
            scope(&["127.0.0.2/32"], &[], false, 4),
            true,
        ),
    );
    let broad = up(
        &rt,
        spec("net-broad", scope(&["0.0.0.0/0"], &[], false, 4), true),
    );

    let a = ask(&ungranted, &format!("connect {address}")).await;
    let b = ask(&elsewhere, &format!("connect {address}")).await;
    let c = ask(&broad, &format!("connect {address}")).await;
    let d = ask(&broad, "connect-host localhost 9").await;

    assert_eq!(
        (a.as_str(), b.as_str(), c.as_str()),
        ("err:denied", "err:denied", "err:denied")
    );
    assert!(d.starts_with("err:"), "{d}");
    let metrics = rt.0.render_metrics();
    assert!(
        metrics.contains("paddock_sockets_refused_total{app=\"net-broad\"}"),
        "{metrics}"
    );
    for h in [ungranted, elsewhere, broad] {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_host____no_connect_rules____name_lookup_denied() {
    let rt = runtime();
    let h = up(
        &rt,
        spec("net-lookup", scope(&[], &["127.0.0.1/32"], false, 4), true),
    );

    let reply = ask(&h, "connect-host localhost 9").await;

    // wasmtime-wasi reports a lookup it may not make as a resolver failure.
    assert!(reply.contains("permanent-resolver-failure"), "{reply}");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_sockets____across_instances_and_udp____limited_until_one_closes() {
    let (address, _accepted) = peer().await;
    let rt = runtime();
    let rules = || scope(&["127.0.0.1/32"], &["127.0.0.1/32"], false, 3);
    let a = up(&rt, spec("net-cap", rules(), true));
    let b = up(&rt, spec("net-cap", rules(), true));

    let first = ask(&a, &format!("connect {address}")).await;
    let udp = ask(&a, "udp 127.0.0.1:0").await;
    let second = ask(&b, &format!("connect {address}")).await;
    let over = ask(&b, &format!("connect {address}")).await;
    let over_udp = ask(&b, "udp 127.0.0.1:0").await;
    let open = rt.0.render_metrics();
    ask(&a, &format!("close {udp}")).await;
    let after_close = ask(&b, &format!("connect {address}")).await;
    a.kill();
    a.wait().await;
    let after_exit = ask(&b, &format!("connect {address}")).await;

    assert_eq!(
        (first.as_str(), udp.as_str(), second.as_str()),
        ("1", "2", "1")
    );
    assert_eq!(
        (over.as_str(), over_udp.as_str()),
        ("err:limited", "err:limited")
    );
    assert!(
        open.contains("paddock_sockets_open{app=\"net-cap\"} 3"),
        "{open}"
    );
    assert_eq!(after_close, "2");
    assert_eq!(after_exit, "3", "the exited instance's socket was released");
    let metrics = rt.0.render_metrics();
    assert!(
        metrics.contains("paddock_sockets_open{app=\"net-cap\"} 3"),
        "{metrics}"
    );
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen____with_listen_and_bind_rule____accepts_and_reads() {
    let rt = runtime();
    let h = up(
        &rt,
        spec("net-listen", scope(&[], &["127.0.0.1/32"], true, 4), true),
    );
    let refused = up(
        &rt,
        spec(
            "net-nolisten",
            scope(&[], &["127.0.0.1/32"], false, 4),
            true,
        ),
    );

    let reply = ask(&h, "listen 127.0.0.1:0").await;
    let (id, port) = reply.split_once(' ').unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap()))
        .await
        .unwrap();
    client.write_all(b"ping").await.unwrap();
    let received = ask_until(&h, "received", |r| r.contains(":ping")).await;
    let no = ask(&refused, "listen 127.0.0.1:0").await;

    assert_eq!(id, "1");
    assert_eq!(received, "1:accepted 2,2:ping");
    assert_eq!(no, "err:denied");
    h.kill();
    refused.kill();
}
