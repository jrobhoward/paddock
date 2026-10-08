//! Spike S15: HTTP in the guest versus in the host. See ../README.md.
//!
//! Usage: s15-bench [server-workers]   (from spikes/, after building the
//! guests; paths are fixed below)

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use paddock_host::{Envelope, InstanceHandle, InstanceSpec};
use tokio::net::{TcpListener, TcpStream};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, Store};
use wasmtime_wasi::p2::bindings::Command;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

const WASM: &str = "target/wasm32-wasip2/release";
const CHAOS: &str = "../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm";
const SEQUENTIAL: usize = 10_000;
const CONNECTIONS: usize = 32;
const LOAD_TIME: Duration = Duration::from_secs(2);

// --- wasmtime state for the wasi:http and wasi:sockets guests -------------

struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl State {
    fn new(wasi: WasiCtx) -> Self {
        Self {
            wasi,
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        }
    }
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

fn engine() -> Engine {
    let mut config = Config::new();
    config.allocation_strategy(InstanceAllocationStrategy::pooling());
    Engine::new(&config).unwrap()
}

fn linker(engine: &Engine) -> Linker<State> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker).unwrap();
    linker
}

// --- servers --------------------------------------------------------------

type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, anyhow::Error>;

fn ok(bytes: impl Into<Bytes>) -> Response<Body> {
    Response::new(
        Full::new(bytes.into())
            .map_err(|never| match never {})
            .boxed_unsync(),
    )
}

/// Serve every connection on `listener` with `handle`.
async fn serve<F, Fut>(listener: TcpListener, handle: F)
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Response<Body>>> + Send + 'static,
{
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        stream.set_nodelay(true).unwrap();
        let handle = handle.clone();
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service_fn(handle))
                .await;
        });
    }
}

async fn bind() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

/// A native hyper server answering `ok`.
async fn native_server() -> SocketAddr {
    let (l, addr) = bind().await;
    tokio::spawn(serve(l, |_req| async { Ok(ok("ok")) }));
    addr
}

/// Host hyper; each request is a call into a Paddock instance (round-robin).
async fn paddock_server(instances: usize) -> SocketAddr {
    let rt = paddock_host::Runtime::new(paddock_host::RuntimeConfig {
        max_instances: 64,
        ..Default::default()
    })
    .unwrap();
    let app = rt.load(&std::fs::read(CHAOS).expect(CHAOS)).unwrap();
    let pool: Arc<Vec<InstanceHandle>> = Arc::new(
        (0..instances)
            .map(|_| rt.spawn(&app, InstanceSpec::new("chaos", "0.1.0")).unwrap())
            .collect(),
    );
    let next = Arc::new(AtomicUsize::new(0));
    let (l, addr) = bind().await;
    tokio::spawn(serve(l, move |_req| {
        let (pool, next, _rt) = (pool.clone(), next.clone(), rt.clone());
        async move {
            let i = next.fetch_add(1, Ordering::Relaxed) % pool.len();
            let reply = pool[i]
                .call(
                    Envelope::new("text/plain", "echo:ok"),
                    Duration::from_secs(5),
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(ok(reply.body))
        }
    }));
    addr
}

/// `wasi:http/incoming-handler`, as `wasmtime serve` runs it: a new
/// instance per request from a pre-instantiated component.
async fn wasi_http_server(engine: &Engine) -> SocketAddr {
    let component = Component::from_file(engine, format!("{WASM}/s15_proxy.wasm")).unwrap();
    let pre = ProxyPre::new(linker(engine).instantiate_pre(&component).unwrap()).unwrap();
    let (l, addr) = bind().await;
    tokio::spawn(serve(l, move |req| {
        let pre = pre.clone();
        async move {
            let mut store = Store::new(pre.engine(), State::new(WasiCtx::builder().build()));
            let (tx, rx) = tokio::sync::oneshot::channel();
            let wt = |e: wasmtime::Error| anyhow::anyhow!("{e:?}");
            let req = store
                .data_mut()
                .http()
                .new_incoming_request(Scheme::Http, req)
                .map_err(wt)?;
            let out = store.data_mut().http().new_response_outparam(tx).map_err(wt)?;
            tokio::spawn(async move {
                let proxy = pre.instantiate_async(&mut store).await?;
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(store, req, out)
                    .await
            });
            let resp: Response<HyperOutgoingBody> =
                rx.await?.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            Ok(resp.map(|b| b.map_err(|e| anyhow::anyhow!("{e:?}")).boxed_unsync()))
        }
    }));
    addr
}

/// Run a command component with network access; returns its stdout.
async fn run_command(engine: &Engine, wasm: &str, args: &[&str]) -> String {
    let component = Component::from_file(engine, format!("{WASM}/{wasm}")).unwrap();
    let stdout = MemoryOutputPipe::new(1 << 16);
    let mut wasi = WasiCtx::builder();
    wasi.args(args)
        .stdout(stdout.clone())
        .inherit_stderr()
        .inherit_network()
        .allow_tcp(true)
        .allow_ip_name_lookup(true);
    let mut store = Store::new(engine, State::new(wasi.build()));
    let command = Command::instantiate_async(&mut store, &component, &linker(engine))
        .await
        .unwrap();
    let _ = command.wasi_cli_run().call_run(&mut store).await.unwrap();
    String::from_utf8(stdout.contents().to_vec()).unwrap()
}

/// The guest-owned socket server, listening on `addr`.
async fn guest_socket_server(engine: &Engine) -> SocketAddr {
    let addr = {
        let (l, a) = bind().await;
        drop(l);
        a
    };
    let engine = engine.clone();
    let text = addr.to_string();
    tokio::spawn(async move {
        run_command(&engine, "s15-tcpserver.wasm", &["s15-tcpserver", &text]).await
    });
    for _ in 0..200 {
        if TcpStream::connect(addr).await.is_ok() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("guest server did not start");
}

// --- load ----------------------------------------------------------------

async fn connect(addr: SocketAddr) -> hyper::client::conn::http1::SendRequest<Empty<Bytes>> {
    let stream = TcpStream::connect(addr).await.unwrap();
    stream.set_nodelay(true).unwrap();
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    sender
}

async fn get(sender: &mut hyper::client::conn::http1::SendRequest<Empty<Bytes>>, addr: SocketAddr) {
    sender.ready().await.unwrap();
    let req = Request::get(format!("http://{addr}/"))
        .body(Empty::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[derive(Default)]
struct Stats {
    samples: Vec<Duration>,
    elapsed: Duration,
}

impl Stats {
    fn line(mut self) -> String {
        self.samples.sort();
        let at = |q: f64| {
            let i = ((self.samples.len() - 1) as f64 * q).round() as usize;
            self.samples[i].as_secs_f64() * 1e6
        };
        let rate = self.samples.len() as f64 / self.elapsed.as_secs_f64();
        format!("{:.1} µs | {:.1} µs | {rate:.0}", at(0.5), at(0.99))
    }
}

/// One keep-alive connection, sequential requests.
async fn sequential(addr: SocketAddr) -> Stats {
    let mut sender = connect(addr).await;
    for _ in 0..200 {
        get(&mut sender, addr).await;
    }
    let mut s = Stats::default();
    let started = Instant::now();
    for _ in 0..SEQUENTIAL {
        let t = Instant::now();
        get(&mut sender, addr).await;
        s.samples.push(t.elapsed());
    }
    s.elapsed = started.elapsed();
    s
}

/// Many keep-alive connections for a fixed time.
async fn concurrent(addr: SocketAddr) -> Stats {
    let deadline = Instant::now() + LOAD_TIME;
    let started = Instant::now();
    let tasks: Vec<_> = (0..CONNECTIONS)
        .map(|_| {
            tokio::spawn(async move {
                let mut sender = connect(addr).await;
                let mut samples = Vec::new();
                while Instant::now() < deadline {
                    let t = Instant::now();
                    get(&mut sender, addr).await;
                    samples.push(t.elapsed());
                }
                samples
            })
        })
        .collect();
    let mut s = Stats::default();
    for t in tasks {
        s.samples.extend(t.await.unwrap());
    }
    s.elapsed = started.elapsed();
    s
}

/// Run the load generator on its own runtime and threads.
fn load<F, Fut>(f: F) -> Stats
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Stats>,
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

// --- main ----------------------------------------------------------------

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().unwrap());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(workers)).await })
        .unwrap();
}

async fn run(workers: usize) {
    let engine = engine();
    println!("server tokio workers: {workers}; load generator: 4 workers, separate threads");

    println!("\n## Ingress (a native client calls a server)\n");
    println!("| Server | 1 connection: p50 | p99 | req/s | {CONNECTIONS} connections: p50 | p99 | req/s |");
    println!("|---|---|---|---|---|---|---|");
    let servers = [
        ("native hyper", native_server().await),
        ("host hyper -> Paddock instance call", paddock_server(workers).await),
        ("wasi:http incoming-handler (instance per request)", wasi_http_server(&engine).await),
        ("guest-owned socket (wasi:sockets, guest parses HTTP)", guest_socket_server(&engine).await),
    ];
    for (label, addr) in servers {
        let one = tokio::task::spawn_blocking(move || load(move || sequential(addr)))
            .await
            .unwrap();
        let many = tokio::task::spawn_blocking(move || load(move || concurrent(addr)))
            .await
            .unwrap();
        println!("| {label} | {} | {} |", one.line(), many.line());
    }

    println!("\n## Egress (a client calls a native hyper server)\n");
    let target = native_server().await;
    let n_conn = 2_000u32;
    let native_keepalive = tokio::task::spawn_blocking(move || load(move || sequential(target)))
        .await
        .unwrap();
    let native_new = tokio::task::spawn_blocking(move || {
        load(move || async move {
            let mut s = Stats::default();
            let started = Instant::now();
            for _ in 0..n_conn {
                let t = Instant::now();
                let mut sender = connect(target).await;
                get(&mut sender, target).await;
                s.samples.push(t.elapsed());
            }
            s.elapsed = started.elapsed();
            s
        })
    })
    .await
    .unwrap();
    let per = |out: String, n: u32| out.trim().parse::<f64>().unwrap() / f64::from(n) / 1e3;
    let t = target.to_string();
    let guest_http = per(
        run_command(&engine, "s15-client.wasm", &["s15-client", "http", &t, &n_conn.to_string()]).await,
        n_conn,
    );
    let n_tcp = SEQUENTIAL as u32;
    let guest_tcp = per(
        run_command(&engine, "s15-client.wasm", &["s15-client", "tcp", &t, &n_tcp.to_string()]).await,
        n_tcp,
    );
    println!("| Client | per request (p50 \\| p99 \\| req/s, or mean) |");
    println!("|---|---|");
    println!("| native hyper, one keep-alive connection | {} |", native_keepalive.line());
    println!("| native hyper, new connection per request | {} |", native_new.line());
    println!("| guest, wasi:http outgoing-handler (wstd Client) | mean {guest_http:.1} µs |");
    println!("| guest, wasi:sockets, one keep-alive connection, guest parses HTTP | mean {guest_tcp:.1} µs |");
}

#[allow(dead_code)]
fn _infallible(_: Infallible) {}
