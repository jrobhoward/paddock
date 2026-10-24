//! `wasi:sockets`: TCP and UDP from apps (ROADMAP N1).
//!
//! wasmtime-wasi implements the interfaces and every app is linked to them;
//! this service narrows them. Without the capability granted, TCP, UDP and
//! name lookups are off, so every socket call fails with `access-denied`.
//! With it, each address a socket binds, listens on, accepts from, connects
//! or sends to passes the app's policy (the `policy` module: the
//! manifest's rules and the address guard), and name lookups are on only
//! for an app with `connect` rules. A refusal is `access-denied`, never a trap.
//!
//! An app holds at most `max_sockets` sockets open across its instances:
//! sockets created and accepted, TCP and UDP. wasmtime-wasi has no hook for
//! that count, so `shadow` redefines the functions that make a socket
//! (`create-tcp-socket`, `create-udp-socket`, `accept`) and the two socket
//! resources' destructors on the linker, taking a permit for each socket
//! and returning it when the guest drops the socket or its Store goes.
//! Over the count, the guest sees `new-socket-limit`.
//!
//! Node config's `[sockets]` is a ceiling every app's rules must fit
//! inside; the node checks it at deploy ([`within`]).

mod policy;

pub use policy::within;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use paddock_bundle::manifest::{AppSpec, Capability, SocketsScope};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wasmtime::component::{Linker, Resource, ResourceType};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p2::bindings::sockets::network::{ErrorCode, IpAddressFamily};
use wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket;
use wasmtime_wasi::p2::bindings::sockets::udp::HostUdpSocket;
use wasmtime_wasi::p2::bindings::sockets::{tcp_create_socket, udp_create_socket};
use wasmtime_wasi::p2::{DynInputStream, DynOutputStream, SocketResult, TcpSocket, UdpSocket};
use wasmtime_wasi::sockets::WasiSocketsView;

use super::{HostState, InstanceInfo, Service};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::metrics::AppStats;
use crate::spec::InstanceSpec;
use policy::Policy;

/// The `wasi:sockets` version wasmtime-wasi links.
const VERSION: &str = "0.2.12";

/// The sockets service.
pub(crate) struct Sockets;

/// What the manifest asks of sockets for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SocketsSpec {
    /// The manifest's `wasi:sockets` scope; `None` when it declares none.
    pub scope: Option<SocketsScope>,
}

/// Node-wide sockets state: each app's limit and its semaphore.
pub(crate) struct Shared {
    limits: Mutex<HashMap<String, (u32, Arc<Semaphore>)>>,
}

/// Sockets state for one instance.
pub(crate) struct Instance {
    granted: bool,
    limit: Arc<Semaphore>,
    /// A permit for each socket the guest holds, by resource rep.
    held: HashMap<u32, Held>,
    stats: Arc<AppStats>,
}

/// One open socket's share of the app's count.
struct Held {
    _permit: OwnedSemaphorePermit,
    stats: Arc<AppStats>,
}

impl Held {
    fn new(permit: OwnedSemaphorePermit, stats: &Arc<AppStats>) -> Self {
        stats.socket_opened();
        Self {
            _permit: permit,
            stats: stats.clone(),
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.stats.socket_closed();
    }
}

impl Service for Sockets {
    const CAPABILITY: Capability = Capability::Sockets;
    // wasmtime-wasi links `wasi:sockets` for every app (`crate::deny`).
    const INTERFACES: &'static [&'static str] = &[];
    type Spec = SocketsSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> SocketsSpec {
        SocketsSpec {
            scope: manifest.capabilities.sockets.clone(),
        }
    }

    fn shared(_config: &RuntimeConfig) -> Result<Shared, HostError> {
        Ok(Shared {
            limits: Mutex::default(),
        })
    }

    fn instance(shared: &Arc<Shared>, spec: &SocketsSpec, info: &InstanceInfo<'_>) -> Instance {
        let app = info.handle.app().to_string();
        let max = spec.scope.as_ref().map_or(0, |s| s.max_sockets);
        let limit = {
            let mut limits = shared.limits.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = limits
                .entry(app.clone())
                .or_insert_with(|| (max, Arc::new(Semaphore::new(permits(max)))));
            // An upgrade that changes the limit starts a new count; sockets
            // open under the old one keep their permits until closed.
            if entry.0 != max {
                *entry = (max, Arc::new(Semaphore::new(permits(max))));
            }
            entry.1.clone()
        };
        Instance {
            granted: info.granted && spec.scope.is_some(),
            limit,
            held: HashMap::new(),
            stats: info.metrics.stats(&app),
        }
    }

    // Linked by wasmtime-wasi; [`shadow`] adds the count.
    fn link<T: HostState<Self>>(_linker: &mut Linker<T>) -> wasmtime::Result<()> {
        Ok(())
    }
}

fn permits(max: u32) -> usize {
    usize::try_from(max)
        .unwrap_or(usize::MAX)
        .min(Semaphore::MAX_PERMITS)
}

/// Narrow a new instance's WASI context to its app's sockets policy.
pub(crate) fn configure(wasi: &mut WasiCtxBuilder, spec: &InstanceSpec, stats: Arc<AppStats>) {
    let Some(scope) = spec.services.sockets.scope.as_ref() else {
        return;
    };
    if !spec.granted.contains(&Capability::Sockets) {
        return;
    }
    let policy = Arc::new(Policy::new(scope));
    let app = spec.app.clone();
    wasi.allow_tcp(true)
        .allow_udp(true)
        .allow_ip_name_lookup(policy.name_lookup())
        .socket_addr_check(move |addr, why| {
            let allowed = policy.allows(addr, why);
            if !allowed {
                tracing::debug!(%app, %addr, ?why, "socket address refused");
                stats.socket_refused();
            }
            Box::pin(async move { allowed })
        });
}

impl Instance {
    /// A permit for one more socket, or `None` over the app's count. An
    /// instance without the capability takes none: wasmtime-wasi refuses
    /// its sockets anyway.
    fn take(&self) -> Result<Option<OwnedSemaphorePermit>, ErrorCode> {
        if !self.granted {
            return Ok(None);
        }
        match self.limit.clone().try_acquire_owned() {
            Ok(permit) => Ok(Some(permit)),
            Err(_) => {
                self.stats.socket_refused();
                Err(ErrorCode::NewSocketLimit)
            }
        }
    }

    fn keep(&mut self, rep: u32, permit: Option<OwnedSemaphorePermit>) {
        if let Some(permit) = permit {
            self.held.insert(rep, Held::new(permit, &self.stats));
        }
    }

    fn release(&mut self, rep: u32) {
        self.held.remove(&rep);
    }
}

/// A host function's result as the guest sees it: a socket error as a
/// value, anything else a trap.
fn settle<R>(made: SocketResult<R>) -> wasmtime::Result<Result<R, ErrorCode>> {
    match made {
        Ok(r) => Ok(Ok(r)),
        Err(e) => Ok(Err(e.downcast()?)),
    }
}

/// Redefine the functions that make sockets, and the sockets' destructors,
/// to keep the app's count. Call after wasmtime-wasi's `add_to_linker`.
///
/// # Errors
///
/// A wasmtime error if an interface is missing.
pub(crate) fn shadow<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: HostState<Sockets> + WasiSocketsView,
{
    linker.allow_shadowing(true);
    let mut create = linker.instance(&format!("wasi:sockets/tcp-create-socket@{VERSION}"))?;
    create.func_wrap(
        "create-tcp-socket",
        |mut store, (family,): (IpAddressFamily,)| {
            let t = store.data_mut();
            let permit = match t.service().state.take() {
                Ok(permit) => permit,
                Err(code) => return Ok((Err(code),)),
            };
            let made = settle(tcp_create_socket::Host::create_tcp_socket(
                &mut t.sockets(),
                family,
            ))?;
            if let Ok(socket) = &made {
                t.service().state.keep(socket.rep(), permit);
            }
            Ok((made,))
        },
    )?;
    let mut create = linker.instance(&format!("wasi:sockets/udp-create-socket@{VERSION}"))?;
    create.func_wrap_async(
        "create-udp-socket",
        |mut store, (family,): (IpAddressFamily,)| {
            Box::new(async move {
                let t = store.data_mut();
                let permit = match t.service().state.take() {
                    Ok(permit) => permit,
                    Err(code) => return Ok((Err(code),)),
                };
                let made = settle(
                    udp_create_socket::Host::create_udp_socket(&mut t.sockets(), family).await,
                )?;
                if let Ok(socket) = &made {
                    t.service().state.keep(socket.rep(), permit);
                }
                Ok((made,))
            })
        },
    )?;
    let mut tcp = linker.instance(&format!("wasi:sockets/tcp@{VERSION}"))?;
    tcp.func_wrap(
        "[method]tcp-socket.accept",
        |mut store, (this,): (Resource<TcpSocket>,)| {
            type Accepted = (
                Resource<TcpSocket>,
                Resource<DynInputStream>,
                Resource<DynOutputStream>,
            );
            let t = store.data_mut();
            let permit = match t.service().state.take() {
                Ok(permit) => permit,
                Err(code) => return Ok((Err::<Accepted, _>(code),)),
            };
            let made = settle(HostTcpSocket::accept(&mut t.sockets(), this))?;
            if let Ok((socket, _, _)) = &made {
                t.service().state.keep(socket.rep(), permit);
            }
            Ok((made,))
        },
    )?;
    tcp.resource(
        "tcp-socket",
        ResourceType::host::<TcpSocket>(),
        |mut store, rep| {
            let t = store.data_mut();
            t.service().state.release(rep);
            HostTcpSocket::drop(&mut t.sockets(), Resource::new_own(rep))
        },
    )?;
    let mut udp = linker.instance(&format!("wasi:sockets/udp@{VERSION}"))?;
    udp.resource(
        "udp-socket",
        ResourceType::host::<UdpSocket>(),
        |mut store, rep| {
            let t = store.data_mut();
            t.service().state.release(rep);
            HostUdpSocket::drop(&mut t.sockets(), Resource::new_own(rep))
        },
    )?;
    linker.allow_shadowing(false);
    Ok(())
}
