//! `wasi:sockets`: TCP and UDP from apps.
//!
//! Without the capability granted, every socket request is refused. With
//! it, each address a socket binds, listens on, accepts from, connects or
//! sends to passes the app's policy (the `policy` module: the manifest's
//! rules and the address guard), and name lookups are on only for an app
//! with `connect` rules. A refusal is `access-denied`, never a crash.
//!
//! An app holds at most `max_sockets` sockets open across its instances:
//! sockets created and accepted, TCP and UDP. Over the count, the app sees
//! `new-socket-limit`.
//!
//! Node config's `[sockets]` is a ceiling every app's rules must fit
//! inside; the node checks it at deploy ([`within`]). The sockets
//! themselves are in `native`.

#[cfg(unix)]
mod native;
mod policy;

#[cfg(unix)]
pub(crate) use native::Reply as NativeReply;

pub use policy::within;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use bulkhead_bundle::manifest::{AppSpec, Capability, SocketsScope};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{InstanceInfo, Service};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::metrics::AppStats;
use policy::Policy;

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
    limit: Arc<Semaphore>,
    stats: Arc<AppStats>,
    /// A native app's sockets.
    #[cfg(unix)]
    native: native::Native,
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
        let granted = info.granted && spec.scope.is_some();
        Instance {
            limit,
            stats: info.metrics.stats(&app),
            #[cfg(unix)]
            native: native::Native::new(
                spec.scope.as_ref().filter(|_| granted).map(Policy::new),
                info.handle,
            ),
        }
    }
}

fn permits(max: u32) -> usize {
    usize::try_from(max)
        .unwrap_or(usize::MAX)
        .min(Semaphore::MAX_PERMITS)
}

impl Instance {}
