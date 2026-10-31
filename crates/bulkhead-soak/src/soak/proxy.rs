//! A user-space UDP proxy in front of each node (QUIC is UDP; no root).
//!
//! Each node binds its own address and advertises its proxy's
//! (`cluster.advertise`), so every frame between two nodes passes through
//! the receiving side's proxy. A proxy tells the sending node from the
//! datagram's source address (each node sends from the address it binds),
//! and applies that direction's [`Mode`] to frames both ways: the dialer's
//! datagrams on their way in, and the replies on their way back.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::net::UdpSocket;

use super::rng::Rng;

/// What happens to datagrams in one direction between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Delivered.
    Pass,
    /// Dropped.
    Cut,
    /// Dropped `per_mille` times in a thousand.
    Drop(u64),
    /// Kept, then delivered in order when the mode changes.
    Hold,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "pass"),
            Self::Cut => write!(f, "cut"),
            Self::Drop(p) => write!(f, "drop {p}/1000"),
            Self::Hold => write!(f, "hold"),
        }
    }
}

/// At most this many datagrams held per direction; later ones are dropped.
const HELD_MAX: usize = 4096;

/// A datagram on its way: through `socket`, to `to` (or the socket's
/// connected peer).
#[derive(Debug)]
struct Datagram {
    socket: Arc<UdpSocket>,
    to: Option<SocketAddr>,
    data: Vec<u8>,
}

impl Datagram {
    async fn send(self) {
        // A node that is down refuses datagrams; that is the point.
        let _ = match self.to {
            Some(to) => self.socket.send_to(&self.data, to).await,
            None => self.socket.send(&self.data).await,
        };
    }
}

#[derive(Debug, Default)]
struct State {
    modes: HashMap<(u32, u32), Mode>,
    held: HashMap<(u32, u32), VecDeque<Datagram>>,
}

/// The mode of every direction, shared by the proxies.
#[derive(Debug)]
pub(crate) struct Faults {
    state: Mutex<State>,
    rng: Mutex<Rng>,
}

impl Faults {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            state: Mutex::default(),
            rng: Mutex::new(Rng::new(seed)),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Set the mode from `from` to `to`; datagrams held under `Hold` are
    /// delivered when it changes.
    pub(crate) async fn set(&self, from: u32, to: u32, mode: Mode) {
        let released = {
            let mut state = self.state();
            state.modes.insert((from, to), mode);
            if mode == Mode::Hold {
                VecDeque::new()
            } else {
                state.held.remove(&(from, to)).unwrap_or_default()
            }
        };
        for d in released {
            d.send().await;
        }
    }

    /// Every direction back to `Pass`, held datagrams delivered.
    pub(crate) async fn heal_all(&self) {
        let directions: Vec<(u32, u32)> = self.state().modes.keys().copied().collect();
        for (from, to) in directions {
            self.set(from, to, Mode::Pass).await;
        }
    }

    async fn deliver(&self, from: u32, to: u32, datagram: Datagram) {
        let mode = self
            .state()
            .modes
            .get(&(from, to))
            .copied()
            .unwrap_or(Mode::Pass);
        let pass = match mode {
            Mode::Pass => true,
            Mode::Cut => false,
            Mode::Drop(p) => !self
                .rng
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .chance(p),
            Mode::Hold => {
                let mut state = self.state();
                // The mode may have changed since it was read; deliver then.
                if state.modes.get(&(from, to)) == Some(&Mode::Hold) {
                    let queue = state.held.entry((from, to)).or_default();
                    if queue.len() < HELD_MAX {
                        queue.push_back(datagram);
                    }
                    return;
                }
                true
            }
        };
        if pass {
            datagram.send().await;
        }
    }
}

/// Start a proxy for `node`, which binds `target`. `peers` names each
/// node by the address it binds. Returns the address to advertise.
pub(crate) async fn start(
    node: u32,
    target: SocketAddr,
    peers: HashMap<SocketAddr, u32>,
    faults: Arc<Faults>,
) -> std::io::Result<SocketAddr> {
    let public = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let address = public.local_addr()?;
    tokio::spawn(async move {
        let mut upstreams: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
        let mut buf = vec![0u8; 65_536];
        loop {
            // Errors here are ICMP refusals from a node that is down.
            let Ok((n, source)) = public.recv_from(&mut buf).await else {
                continue;
            };
            let from = peers.get(&source).copied().unwrap_or(0);
            let upstream = match upstreams.get(&source) {
                Some(u) => u.clone(),
                None => {
                    let Ok(u) = upstream(target).await else {
                        continue;
                    };
                    let u = Arc::new(u);
                    tokio::spawn(replies(
                        u.clone(),
                        public.clone(),
                        source,
                        node,
                        from,
                        faults.clone(),
                    ));
                    upstreams.insert(source, u.clone());
                    u
                }
            };
            let datagram = Datagram {
                socket: upstream,
                to: None,
                data: buf[..n].to_vec(),
            };
            faults.deliver(from, node, datagram).await;
        }
    });
    Ok(address)
}

async fn upstream(target: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    socket.connect(target).await?;
    Ok(socket)
}

/// Carry `node`'s replies from `upstream` back to `source` (node `from`).
async fn replies(
    upstream: Arc<UdpSocket>,
    public: Arc<UdpSocket>,
    source: SocketAddr,
    node: u32,
    from: u32,
    faults: Arc<Faults>,
) {
    let mut buf = vec![0u8; 65_536];
    loop {
        let Ok(n) = upstream.recv(&mut buf).await else {
            // Refused while the node is down; keep going for its restart.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            continue;
        };
        let datagram = Datagram {
            socket: public.clone(),
            to: Some(source),
            data: buf[..n].to_vec(),
        };
        faults.deliver(node, from, datagram).await;
    }
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod proxy_tests;
