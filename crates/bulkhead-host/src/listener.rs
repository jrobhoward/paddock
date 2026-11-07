//! Host-owned listeners and connection handlers (`bulkhead:connection`).
//!
//! A [`Listener`] binds one address and accepts on several tasks (the Ranch
//! pattern). Each accepted connection gets a new instance of a
//! connection-handler component, which reads and writes the socket through
//! `wasi:io` streams (spike S13: native throughput with an instance per
//! connection). The instance has a pid and the instance's limits and host
//! services, but no mailbox.
//!
//! - **Idle timeout:** a connection with no bytes in either direction for
//!   the listener's `idle_timeout` is closed and its instance dropped. This
//!   also ends a handler spinning without I/O; there is no per-callback
//!   deadline, because `serve` lasts as long as the connection.
//! - **Crashes** end their connection only. Nothing restarts.
//! - **Upgrades** ([`Listener::replace`]) apply to new connections; open
//!   connections keep the component they started with until they close.
//! - **Limits:** at most `max_connections` at once; further connections are
//!   closed at once and counted as refused. Each connection uses an
//!   instance slot of the runtime's pool.
//!
//! A native app's listener ([`Runtime::listen_native`], ROADMAP Z8g)
//! hands each connection to an instance of the app's pool instead (by the
//! peer's address under hash routing, otherwise in turn), as an operation
//! ahead of its mailbox: the node passes the stream on the instance's
//! descriptor socket and runs its `handle_connection`, and the instance
//! serves many connections, reading each when the node says it is
//! readable. The listener keeps its own copy of each socket: it counts the
//! connection until the app closes it or the instance exits, shuts it
//! down when the listener stops, and closes it once the kernel's
//! `TCP_INFO` shows no data either way for the idle timeout. A crash
//! closes every connection its instance held; `crashed` counts component
//! handlers only (a native instance's crash is its app's restart). The
//! app's upgrades are its own; the listener sends new connections to
//! whichever instances run.
//!
//! [`Runtime::listen_native`]: crate::Runtime::listen_native

use std::net::SocketAddr;
use std::pin::Pin;
#[cfg(all(feature = "http", feature = "egress"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use wasmtime::component::Resource;
use wasmtime_wasi::p2::bindings::io::streams::{InputStream, OutputStream};
use wasmtime_wasi::p2::pipe::{AsyncReadStream, AsyncWriteStream};

use crate::apps::AppHandle;
use crate::delivery;
use crate::error::HostError;
use crate::instance::{self, classify, new_store};
use crate::message::ExitReason;
use crate::pid::Pid;
use crate::probe;
use crate::runtime::RuntimeInner;
use crate::spec::InstanceSpec;
use crate::state::InstanceState;

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    wasmtime::component::bindgen!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-io",
            "../../wit/connection",
        ],
        world: "bulkhead:connection/connection-handler",
        exports: { default: async },
        with: {
            "wasi:io": wasmtime_wasi::p2::bindings::io,
            "bulkhead:runtime": crate::bindings::bulkhead::runtime,
        },
    });
}

use bindings::ConnectionHandlerPre;
use bindings::exports::bulkhead::connection::handler::ConnectionInfo;

/// The world a connection-handler component implements.
pub const CONNECTION_WORLD: &str = "bulkhead:connection/connection-handler@0.1.0";

/// Bytes the host buffers for a connection's output stream.
const WRITE_BUDGET: usize = 64 * 1024;

/// A compiled connection-handler component, linked against the host.
#[derive(Clone)]
pub struct LoadedConnection {
    pub(crate) pre: ConnectionHandlerPre<InstanceState>,
    pub(crate) stubbed: Arc<Vec<String>>,
}

impl LoadedConnection {
    /// Imported interfaces no host here provides, linked to deny stubs.
    #[must_use]
    pub fn stubbed_imports(&self) -> &[String] {
        &self.stubbed
    }
}

impl std::fmt::Debug for LoadedConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedConnection")
            .field("stubbed", &self.stubbed)
            .finish_non_exhaustive()
    }
}

impl LoadedConnection {
    pub(crate) fn new(
        pre: wasmtime::component::InstancePre<InstanceState>,
        stubbed: Vec<String>,
    ) -> wasmtime::Result<Self> {
        Ok(Self {
            pre: ConnectionHandlerPre::new(pre)?,
            stubbed: Arc::new(stubbed),
        })
    }
}

/// One listener's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListenerSpec {
    /// The listener's name in the app's manifest; handlers see it.
    pub name: String,
    /// Where to listen. Port 0 picks a free port ([`Listener::local_addr`]).
    pub address: SocketAddr,
    /// Tasks accepting connections at once.
    pub acceptors: usize,
    /// Most connections open at once; more are closed and counted.
    pub max_connections: usize,
    /// A connection with no bytes in either direction for this long is
    /// closed.
    pub idle_timeout: Duration,
}

impl ListenerSpec {
    /// A listener with defaults: 4 accept tasks, 1,024 connections, a 60 s
    /// idle timeout.
    #[must_use]
    pub fn new(name: impl Into<String>, address: SocketAddr) -> Self {
        Self {
            name: name.into(),
            address,
            acceptors: 4,
            max_connections: 1024,
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// A listener's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ListenerStats {
    /// Connections open now.
    pub active: usize,
    /// Connections accepted and handed to an instance, in total.
    pub accepted: u64,
    /// Connections closed at once because `max_connections` were open.
    pub refused: u64,
    /// Connections whose handler trapped, returned an error or could not
    /// start.
    pub crashed: u64,
    /// Connections closed by the idle timeout.
    pub idle_closed: u64,
}

/// Where a listener's connections go.
enum Target {
    /// A new connection-handler instance each, of this component.
    Component(Box<RwLock<(LoadedConnection, InstanceSpec)>>),
    /// An instance of this native app's pool.
    Native(AppHandle),
}

/// A connection accepted for a native app's instance
/// ([`InstanceHandle::connection`](crate::InstanceHandle)).
pub(crate) struct Accepted {
    /// The listener's name.
    pub(crate) listener: String,
    pub(crate) stream: std::net::TcpStream,
    /// Held while the app holds the stream.
    pub(crate) tie: Tie,
}

impl std::fmt::Debug for Accepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accepted")
            .field("listener", &self.listener)
            .finish_non_exhaustive()
    }
}

/// Ties a connection passed to a native instance to its listener: dropped
/// when the app closes the stream or the instance exits, which ends the
/// listener's count of it.
pub(crate) struct Tie(#[allow(dead_code)] tokio::sync::oneshot::Sender<()>);

struct Shared {
    spec: ListenerSpec,
    local_addr: SocketAddr,
    target: Target,
    slots: Arc<Semaphore>,
    accepted: AtomicU64,
    refused: AtomicU64,
    crashed: AtomicU64,
    idle_closed: AtomicU64,
    /// Becomes `true` when the listener stops; closes every connection.
    stop: watch::Sender<bool>,
}

/// A running listener. Cloning shares it; [`Listener::stop`] ends it.
#[derive(Clone)]
pub struct Listener {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("name", &self.shared.spec.name)
            .field("local_addr", &self.shared.local_addr)
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// The address it listens on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.local_addr
    }

    /// Its spec.
    #[must_use]
    pub fn spec(&self) -> &ListenerSpec {
        &self.shared.spec
    }

    /// Its counters.
    #[must_use]
    pub fn stats(&self) -> ListenerStats {
        let s = &self.shared;
        ListenerStats {
            active: s.spec.max_connections - s.slots.available_permits(),
            accepted: s.accepted.load(Ordering::Relaxed),
            refused: s.refused.load(Ordering::Relaxed),
            crashed: s.crashed.load(Ordering::Relaxed),
            idle_closed: s.idle_closed.load(Ordering::Relaxed),
        }
    }

    /// Serve new connections with `connection` and `spec` (an upgrade or a
    /// rollback). Open connections keep what they started with. A native
    /// app's listener ignores it: the app's instances are upgraded instead.
    pub fn replace(&self, connection: LoadedConnection, spec: InstanceSpec) {
        if let Target::Component(current) = &self.shared.target {
            *current.write().unwrap_or_else(PoisonError::into_inner) = (connection, spec);
        }
    }

    /// Stop accepting and close every open connection.
    pub fn stop(&self) {
        self.shared.stop.send_replace(true);
    }
}

/// Bind `spec.address` and start accepting, an instance of `connection`
/// per connection.
pub(crate) fn listen(
    rt: &Arc<RuntimeInner>,
    spec: ListenerSpec,
    connection: LoadedConnection,
    instance: InstanceSpec,
) -> Result<Listener, HostError> {
    bind(
        rt,
        spec,
        Target::Component(Box::new(RwLock::new((connection, instance)))),
    )
}

/// Bind `spec.address` and start accepting for `app`'s instances.
pub(crate) fn listen_native(
    rt: &Arc<RuntimeInner>,
    spec: ListenerSpec,
    app: AppHandle,
) -> Result<Listener, HostError> {
    if !cfg!(target_os = "linux") {
        return Err(HostError::Listen(format!(
            "{}: native apps run on Linux only",
            spec.name
        )));
    }
    bind(rt, spec, Target::Native(app))
}

fn bind(rt: &Arc<RuntimeInner>, spec: ListenerSpec, target: Target) -> Result<Listener, HostError> {
    let bind =
        |e: std::io::Error| HostError::Listen(format!("{} ({}): {e}", spec.name, spec.address));
    let std_listener = std::net::TcpListener::bind(spec.address).map_err(bind)?;
    std_listener.set_nonblocking(true).map_err(bind)?;
    let local_addr = std_listener.local_addr().map_err(bind)?;
    let listener = Arc::new(TcpListener::from_std(std_listener).map_err(bind)?);
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        slots: Arc::new(Semaphore::new(spec.max_connections)),
        local_addr,
        target,
        accepted: AtomicU64::new(0),
        refused: AtomicU64::new(0),
        crashed: AtomicU64::new(0),
        idle_closed: AtomicU64::new(0),
        stop,
        spec,
    });
    for _ in 0..shared.spec.acceptors.max(1) {
        tokio::spawn(accept(rt.clone(), shared.clone(), listener.clone()));
    }
    Ok(Listener { shared })
}

async fn accept(rt: Arc<RuntimeInner>, shared: Arc<Shared>, listener: Arc<TcpListener>) {
    let mut stop = shared.stop.subscribe();
    loop {
        let accepted = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, peer)) = accepted else {
            // Out of file descriptors and the like: back off briefly.
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let Ok(permit) = shared.slots.clone().try_acquire_owned() else {
            shared.refused.fetch_add(1, Ordering::Relaxed);
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        if let Target::Native(app) = &shared.target {
            hand_over(&shared, app, stream, peer, permit);
            continue;
        }
        shared.accepted.fetch_add(1, Ordering::Relaxed);
        let (rt, shared) = (rt.clone(), shared.clone());
        tokio::spawn(async move {
            serve(&rt, &shared, stream, peer).await;
            drop(permit);
        });
    }
}

/// Why a connection ended.
enum End {
    Done(ExitReason),
    Idle,
    Stopped,
}

/// Pass a connection to an instance of the native app, and watch it until
/// the app closes it, the instance exits, the listener stops or it idles.
/// No instance running refuses it.
fn hand_over(
    shared: &Arc<Shared>,
    app: &AppHandle,
    stream: TcpStream,
    peer: SocketAddr,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let Some(instance) = app.route_peer(peer.ip()) else {
        shared.refused.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Ok(stream) = stream.into_std() else {
        shared.refused.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Ok(copy) = stream.try_clone() else {
        shared.refused.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let (tie, untied) = tokio::sync::oneshot::channel();
    shared.accepted.fetch_add(1, Ordering::Relaxed);
    instance.connection(Accepted {
        listener: shared.spec.name.clone(),
        stream,
        tie: Tie(tie),
    });
    let shared = shared.clone();
    tokio::spawn(async move {
        watch_passed(&shared, &copy, untied).await;
        drop(permit);
    });
}

/// Watch a passed connection through the listener's copy until it ends:
/// shut down, so its peer sees the end whoever still holds a descriptor.
async fn watch_passed(
    shared: &Shared,
    copy: &std::net::TcpStream,
    untied: tokio::sync::oneshot::Receiver<()>,
) {
    let mut stop = shared.stop.subscribe();
    tokio::select! {
        _ = untied => {}
        _ = stop.wait_for(|s| *s) => {}
        () = quiet_for(copy, shared.spec.idle_timeout) => {
            shared.idle_closed.fetch_add(1, Ordering::Relaxed);
        }
    }
    let _ = copy.shutdown(std::net::Shutdown::Both);
}

/// Resolve once the kernel has seen no data either way on `stream` for
/// `limit`.
async fn quiet_for(stream: &std::net::TcpStream, limit: Duration) {
    loop {
        let quiet = match quiet(stream) {
            Ok(quiet) => quiet,
            Err(e) => {
                tracing::debug!(error = %e, "TCP_INFO unread; the connection counts as active");
                Duration::ZERO
            }
        };
        if quiet >= limit {
            return;
        }
        tokio::time::sleep(limit - quiet).await;
    }
}

/// How long since `stream` last sent or received data, from `TCP_INFO`.
#[cfg(target_os = "linux")]
fn quiet(stream: &std::net::TcpStream) -> std::io::Result<Duration> {
    use std::mem::{MaybeUninit, size_of};
    use std::os::fd::AsRawFd;

    let mut info = MaybeUninit::<libc::tcp_info>::zeroed();
    let mut len =
        libc::socklen_t::try_from(size_of::<libc::tcp_info>()).map_err(std::io::Error::other)?;
    // SAFETY: the descriptor is open for the call (`stream` is borrowed),
    // and the kernel writes at most `len` bytes into `info`, which is that
    // large.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            info.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: zeroed, then partly written by the kernel; every field is an
    // integer, for which any bytes are a value.
    let info = unsafe { info.assume_init() };
    let ms = info.tcpi_last_data_recv.min(info.tcpi_last_data_sent);
    Ok(Duration::from_millis(u64::from(ms)))
}

/// Native listeners run on Linux only ([`listen_native`]).
#[cfg(not(target_os = "linux"))]
fn quiet(_stream: &std::net::TcpStream) -> std::io::Result<Duration> {
    Ok(Duration::ZERO)
}

async fn serve(rt: &Arc<RuntimeInner>, shared: &Shared, stream: TcpStream, peer: SocketAddr) {
    let Target::Component(current) = &shared.target else {
        return;
    };
    let (connection, spec) = current
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let pid = Pid {
        node: rt.node(),
        id: rt.next_id(),
        incarnation: 1,
    };
    probe::connection_open(pid, &shared.spec.name);
    let (_handle, ctx) = instance::detached(pid, &spec, rt.clone());
    let state = match InstanceState::new(pid, &spec, ctx) {
        Ok(state) => state,
        Err(why) => {
            finish(rt, shared, pid, End::Done(ExitReason::Crashed(why)));
            return;
        }
    };
    // A Store slot, as an instance holds; none free ends the connection,
    // as a full pooling allocator would.
    rt.pool_memory.taken();
    let Ok(_slot) = rt.slots.clone().try_acquire_owned() else {
        finish(
            rt,
            shared,
            pid,
            End::Done(ExitReason::Crashed("no instance slot".into())),
        );
        return;
    };
    let mut store = new_store(&rt.engine, state);
    let activity = Arc::new(Activity::new());
    let (read, write) = stream.into_split();
    let table = store.data_mut().table_mut();
    let streams = (
        table.push(
            Box::new(AsyncReadStream::new(Tracked::new(read, activity.clone()))) as InputStream,
        ),
        table.push(Box::new(AsyncWriteStream::new(
            WRITE_BUDGET,
            Tracked::new(write, activity.clone()),
        )) as OutputStream),
    );
    let (Ok(input), Ok(output)) = streams else {
        finish(
            rt,
            shared,
            pid,
            End::Done(ExitReason::Crashed("resource table full".into())),
        );
        return;
    };
    let info = ConnectionInfo {
        listener: shared.spec.name.clone(),
        peer: peer.to_string(),
    };
    let mut stop = shared.stop.subscribe();
    let idle = shared.spec.idle_timeout;
    let end = tokio::select! {
        result = run(&connection, &mut store, info, input, output) => End::Done(result),
        () = activity.idle_for(idle) => End::Idle,
        _ = stop.wait_for(|s| *s) => End::Stopped,
    };
    finish(rt, shared, pid, end);
}

/// Instantiate the handler and run `serve` to its end.
async fn run(
    connection: &LoadedConnection,
    store: &mut wasmtime::Store<InstanceState>,
    info: ConnectionInfo,
    input: Resource<InputStream>,
    output: Resource<OutputStream>,
) -> ExitReason {
    let result = async {
        let handler = connection.pre.instantiate_async(&mut *store).await?;
        handler
            .bulkhead_connection_handler()
            .call_serve(&mut *store, &info, input, output)
            .await
    }
    .await;
    match result {
        Ok(Ok(())) => ExitReason::Normal,
        Ok(Err(why)) => ExitReason::Crashed(why),
        Err(err) => classify(&err, store.data()),
    }
}

fn finish(rt: &RuntimeInner, shared: &Shared, pid: Pid, end: End) {
    let reason = match end {
        End::Done(reason) => reason,
        End::Idle => {
            shared.idle_closed.fetch_add(1, Ordering::Relaxed);
            ExitReason::Timeout
        }
        End::Stopped => ExitReason::Shutdown,
    };
    if !reason.is_normal() && reason != ExitReason::Timeout && reason != ExitReason::Shutdown {
        shared.crashed.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(listener = %shared.spec.name, %pid, %reason, "connection handler failed");
    }
    rt.registry.unregister_all(pid);
    probe::instance_exit(pid, &reason);
    delivery::notify_exit(rt, pid, &reason);
}

/// When a connection (or an HTTP handler's request) last moved a byte.
pub(crate) struct Activity {
    started: Instant,
    /// Milliseconds after `started`.
    last: AtomicU64,
    /// An HTTP handler has sent its response's headers.
    #[cfg(all(feature = "http", feature = "egress"))]
    responded: AtomicBool,
}

impl Activity {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            last: AtomicU64::new(0),
            #[cfg(all(feature = "http", feature = "egress"))]
            responded: AtomicBool::new(false),
        }
    }

    pub(crate) fn touch(&self) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.store(now, Ordering::Relaxed);
    }

    /// Record that the response's headers went out, as activity.
    #[cfg(all(feature = "http", feature = "egress"))]
    pub(crate) fn respond(&self) {
        self.touch();
        self.responded.store(true, Ordering::Relaxed);
    }

    #[cfg(all(feature = "http", feature = "egress"))]
    pub(crate) fn responded(&self) -> bool {
        self.responded.load(Ordering::Relaxed)
    }

    /// Resolve once nothing has moved for `limit`.
    pub(crate) async fn idle_for(&self, limit: Duration) {
        loop {
            let last = Duration::from_millis(self.last.load(Ordering::Relaxed));
            let quiet = self.started.elapsed().saturating_sub(last);
            if quiet >= limit {
                return;
            }
            tokio::time::sleep(limit - quiet).await;
        }
    }
}

/// A socket half that records activity.
struct Tracked<T> {
    inner: T,
    activity: Arc<Activity>,
}

impl<T> Tracked<T> {
    fn new(inner: T, activity: Arc<Activity>) -> Self {
        Self { inner, activity }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Tracked<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.activity.touch();
        }
        poll
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tracked<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(poll, Poll::Ready(Ok(n)) if n > 0) {
            self.activity.touch();
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
