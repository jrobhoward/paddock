//! The Phase 2a protocol path end to end (ROADMAP F6): a node polls a
//! Modbus TCP device through the built-in `paddock-modbus:modbus`
//! extension, routes its updates by device to the `points` pool, which
//! keeps them in a shared bucket, and the `hmi` connection handler answers
//! over a listener. The device is a fake served from this test.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use paddock::node::{LogRing, Node, NodeConfig, builtin_extensions};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
        .arg("--target-dir")
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok, "building the scada examples failed");
    target.join("wasm32-wasip2/release")
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

/// A Modbus device: ten holding registers, readable and writable.
#[derive(Clone)]
struct FakeDevice {
    holding: Arc<Mutex<Vec<u16>>>,
}

impl tokio_modbus::server::Service for FakeDevice {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = Ready<Result<Response, ExceptionCode>>;

    fn call(&self, request: Self::Request) -> Self::Future {
        let mut regs = self.holding.lock().unwrap();
        ready(match request {
            Request::ReadHoldingRegisters(a, n) => regs
                .get(usize::from(a)..usize::from(a) + usize::from(n))
                .map(|r| Response::ReadHoldingRegisters(r.to_vec()))
                .ok_or(ExceptionCode::IllegalDataAddress),
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

async fn fake_device(holding: Arc<Mutex<Vec<u16>>>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let device = FakeDevice { holding };
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

/// Send `line` to the hmi connection and read its reply.
async fn ask(
    read: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
) -> String {
    write
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    read.read_line(&mut reply).await.unwrap();
    reply.trim_end().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scada____modbus_device_to_pool_bucket_and_listener____polled_routed_answered_written() {
    let wasm = build();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("a.sock");
    let holding = Arc::new(Mutex::new((100..110).collect::<Vec<u16>>()));
    let (device, device_task) = fake_device(holding.clone()).await;
    let keys = paddock(
        &socket,
        &["keygen", "--no-password", s(&dir.path().join("k"))],
    )
    .await;
    assert!(keys.ok, "{}", keys.stderr);
    let key = dir.path().join("k.key");
    let ex = root().join("examples/scada");
    let mut bundles = Vec::new();
    for (name, wasm_file) in [
        ("points", "example_scada_points.wasm"),
        ("hmi", "example_scada_hmi.wasm"),
    ] {
        let out = dir.path().join(format!("{name}.rbp"));
        let manifest = ex.join(format!("{name}/paddock.toml"));
        let r = paddock(
            &socket,
            &[
                "bundle",
                s(&manifest),
                s(&wasm.join(wasm_file)),
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
        "[apps.points]\ngrant = [\"wasi:logging\", \"wasi:keyvalue\", \"paddock-modbus:modbus\"]\n\
         buckets = [\"points\"]\n",
    )
    .unwrap();
    let hmi_port = free_port();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {:?}\nmax_instances = 32\ntrusted_keys = [{:?}]\n\
         policy = {policy:?}\n\
         [listeners.hmi]\naddress = \"127.0.0.1:{hmi_port}\"\n\
         [buckets.points]\ndurability = \"memory\"\n\
         [extensions.\"paddock-modbus:modbus\".devices.pump-1]\naddress = \"{device}\"\n\
         poll_ms = 50\ntimeout_ms = 500\ndeliver_to = \"points\"\n\
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
    for b in &bundles {
        let r = paddock(&socket, &["deploy", s(b)]).await;
        assert!(r.ok, "deploy {}: {}", b.display(), r.stderr);
    }
    let (read, mut write) = tokio::net::TcpStream::connect(("127.0.0.1", hmi_port))
        .await
        .unwrap()
        .into_split();
    let mut read = BufReader::new(read);
    // Polled and stored: the first scan reaches the pool.
    let mut first = String::new();
    for _ in 0..200 {
        first = ask(&mut read, &mut write, "get pump-1 holding 2").await;
        if first == "102" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = ask(&mut read, &mut write, "status pump-1").await;
    // A change on the device arrives as a change.
    holding.lock().unwrap()[3] = 7;
    let mut changed = String::new();
    for _ in 0..200 {
        changed = ask(&mut read, &mut write, "get pump-1 holding 3").await;
        if changed == "7" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A write from the operator reaches the device.
    let wrote = ask(&mut read, &mut write, "set pump-1 holding 1 555").await;
    let unlisted = ask(&mut read, &mut write, "set pump-2 holding 1 1").await;
    let on_device = holding.lock().unwrap()[1];
    // The device goes away: the pool records it offline.
    device_task.abort();
    let _ = device_task.await;
    let mut offline = String::new();
    for _ in 0..200 {
        offline = ask(&mut read, &mut write, "status pump-1").await;
        if offline.starts_with("offline") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(first, "102");
    assert_eq!(status, "online");
    assert_eq!(changed, "7");
    assert_eq!(wrote, "ok");
    assert_eq!(on_device, 555);
    assert_eq!(unlisted, "err:access-denied");
    assert!(offline.starts_with("offline: "), "{offline}");
    node.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap();
}
