//! What an outgoing HTTP request from an app costs, for `docs/BASELINE.md`:
//! a native client on a kept-alive connection against the
//! same request from a guest through `wasi:http` and the node's connection
//! pool, one request at a time and from 16 instances at once. Spike S15
//! measured the guest at about 172 µs per request when wasmtime's sender
//! opened a connection per request.
//!
//! The server is a local hyper server answering `hi`; the guest is
//! `paddock-fixture-sdk` (`fetch GET <url>`), driven by calls, so each guest
//! request also includes a call into the instance (about 3 µs).
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk
//! cargo run --release -p paddock-host --example egress_cost -- 4
//! ```
//!
//! Arguments: tokio worker threads (default 4), and `nopool` to keep no
//! idle connection (a new connection per request, as before the pool).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::{Request, Response};
use paddock_bundle::manifest::Capability;
use paddock_host::{Envelope, InstanceHandle, InstanceSpec, Runtime, RuntimeConfig};

const LONG: Duration = Duration::from_secs(60);
const SEQUENTIAL: usize = 2_000;
const CLIENTS: usize = 16;
const EACH: usize = 500;

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |w| w.parse().expect("worker threads"));
    let pooled = std::env::args().nth(2).as_deref() != Some("nopool");
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/paddock_fixture_sdk.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(component, workers, pooled)).await })
        .unwrap();
}

async fn server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(|_req: Request<hyper::body::Incoming>| async {
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"hi"))))
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

fn report(what: &str, mut times: Vec<Duration>, wall: Duration) {
    times.sort();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let mean = times.iter().sum::<Duration>() / u32::try_from(times.len()).unwrap();
    println!(
        "{what}: p50 {:.1} µs, p99 {:.1} µs, mean {:.1} µs; {:.0} requests/s",
        us(times[times.len() / 2]),
        us(times[times.len() * 99 / 100]),
        us(mean),
        times.len() as f64 / wall.as_secs_f64()
    );
}

async fn ask(h: &InstanceHandle, command: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", command), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

async fn run(component: Vec<u8>, workers: usize, pooled: bool) {
    let address = server().await;
    println!("tokio workers: {workers}, server {address}, egress pool: {pooled}");

    let native: hyper_util::client::legacy::Client<_, Empty<Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
    let uri: hyper::Uri = format!("http://{address}/").parse().unwrap();
    let mut times = Vec::with_capacity(SEQUENTIAL);
    let started = Instant::now();
    for _ in 0..SEQUENTIAL {
        let t = Instant::now();
        let r = native.get(uri.clone()).await.unwrap();
        let _ = r.into_body().collect().await.unwrap();
        times.push(t.elapsed());
    }
    report("native hyper, pooled, sequential", times, started.elapsed());

    let mut egress = paddock_host::Egress::default();
    if !pooled {
        egress.pool_max_idle_per_host = 0;
    }
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 64,
        egress,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let spawn = |name: &str| {
        let mut spec = InstanceSpec {
            callback_deadline: LONG,
            granted: [Capability::HttpOutgoing].into_iter().collect(),
            ..InstanceSpec::new(name, "1.0.0")
        };
        spec.services.egress.allow = vec![format!("http://{address}")];
        spec.services.egress.max_concurrent = Some(1_000);
        rt.spawn(&app, spec).unwrap()
    };
    let command = format!("fetch GET http://{address}/");
    let one = spawn("egress");
    assert_eq!(ask(&one, &command).await, "200 hi");
    let mut times = Vec::with_capacity(SEQUENTIAL);
    let started = Instant::now();
    for _ in 0..SEQUENTIAL {
        let t = Instant::now();
        ask(&one, &command).await;
        times.push(t.elapsed());
    }
    report("guest wasi:http, sequential", times, started.elapsed());

    let many: Vec<InstanceHandle> = (0..CLIENTS).map(|_| spawn("egress")).collect();
    let started = Instant::now();
    let tasks: Vec<_> = many
        .iter()
        .map(|h| {
            let (h, command) = (h.clone(), command.clone());
            tokio::spawn(async move {
                let mut times = Vec::with_capacity(EACH);
                for _ in 0..EACH {
                    let t = Instant::now();
                    ask(&h, &command).await;
                    times.push(t.elapsed());
                }
                times
            })
        })
        .collect();
    let mut times = Vec::new();
    for t in tasks {
        times.extend(t.await.unwrap());
    }
    report(
        &format!("guest wasi:http, {CLIENTS} instances at once"),
        times,
        started.elapsed(),
    );
}
