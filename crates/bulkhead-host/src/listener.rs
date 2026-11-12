//! Host-owned listeners, handing connections to a native app's instances.
//!
//! A [`Listener`] binds one address and accepts on several tasks (the Ranch
//! pattern). Each connection goes to an instance of the app's pool (by the
//! peer's address under hash routing, otherwise in turn), as an operation
//! ahead of its mailbox: the node passes the stream on the instance's
//! descriptor socket and runs its `handle_connection`, and the instance
//! serves many connections, reading each when the node says it is
//! readable. The listener keeps its own copy of each socket: it counts the
//! connection until the app closes it or the instance exits, shuts it
//! down when the listener stops, and closes it once the kernel's
//! `TCP_INFO` shows no data either way for the idle timeout
//! (`native::os::TcpQuiet`). A crash
//! closes every connection its instance held. The app's upgrades are its
//! own; the listener sends new connections to whichever instances run.
//!
//! At most `max_connections` at once; further connections are closed at
//! once and counted as refused.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

use crate::apps::AppHandle;
use crate::error::HostError;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use crate::native::os::TcpQuiet as Quiet;
use crate::runtime::RuntimeInner;

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
    /// Connections closed by the idle timeout.
    pub idle_closed: u64,
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
    app: AppHandle,
    slots: Arc<Semaphore>,
    accepted: AtomicU64,
    refused: AtomicU64,
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
            idle_closed: s.idle_closed.load(Ordering::Relaxed),
        }
    }

    /// Stop accepting and close every open connection.
    pub fn stop(&self) {
        self.shared.stop.send_replace(true);
    }
}

/// Bind `spec.address` and start accepting for `app`'s instances.
pub(crate) fn listen_native(
    rt: &Arc<RuntimeInner>,
    spec: ListenerSpec,
    app: AppHandle,
) -> Result<Listener, HostError> {
    if !cfg!(any(target_os = "linux", target_os = "freebsd")) {
        return Err(HostError::Listen(format!(
            "{}: native apps run on Linux and FreeBSD only",
            spec.name
        )));
    }
    bind(rt, spec, app)
}

fn bind(
    _rt: &Arc<RuntimeInner>,
    spec: ListenerSpec,
    app: AppHandle,
) -> Result<Listener, HostError> {
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
        app,
        accepted: AtomicU64::new(0),
        refused: AtomicU64::new(0),
        idle_closed: AtomicU64::new(0),
        stop,
        spec,
    });
    for _ in 0..shared.spec.acceptors.max(1) {
        tokio::spawn(accept(shared.clone(), listener.clone()));
    }
    Ok(Listener { shared })
}

async fn accept(shared: Arc<Shared>, listener: Arc<TcpListener>) {
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
        hand_over(&shared, stream, peer, permit);
    }
}

/// Pass a connection to an instance of the native app, and watch it until
/// the app closes it, the instance exits, the listener stops or it idles.
/// No instance running refuses it.
fn hand_over(
    shared: &Arc<Shared>,
    stream: TcpStream,
    peer: SocketAddr,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let Some(instance) = shared.app.route_peer(peer.ip()) else {
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
    let mut clock = Quiet::new();
    loop {
        let quiet = match clock.quiet(stream) {
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

/// Native listeners run on Linux and FreeBSD only ([`listen_native`]).
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
struct Quiet;

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
impl Quiet {
    fn new() -> Self {
        Self
    }

    #[expect(clippy::unused_self, reason = "the same call as on Linux and FreeBSD")]
    fn quiet(&mut self, _stream: &std::net::TcpStream) -> std::io::Result<Duration> {
        Ok(Duration::ZERO)
    }
}
