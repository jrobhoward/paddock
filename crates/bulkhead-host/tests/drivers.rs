//! Driver apps (ROADMAP DA1): the profile node config gives an app by
//! name, beyond its manifest. Listening sockets the node binds and keeps,
//! on which the driver accepts itself; files it may read under /driver;
//! the apps it may call. The native fixture through `bulkhead-runner`.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{
    DriverListener, DriverProfile, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, Runtime,
    RuntimeConfig,
};
use common::{ask, native_config, native_fixtures};

struct Node {
    rt: Runtime,
    code: LoadedApp,
    /// Where the driver's listener `main` listens.
    address: SocketAddr,
    /// The driver's file, and one beside it it was not given.
    file: PathBuf,
    other_file: PathBuf,
}

/// A runtime whose app `drv` is a driver: listener `main` on a port of
/// its own, one file, and `drv-target` the only app it may call.
fn node(test: &str) -> Node {
    node_with(test, false)
}

/// [`node`], with the driver asking for the HTTP route table or not.
fn node_with(test: &str, routes: bool) -> Node {
    let dir = std::env::temp_dir().join(format!("bulkhead-drivers-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("shop.pem");
    let other_file = dir.join("other.pem");
    std::fs::write(&file, "the driver's certificate").unwrap();
    std::fs::write(&other_file, "not given").unwrap();
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native: native_config(),
        drivers: BTreeMap::from([(
            "drv".to_string(),
            DriverProfile {
                listeners: vec![DriverListener {
                    name: "main".into(),
                    socket: Arc::new(socket),
                }],
                files: vec![file.clone()],
                calls: vec!["drv-target".into()],
                routes,
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let code = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    Node {
        rt,
        code,
        address,
        file,
        other_file,
    }
}

fn up(n: &Node, app: &str) -> InstanceHandle {
    let spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new(app, "1.0.0")
    };
    n.rt.spawn(&n.code, spec).unwrap()
}

/// Connect to `address` and read what the driver writes until it closes.
fn greeted(address: SocketAddr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = TcpStream::connect(address).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut got = String::new();
        s.read_to_string(&mut got).unwrap();
        got
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____client_connects____the_driver_accepts_on_its_own_listener() {
    let n = node("accept");
    let d = up(&n, "drv");

    let names = ask(&d, "listeners").await;
    let client = greeted(n.address);
    let peer = ask(&d, "accept main").await;
    let got = client.join().unwrap();

    assert_eq!(names, "main");
    assert!(!peer.starts_with("err:"), "{peer}");
    assert_eq!(got, "hello\n");
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____restarted____the_port_stays_and_a_waiting_client_is_served() {
    let n = node("restart");
    let first = up(&n, "drv");
    ask(&first, "listeners").await;
    first.kill();
    let _ = first.wait().await;

    let client = greeted(n.address);
    let second = up(&n, "drv");
    let peer = ask(&second, "accept main").await;
    let got = client.join().unwrap();

    assert!(!peer.starts_with("err:"), "{peer}");
    assert_eq!(got, "hello\n");
    second.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listeners____not_a_driver____none() {
    let n = node("plain");
    let plain = up(&n, "plain");

    let names = ask(&plain, "listeners").await;

    assert_eq!(names, "");
    plain.kill();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept____on_a_descriptor_not_passed____refused_for_drivers_and_apps() {
    let n = node("refused");
    let d = up(&n, "drv");
    let plain = up(&n, "plain");

    let driver_elsewhere = ask(&d, "accept-fd 3").await;
    let app_on_eight = ask(&plain, "accept-fd 8").await;

    assert!(
        driver_elsewhere.contains("not permitted"),
        "{driver_elsewhere}"
    );
    assert!(app_on_eight.contains("not permitted"), "{app_on_eight}");
    d.kill();
    plain.kill();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____its_files____readable_under_driver_and_others_beside_them_not() {
    let n = node("files");
    let d = up(&n, "drv");

    let given = ask(&d, "vcat /driver/shop.pem").await;
    let beside = ask(&d, &format!("read {}", n.other_file.display())).await;
    let direct = ask(&d, &format!("read {}", n.file.display())).await;

    assert_eq!(given, "the driver's certificate");
    assert!(beside.starts_with("err:"), "{beside}");
    assert_eq!(direct, "ok");
    d.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____calls____only_the_apps_its_profile_lists() {
    let n = node("calls");
    let d = up(&n, "drv");
    let target = up(&n, "drv-target");
    let other = up(&n, "drv-other");
    let target_pid = ask(&target, "self").await;
    let other_pid = ask(&other, "self").await;

    let allowed = ask(&d, &format!("call {target_pid} echo x")).await;
    let refused = ask(&d, &format!("call {other_pid} echo x")).await;

    assert_eq!(allowed, "x");
    assert_eq!(refused, "err:AccessDenied");
    d.kill();
    target.kill();
    other.kill();
}

#[cfg(feature = "http")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver____asks_for_routes____told_the_table_and_each_change() {
    use bulkhead_host::http::{HttpIngress, HttpRoute};
    let n = node_with("routes", true);
    let d = up(&n, "drv");
    let plain = up(&n, "plain");
    ask(&d, "listeners").await;
    let apps = n.rt.start_apps(Vec::new()).await.unwrap();
    let ingress = HttpIngress::new(apps);
    let route = |prefix: &str| HttpRoute {
        host: None,
        path_prefix: prefix.into(),
        hash_header: Some("x-customer".into()),
        max_body: 1024,
        timeout: Duration::from_secs(3),
    };

    ingress.set_routes("shop", vec![route("/shop")]).unwrap();
    let first = common::ask_until(&d, "infos", |i| i.contains("/shop")).await;
    ingress
        .set_routes("orders", vec![route("/orders")])
        .unwrap();
    let second = common::ask_until(&d, "infos", |i| i.contains("/orders")).await;
    let not_asked = ask(&plain, "infos").await;

    assert!(first.contains("event routes"), "{first}");
    assert!(first.contains(r#""app":"shop""#), "{first}");
    assert!(first.contains(r#""hash_header":"x-customer""#), "{first}");
    assert!(second.contains(r#""app":"orders""#), "{second}");
    assert!(!not_asked.contains("routes"), "{not_asked}");
    d.kill();
    plain.kill();
}
