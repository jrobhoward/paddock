//! Listeners and connection handlers (`bulkhead:connection`), with the
//! connection fixture's line protocol built as a native program, whose pool
//! of instances each hold many connections.

// Native apps run on Linux only.
#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{
    AppDeployment, Apps, InstanceHandle, InstanceSpec, LifecycleState, Listener, ListenerSpec,
    LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use common::{app_manifest, spec};
use rstest::rstest;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;

/// The connection fixture built as a native program, once per test binary.
fn native_conn() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target/fixtures/native-conn");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "bulkhead-fixture-conn",
                "--features",
                "native",
            ])
            .args(["--bin", "bulkhead-fixture-conn", "--target-dir"])
            .arg(&target)
            .status()
            .unwrap()
            .success();
        assert!(ok, "building the native connection fixture failed");
        target.join("debug/bulkhead-fixture-conn")
    })
}

/// A listener on the fixture, and what it hands connections to.
struct Served {
    listener: Listener,
    rt: Runtime,
    /// The chaos app, for targets of calls.
    chaos: LoadedApp,
    /// The app whose instances take connections.
    apps: Apps,
}

impl Served {
    /// Start an instance of the chaos app beside the handler.
    fn spawn(&self, spec: InstanceSpec) -> InstanceHandle {
        self.rt.spawn(&self.chaos, spec).unwrap()
    }

    /// The app's instances, once each is running.
    async fn instances(&self, n: usize) -> Vec<InstanceHandle> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let running: Vec<InstanceHandle> = self
                .apps
                .app("conn")
                .unwrap()
                .instances()
                .into_iter()
                .filter(|h| h.state() == LifecycleState::Active)
                .collect();
            if running.len() == n {
                return running;
            }
            assert!(
                Instant::now() < deadline,
                "{} of {n} running",
                running.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// A listener on the fixture; `instances` declares the app's pool (`count`,
/// `routing`).
async fn serve(
    name: &str,
    instances: serde_json::Value,
    configure: impl FnOnce(&mut ListenerSpec),
) -> Served {
    let mut native = NativeConfig::default();
    native.runner = Some(common::native_fixtures().0.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let chaos = rt.load_in_process::<bulkhead_host::in_process::chaos::Chaos>();
    let code = rt.load_native(NativeCode {
        path: native_conn().clone(),
        kind: NativeProgram::Executable,
    });
    let manifest = app_manifest(
        "conn",
        json!({
            "kind": "native-process",
            "native": { "program": "executable" },
            "instances": instances,
            "limits": { "callback_deadline_ms": 300, "memory_max": "64MiB" }
        }),
    );
    let apps = rt
        .start_apps(vec![AppDeployment::new(manifest, code)])
        .await
        .unwrap();
    let mut s = ListenerSpec::new(name, "127.0.0.1:0".parse().unwrap());
    configure(&mut s);
    let listener = rt.listen_native(s, apps.app("conn").unwrap()).unwrap();
    let served = Served {
        listener,
        rt,
        chaos,
        apps,
    };
    let count = instances["count"].as_u64().unwrap_or(1);
    served.instances(usize::try_from(count).unwrap()).await;
    served
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

async fn eventually(listener: &Listener, done: impl Fn(&bulkhead_host::ListenerStats) -> bool) {
    for _ in 0..200 {
        if done(&listener.stats()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not reached: {:?}", listener.stats());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____line_protocol____served_and_closed_by_quit() {
    let served = serve("lines", json!({ "count": 2 }), |_| {}).await;
    let listener = &served.listener;
    let mut a = Client::connect(listener).await;
    let mut b = Client::connect(listener).await;

    let echo = a.ask("echo hello").await;
    let info = a.ask("info").await.unwrap();
    let (pid_a, pid_b) = (a.ask("self").await.unwrap(), b.ask("self").await.unwrap());
    let quit = a.ask("quit").await;
    eventually(listener, |s| s.active == 1).await;

    assert_eq!(echo.as_deref(), Some("hello"));
    assert!(info.starts_with("lines 127.0.0.1:"), "{info}");
    assert_ne!(pid_a, pid_b, "each on its own instance");
    assert_eq!(quit, None);
    let stats = listener.stats();
    assert_eq!((stats.accepted, stats.active, stats.crashed), (2, 1, 0));
    listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____call_registered_instance____reply_comes_back() {
    let served = serve("calls", json!({}), |_| {}).await;
    let mut s = spec("register:conn-target");
    s.registered_names = vec!["conn-target".into()];
    let target = served.spawn(s);
    target
        .call(common::text("echo:up"), common::LONG)
        .await
        .unwrap();
    let mut c = Client::connect(&served.listener).await;

    let reply = c.ask("call conn-target echo:through the host").await;
    let missing = c.ask("call nobody echo:x").await;

    assert_eq!(reply.as_deref(), Some("through the host"));
    assert_eq!(missing.as_deref(), Some("err:noproc"));
    served.listener.stop();
    target.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native____error____closes_that_connection_only() {
    let served = serve("errors", json!({}), |_| {}).await;
    let mut keep = Client::connect(&served.listener).await;
    let mut error = Client::connect(&served.listener).await;
    keep.ask("echo up").await;

    let errored = error.ask("error").await;
    eventually(&served.listener, |s| s.active == 1).await;
    let still = keep.ask("echo still here").await;

    assert_eq!(errored, None);
    assert_eq!(still.as_deref(), Some("still here"));
    served.listener.stop();
}

#[rstest]
#[case::crash("crash")]
#[case::spin("spin")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native____crash_or_spin____closes_the_instances_connections_then_it_restarts(
    #[case] command: &str,
) {
    let served = serve("crashes", json!({}), |_| {}).await;
    let before = served.instances(1).await[0].pid();
    let mut other = Client::connect(&served.listener).await;
    let mut failing = Client::connect(&served.listener).await;
    other.ask("echo up").await;

    let failed = failing.ask(command).await;
    let lost = other.line().await;
    eventually(&served.listener, |s| s.active == 0).await;
    let after = served.instances(1).await[0].pid();
    let mut next = Client::connect(&served.listener).await;
    let fresh = next.ask("echo new").await;

    assert_eq!((failed, lost), (None, None));
    assert_ne!(before, after, "restarted");
    assert_eq!(fresh.as_deref(), Some("new"));
    served.listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native____many_connections____one_instance_serves_them_all() {
    let served = serve("many", json!({}), |_| {}).await;
    let mut clients = Vec::new();
    for _ in 0..8 {
        clients.push(Client::connect(&served.listener).await);
    }

    let mut pids = std::collections::BTreeSet::new();
    let mut echoes = Vec::new();
    for (i, c) in clients.iter_mut().enumerate() {
        pids.insert(c.ask("self").await.unwrap());
        echoes.push(c.ask(&format!("echo {i}")).await.unwrap());
    }

    assert_eq!(pids.len(), 1);
    assert_eq!(echoes, (0..8).map(|i| i.to_string()).collect::<Vec<_>>());
    assert_eq!(served.listener.stats().active, 8);
    served.listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native____hash_routing____a_peers_connections_share_an_instance() {
    let pool = json!({ "count": 3, "routing": "hash:peer" });
    let served = serve("hashed", pool, |_| {}).await;
    let mut pids = std::collections::BTreeSet::new();

    for _ in 0..6 {
        let mut c = Client::connect(&served.listener).await;
        pids.insert(c.ask("self").await.unwrap());
    }

    assert_eq!(pids.len(), 1, "one peer address, one instance: {pids:?}");
    served.listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____idle____closed_by_idle_timeout() {
    let configure = |s: &mut ListenerSpec| s.idle_timeout = Duration::from_millis(200);
    let served = serve("idle", json!({}), configure).await;
    let mut talker = Client::connect(&served.listener).await;
    let mut silent = Client::connect(&served.listener).await;

    let mut replies = Vec::new();
    for _ in 0..4 {
        replies.push(talker.ask("echo on").await);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let quiet = silent.line().await;
    eventually(&served.listener, |s| s.idle_closed >= 1).await;
    let after = talker.line().await;

    assert!(
        replies.iter().all(|r| r.as_deref() == Some("on")),
        "{replies:?}"
    );
    assert_eq!((quiet, after), (None, None));
    eventually(&served.listener, |s| s.active == 0).await;
    assert_eq!(served.listener.stats().idle_closed, 2);
    served.listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection____over_max_connections____refused() {
    let configure = |s: &mut ListenerSpec| s.max_connections = 2;
    let served = serve("capped", json!({}), configure).await;
    let listener = &served.listener;
    let mut a = Client::connect(listener).await;
    let mut b = Client::connect(listener).await;
    a.ask("echo a").await;
    b.ask("echo b").await;

    let mut c = Client::connect(listener).await;
    let refused = c.ask("echo c").await;
    a.ask("quit").await;
    eventually(listener, |s| s.active == 1).await;
    let mut d = Client::connect(listener).await;
    let admitted = d.ask("echo d").await;

    assert_eq!(refused, None);
    assert_eq!(admitted.as_deref(), Some("d"));
    assert_eq!(listener.stats().refused, 1);
    listener.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener____stop____closes_connections_and_stops_accepting() {
    let served = serve("stopping", json!({}), |_| {}).await;
    let listener = &served.listener;
    let mut open = Client::connect(listener).await;
    open.ask("echo up").await;
    let addr = listener.local_addr();

    listener.stop();
    let closed = open.line().await;
    eventually(listener, |s| s.active == 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reconnect = TcpStream::connect(addr).await;

    assert_eq!(closed, None);
    assert!(reconnect.is_err(), "still accepting");
}
