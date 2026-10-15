//! QUIC between nodes (ROADMAP D1): a [`Transport`] over quinn with mutual
//! TLS (rustls on ring).
//!
//! **Identity.** Pinned public keys, as WireGuard does, with no
//! certificate authority ([`identity`]): each node holds an Ed25519 key,
//! and a peers file lists every node's public key against its id. The TLS
//! 1.3 handshake carries raw public keys (RFC 7250); each side accepts the
//! other only if its key is listed. Each then sends a [`Hello`] naming its
//! node id, and the other checks the key belongs to that node, so a node
//! cannot claim another's id. Seeds are plain addresses: the dialer learns
//! the seed's id from its hello. [`QuicTransport::reload`] reads the files
//! again (a rotation, a new peer); connections already open keep the keys
//! they started with.
//!
//! **Connections.** One per pair of nodes, carrying two streams opened by
//! the dialer: control (heartbeats, membership, failover) and data
//! (messages, links, monitors). Each is ordered, so frames between a pair
//! keep their order within each stream; heartbeats never wait behind a
//! large message. Frames are length-prefixed [`wire`] packets of at most
//! [`MAX_FRAME`] bytes. If both nodes dial each other at once, the
//! connection dialed by the lower node id is kept.
//!
//! **Loss.** When a connection closes (the peer went away, QUIC's idle
//! timeout, a bad frame, or a newer connection replaced it), the session
//! with that node ends (`end_session`): links and monitors with it end with
//! `noconnection`. The node with the lower id redials a member it knows
//! (and any node redials its seeds), with backoff; heartbeats
//! ([`super::liveness`]) find a peer that stalls without the connection
//! closing. A node that learns of a new member dials it if its own id is
//! lower; the other waits to be dialed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    Connection, Endpoint, ReadExactError, RecvStream, SendStream, TransportConfig, VarInt,
};
use rustls::client::AlwaysResolvesClientRawPublicKeys;
use rustls::crypto::ring::default_provider;
use rustls::server::AlwaysResolvesServerRawPublicKeys;
use rustls_pki_types::CertificateDer;
use tokio::sync::{mpsc, watch};

pub mod identity;

pub use identity::{
    Accepts, NodeIdentity, PublicKey, admin_client_tls, admin_server_tls, generate_key,
    parse_operators, presented_key, public_key_of,
};

use identity::{Loaded, PinnedKeys};

use super::lane::{Class, Lane, LaneStats, Ticket};
use super::liveness::NodeEvent;
use super::wire::{self, Hello};
use super::{Packet, SendError, Transport, control};
use crate::error::HostError;
use crate::runtime::{Runtime, RuntimeInner};

/// The ALPN protocol both ends must offer.
const ALPN: &[u8] = b"paddock";

/// The largest frame accepted, in bytes; a larger one closes the
/// connection.
pub const MAX_FRAME: u32 = 64 << 20;

/// What each side sends once it has checked the other's hello and key.
const ACCEPTED: &[u8] = b"accepted";

/// Control frames queued before sends report the node unreachable.
const QUEUE: usize = 4_096;

/// Data frames (messages and notices) queued before a send is refused as
/// full ([`super::lane`]): the peer has stopped reading.
const DATA_FRAMES_MAX: usize = 65_536;

/// The default message budget per peer ([`QuicConfig::send_buffer_bytes`]).
pub const SEND_BUFFER_BYTES: u64 = 64 << 20;

/// How the transport runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuicConfig {
    /// The UDP address to listen on (and dial from).
    pub bind: SocketAddr,
    /// The node's key and its peers' keys.
    pub identity: NodeIdentity,
    /// A connection silent this long closes (QUIC's idle timeout, a
    /// backstop to heartbeats).
    pub idle_timeout: Duration,
    /// Bytes of messages (casts, calls, replies) queued for one peer before
    /// more are refused as busy: a cast is then dropped and counted, a call
    /// fails with `overloaded`. Link and monitor notices are let in past it
    /// (see DESIGN.md, "Backpressure between nodes"). [`SEND_BUFFER_BYTES`]
    /// by default.
    pub send_buffer_bytes: u64,
}

/// The QUIC transport. Cheap to clone.
#[derive(Clone)]
pub struct QuicTransport {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for QuicTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicTransport")
            .field("node", &self.inner.node)
            .finish_non_exhaustive()
    }
}

struct Inner {
    rt: Weak<RuntimeInner>,
    node: u32,
    creation: u32,
    endpoint: Endpoint,
    config: QuicConfig,
    /// The keys as last loaded, for checking hellos.
    loaded: RwLock<Loaded>,
    /// The identity files' modification times at the last load.
    stamps: Mutex<[Option<std::time::SystemTime>; 2]>,
    client: RwLock<quinn::ClientConfig>,
    peers: Mutex<HashMap<u32, Peer>>,
    /// Sends refused per peer since the transport started: busy or full.
    refused: Mutex<HashMap<u32, u64>>,
    /// Seed addresses, redialed whenever their connection is lost.
    seeds: Mutex<Vec<SocketAddr>>,
    stop: watch::Sender<bool>,
}

struct Peer {
    conn: Connection,
    /// The node that dialed this connection.
    dialer: u32,
    version: u16,
    control: mpsc::Sender<Vec<u8>>,
    /// Messages and notices, let in by `lane`.
    data: mpsc::UnboundedSender<(Vec<u8>, Ticket)>,
    lane: Arc<Lane>,
    /// The tasks writing the two streams; they end once the queues are
    /// written and the senders dropped.
    writers: Vec<tokio::task::JoinHandle<()>>,
}

fn tls_error(e: impl std::fmt::Display) -> HostError {
    HostError::Listen(format!("cluster TLS: {e}"))
}

/// The server and client configurations an identity makes.
fn configs(
    loaded: &Loaded,
    idle_timeout: Duration,
) -> Result<(quinn::ServerConfig, quinn::ClientConfig), HostError> {
    let provider = Arc::new(default_provider());
    let pinned = Arc::new(PinnedKeys::new(loaded.clone()));
    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .with_client_cert_verifier(pinned.clone())
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(
            loaded.certified.clone(),
        )));
    server.alpn_protocols = vec![ALPN.to_vec()];
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(pinned)
        .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(
            loaded.certified.clone(),
        )));
    client.alpn_protocols = vec![ALPN.to_vec()];

    let mut transport = TransportConfig::default();
    transport
        .max_idle_timeout(Some(idle_timeout.try_into().map_err(tls_error)?))
        .keep_alive_interval(Some(idle_timeout / 3));
    let transport = Arc::new(transport);
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(server).map_err(tls_error)?,
    ));
    server.transport_config(transport.clone());
    let mut client = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(client).map_err(tls_error)?,
    ));
    client.transport_config(transport);
    Ok((server, client))
}

/// The public key the connection's peer presented.
fn peer_key(conn: &Connection) -> Option<PublicKey> {
    let identity = conn.peer_identity()?;
    let chain = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    chain.first().map(|k| PublicKey::from_der(k.as_ref()))
}

async fn write_frame(stream: &mut SendStream, bytes: &[u8]) -> Result<(), String> {
    let len = u32::try_from(bytes.len()).map_err(|_| "frame too large".to_string())?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(bytes).await.map_err(|e| e.to_string())
}

async fn read_frame(stream: &mut RecvStream) -> Result<Vec<u8>, String> {
    next_frame(stream)
        .await?
        .ok_or_else(|| "the stream finished".to_string())
}

/// The next frame, or `None` if the peer finished the stream between
/// frames (a graceful shutdown).
async fn next_frame(stream: &mut RecvStream) -> Result<Option<Vec<u8>>, String> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        return Err(format!("frame of {len} bytes"));
    }
    let mut buf = vec![0u8; usize::try_from(len).map_err(|e| e.to_string())?];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(buf))
}

impl QuicTransport {
    /// Listen on `config.bind` and become `rt`'s transport. Call inside the
    /// tokio runtime.
    ///
    /// # Errors
    ///
    /// [`HostError::Listen`] for identity files that cannot be read, or an
    /// address that cannot be bound; [`HostError::Engine`] if `rt` already
    /// has a transport.
    pub fn start(rt: &Runtime, config: QuicConfig) -> Result<Self, HostError> {
        let loaded = config.identity.load()?;
        let (server, client) = configs(&loaded, config.idle_timeout)?;
        let endpoint = Endpoint::server(server, config.bind)
            .map_err(|e| HostError::Listen(format!("cluster {}: {e}", config.bind)))?;
        let inner = Arc::new(Inner {
            rt: Arc::downgrade(rt.inner()),
            node: rt.node(),
            creation: rt.inner().creation(),
            endpoint,
            stamps: Mutex::new(config.identity.stamps()),
            config,
            loaded: RwLock::new(loaded),
            client: RwLock::new(client),
            peers: Mutex::default(),
            refused: Mutex::default(),
            seeds: Mutex::default(),
            stop: watch::channel(false).0,
        });
        let transport = Self { inner };
        rt.set_transport(Arc::new(transport.clone()))?;
        tokio::spawn(accept(transport.inner.clone()));
        tokio::spawn(dial_new_members(transport.inner.clone(), rt.node_events()));
        Ok(transport)
    }

    /// The UDP address the transport listens on.
    ///
    /// # Errors
    ///
    /// If the socket cannot say.
    pub fn local_addr(&self) -> Result<SocketAddr, HostError> {
        self.inner
            .endpoint
            .local_addr()
            .map_err(|e| HostError::Listen(e.to_string()))
    }

    /// Dial the seed at `address`, join the cluster through it, and keep
    /// redialing it whenever the connection is lost. Returns the seed's
    /// node id.
    ///
    /// # Errors
    ///
    /// [`HostError::Listen`] if the seed cannot be reached or refuses this
    /// node; the seed is redialed in the background all the same.
    pub async fn join_seed(&self, address: SocketAddr) -> Result<u32, HostError> {
        let inner = &self.inner;
        {
            let mut seeds = inner.seeds.lock().unwrap_or_else(PoisonError::into_inner);
            if !seeds.contains(&address) {
                seeds.push(address);
            }
        }
        match dial(inner, address).await {
            Ok(node) => Ok(node),
            Err(e) => {
                tokio::spawn(redial(inner.clone(), address, None));
                Err(HostError::Listen(format!("seed {address}: {e}")))
            }
        }
    }

    /// Read the key and peers files again, for new connections.
    ///
    /// # Errors
    ///
    /// As for [`QuicTransport::start`]; the old identity stays in use.
    pub fn reload(&self) -> Result<(), HostError> {
        let inner = &self.inner;
        let loaded = inner.config.identity.load()?;
        let (server, client) = configs(&loaded, inner.config.idle_timeout)?;
        inner.endpoint.set_server_config(Some(server));
        *inner.client.write().unwrap_or_else(PoisonError::into_inner) = client;
        *inner.loaded.write().unwrap_or_else(PoisonError::into_inner) = loaded;
        *inner.stamps.lock().unwrap_or_else(PoisonError::into_inner) =
            inner.config.identity.stamps();
        Ok(())
    }

    /// Reload if the key or peers file changed since the last load.
    /// Returns whether it reloaded.
    ///
    /// # Errors
    ///
    /// As for [`QuicTransport::reload`].
    pub fn reload_if_changed(&self) -> Result<bool, HostError> {
        let now = self.inner.config.identity.stamps();
        let changed = *self
            .inner
            .stamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            != now;
        if changed {
            self.reload()?;
        }
        Ok(changed)
    }

    /// The nodes with an open connection.
    #[must_use]
    pub fn connected(&self) -> Vec<u32> {
        let mut nodes: Vec<u32> = self
            .inner
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        nodes.sort_unstable();
        nodes
    }

    /// Each peer's data lane: messages and frames queued now (for a peer
    /// with a connection), and sends refused as busy or full since the
    /// transport started.
    #[must_use]
    pub fn send_stats(&self) -> Vec<(u32, LaneStats)> {
        let refused = self
            .inner
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut stats: std::collections::BTreeMap<u32, LaneStats> = self
            .inner
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(&node, peer)| (node, peer.lane.stats()))
            .collect();
        for s in stats.values_mut() {
            s.refused = 0;
        }
        for (node, count) in refused {
            stats.entry(node).or_default().refused = count;
        }
        stats.into_iter().collect()
    }

    /// Write what is queued to every peer (a [`super::Frame::Leave`], say) and
    /// wait until each has read it and closed the connection, for up to a
    /// second in all; then close every connection and stop listening and
    /// redialing.
    pub async fn shutdown_gracefully(&self) {
        self.inner.stop.send_replace(true);
        let peers: Vec<Peer> = self
            .inner
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, p)| p)
            .collect();
        let conns: Vec<Connection> = peers.iter().map(|p| p.conn.clone()).collect();
        let writers: Vec<_> = peers
            .into_iter()
            .flat_map(|p| {
                // Dropping the senders ends the writers once the queues
                // are written.
                drop((p.control, p.data));
                p.writers
            })
            .collect();
        // Wait for each peer to close the connection, which it does once it
        // has read both streams to the end: closing first would discard
        // frames it has received but not yet read.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            for w in writers {
                let _ = w.await;
            }
            for conn in conns {
                conn.closed().await;
            }
        })
        .await;
        self.shutdown();
    }

    /// Close every connection at once and stop listening and redialing.
    pub fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        self.inner.endpoint.close(VarInt::from_u32(0), b"shutdown");
        self.inner
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

impl Transport for QuicTransport {
    fn send(&self, node: u32, packet: Packet) -> Result<(), SendError> {
        let peers = self
            .inner
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let peer = peers.get(&node).ok_or(SendError::Unreachable)?;
        let bytes = wire::encode(peer.version, &packet).map_err(|_| SendError::Unreachable)?;
        if control(&packet.frame) {
            return peer
                .control
                .try_send(bytes)
                .map_err(|_| SendError::Unreachable);
        }
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let ticket = match peer.lane.admit(Class::of(&packet.frame), size) {
            Ok(ticket) => ticket,
            Err(why) => {
                drop(peers);
                *self
                    .inner
                    .refused
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(node)
                    .or_default() += 1;
                return Err(why);
            }
        };
        peer.data
            .send((bytes, ticket))
            .map_err(|_| SendError::Unreachable)
    }
}

async fn accept(inner: Arc<Inner>) {
    let mut stop = inner.stop.subscribe();
    loop {
        let incoming = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            incoming = inner.endpoint.accept() => incoming,
        };
        let Some(incoming) = incoming else {
            return;
        };
        let inner = inner.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = establish_within(&inner, conn, false).await {
                        tracing::debug!(error = %e, "cluster connection refused");
                    }
                }
                Err(e) => tracing::debug!(error = %e, "cluster handshake failed"),
            }
        });
    }
}

/// Dial `address`; returns the node there.
async fn dial(inner: &Arc<Inner>, address: SocketAddr) -> Result<u32, String> {
    let client = inner
        .client
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    // The name is not used: the key, checked against the hello, names the
    // node.
    let conn = inner
        .endpoint
        .connect_with(client, address, "paddock")
        .map_err(|e| e.to_string())?
        .await
        .map_err(|e| e.to_string())?;
    establish_within(inner, conn, true).await
}

/// [`establish`], given up after the idle timeout. Keep-alives stop a
/// connection idling out while a peer that never answers the hello still
/// holds it: a stalled or hostile peer, or a handshake held in the network
/// and delivered after the process that answered it was replaced at the
/// same address. Unbounded, such a dial never returns and the redial loop
/// waiting on it never tries again.
async fn establish_within(
    inner: &Arc<Inner>,
    conn: Connection,
    dialer: bool,
) -> Result<u32, String> {
    let limit = inner.config.idle_timeout;
    if let Ok(result) = tokio::time::timeout(limit, establish(inner, conn.clone(), dialer)).await {
        return result;
    }
    conn.close(VarInt::from_u32(3), b"no hello in time");
    Err(format!("no hello within {} ms", limit.as_millis()))
}

/// Exchange hellos on a new connection, check the peer, keep the
/// connection and start its streams. Returns the peer's node id.
async fn establish(inner: &Arc<Inner>, conn: Connection, dialer: bool) -> Result<u32, String> {
    let mine = Hello::new(inner.node, inner.creation);
    let (mut control_tx, mut control_rx) = if dialer {
        conn.open_bi().await.map_err(|e| e.to_string())?
    } else {
        conn.accept_bi().await.map_err(|e| e.to_string())?
    };
    let refuse = |why: String| {
        conn.close(VarInt::from_u32(1), why.as_bytes());
        why
    };
    write_frame(
        &mut control_tx,
        &wire::to_cbor(&mine).map_err(|e| e.to_string())?,
    )
    .await?;
    let theirs: Hello =
        wire::from_cbor(&read_frame(&mut control_rx).await?).map_err(|e| refuse(e.to_string()))?;
    let Some(version) = wire::agree(&mine, &theirs) else {
        return Err(refuse(format!(
            "node {} speaks versions {}..={}",
            theirs.node, theirs.min_version, theirs.max_version
        )));
    };
    if theirs.node == inner.node {
        return Err(refuse("a node cannot connect to itself".into()));
    }
    let named = peer_key(&conn).is_some_and(|key| {
        inner
            .loaded
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .names(theirs.node, &key)
    });
    if !named {
        return Err(refuse(format!(
            "the key presented is not node {}'s",
            theirs.node
        )));
    }
    // Each side has checked the other; neither counts the connection until
    // the other says it accepted too. In TLS 1.3 the client finishes before
    // the server has checked the client's key, so without this a refused
    // dialer could believe it was connected.
    write_frame(&mut control_tx, ACCEPTED).await?;
    if read_frame(&mut control_rx).await? != ACCEPTED {
        return Err(refuse("the peer did not accept the connection".into()));
    }
    // The data stream: the dialer opens it and writes first, so the other
    // side sees it.
    let (data_tx, data_rx) = if dialer {
        let (mut tx, rx) = conn.open_bi().await.map_err(|e| e.to_string())?;
        write_frame(&mut tx, &[]).await?;
        (tx, rx)
    } else {
        let (tx, mut rx) = conn.accept_bi().await.map_err(|e| e.to_string())?;
        read_frame(&mut rx).await?;
        (tx, rx)
    };
    let node = theirs.node;
    let dialer_node = if dialer { inner.node } else { node };
    let (control, control_queue) = mpsc::channel(QUEUE);
    let (data, data_queue) = mpsc::unbounded_channel();
    let lane = Arc::new(Lane::new(inner.config.send_buffer_bytes, DATA_FRAMES_MAX));
    let writers = vec![
        tokio::spawn(writer(control_tx, control_queue)),
        tokio::spawn(data_writer(data_tx, data_queue, lane.clone())),
    ];
    {
        let mut peers = inner.peers.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = peers.get(&node)
            && old.dialer < dialer_node
            && old.conn.close_reason().is_none()
        {
            // Both dialed; the lower dialer's connection stays.
            drop(peers);
            return Err(refuse("a connection to this node exists".into()));
        }
        if let Some(old) = peers.insert(
            node,
            Peer {
                conn: conn.clone(),
                dialer: dialer_node,
                version,
                control,
                data,
                lane,
                writers,
            },
        ) {
            old.conn.close(VarInt::from_u32(2), b"replaced");
        }
    }
    let finished = Arc::new(AtomicU8::new(0));
    tokio::spawn(reader(
        inner.clone(),
        conn.clone(),
        node,
        version,
        control_rx,
        finished.clone(),
    ));
    tokio::spawn(reader(
        inner.clone(),
        conn.clone(),
        node,
        version,
        data_rx,
        finished,
    ));
    tokio::spawn(watch_close(inner.clone(), conn, node));
    if let Some(rt) = inner.rt.upgrade() {
        super::join(&rt, &[node]);
    }
    tracing::info!(node, version, dialer, "cluster connection up");
    Ok(node)
}

/// The data stream's writer: as [`writer`], giving each frame's ticket
/// back to the lane once it is written.
async fn data_writer(
    mut stream: SendStream,
    mut queue: mpsc::UnboundedReceiver<(Vec<u8>, Ticket)>,
    lane: Arc<Lane>,
) {
    while let Some((bytes, ticket)) = queue.recv().await {
        let written = write_frame(&mut stream, &bytes).await;
        lane.done(ticket);
        if written.is_err() {
            return;
        }
    }
    if stream.finish().is_ok() {
        let _ = stream.stopped().await;
    }
}

async fn writer(mut stream: SendStream, mut queue: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = queue.recv().await {
        if write_frame(&mut stream, &bytes).await.is_err() {
            return;
        }
    }
    // The queue closed: the transport is shutting down. Let the peer read
    // what was written before the connection closes.
    if stream.finish().is_ok() {
        let _ = stream.stopped().await;
    }
}

/// Deliver every frame on one of a connection's streams. When the peer
/// finishes both streams (a graceful shutdown), every frame it sent, its
/// `Leave` included, has been delivered: close the connection then, which
/// tells the peer it may close its endpoint. Closing on the first finish
/// would discard frames still unread on the other stream.
async fn reader(
    inner: Arc<Inner>,
    conn: Connection,
    node: u32,
    version: u16,
    mut stream: RecvStream,
    finished: Arc<AtomicU8>,
) {
    loop {
        let bytes = match next_frame(&mut stream).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                if finished.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
                    conn.close(VarInt::from_u32(0), b"left");
                }
                return;
            }
            Err(_) => {
                conn.close(VarInt::from_u32(3), b"stream ended");
                return;
            }
        };
        let packet = match wire::decode(version, &bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(node, error = %e, "bad frame; closing the connection");
                conn.close(VarInt::from_u32(4), b"bad frame");
                return;
            }
        };
        let Some(rt) = inner.rt.upgrade() else {
            return;
        };
        super::deliver(&rt, node, packet);
    }
}

/// When `conn` closes: forget it, end the session, and redial if this
/// node should.
///
/// A connection replaced by a newer one (a redial that won the race to be
/// kept) still ends the session when it closes: frames queued on it are
/// lost with it, and the other side, having seen its own end of it close,
/// has ended the session too (spike S12, R2). Only the current connection
/// is forgotten and redialed.
async fn watch_close(inner: Arc<Inner>, conn: Connection, node: u32) {
    let reason = conn.closed().await;
    let current = {
        let mut peers = inner.peers.lock().unwrap_or_else(PoisonError::into_inner);
        let current = peers
            .get(&node)
            .is_some_and(|p| p.conn.stable_id() == conn.stable_id());
        if current {
            peers.remove(&node);
        }
        current
    };
    if *inner.stop.borrow() {
        return;
    }
    let Some(rt) = inner.rt.upgrade() else {
        return;
    };
    if !current {
        tracing::info!(node, %reason, "replaced cluster connection closed");
        super::end_session(&rt, node);
        return;
    }
    tracing::info!(node, %reason, "cluster connection lost");
    super::end_session(&rt, node);
    let address = rt
        .membership
        .list()
        .into_iter()
        .find(|m| m.node == node)
        .and_then(|m| m.address.parse::<SocketAddr>().ok());
    let seed = inner
        .seeds
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&conn.remote_address());
    drop(rt);
    if seed {
        tokio::spawn(redial(inner, conn.remote_address(), Some(node)));
    } else if let Some(address) = address
        && inner.node < node
    {
        tokio::spawn(redial(inner, address, Some(node)));
    }
}

/// Dial `address` until it answers (as `node`, if given) or the transport
/// stops, backing off from 100 ms to 5 s. Boxed: connections redial from
/// inside the tasks a connection starts.
fn redial(
    inner: Arc<Inner>,
    address: SocketAddr,
    node: Option<u32>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let mut stop = inner.stop.subscribe();
        let mut wait = Duration::from_millis(100);
        loop {
            tokio::select! {
                _ = stop.wait_for(|s| *s) => return,
                () = tokio::time::sleep(wait) => {}
            }
            if node.is_some_and(|n| {
                inner
                    .peers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains_key(&n)
            }) {
                return;
            }
            match dial(&inner, address).await {
                Ok(_) => return,
                Err(e) => tracing::debug!(%address, error = %e, "redial failed"),
            }
            wait = (wait * 2).min(Duration::from_secs(5));
        }
    })
}

/// Dial each member this node learns of, if its id is lower.
async fn dial_new_members(
    inner: Arc<Inner>,
    mut events: tokio::sync::broadcast::Receiver<NodeEvent>,
) {
    let mut stop = inner.stop.subscribe();
    loop {
        let event = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            event = events.recv() => event,
        };
        let node = match event {
            Ok(NodeEvent::Joined { node, .. }) => node,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        if node <= inner.node
            || inner
                .peers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&node)
        {
            continue;
        }
        let Some(rt) = inner.rt.upgrade() else {
            return;
        };
        let address = rt
            .membership
            .list()
            .into_iter()
            .find(|m| m.node == node)
            .and_then(|m| m.address.parse::<SocketAddr>().ok());
        drop(rt);
        if let Some(address) = address {
            tokio::spawn(redial(inner.clone(), address, Some(node)));
        }
    }
}

#[cfg(test)]
#[path = "quic_tests.rs"]
mod quic_tests;
