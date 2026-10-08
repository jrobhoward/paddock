//! Listeners and connection handlers (`paddock:connection`), with the
//! connection fixture's line protocol.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use common::{fixture, shared, spec};
use paddock_host::{Listener, ListenerSpec, LoadedConnection};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;

fn conn_v1() -> &'static LoadedConnection {
    static C: OnceLock<LoadedConnection> = OnceLock::new();
    C.get_or_init(|| {
        shared()
            .0
            .load_connection(&fixture("paddock_fixture_conn"))
            .unwrap()
    })
}

fn conn_v2() -> &'static LoadedConnection {
    static C: OnceLock<LoadedConnection> = OnceLock::new();
    C.get_or_init(|| {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target/fixtures/conn-v2");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "paddock-fixture-conn",
                "--features",
                "v2",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "building the v2 connection fixture failed"
        );
        let bytes =
            std::fs::read(target.join("wasm32-wasip2/release/paddock_fixture_conn.wasm")).unwrap();
        shared().0.load_connection(&bytes).unwrap()
    })
}

fn listen(name: &str, configure: impl FnOnce(&mut ListenerSpec)) -> Listener {
    let mut s = ListenerSpec::new(name, "127.0.0.1:0".parse().unwrap());
    configure(&mut s);
    shared().0.listen(s, conn_v1().clone(), spec("")).unwrap()
}

/// A line client.
struct Client {
    read: BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: OwnedWriteHalf,
}

impl Client {
    async fn connect(listener: &Listener) -> Self {
        let (read, write) = TcpStream::connect(listener.local_addr())
            .await
            .unwrap()
            .into_split();
        Self {
            read: BufReader::new(read),
            write,
        }
    }

    async fn send(&mut self, line: &str) {
        self.write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    /// The next line, or `None` once the server closed the connection.
    async fn line(&mut self) -> Option<String> {
        let mut s = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.read.read_line(&mut s))
            .await
            .expect("no reply within 5 s")
            .ok()?;
        (n > 0).then(|| s.trim_end().to_string())
    }

    async fn ask(&mut self, line: &str) -> Option<String> {
        self.send(line).await;
        self.line().await
    }
}

async fn eventually(listener: &Listener, done: impl Fn(&paddock_host::ListenerStats) -> bool) {
    for _ in 0..200 {
        if done(&listener.stats()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not reached: {:?}", listener.stats());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____line_protocol____served_by_its_own_instance() {
    let listener = listen("lines", |_| {});
    let mut a = Client::connect(&listener).await;
    let mut b = Client::connect(&listener).await;

    let echo = a.ask("echo hello").await;
    let info = a.ask("info").await.unwrap();
    let (pid_a, pid_b) = (a.ask("self").await.unwrap(), b.ask("self").await.unwrap());
    let quit = a.ask("quit").await;
    eventually(&listener, |s| s.active == 1).await;

    assert_eq!(echo.as_deref(), Some("hello"));
    assert!(info.starts_with("lines 127.0.0.1:"), "{info}");
    assert_ne!(pid_a, pid_b);
    assert_eq!(quit, None);
    let stats = listener.stats();
    assert_eq!((stats.accepted, stats.active, stats.crashed), (2, 1, 0));
    listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____call_registered_instance____reply_comes_back() {
    let (rt, app) = shared();
    let mut s = spec("register:conn-target");
    s.registered_names = vec!["conn-target".into()];
    let target = rt.spawn(app, s).unwrap();
    target
        .call(common::text("echo:up"), common::LONG)
        .await
        .unwrap();
    let listener = listen("calls", |_| {});
    let mut c = Client::connect(&listener).await;

    let reply = c.ask("call conn-target echo:through the host").await;
    let missing = c.ask("call nobody echo:x").await;

    assert_eq!(reply.as_deref(), Some("through the host"));
    assert_eq!(missing.as_deref(), Some("err:noproc"));
    listener.stop();
    target.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____crash_or_error____only_that_connection_closes() {
    let listener = listen("crashes", |_| {});
    let mut keep = Client::connect(&listener).await;
    let mut crash = Client::connect(&listener).await;
    let mut error = Client::connect(&listener).await;

    let crashed = crash.ask("crash").await;
    let errored = error.ask("error").await;
    eventually(&listener, |s| s.crashed == 2).await;
    let still = keep.ask("echo still here").await;
    let mut next = Client::connect(&listener).await;
    let fresh = next.ask("echo new").await;

    assert_eq!((crashed, errored), (None, None));
    assert_eq!(still.as_deref(), Some("still here"));
    assert_eq!(fresh.as_deref(), Some("new"));
    listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____idle_or_spinning____closed_by_idle_timeout() {
    let listener = listen("idle", |s| s.idle_timeout = Duration::from_millis(200));
    let mut spinner = Client::connect(&listener).await;
    let mut silent = Client::connect(&listener).await;

    spinner.send("spin").await;
    let spun = spinner.line().await;
    let quiet = silent.line().await;
    eventually(&listener, |s| s.idle_closed == 2).await;

    assert_eq!((spun, quiet), (None, None));
    assert_eq!(listener.stats().active, 0);
    listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____over_max_connections____refused() {
    let listener = listen("capped", |s| s.max_connections = 2);
    let mut a = Client::connect(&listener).await;
    let mut b = Client::connect(&listener).await;
    a.ask("echo a").await;
    b.ask("echo b").await;

    let mut c = Client::connect(&listener).await;
    let refused = c.ask("echo c").await;
    a.ask("quit").await;
    eventually(&listener, |s| s.active == 1).await;
    let mut d = Client::connect(&listener).await;
    let admitted = d.ask("echo d").await;

    assert_eq!(refused, None);
    assert_eq!(admitted.as_deref(), Some("d"));
    assert_eq!(listener.stats().refused, 1);
    listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener____replace____new_connections_get_new_version() {
    let listener = listen("upgrades", |_| {});
    let mut old = Client::connect(&listener).await;
    old.ask("echo before").await;

    listener.replace(conn_v2().clone(), spec(""));
    let mut new = Client::connect(&listener).await;
    let from_new = new.ask("echo x").await;
    let from_old = old.ask("echo x").await;

    assert_eq!(from_new.as_deref(), Some("v2:x"));
    assert_eq!(from_old.as_deref(), Some("x"));
    listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener____stop____closes_connections_and_stops_accepting() {
    let listener = listen("stopping", |_| {});
    let mut open = Client::connect(&listener).await;
    open.ask("echo up").await;
    let addr = listener.local_addr();

    listener.stop();
    let closed = open.line().await;
    eventually(&listener, |s| s.active == 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reconnect = TcpStream::connect(addr).await;

    assert_eq!(closed, None);
    assert!(reconnect.is_err(), "still accepting");
}
