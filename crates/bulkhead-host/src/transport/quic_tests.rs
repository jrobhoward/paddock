#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::Path;

use super::*;
use crate::RuntimeConfig;

/// Write node `n`'s private key, readable by its owner only, and return
/// its public key.
fn write_key(dir: &Path, n: u32) -> PublicKey {
    let (private, public) = generate_key().unwrap();
    let path = dir.join(format!("node-{n}.key"));
    std::fs::write(&path, private).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    public
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_seed____peer_completes_the_handshake_but_never_says_hello____gives_up_after_the_idle_timeout()
 {
    let dir = tempfile::tempdir().unwrap();
    let (one, two) = (write_key(dir.path(), 1), write_key(dir.path(), 2));
    let peers = dir.path().join("peers.toml");
    std::fs::write(
        &peers,
        format!("[peers]\n1 = [\"{one}\"]\n2 = [\"{two}\"]\n"),
    )
    .unwrap();
    let idle_timeout = Duration::from_millis(500);
    // Node 2 as a bare QUIC endpoint: it accepts connections and holds
    // them, answering keep-alives, but its application never speaks.
    let silent_identity = NodeIdentity {
        key: dir.path().join("node-2.key"),
        peers: peers.clone(),
    };
    let (server, _) = configs(&silent_identity.load().unwrap(), idle_timeout).unwrap();
    let silent = Endpoint::server(server, "127.0.0.1:0".parse().unwrap()).unwrap();
    let silent_addr = silent.local_addr().unwrap();
    let held = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(incoming) = silent.accept().await {
            if let Ok(conn) = incoming.await {
                held.push(conn);
            }
        }
    });
    let rt = Runtime::new(RuntimeConfig {
        node: 1,
        max_instances: 4,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let quic = QuicTransport::start(
        &rt,
        QuicConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            identity: NodeIdentity {
                key: dir.path().join("node-1.key"),
                peers,
            },
            idle_timeout,
            send_buffer_bytes: SEND_BUFFER_BYTES,
        },
    )
    .unwrap();

    let started = std::time::Instant::now();
    let joined = tokio::time::timeout(Duration::from_secs(10), quic.join_seed(silent_addr)).await;

    let error = joined.expect("the dial never returned").unwrap_err();
    assert!(error.to_string().contains("no hello"), "{error}");
    assert!(
        started.elapsed() < idle_timeout * 4,
        "{:?}",
        started.elapsed()
    );
    quic.shutdown();
    held.abort();
}
