//! The SCADA example (`examples/scada`) on one node: the node polls a
//! Modbus TCP device through the built-in `paddock-modbus:modbus`
//! extension and routes its updates by device to the `points` pool, which
//! keeps them in a shared bucket; `hmi` serves the table over HTTP with
//! long polls, and the `control` singleton raises and clears alarms from
//! it, writes to the device, keeps an event log, and carries its alarms
//! through an upgrade with a snapshot migration and back through a
//! rollback. The device is a fake served from this test.
//!
//! And on an HA pair with a witness, three `paddock node` processes: the
//! primary alone polls the device into the `cluster` point table, the
//! standby's table comes from it, and `control`'s replicated alarms keep
//! an acknowledgement through a planned switchover.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use paddock::node::{LogRing, Node, NodeConfig, builtin_extensions};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
use tokio_modbus::{ExceptionCode, Request, Response};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn build() -> PathBuf {
    let target = root().join("target/fixtures/examples-scada");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = StdCommand::new(cargo)
        .current_dir(root())
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .args(["-p", "example-scada-points", "-p", "example-scada-hmi"])
        .args(["-p", "example-scada-control"])
        .arg("--target-dir")
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok, "building the scada examples failed");
    target.join("wasm32-wasip2/release")
}

/// `control` version 2, in its own target directory (features differ).
fn build_v2() -> PathBuf {
    let target = root().join("target/fixtures/examples-scada-v2");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = StdCommand::new(cargo)
        .current_dir(root())
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .args(["-p", "example-scada-control", "--features", "v2"])
        .arg("--target-dir")
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok, "building control v2 failed");
    target.join("wasm32-wasip2/release/example_scada_control.wasm")
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn paddock(socket: &Path, args: &[&str]) -> Run {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .await
        .unwrap();
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A Modbus device: ten holding registers, readable and writable, and a
/// count of the reads it answered.
#[derive(Clone)]
struct FakeDevice {
    holding: Arc<Mutex<Vec<u16>>>,
    reads: Arc<AtomicUsize>,
}

impl tokio_modbus::server::Service for FakeDevice {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = Ready<Result<Response, ExceptionCode>>;

    fn call(&self, request: Self::Request) -> Self::Future {
        let mut regs = self.holding.lock().unwrap();
        if matches!(request, Request::ReadHoldingRegisters(..)) {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        ready(match request {
            Request::ReadHoldingRegisters(a, n) => regs
                .get(usize::from(a)..usize::from(a) + usize::from(n))
                .map(|r| Response::ReadHoldingRegisters(r.to_vec()))
                .ok_or(ExceptionCode::IllegalDataAddress),
            Request::WriteSingleRegister(a, value) => match regs.get_mut(usize::from(a)) {
                Some(slot) => {
                    *slot = value;
                    Ok(Response::WriteSingleRegister(a, value))
                }
                None => Err(ExceptionCode::IllegalDataAddress),
            },
            Request::WriteMultipleRegisters(a, values) => {
                let at = usize::from(a);
                match regs.get_mut(at..at + values.len()) {
                    Some(slot) => {
                        slot.copy_from_slice(&values);
                        Ok(Response::WriteMultipleRegisters(
                            a,
                            u16::try_from(values.len()).unwrap(),
                        ))
                    }
                    None => Err(ExceptionCode::IllegalDataAddress),
                }
            }
            _ => Err(ExceptionCode::IllegalFunction),
        })
    }
}

async fn fake_device(device: FakeDevice) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let on_connected = |stream, addr| {
            let device = device.clone();
            async move { accept_tcp_connection(stream, addr, |_| Ok(Some(device.clone()))) }
        };
        let _ = Server::new(listener).serve(&on_connected, |_| {}).await;
    });
    (address, task)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One request on its own connection: (status, body as JSON or a string).
async fn http(port: u16, method: &str, path: &str, body: &str) -> Option<(u16, Value)> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.ok()?;
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let status = head.split(' ').nth(1)?.parse().ok()?;
    let body = serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()));
    Some((status, body))
}

/// GET `path` until `test` passes on its body; the last body.
async fn until(port: u16, path: &str, test: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..250 {
        if let Some((200, body)) = http(port, "GET", path, "").await {
            if test(&body) {
                return body;
            }
            last = body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("GET {path} never passed; last: {last}");
}

fn write(register: u16, value: u16) -> String {
    format!(r#"{{"device":"pump-1","table":"holding","address":{register},"value":{value}}}"#)
}

fn alarm<'a>(body: &'a Value, name: &str) -> &'a Value {
    &body["alarms"][name]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scada____device_pool_hmi_and_control____polled_watched_alarmed_upgraded_rolled_back() {
    let wasm = build();
    let control_v2 = build_v2();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("a.sock");
    let holding = Arc::new(Mutex::new((100..110).collect::<Vec<u16>>()));
    let (device, device_task) = fake_device(FakeDevice {
        holding: holding.clone(),
        reads: Arc::default(),
    })
    .await;
    let keys = paddock(
        &socket,
        &["keygen", "--no-password", s(&dir.path().join("k"))],
    )
    .await;
    assert!(keys.ok, "{}", keys.stderr);
    let key = dir.path().join("k.key");
    let ex = root().join("examples/scada");
    // One node has no replication: control's buckets on disk instead. With
    // no failover the node runs singletons, so `primary_only` still polls.
    for v in ["paddock.toml", "paddock-v2.toml"] {
        let manifest = std::fs::read_to_string(ex.join("control").join(v)).unwrap();
        assert!(manifest.contains("\"replicated\""), "{v}");
        let single = manifest.replace("\"replicated\"", "\"disk\"");
        std::fs::write(dir.path().join(format!("control-{v}")), single).unwrap();
    }
    let mut bundles = Vec::new();
    for (name, manifest, wasm_file) in [
        (
            "points",
            "points/paddock.toml",
            wasm.join("example_scada_points.wasm"),
        ),
        (
            "hmi",
            "hmi/paddock.toml",
            wasm.join("example_scada_hmi.wasm"),
        ),
        (
            "control",
            "control-paddock.toml",
            wasm.join("example_scada_control.wasm"),
        ),
        ("control-v2", "control-paddock-v2.toml", control_v2),
    ] {
        let out = dir.path().join(format!("{name}.rbp"));
        let manifest = if name.starts_with("control") {
            dir.path().join(manifest)
        } else {
            ex.join(manifest)
        };
        let r = paddock(
            &socket,
            &[
                "bundle",
                s(&manifest),
                s(&wasm_file),
                "--key",
                s(&key),
                "-o",
                s(&out),
            ],
        )
        .await;
        assert!(r.ok, "bundle {name}: {}", r.stderr);
        bundles.push(out);
    }
    let policy = dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.points]\ngrant = [\"wasi:logging\", \"wasi:keyvalue\"]\nbuckets = [\"points\"]\n\
         [apps.hmi]\ngrant = [\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\"]\n\
         buckets = [\"points\"]\n\
         [apps.control]\ngrant = [\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\", \
         \"wasi:config\", \"paddock-modbus:modbus\"]\nbuckets = [\"points\"]\n",
    )
    .unwrap();
    let port = free_port();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {:?}\nmax_instances = 32\ntrusted_keys = [{:?}]\n\
         policy = {policy:?}\n\
         [http.main]\naddress = \"127.0.0.1:{port}\"\n\
         [buckets.points]\ndurability = \"memory\"\n\
         [extensions.\"paddock-modbus:modbus\".devices.pump-1]\naddress = \"{device}\"\n\
         poll_ms = 50\ntimeout_ms = 500\ndeliver_to = \"points\"\n\
         primary_only = true\n\
         holding = [{{ start = 0, count = 4 }}]\n",
        dir.path().join("data"),
        socket,
        keys.stdout,
    );
    let node = Arc::new(
        Node::open_with(
            NodeConfig::from_toml_str(&toml).unwrap(),
            LogRing::new(100),
            builtin_extensions(),
        )
        .await
        .unwrap(),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn({
        let node = node.clone();
        async move {
            paddock::node::serve(node, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        }
    });
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for b in &bundles[..3] {
        let r = paddock(&socket, &["deploy", s(b)]).await;
        assert!(r.ok, "deploy {}: {}", b.display(), r.stderr);
    }

    // Polled and stored: the first scan reaches the table.
    let table = until(port, "/scada/points", |t| {
        t["points"]["pump-1/holding/2"] == 102 && t["devices"]["pump-1"] == "online"
    })
    .await;
    let revision = table["revision"].as_u64().unwrap();
    // A long poll on the current revision waits out its time when nothing
    // changes, and answers with the same revision.
    let quiet = http(
        port,
        "GET",
        &format!("/scada/points?after={revision}&wait_ms=200"),
        "",
    )
    .await
    .unwrap();
    // One held across a change on the device answers with it.
    let held = tokio::spawn(async move {
        let path = format!("/scada/points?after={revision}&wait_ms=20000");
        http(port, "GET", &path, "").await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    holding.lock().unwrap()[3] = 7;
    let woken = held.await.unwrap().unwrap();
    // A write through control reaches the device and raises an alarm.
    let wrote = http(port, "POST", "/scada/write", &write(1, 200))
        .await
        .unwrap();
    let on_device = holding.lock().unwrap()[1];
    let raised = until(port, "/scada/alarms", |a| {
        alarm(a, "pump-1-pressure")["status"] == "active"
    })
    .await;
    let acked = http(port, "POST", "/scada/alarms/pump-1-pressure/ack", "")
        .await
        .unwrap();
    let again = http(port, "POST", "/scada/alarms/pump-1-pressure/ack", "")
        .await
        .unwrap();
    let unknown = http(port, "POST", "/scada/alarms/nothing/ack", "")
        .await
        .unwrap();
    http(port, "POST", "/scada/write", &write(1, 100))
        .await
        .unwrap();
    until(port, "/scada/alarms", |a| {
        alarm(a, "pump-1-pressure")["status"] == "normal"
    })
    .await;
    // Upgraded with its alarms, now counting raises from the migration.
    http(port, "POST", "/scada/write", &write(1, 300))
        .await
        .unwrap();
    until(port, "/scada/alarms", |a| {
        alarm(a, "pump-1-pressure")["status"] == "active"
    })
    .await;
    let up = paddock(&socket, &["upgrade", "control", s(&bundles[3])]).await;
    let upgraded = until(port, "/scada/alarms", |a| a["version"] == "2.0.0").await;
    // Rolled back with them: version 1 reads version 2's snapshot.
    let back = paddock(&socket, &["rollback", "control"]).await;
    let rolled = until(port, "/scada/alarms", |a| a["version"] == "1.0.0").await;
    let events = until(port, "/scada/events", |_| true).await;
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    let after_two = until(port, "/scada/events?after=2", |_| true).await;
    // The device goes away: an offline alarm.
    device_task.abort();
    let _ = device_task.await;
    let offline = until(port, "/scada/alarms", |a| {
        alarm(a, "pump-1-offline")["status"] == "active"
    })
    .await;

    assert_eq!(quiet.0, 200);
    assert_eq!(quiet.1["revision"].as_u64(), Some(revision));
    assert_eq!(woken.0, 200);
    assert_eq!(woken.1["points"]["pump-1/holding/3"], 7);
    assert_ne!(woken.1["revision"].as_u64(), Some(revision));
    assert_eq!(wrote.0, 200, "{}", wrote.1);
    assert_eq!(on_device, 200);
    assert_eq!(alarm(&raised, "pump-1-pressure")["value"], "200");
    assert_eq!(raised["version"], "1.0.0");
    assert_eq!(acked.0, 200, "{}", acked.1);
    assert_eq!(acked.1["status"], "acked");
    assert_eq!(again.0, 409);
    assert_eq!(unknown.0, 404);
    assert!(up.ok, "{} {}", up.stdout, up.stderr);
    assert!(
        up.stdout.starts_with("1.0.0 -> 2.0.0 (snapshot)"),
        "{}",
        up.stdout
    );
    assert_eq!(alarm(&upgraded, "pump-1-pressure")["status"], "active");
    assert_eq!(alarm(&upgraded, "pump-1-pressure")["raised"], 1);
    assert!(back.ok, "{} {}", back.stdout, back.stderr);
    assert_eq!(alarm(&rolled, "pump-1-pressure")["status"], "active");
    assert_eq!(alarm(&rolled, "pump-1-pressure")["raised"], Value::Null);
    assert_eq!(
        kinds,
        [
            "started", "write", "raised", "acked", "write", "normal", "write", "raised", "started",
            "started"
        ]
    );
    assert_eq!(events["next"], 10);
    assert_eq!(after_two["events"][0]["seq"], 3);
    assert!(
        alarm(&offline, "pump-1-offline")["value"]
            .as_str()
            .unwrap()
            .starts_with("offline: "),
        "{offline}"
    );
    node.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap();
}

// ----------------------------------------------------------- on a pair

/// A `paddock node` process with its config at `dir/node-<n>.toml`,
/// output in `dir/node-<n>.log`; killed when dropped.
struct Process {
    _child: tokio::process::Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Process {
    async fn start(dir: &Path, n: u32, toml: &str) -> Self {
        let socket = dir.join(format!("n{n}.sock"));
        let path = dir.join(format!("node-{n}.toml"));
        std::fs::write(&path, toml).unwrap();
        let log = dir.join(format!("node-{n}.log"));
        let out = std::fs::File::create(&log).unwrap();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(["node", "--config"])
            .arg(&path)
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        common::wait_for_socket(&mut child, &socket, &log).await;
        Self {
            _child: child,
            socket,
            log,
        }
    }

    async fn role(&self) -> String {
        paddock::api::Client::new(&self.socket)
            .failover()
            .await
            .map(|f| f.role)
            .unwrap_or_default()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

async fn until_role(p: &Process, role: &str) {
    for _ in 0..500 {
        if p.role().await == role {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{} never became {role}", p.socket.display());
}

/// Reads the device answers in `secs` seconds.
async fn reads_in(reads: &AtomicUsize, secs: u64) -> usize {
    let before = reads.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(secs)).await;
    reads.load(Ordering::Relaxed) - before
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scada____on_a_pair____primary_alone_polls_and_an_ack_survives_switchover() {
    use std::os::unix::fs::PermissionsExt;

    let wasm = build();
    let dir = TempDir::new().unwrap();
    let d = dir.path();
    let cli = d.join("cli.sock");
    let holding = Arc::new(Mutex::new((100..110).collect::<Vec<u16>>()));
    let reads = Arc::new(AtomicUsize::new(0));
    let (device, _device_task) = fake_device(FakeDevice {
        holding: holding.clone(),
        reads: reads.clone(),
    })
    .await;
    let keys = paddock(&cli, &["keygen", "--no-password", s(&d.join("k"))]).await;
    assert!(keys.ok, "{}", keys.stderr);
    let ex = root().join("examples/scada");
    let mut bundles = Vec::new();
    for app in ["points", "hmi", "control"] {
        let out = d.join(format!("{app}.rbp"));
        let wasm_file = wasm.join(format!("example_scada_{app}.wasm"));
        let r = paddock(
            &cli,
            &[
                "bundle",
                s(&ex.join(app).join("paddock.toml")),
                s(&wasm_file),
                "--key",
                s(&d.join("k.key")),
                "-o",
                s(&out),
            ],
        )
        .await;
        assert!(r.ok, "bundle {app}: {}", r.stderr);
        bundles.push(out);
    }
    std::fs::write(
        d.join("policy.toml"),
        "[apps.points]\ngrant = [\"wasi:logging\", \"wasi:keyvalue\"]\nbuckets = [\"points\"]\n\
         [apps.hmi]\ngrant = [\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\"]\n\
         buckets = [\"points\"]\n\
         [apps.control]\ngrant = [\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\", \
         \"wasi:config\", \"paddock-modbus:modbus\"]\nbuckets = [\"points\"]\n",
    )
    .unwrap();
    let mut peers = String::from("[peers]\n");
    for n in 1..=3 {
        let (private, public) = paddock_host::transport::quic::generate_key().unwrap();
        let key = d.join(format!("node-{n}.key"));
        std::fs::write(&key, private).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        peers.push_str(&format!("{n} = [\"{public}\"]\n"));
    }
    std::fs::write(d.join("peers.toml"), peers).unwrap();
    let addresses = [0, 1, 2].map(|_| {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string()
    });
    let ports = [free_port(), free_port()];
    let toml = |n: u32| {
        let i = usize::try_from(n - 1).unwrap();
        let mut t = format!(
            "node = {n}\ndata_dir = {:?}\nadmin_socket = {:?}\nmax_instances = 32\n\
             trusted_keys = [{:?}]\npolicy = {:?}\n",
            d.join(format!("data-{n}")),
            d.join(format!("n{n}.sock")),
            keys.stdout,
            d.join("policy.toml"),
        );
        if n != 3 {
            t += &format!(
                "[http.main]\naddress = \"127.0.0.1:{}\"\nstandby = \"forward\"\n\
                 [buckets.points]\ndurability = \"disk\"\nscope = \"cluster\"\n\
                 [extensions.\"paddock-modbus:modbus\".devices.pump-1]\naddress = \"{device}\"\n\
                 poll_ms = 100\ntimeout_ms = 500\ndeliver_to = \"points\"\nprimary_only = true\n\
                 holding = [{{ start = 0, count = 4 }}]\n",
                ports[i]
            );
        }
        let seeds = if n == 3 {
            String::new()
        } else {
            format!("seeds = [\"{}\"]\n", addresses[2])
        };
        t += &format!(
            "[cluster]\naddress = \"{}\"\n{seeds}key = {:?}\npeers = {:?}\n\
             heartbeat_interval_ms = 100\nnode_timeout_ms = 1000\n",
            addresses[i],
            d.join(format!("node-{n}.key")),
            d.join("peers.toml"),
        );
        if n == 3 {
            t += "role = \"witness\"\n[witness]\npair = [1, 2]\nfirst = 1\n";
        } else {
            t += &format!(
                "[failover]\npartner = {}\narbiter = \"witness\"\nwitness = 3\nreplication = true\n",
                3 - n
            );
        }
        t + "lease_ms = 1000\nmargin_ms = 200\n"
    };
    let _witness = Process::start(d, 3, &toml(3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let n1 = Process::start(d, 1, &toml(1)).await;
    let n2 = Process::start(d, 2, &toml(2)).await;
    for p in [&n1, &n2] {
        for b in &bundles {
            let r = paddock(&p.socket, &["deploy", s(b)]).await;
            assert!(r.ok, "deploy {}: {}", b.display(), r.stderr);
        }
    }
    until_role(&n1, "primary").await;
    until_role(&n2, "standby").await;
    let [p1, p2] = ports;

    // The standby's table, which it does not poll, comes from the primary.
    let first =
        |t: &Value| t["points"]["pump-1/holding/2"] == 102 && t["devices"]["pump-1"] == "online";
    until(p2, "/scada/points", first).await;
    holding.lock().unwrap()[3] = 7;
    until(p2, "/scada/points", |t| {
        t["points"]["pump-1/holding/3"] == 7
    })
    .await;
    // One poller: about ten reads a second at 100 ms, not twenty.
    let one_poller = reads_in(&reads, 2).await;
    let log2 = n2.log();
    // An alarm raised through the standby (forwarded to control on the
    // primary) and acknowledged there.
    let wrote = http(p2, "POST", "/scada/write", &write(1, 200))
        .await
        .unwrap();
    until(p1, "/scada/alarms", |a| {
        alarm(a, "pump-1-pressure")["status"] == "active"
    })
    .await;
    let acked = http(p2, "POST", "/scada/alarms/pump-1-pressure/ack", "")
        .await
        .unwrap();

    // A planned switchover: node 2 polls now, and control there starts
    // from the replicated alarms with the acknowledgement.
    paddock::api::Client::new(&n1.socket)
        .switchover()
        .await
        .unwrap();
    until_role(&n2, "primary").await;
    until_role(&n1, "standby").await;
    let after = until(p2, "/scada/alarms", |a| {
        !alarm(a, "pump-1-pressure").is_null()
    })
    .await;
    holding.lock().unwrap()[3] = 8;
    until(p1, "/scada/points", |t| {
        t["points"]["pump-1/holding/3"] == 8
    })
    .await;
    until(p2, "/scada/points", |t| {
        t["points"]["pump-1/holding/3"] == 8
    })
    .await;
    let still_one = reads_in(&reads, 2).await;

    assert!(
        log2.contains("modbus polling paused") && !log2.contains("modbus polling resumed"),
        "{log2}"
    );
    assert!(one_poller <= 26, "{one_poller} reads in 2 s");
    assert_eq!(wrote.0, 200, "{}", wrote.1);
    assert_eq!(acked.0, 200, "{}", acked.1);
    assert_eq!(
        alarm(&after, "pump-1-pressure")["status"],
        "acked",
        "{after}"
    );
    assert!(n2.log().contains("modbus polling resumed"), "{}", n2.log());
    assert!(n1.log().contains("modbus polling paused"), "{}", n1.log());
    assert!(still_one <= 26, "{still_one} reads in 2 s");
}
