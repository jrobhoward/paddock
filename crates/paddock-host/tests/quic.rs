//! The QUIC transport (ROADMAP D1): nodes joined through a seed address
//! with mutual TLS, calls across them, refused identities, and a peer that
//! goes away and comes back.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use common::{ask, ask_until, fixture, pid_text, spec};
use paddock_host::transport::quic::{
    NodeIdentity, PublicKey, QuicConfig, QuicTransport, generate_key,
};
use paddock_host::{InstanceHandle, LoadedApp, Runtime, RuntimeConfig};

fn chaos() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| fixture("paddock_fixture_chaos"))
}

/// Keys for nodes, written to `dir`, and a peers file listing `listed`.
struct Keys {
    dir: PathBuf,
    public: BTreeMap<u32, PublicKey>,
}

impl Keys {
    fn new(dir: &Path, nodes: &[u32]) -> Self {
        let mut public = BTreeMap::new();
        for &n in nodes {
            let (private, key) = generate_key().unwrap();
            std::fs::write(dir.join(format!("node-{n}.key")), private).unwrap();
            public.insert(n, key);
        }
        Self {
            dir: dir.to_path_buf(),
            public,
        }
    }

    /// A peers file listing each of `listed` against the node id given.
    fn peers(&self, name: &str, listed: &[(u32, u32)]) -> PathBuf {
        let mut text = String::from("[peers]\n");
        for &(id, key_of) in listed {
            text.push_str(&format!("{id} = [\"{}\"]\n", self.public[&key_of]));
        }
        let path = self.dir.join(format!("{name}.toml"));
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Node `n`'s identity with peers file `peers`.
    fn identity(&self, n: u32, peers: &Path) -> NodeIdentity {
        NodeIdentity {
            key: self.dir.join(format!("node-{n}.key")),
            peers: peers.to_path_buf(),
        }
    }
}

fn free_udp() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

struct Node {
    rt: Runtime,
    app: LoadedApp,
    quic: QuicTransport,
    address: SocketAddr,
}

impl Node {
    fn start(node: u32, creation: u32, address: SocketAddr, identity: NodeIdentity) -> Self {
        let rt = Runtime::new(RuntimeConfig {
            node,
            creation,
            max_instances: 8,
            heartbeat_interval: Duration::from_millis(100),
            node_timeout: Duration::from_millis(1_000),
            address: address.to_string(),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let quic = QuicTransport::start(
            &rt,
            QuicConfig {
                bind: address,
                identity,
                idle_timeout: Duration::from_secs(2),
            },
        )
        .unwrap();
        let app = rt.load(chaos()).unwrap();
        Self {
            rt,
            app,
            quic,
            address,
        }
    }

    fn spawn(&self) -> InstanceHandle {
        self.rt.spawn(&self.app, spec("")).unwrap()
    }
}

/// Start node `node` at `address` once its old endpoint has let the port
/// go.
async fn restart(node: u32, creation: u32, address: SocketAddr, id: NodeIdentity) -> Node {
    for _ in 0..100 {
        if std::net::UdpSocket::bind(address).is_ok() {
            return Node::start(node, creation, address, id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{address} stayed bound");
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..300 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_seed____two_nodes____members_and_calls_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::new(dir.path(), &[1, 2]);
    let peers = keys.peers("peers", &[(1, 1), (2, 2)]);
    let n1 = Node::start(1, 1, free_udp(), keys.identity(1, &peers));
    let n2 = Node::start(2, 1, free_udp(), keys.identity(2, &peers));

    let seed = n2.quic.join_seed(n1.address).await.unwrap();
    until("membership", || {
        n1.rt.members().len() == 2 && n2.rt.members().len() == 2
    })
    .await;
    let a = n1.spawn();
    let b = n2.spawn();
    let there = ask(
        &a,
        &format!("call:{}:2000:echo:over-quic", pid_text(b.pid())),
    )
    .await;
    let back = ask(
        &b,
        &format!("call:{}:2000:echo:and-back", pid_text(a.pid())),
    )
    .await;

    assert_eq!(seed, 1);
    assert_eq!(n1.quic.connected(), vec![2]);
    assert_eq!(there, "ok:over-quic");
    assert_eq!(back, "ok:and-back");
    a.kill();
    b.kill();
    n1.quic.shutdown();
    n2.quic.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_seed____another_nodes_key_or_an_unlisted_key____refused() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::new(dir.path(), &[1, 2, 3, 4]);
    // Node 1 knows nodes 2 and 3; node 4's key is listed nowhere.
    let peers = keys.peers("peers", &[(1, 1), (2, 2), (3, 3)]);
    let n1 = Node::start(1, 1, free_udp(), keys.identity(1, &peers));
    // Node 2, holding node 3's key.
    let impostor = Node::start(2, 1, free_udp(), keys.identity(3, &peers));
    let stranger = Node::start(4, 1, free_udp(), keys.identity(4, &peers));

    let wrong_key = impostor.quic.join_seed(n1.address).await;
    let unlisted = stranger.quic.join_seed(n1.address).await;

    assert!(wrong_key.is_err(), "{wrong_key:?}");
    assert!(unlisted.is_err(), "{unlisted:?}");
    assert!(n1.quic.connected().is_empty());
    for n in [&n1, &impostor, &stranger] {
        n.quic.shutdown();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload____peer_added_to_the_peers_file____then_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::new(dir.path(), &[1, 2]);
    let only_me = keys.peers("n1-peers", &[(1, 1)]);
    let both = keys.peers("n2-peers", &[(1, 1), (2, 2)]);
    let n1 = Node::start(1, 1, free_udp(), keys.identity(1, &only_me));
    let n2 = Node::start(2, 1, free_udp(), keys.identity(2, &both));

    let before = n2.quic.join_seed(n1.address).await;
    keys.peers("n1-peers", &[(1, 1), (2, 2)]);
    let reloaded = n1.quic.reload_if_changed().unwrap();
    until("the redialed seed", || n1.quic.connected() == vec![2]).await;

    assert!(before.is_err(), "{before:?}");
    assert!(reloaded);
    n1.quic.shutdown();
    n2.quic.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_restart____monitor_gets_noconnection_then_the_lower_node_redials() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::new(dir.path(), &[1, 2]);
    let peers = keys.peers("peers", &[(1, 1), (2, 2)]);
    let n1 = Node::start(1, 1, free_udp(), keys.identity(1, &peers));
    let n2_address = free_udp();
    let n2_id = keys.identity(2, &peers);
    let n2 = Node::start(2, 1, n2_address, n2_id.clone());
    n2.quic.join_seed(n1.address).await.unwrap();
    until("connection", || n1.quic.connected() == vec![2]).await;
    let watcher = n1.spawn();
    let target = n2.spawn();
    ask(&watcher, &format!("monitor:{}", pid_text(target.pid()))).await;

    // The link to node 2 goes away while its instance lives.
    n2.quic.shutdown();
    let downs = ask_until(&watcher, "infos", |i| i.contains("noconnection")).await;
    // Then node 2 restarts, as a crashed process would: nothing of it stays.
    target.kill();
    target.wait().await;
    drop(n2);
    let again = restart(2, 2, n2_address, n2_id).await;
    until("redial", || n1.quic.connected() == vec![2]).await;
    // Frames to node 2 flow again once a heartbeat says it is back.
    until("reachable", || {
        n1.rt.peer_status(2).is_some_and(|s| s.reachable)
    })
    .await;
    let b = again.spawn();
    let call = ask(
        &watcher,
        &format!("call:{}:2000:echo:back", pid_text(b.pid())),
    )
    .await;

    assert!(downs.contains("noconnection"), "{downs}");
    assert_eq!(call, "ok:back");
    watcher.kill();
    b.kill();
    n1.quic.shutdown();
    again.quic.shutdown();
}
