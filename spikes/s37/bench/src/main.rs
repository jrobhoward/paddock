//! S37's harness, in two roles:
//!
//! - `node RUNNER APP DRIVER POOL DRIVERS`: a runtime running the echo app
//!   (a pool of POOL) behind the node's own ingress, HTTP and HTTPS, and
//!   the driver (a pool of DRIVERS) on two listeners, HTTP and HTTPS,
//!   calling the same pool. Prints the four addresses and its pid, then
//!   runs until killed.
//! - `load ADDR CONNS SECS BODY TLS NODE_PID WORKERS`: CONNS keep-alive
//!   connections sending requests one at a time each (a GET, or a POST of
//!   BODY bytes that the app echoes) for SECS seconds after a second's
//!   warm-up. Prints requests a second, p50 and p99, and the CPU the node
//!   and its children (the instances) spent per request.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{AppInfo, AppManifest, AppSpec, NativeProgram, ProgramRef, Provenance};
use bulkhead_host::http::{HttpIngress, HttpRoute, IngressListen, IngressTls, TlsCertificate};
use bulkhead_host::{
    AppDeployment, DriverListener, DriverProfile, ListenerSpec, NativeCode, NativeConfig, Runtime,
    RuntimeConfig,
};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("node") => node(&args[1..]),
        Some("load") => load(&args[1..]),
        _ => {
            eprintln!("usage: s37-bench node RUNNER APP DRIVER POOL DRIVERS | load ADDR CONNS SECS BODY TLS NODE_PID WORKERS [PATH]");
            std::process::exit(2);
        }
    }
}

fn manifest(name: &str, spec: serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: name.into(),
            version: "1.0.0".parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        program: ProgramRef { path: String::new(), checksum: String::new() },
        provenance: Provenance { signer_key: String::new(), build_info: None, sbom: None },
        spec: AppSpec::from_json_value(&spec).unwrap(),
    }
}

fn node(args: &[String]) {
    let runner = PathBuf::from(&args[0]);
    let app = PathBuf::from(&args[1]);
    let driver = PathBuf::from(&args[2]);
    let pool: usize = args[3].parse().unwrap();
    let drivers: usize = args[4].parse().unwrap();
    let workers: usize = std::env::var("S37_WORKERS").ok().and_then(|w| w.parse().ok()).unwrap_or(4);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(serve(runner, app, driver, pool, drivers)).await })
        .unwrap();
}

async fn serve(runner: PathBuf, app: PathBuf, driver: PathBuf, pool: usize, drivers: usize) {
    let mut native = NativeConfig::default();
    native.runner = Some(runner);
    native.landlock_optional = cfg!(target_os = "freebsd");
    // `S37_SPIN_US` and `S37_SENDER_SPIN_US`: `[native] spin_us` and
    // `sender_spin_us` (DA1b).
    if let Some(us) = std::env::var("S37_SPIN_US").ok().and_then(|v| v.parse::<u64>().ok()) {
        native.spin = std::time::Duration::from_micros(us);
    }
    if let Some(us) = std::env::var("S37_SENDER_SPIN_US").ok().and_then(|v| v.parse::<u64>().ok()) {
        native.sender_spin = std::time::Duration::from_micros(us);
    }
    // `S37_DRIVER=da1`: DA1's driver, one process with `drivers` worker
    // threads, on listeners and certificate files node config would give
    // it (`[drivers.driver]`), calling `echo` by name.
    let da1 = std::env::var("S37_DRIVER").is_ok_and(|v| v == "da1");
    let certs = std::env::temp_dir().join(format!("s37-{}", std::process::id()));
    std::fs::create_dir_all(&certs).unwrap();
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    std::fs::write(certs.join("c.pem"), made.cert.pem()).unwrap();
    std::fs::write(certs.join("k.pem"), made.signing_key.serialize_pem()).unwrap();
    let da1_http = Arc::new(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let da1_https = Arc::new(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let mut drivers_config = std::collections::BTreeMap::new();
    if da1 {
        drivers_config.insert(
            "driver".to_string(),
            DriverProfile {
                listeners: vec![
                    DriverListener { name: "http".into(), socket: da1_http.clone() },
                    DriverListener { name: "https".into(), socket: da1_https.clone() },
                ],
                files: vec![certs.join("c.pem"), certs.join("k.pem")],
                calls: vec!["echo".into()],
                routes: false,
            },
        );
    }
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 256,
        native,
        drivers: drivers_config,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app_code = rt.load_native(NativeCode { path: app, kind: NativeProgram::Executable });
    let driver_code = rt.load_native(NativeCode { path: driver, kind: NativeProgram::Executable });
    let limits = json!({
        "message_max_bytes": "4MiB",
        "mailbox_max_bytes": "64MiB",
        "callback_deadline_ms": 10000,
        "memory_max": "256MiB"
    });
    let names: Vec<String> = (0..pool).map(|n| format!("echo-{n}")).collect();
    let echo = manifest(
        "echo",
        json!({
            "kind": "native-process",
            "native": { "program": "executable" },
            "instances": { "count": pool },
            "registered_names": names,
            "limits": limits,
            "capabilities": { "bulkhead:http-routes": {
                "routes": [{ "path_prefix": "/" }],
                "max_body": 4194304,
                "timeout_ms": 10000
            } }
        }),
    );
    let drv = if da1 {
        manifest(
            "driver",
            json!({
                "kind": "native-process",
                "native": { "program": "executable" },
                "instances": { "count": 1 },
                "lifecycle": { "start_args": {
                    "threads": drivers,
                    "spin_us": std::env::var("S37_DRIVER_SPIN_US").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                } },
                "limits": limits
            }),
        )
    } else {
        manifest(
            "driver",
            json!({
                "kind": "native-process",
                "native": { "program": "executable" },
                "instances": { "count": drivers },
                "limits": limits,
                "listeners": [
                    { "name": "http", "idle_timeout_ms": 600000 },
                    { "name": "https", "idle_timeout_ms": 600000 }
                ]
            }),
        )
    };
    let routes = HttpRoute::from_manifest(&echo);
    let apps = rt
        .start_apps(vec![AppDeployment::new(echo, app_code), AppDeployment::new(drv, driver_code)])
        .await
        .unwrap();
    let ingress = HttpIngress::new(apps.clone());
    ingress.set_routes("echo", routes).unwrap();
    let tls = IngressTls::load(vec![TlsCertificate {
        cert: certs.join("c.pem"),
        key: certs.join("k.pem"),
        hosts: vec!["localhost".into()],
    }])
    .unwrap();
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let big = |tls| IngressListen {
        max_connections: 4096,
        max_response: 8 << 20,
        tls,
        ..IngressListen::new(any)
    };
    let in_http = ingress.listen(big(None)).await.unwrap();
    let in_https = ingress.listen(big(Some(tls))).await.unwrap();
    if da1 {
        println!(
            "ready pid={} ingress-http={in_http} ingress-https={in_https} driver-http={} driver-https={}",
            std::process::id(),
            da1_http.local_addr().unwrap(),
            da1_https.local_addr().unwrap()
        );
        std::future::pending::<()>().await;
        drop((rt, apps, ingress));
        return;
    }
    let handle = apps.app("driver").unwrap();
    let listen = |name: &str| {
        let mut s = ListenerSpec::new(name, any);
        s.max_connections = 4096;
        s.idle_timeout = Duration::from_secs(600);
        rt.listen_native(s, handle.clone()).unwrap()
    };
    let d_http = listen("http");
    let d_https = listen("https");
    println!(
        "ready pid={} ingress-http={in_http} ingress-https={in_https} driver-http={} driver-https={}",
        std::process::id(),
        d_http.local_addr(),
        d_https.local_addr()
    );
    std::future::pending::<()>().await;
    drop((rt, apps, ingress, d_http, d_https));
}

/// User and system seconds of `pid` and its children.
fn cpu_seconds(pid: u32) -> f64 {
    let ticks = |p: &str| -> Option<(u32, f64)> {
        let stat = std::fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
        let f: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
        let t = f[11].parse::<f64>().ok()? + f[12].parse::<f64>().ok()?;
        Some((f[1].parse().ok()?, t / 100.0))
    };
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|p| {
            let (parent, t) = ticks(&p)?;
            (p == pid.to_string() || parent == pid).then_some(t)
        })
        .sum()
}

fn load(args: &[String]) {
    let addr: SocketAddr = args[0].parse().unwrap();
    let conns: usize = args[1].parse().unwrap();
    let secs: u64 = args[2].parse().unwrap();
    let body: usize = args[3].parse().unwrap();
    let tls = args[4] == "tls";
    let node_pid: u32 = args[5].parse().unwrap();
    let workers: usize = args[6].parse().unwrap();
    let path = args.get(7).cloned().unwrap_or_else(|| "/x".into());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_load(addr, conns, secs, body, tls, node_pid, path));
}

#[derive(Debug)]
struct AnyCert;

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn run_load(addr: SocketAddr, conns: usize, secs: u64, body: usize, tls: bool, node_pid: u32, path: String) {
    let connector = tls.then(|| {
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert))
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    });
    let request: Arc<Vec<u8>> = Arc::new(if body == 0 {
        format!("GET {path} HTTP/1.1\r\nhost: localhost\r\n\r\n").into_bytes()
    } else {
        let mut r = format!("POST {path} HTTP/1.1\r\nhost: localhost\r\ncontent-length: {body}\r\n\r\n").into_bytes();
        r.extend(std::iter::repeat_n(b'a', body));
        r
    });
    let start = Instant::now() + Duration::from_secs(1);
    let end = start + Duration::from_secs(secs);
    let mut tasks = Vec::new();
    for _ in 0..conns {
        let request = request.clone();
        let connector = connector.clone();
        tasks.push(tokio::spawn(async move {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            tcp.set_nodelay(true).unwrap();
            match connector {
                Some(c) => {
                    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                    let s = c.connect(name, tcp).await.unwrap();
                    drive(s, &request, start, end).await
                }
                None => drive(tcp, &request, start, end).await,
            }
        }));
    }
    tokio::time::sleep_until(start.into()).await;
    let cpu0 = cpu_seconds(node_pid);
    let mut lat: Vec<u64> = Vec::new();
    let mut errors = 0u64;
    for t in tasks {
        let (l, e) = t.await.unwrap();
        lat.extend(l);
        errors += e;
    }
    let cpu = cpu_seconds(node_pid) - cpu0;
    let elapsed = secs as f64;
    lat.sort_unstable();
    let pct = |q: f64| lat.get(((lat.len().saturating_sub(1)) as f64 * q) as usize).copied().unwrap_or(0);
    let n = lat.len() as f64;
    println!(
        "conns={conns} body={body} tls={tls}: {:.0} req/s, p50 {:.1} µs, p99 {:.1} µs, {:.1} µs CPU/req (node+instances), errors {errors}",
        n / elapsed,
        pct(0.5) as f64 / 1e3,
        pct(0.99) as f64 / 1e3,
        if n > 0.0 { cpu / n * 1e6 } else { 0.0 },
    );
}

/// Requests one at a time until `end`; latencies (ns) from `start` on.
async fn drive<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, request: &[u8], start: Instant, end: Instant) -> (Vec<u64>, u64) {
    let mut lat = Vec::new();
    let mut errors = 0;
    let mut buf = vec![0u8; 64 << 10];
    let mut have: Vec<u8> = Vec::new();
    loop {
        let t = Instant::now();
        if t >= end {
            break;
        }
        if s.write_all(request).await.is_err() {
            errors += 1;
            break;
        }
        // Read one response: headers, then content-length bytes.
        let (head, length) = loop {
            if let Some(i) = have.windows(4).position(|w| w == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&have[..i]).to_ascii_lowercase();
                let len = text
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                    .unwrap_or(0);
                if !text.starts_with("http/1.1 200") {
                    errors += 1;
                }
                break (i + 4, len);
            }
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return (lat, errors + 1),
                Ok(n) => have.extend_from_slice(&buf[..n]),
            }
        };
        while have.len() < head + length {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return (lat, errors + 1),
                Ok(n) => have.extend_from_slice(&buf[..n]),
            }
        }
        have.drain(..head + length);
        if t >= start {
            lat.push(t.elapsed().as_nanos() as u64);
        }
    }
    (lat, errors)
}
