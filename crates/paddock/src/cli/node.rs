//! `paddock node`: run a Paddock node.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use paddock::node::{LogRing, Node, NodeConfig, RingLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Run a node from its configuration file until SIGTERM or Ctrl-C.
pub(crate) fn run(config: &Path) -> anyhow::Result<()> {
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
        .block_on(serve(config, ring))
}

async fn serve(config: NodeConfig, ring: Arc<LogRing>) -> anyhow::Result<()> {
    let node = Arc::new(Node::open(config, ring).await?);
    paddock::node::serve(node.clone(), shutdown_signal())
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
