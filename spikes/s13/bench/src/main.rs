//! Spike S13: host-owned listeners handing connections to guests as
//! `wasi:io` streams. See ../README.md.
//!
//! Usage (from spikes/): s13-bench [server-workers]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, Store};
use wasmtime_wasi::p2::bindings::io::streams::{InputStream, OutputStream};
use wasmtime_wasi::p2::pipe::{AsyncReadStream, AsyncWriteStream};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit",
        world: "s13:conn/conn",
        exports: { default: async },
        with: { "wasi:io": wasmtime_wasi::p2::bindings::io },
    });
}

use bindings::ConnPre;

const V1: &str = "target/wasm32-wasip2/release/s13_guest.wasm";
const V2: &str = "target/s13v2/wasm32-wasip2/release/s13_guest.wasm";
const LOAD_TIME: Duration = Duration::from_secs(2);
const ACCEPTORS: usize = 4;

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

// --- the host-owned listener ----------------------------------------------

/// Hands each accepted connection to a fresh instance of the current
/// component. `pre` can be swapped at any time (an upgrade); the listener
/// does not notice.
struct StreamServer {
    pre: RwLock<ConnPre<State>>,
    traps: AtomicU64,
}

impl StreamServer {
    async fn serve(self: Arc<Self>, stream: TcpStream) {
        let (read, write) = stream.into_split();
        let pre = self.pre.read().unwrap().clone();
        let mut store = Store::new(
            pre.engine(),
            State {
                wasi: WasiCtx::builder().build(),
                table: ResourceTable::new(),
            },
        );
        let table = &mut store.data_mut().table;
        let input: wasmtime::component::Resource<InputStream> = table
            .push(Box::new(AsyncReadStream::new(read)) as InputStream)
            .unwrap();
        let output: wasmtime::component::Resource<OutputStream> = table
            .push(Box::new(AsyncWriteStream::new(1 << 16, write)) as OutputStream)
            .unwrap();
        let result = async {
            let conn = pre.instantiate_async(&mut store).await?;
            conn.s13_conn_handler()
                .call_serve(&mut store, input, output)
                .await
        }
        .await;
        if result.is_err() {
            self.traps.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Bind once, accept on several tasks sharing the listener.
async fn stream_server(server: Arc<StreamServer>) -> SocketAddr {
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let addr = listener.local_addr().unwrap();
    for _ in 0..ACCEPTORS {
        let (listener, server) = (listener.clone(), server.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                stream.set_nodelay(true).unwrap();
                tokio::spawn(server.clone().serve(stream));
            }
        });
    }
    addr
}

fn pre(engine: &Engine, path: &str) -> ConnPre<State> {
    let component = Component::from_file(engine, path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
    ConnPre::new(linker.instantiate_pre(&component).unwrap()).unwrap()
}

/// Native hyper answering `ok`, for comparison.
async fn native_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            stream.set_nodelay(true).unwrap();
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(|_req| async {
                            Ok::<_, std::convert::Infallible>(Response::new(Full::new(
                                Bytes::from_static(b"ok"),
                            )))
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

// --- load ----------------------------------------------------------------

type Sender = hyper::client::conn::http1::SendRequest<Empty<Bytes>>;

async fn connect(addr: SocketAddr) -> Option<Sender> {
    let stream = TcpStream::connect(addr).await.ok()?;
    stream.set_nodelay(true).ok()?;
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    tokio::spawn(conn);
    Some(sender)
}

/// One GET; the body, or `None` if the request failed.
async fn get(sender: &mut Sender, addr: SocketAddr, path: &str) -> Option<Bytes> {
    sender.ready().await.ok()?;
    let req = Request::get(format!("http://{addr}{path}"))
        .body(Empty::new())
        .unwrap();
    let resp = sender.send_request(req).await.ok()?;
    Some(resp.into_body().collect().await.ok()?.to_bytes())
}

fn line(mut samples: Vec<Duration>, elapsed: Duration) -> String {
    samples.sort();
    let at = |q: f64| {
        let i = ((samples.len() - 1) as f64 * q).round() as usize;
        samples[i].as_secs_f64() * 1e6
    };
    let rate = samples.len() as f64 / elapsed.as_secs_f64();
    format!("{:.1} µs | {:.1} µs | {rate:.0}", at(0.5), at(0.99))
}

async fn concurrent(addr: SocketAddr, connections: usize) -> String {
    let deadline = Instant::now() + LOAD_TIME;
    let started = Instant::now();
    let tasks: Vec<_> = (0..connections)
        .map(|_| {
            tokio::spawn(async move {
                let mut sender = connect(addr).await.unwrap();
                let mut samples = Vec::new();
                while Instant::now() < deadline {
                    let t = Instant::now();
                    get(&mut sender, addr, "/").await.unwrap();
                    samples.push(t.elapsed());
                }
                samples
            })
        })
        .collect();
    let mut samples = Vec::new();
    for t in tasks {
        samples.extend(t.await.unwrap());
    }
    line(samples, started.elapsed())
}

/// A new connection per request, sequentially.
async fn per_connection(addr: SocketAddr, n: usize) -> String {
    let mut samples = Vec::with_capacity(n);
    let started = Instant::now();
    for _ in 0..n {
        let t = Instant::now();
        let mut sender = connect(addr).await.unwrap();
        get(&mut sender, addr, "/").await.unwrap();
        samples.push(t.elapsed());
    }
    line(samples, started.elapsed())
}

/// Run `f` on a separate runtime and threads, as a load generator.
fn load<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap()
            .block_on(f())
    })
    .join()
    .unwrap()
}

async fn in_load<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T>,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || load(f)).await.unwrap()
}

// --- main ----------------------------------------------------------------

fn main() {
    let workers: usize = std::env::args().nth(1).map_or(4, |a| a.parse().unwrap());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(workers)).await })
        .unwrap();
}

async fn run(workers: usize) {
    let mut config = Config::new();
    config.allocation_strategy(InstanceAllocationStrategy::pooling());
    let engine = Engine::new(&config).unwrap();
    let server = Arc::new(StreamServer {
        pre: RwLock::new(pre(&engine, V1)),
        traps: AtomicU64::new(0),
    });
    let guest = stream_server(server.clone()).await;
    let native = native_server().await;
    println!("server workers: {workers}; {ACCEPTORS} accept tasks; an instance per connection");

    println!("\n## Throughput against connections (keep-alive)\n");
    println!("| Server | connections | p50 | p99 | req/s |");
    println!("|---|---|---|---|---|");
    for (label, addr) in [("native hyper", native), ("host listener -> guest streams", guest)] {
        for n in [1usize, 4, 8, 16, 32, 64] {
            let r = in_load(move || concurrent(addr, n)).await;
            println!("| {label} | {n} | {r} |");
        }
    }

    println!("\n## A new connection per request (includes instantiating)\n");
    println!("| Server | p50 | p99 | req/s |");
    println!("|---|---|---|---|");
    for (label, addr) in [("native hyper", native), ("host listener -> guest streams", guest)] {
        let r = in_load(move || per_connection(addr, 2_000)).await;
        println!("| {label} | {r} |");
    }

    println!("\n## Guest crashes do not affect the listener\n");
    let before = server.traps.load(Ordering::Relaxed);
    let (ok, failed) = in_load(move || async move {
        let (mut ok, mut failed) = (0, 0);
        for i in 0..1_000 {
            let path = if i % 100 == 99 { "/crash" } else { "/" };
            let got = match connect(guest).await {
                Some(mut s) => get(&mut s, guest, path).await,
                None => None,
            };
            if got.is_some() { ok += 1 } else { failed += 1 }
        }
        (ok, failed)
    })
    .await;
    let traps = server.traps.load(Ordering::Relaxed) - before;
    println!("1,000 requests on new connections, every 100th asks the guest to trap:");
    println!("answered {ok}, failed {failed}, guest traps {traps}");

    println!("\n## Upgrade under load\n");
    let swap = {
        let (server, engine) = (server.clone(), engine.clone());
        tokio::spawn(async move {
            tokio::time::sleep(LOAD_TIME / 2).await;
            let v2 = tokio::task::spawn_blocking(move || pre(&engine, V2)).await.unwrap();
            let t = Instant::now();
            *server.pre.write().unwrap() = v2;
            t.elapsed()
        })
    };
    let (v1, v2, failed) = in_load(move || async move {
        let deadline = Instant::now() + LOAD_TIME;
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                tokio::spawn(async move {
                    let (mut v1, mut v2, mut failed) = (0u64, 0u64, 0u64);
                    while Instant::now() < deadline {
                        let got = match connect(guest).await {
                            Some(mut s) => get(&mut s, guest, "/").await,
                            None => None,
                        };
                        match got.as_deref() {
                            Some(b"ok") => v1 += 1,
                            Some(b"ok2") => v2 += 1,
                            _ => failed += 1,
                        }
                    }
                    (v1, v2, failed)
                })
            })
            .collect();
        let mut totals = (0, 0, 0);
        for t in tasks {
            let (a, b, c) = t.await.unwrap();
            totals = (totals.0 + a, totals.1 + b, totals.2 + c);
        }
        totals
    })
    .await;
    let swapped_in = swap.await.unwrap();
    println!(
        "8 clients, a new connection per request, for {} s; v2 swapped in halfway (swap held the lock {:.1} µs):",
        LOAD_TIME.as_secs(),
        swapped_in.as_secs_f64() * 1e6
    );
    println!("answered by v1 {v1}, by v2 {v2}, failed {failed}");
}
