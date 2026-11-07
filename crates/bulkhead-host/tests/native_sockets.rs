//! Sockets for native-process apps beyond `tests/sockets.rs` (which runs
//! the SDK's `net` in both builds): UDP through the node, with each
//! destination and source on the manifest's rules, and a stream's close
//! at the node, which shuts the socket down whether or not the app kept
//! its copy (ROADMAP Z8e). The fixture executable's socket commands.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use bulkhead_bundle::manifest::{Capability, NativeProgram, PortRange, SocketRule, SocketsScope};
use bulkhead_host::{
    InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use common::{ask, ask_until, native_fixtures};
use tokio::io::AsyncReadExt;

fn runtime() -> (Runtime, LoadedApp) {
    let mut native = NativeConfig::default();
    native.runner = Some(native_fixtures().0.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    (rt, app)
}

fn spec(app: &str, scope: SocketsScope) -> InstanceSpec {
    let mut s = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(1),
        ..InstanceSpec::new(app, "1.0.0")
    };
    s.granted.insert(Capability::Sockets);
    s.services.sockets.scope = Some(scope);
    s
}

fn rule(cidr: &str, port: Option<u16>) -> SocketRule {
    SocketRule {
        cidr: cidr.into(),
        ports: port.map(|p| PortRange { start: p, end: p }),
    }
}

fn up(rt: &(Runtime, LoadedApp), spec: InstanceSpec) -> InstanceHandle {
    rt.0.spawn(&rt.1, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp____through_the_node____destinations_and_sources_on_the_rules() {
    let allowed = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let stranger = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let to = allowed.local_addr().unwrap();
    let rt = runtime();
    let h = up(
        &rt,
        spec(
            "udp",
            SocketsScope {
                connect: vec![rule("127.0.0.1/32", Some(to.port()))],
                bind: vec![rule("127.0.0.1/32", None)],
                listen: false,
                max_sockets: 4,
            },
        ),
    );
    let socket = ask(&h, "udp-bind 127.0.0.1:0").await;
    let sent = ask(&h, &format!("udp-send {socket} {to} hello")).await;
    let mut buf = [0u8; 64];
    let (n, from) = allowed.recv_from(&mut buf).await.unwrap();
    let elsewhere = ask(
        &h,
        &format!("udp-send {socket} {} nope", stranger.local_addr().unwrap()),
    )
    .await;

    let nothing = ask(&h, &format!("udp-recv {socket}")).await;
    let watched = ask(&h, &format!("net-watch {socket} 7")).await;
    stranger.send_to(b"dropped", from).await.unwrap();
    allowed.send_to(b"back", from).await.unwrap();
    let noticed = ask_until(&h, "infos", |i| i.contains("io-ready")).await;
    let got = ask_until(&h, &format!("udp-recv {socket}"), |r| r != "none").await;
    let after = ask(&h, &format!("udp-recv {socket}")).await;

    assert_eq!(sent, "ok");
    assert_eq!(&buf[..n], b"hello");
    assert_eq!(elsewhere, "err:Denied");
    assert_eq!(nothing, "none");
    assert_eq!(watched, "ok");
    assert!(noticed.contains("event io-ready {\"tag\":7}"), "{noticed}");
    assert_eq!(got, format!("{to} back"), "the stranger's datagram dropped");
    assert_eq!(after, "none");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream____closed_at_the_node____peer_sees_the_end_even_if_kept() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let rt = runtime();
    let h = up(
        &rt,
        spec(
            "tcp-close",
            SocketsScope {
                connect: vec![rule("127.0.0.1/32", None)],
                bind: vec![],
                listen: false,
                max_sockets: 4,
            },
        ),
    );

    let polite = ask(&h, &format!("tcp-connect {address}")).await;
    let (mut a, _) = listener.accept().await.unwrap();
    let liar = ask(&h, &format!("tcp-connect {address}")).await;
    let (mut b, _) = listener.accept().await.unwrap();
    let wrote = ask(&h, &format!("tcp-write {liar} hi")).await;
    let mut hi = [0u8; 2];
    b.read_exact(&mut hi).await.unwrap();
    ask(&h, &format!("tcp-close {polite}")).await;
    ask(&h, &format!("tcp-close-keep {liar}")).await;
    let mut rest = Vec::new();
    let a_end = tokio::time::timeout(Duration::from_secs(5), a.read_to_end(&mut rest)).await;
    let b_end = tokio::time::timeout(Duration::from_secs(5), b.read_to_end(&mut rest)).await;
    let after = ask(&h, &format!("tcp-write {liar} again")).await;

    assert_eq!(wrote, "ok");
    assert_eq!(&hi, b"hi");
    assert!(matches!(a_end, Ok(Ok(0))), "{a_end:?}");
    assert!(
        matches!(b_end, Ok(Ok(0))),
        "the kept copy was shut down too: {b_end:?}"
    );
    assert!(after.starts_with("err:"), "{after}");
    h.kill();
}
