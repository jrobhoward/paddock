//! Node identity for the QUIC transport: pinned public keys, as WireGuard
//! does, with no certificate authority.
//!
//! Each node has one Ed25519 key pair ([`generate_key`]). Its private key
//! stays in a file on the node; its public key ([`PublicKey`], written
//! `ed25519:<base64>`) goes into every other node's peers file, against the
//! node's id:
//!
//! ```toml
//! # peers.toml
//! [peers]
//! 1 = ["ed25519:MCowBQYDK2VwAyEA..."]
//! 2 = ["ed25519:MCowBQYDK2VwAyEA...", "ed25519:...new key during a rotation..."]
//! ```
//!
//! The TLS 1.3 handshake carries raw public keys (RFC 7250), not
//! certificates: each side proves it holds its private key, and accepts
//! the other only if the key is in its peers file. Nothing expires. To
//! rotate a node's key, list the new key beside the old at every peer,
//! reload them, switch the node to its new key, then remove the old one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use paddock_bundle::check_key_permissions;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::{default_provider, sign::any_supported_type};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls13_signature_with_raw_key};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{
    CertificateDer, PrivateKeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use serde::Deserialize;

use crate::error::HostError;

/// The prefix of a public key's text form.
const PREFIX: &str = "ed25519:";

/// A node's public key: its DER SubjectPublicKeyInfo.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey(Vec<u8>);

impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PREFIX}{}", STANDARD.encode(&self.0))
    }
}

impl std::str::FromStr for PublicKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let b64 = s
            .strip_prefix(PREFIX)
            .ok_or_else(|| format!("a public key starts with '{PREFIX}'"))?;
        let der = STANDARD
            .decode(b64.trim())
            .map_err(|e| format!("public key: {e}"))?;
        webpki::RawPublicKeyEntity::try_from(&SubjectPublicKeyInfoDer::from(der.as_slice()))
            .map_err(|e| format!("public key: {e}"))?;
        Ok(Self(der))
    }
}

impl PublicKey {
    /// The key a peer presented in the handshake.
    pub(crate) fn from_der(der: &[u8]) -> Self {
        Self(der.to_vec())
    }
}

/// A new Ed25519 key pair: the private key as PKCS#8 PEM, and its public
/// key.
///
/// # Errors
///
/// If the system's random source fails.
pub fn generate_key() -> Result<(String, PublicKey), HostError> {
    let pem = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
        .map_err(|e| HostError::Listen(format!("cluster key: {e}")))?
        .serialize_pem();
    let key = PrivateKeyDer::from_pem_slice(pem.as_bytes()).map_err(identity_error)?;
    let (_, public) = certify(&key)?;
    Ok((pem, public))
}

/// The certified raw key a private key makes, and its public key.
fn certify(key: &PrivateKeyDer<'_>) -> Result<(Arc<CertifiedKey>, PublicKey), HostError> {
    let signer = any_supported_type(key).map_err(identity_error)?;
    let spki = signer
        .public_key()
        .ok_or_else(|| identity_error("the key has no public key"))?
        .as_ref()
        .to_vec();
    let public = PublicKey(spki.clone());
    Ok((
        Arc::new(CertifiedKey::new(vec![CertificateDer::from(spki)], signer)),
        public,
    ))
}

/// The public key of the private key in PEM file `path`.
///
/// # Errors
///
/// [`HostError::Listen`] if the file cannot be read, its group or others
/// may access it, or it holds no usable key.
pub fn public_key_of(path: &Path) -> Result<PublicKey, HostError> {
    Ok(load_key(path)?.1)
}

/// Where a node's identity lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// This node's private key (PKCS#8 PEM).
    pub key: PathBuf,
    /// The peers file: each node's accepted public keys.
    pub peers: PathBuf,
}

/// The identity as loaded: this node's key and every peer's keys.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    pub(crate) certified: Arc<CertifiedKey>,
    pub(crate) peers: Arc<BTreeMap<u32, Vec<PublicKey>>>,
}

fn identity_error(e: impl std::fmt::Display) -> HostError {
    HostError::Listen(format!("cluster identity: {e}"))
}

fn load_key(path: &Path) -> Result<(Arc<CertifiedKey>, PublicKey), HostError> {
    check_key_permissions(path).map_err(identity_error)?;
    let key = PrivateKeyDer::from_pem_file(path)
        .map_err(|e| identity_error(format!("{}: {e}", path.display())))?;
    certify(&key)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeersFile {
    #[serde(default)]
    peers: BTreeMap<String, Vec<String>>,
}

/// Parse a peers file's text.
pub(crate) fn parse_peers(text: &str) -> Result<BTreeMap<u32, Vec<PublicKey>>, String> {
    let file: PeersFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut peers = BTreeMap::new();
    for (node, keys) in file.peers {
        let id: u32 = node
            .parse()
            .map_err(|_| format!("peers.{node}: a node id is a number"))?;
        let keys = keys
            .iter()
            .map(|k| k.parse().map_err(|e| format!("peers.{node}: {e}")))
            .collect::<Result<Vec<PublicKey>, String>>()?;
        peers.insert(id, keys);
    }
    Ok(peers)
}

impl NodeIdentity {
    pub(crate) fn load(&self) -> Result<Loaded, HostError> {
        let (certified, _) = load_key(&self.key)?;
        let text = std::fs::read_to_string(&self.peers)
            .map_err(|e| identity_error(format!("{}: {e}", self.peers.display())))?;
        let peers = parse_peers(&text)
            .map_err(|e| identity_error(format!("{}: {e}", self.peers.display())))?;
        Ok(Loaded {
            certified,
            peers: Arc::new(peers),
        })
    }

    /// The files' modification times, to notice a change.
    pub(crate) fn stamps(&self) -> [Option<std::time::SystemTime>; 2] {
        [&self.key, &self.peers].map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    }
}

impl Loaded {
    /// Whether `key` is one of node `node`'s keys.
    pub(crate) fn names(&self, node: u32, key: &PublicKey) -> bool {
        self.peers.get(&node).is_some_and(|keys| keys.contains(key))
    }

    fn known(&self, der: &[u8]) -> bool {
        let key = PublicKey::from_der(der);
        self.peers.values().any(|keys| keys.contains(&key))
    }
}

/// Accepts a peer's raw public key if the peers file lists it; which node
/// it belongs to is checked against the peer's hello afterwards.
#[derive(Debug)]
pub(crate) struct PinnedKeys {
    loaded: Loaded,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedKeys {
    pub(crate) fn new(loaded: Loaded) -> Self {
        Self {
            loaded,
            algorithms: default_provider().signature_verification_algorithms,
        }
    }

    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.loaded.known(end_entity.as_ref()) {
            Ok(())
        } else {
            Err(rustls::Error::General(
                "the peer's key is not in the peers file".into(),
            ))
        }
    }

    fn signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.algorithms,
        )
    }
}

impl ServerCertVerifier for PinnedKeys {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for PinnedKeys {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod identity_tests;
