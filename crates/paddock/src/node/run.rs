//! Running a node as a process: what `paddock node` does, and what a node
//! binary built with extra host services calls.
//!
//! ```no_run
//! # fn greeter() -> std::sync::Arc<dyn paddock_host::services::Extension> { unimplemented!() }
//! fn main() -> anyhow::Result<()> {
//!     let config = std::path::PathBuf::from("node.toml");
//!     paddock::node::run(&config, vec![greeter()])
//! }
//! ```

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use paddock_host::services::Extension;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use super::{LogRing, Node, NodeConfig, RingLayer};

/// The extensions `paddock node` runs with: the Modbus TCP extension with
/// the `modbus` feature (on by default), none otherwise.
#[must_use]
pub fn builtin_extensions() -> Vec<Arc<dyn Extension>> {
    #[cfg(feature = "modbus")]
    {
        vec![Arc::new(paddock_modbus::Modbus::default())]
    }
    #[cfg(not(feature = "modbus"))]
    {
        Vec::new()
    }
}

/// Run a node from its configuration file, with `extensions` (none for a
/// plain node), until SIGTERM or Ctrl-C. Logs go to stderr, filtered by
/// `RUST_LOG` (default `info`).
///
/// # Errors
///
/// A bad configuration, a node that cannot open (see [`Node::open_with`]),
/// or an admin socket that cannot be served.
pub fn run(config: &Path, extensions: Vec<Arc<dyn Extension>>) -> anyhow::Result<()> {
    let config = NodeConfig::from_file(config)?;
    let ring = LogRing::new(10_000);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(RingLayer::new(ring.clone()))
        .init();
    // Multi-threaded: instances must never run on a current-thread runtime.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(serve(config, ring, extensions))
}

async fn serve(
    config: NodeConfig,
    ring: Arc<LogRing>,
    extensions: Vec<Arc<dyn Extension>>,
) -> anyhow::Result<()> {
    let node = Arc::new(Node::open_with(config, ring, extensions).await?);
    super::serve(node.clone(), shutdown_signal())
        .await
        .context("admin API")?;
    tracing::info!("shutting down");
    node.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
