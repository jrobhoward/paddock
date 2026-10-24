//! TLS for the HTTP ingress: rustls on ring, certificates
//! from PEM files, chosen by the client's SNI host name, and reloaded from
//! their files without a restart.
//!
//! Each certificate lists the hosts it serves; a host matches exactly or
//! through one leading wildcard label (`*.example.com` matches
//! `a.example.com`). The first certificate also answers clients that send
//! no SNI or a name no certificate lists. A reload reads every file again
//! and swaps the set in one step; if any file fails, the old set stays and
//! the error is returned.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::SystemTime;

use rustls::ServerConfig;
use rustls::crypto::ring::{default_provider, sign::any_supported_type};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::error::HostError;

/// One certificate: its chain and key files, and the hosts it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsCertificate {
    /// PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// PEM private key (PKCS#8, PKCS#1 or SEC1).
    pub key: PathBuf,
    /// Host names it serves (`shop.example`, `*.example.com`); empty for
    /// the default only.
    pub hosts: Vec<String>,
}

/// The certificates as last loaded.
#[derive(Debug)]
struct Loaded {
    default: Arc<CertifiedKey>,
    by_host: HashMap<String, Arc<CertifiedKey>>,
}

/// An ingress address's certificates. Cheap to clone.
#[derive(Debug, Clone)]
pub struct IngressTls {
    inner: Arc<Inner>,
}

struct Inner {
    entries: Vec<TlsCertificate>,
    /// Shared with the resolver rustls asks during each handshake.
    current: Arc<RwLock<Arc<Loaded>>>,
    /// The files' modification times at the last load.
    stamps: Mutex<Vec<Option<SystemTime>>>,
    acceptor: TlsAcceptor,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngressTls")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Resolver(Arc<RwLock<Arc<Loaded>>>);

fn load_key(entry: &TlsCertificate) -> Result<Arc<CertifiedKey>, String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&entry.cert)
        .map_err(|e| format!("{}: {e}", entry.cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", entry.cert.display()))?;
    if chain.is_empty() {
        return Err(format!("{}: no certificate", entry.cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(&entry.key)
        .map_err(|e| format!("{}: {e}", entry.key.display()))?;
    let signer = any_supported_type(&key).map_err(|e| format!("{}: {e}", entry.key.display()))?;
    let certified = CertifiedKey::new(chain, signer);
    certified
        .keys_match()
        .map_err(|e| format!("{} and {}: {e}", entry.cert.display(), entry.key.display()))?;
    Ok(Arc::new(certified))
}

fn load(entries: &[TlsCertificate]) -> Result<Loaded, HostError> {
    let tls = |e: String| HostError::Listen(format!("TLS: {e}"));
    let mut by_host = HashMap::new();
    let mut default = None;
    for entry in entries {
        let key = load_key(entry).map_err(tls)?;
        for host in &entry.hosts {
            by_host.insert(host.to_ascii_lowercase(), key.clone());
        }
        default.get_or_insert(key);
    }
    let default = default.ok_or_else(|| tls("no certificates".into()))?;
    Ok(Loaded { default, by_host })
}

fn stamps(entries: &[TlsCertificate]) -> Vec<Option<SystemTime>> {
    entries
        .iter()
        .flat_map(|e| [&e.cert, &e.key])
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

impl Loaded {
    fn pick(&self, sni: Option<&str>) -> Arc<CertifiedKey> {
        let Some(name) = sni.map(str::to_ascii_lowercase) else {
            return self.default.clone();
        };
        if let Some(k) = self.by_host.get(&name) {
            return k.clone();
        }
        name.split_once('.')
            .and_then(|(_, parent)| self.by_host.get(&format!("*.{parent}")))
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let loaded = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Some(loaded.pick(hello.server_name()))
    }
}

impl IngressTls {
    /// Load `entries` (the first is the default).
    ///
    /// # Errors
    ///
    /// [`HostError::Listen`] for no entries, or a file that cannot be read
    /// or parsed, or a key that does not match its certificate.
    pub fn load(entries: Vec<TlsCertificate>) -> Result<Self, HostError> {
        let current = Arc::new(RwLock::new(Arc::new(load(&entries)?)));
        let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| HostError::Listen(format!("TLS: {e}")))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver(current.clone())));
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            inner: Arc::new(Inner {
                stamps: Mutex::new(stamps(&entries)),
                entries,
                current,
                acceptor: TlsAcceptor::from(Arc::new(config)),
            }),
        })
    }

    /// Read every file again and serve the new set; on any error keep the
    /// old one.
    ///
    /// # Errors
    ///
    /// As for [`IngressTls::load`].
    pub fn reload(&self) -> Result<(), HostError> {
        let loaded = load(&self.inner.entries)?;
        *self
            .inner
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(loaded);
        *self
            .inner
            .stamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = stamps(&self.inner.entries);
        Ok(())
    }

    /// Reload if any file's modification time changed since the last
    /// load. Returns whether it reloaded.
    ///
    /// # Errors
    ///
    /// As for [`IngressTls::reload`]; a failed reload is tried again next
    /// time.
    pub fn reload_if_changed(&self) -> Result<bool, HostError> {
        let now = stamps(&self.inner.entries);
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

    pub(crate) fn acceptor(&self) -> &TlsAcceptor {
        &self.inner.acceptor
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tls_tests;
