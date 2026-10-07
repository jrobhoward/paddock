//! `paddock`: run a Paddock node, build bundles, and manage a node through
//! its admin API.

mod cli;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args as ClapArgs, Parser, Subcommand};
use cli::{bundle, format};
use paddock::api::{Client, RolloutParams};
use serde::Serialize;

/// Run and manage a Paddock node.
#[derive(Debug, Parser)]
#[command(name = "paddock", version)]
struct Cli {
    /// The node's admin socket.
    #[arg(
        long,
        env = "PADDOCK_SOCKET",
        default_value = "/var/lib/paddock/admin.sock",
        global = true
    )]
    socket: PathBuf,
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, ClapArgs)]
struct Rollout {
    /// Approve capabilities the new version adds.
    #[arg(long)]
    approve: bool,
    /// Instances per batch.
    #[arg(long)]
    batch: Option<usize>,
    /// Watch time per batch, in milliseconds.
    #[arg(long)]
    observe_ms: Option<u64>,
    /// Restarts tolerated during the rollout.
    #[arg(long)]
    max_restarts: Option<u64>,
}

impl Rollout {
    fn params(&self) -> RolloutParams {
        RolloutParams {
            approve: self.approve,
            batch: self.batch,
            observe_ms: self.observe_ms,
            max_restarts: self.max_restarts,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a node.
    #[cfg(feature = "node")]
    Node {
        /// Node configuration file (TOML).
        #[arg(short, long, env = "PADDOCK_CONFIG")]
        config: PathBuf,
    },
    /// Build a signed bundle from a component and its paddock.toml.
    Bundle {
        /// The app's paddock.toml.
        manifest: PathBuf,
        /// The component (`.wasm`, built for wasm32-wasip2).
        component: PathBuf,
        /// Minisign secret key (password from PADDOCK_KEY_PASSWORD, or a prompt).
        #[arg(long)]
        key: PathBuf,
        /// Directory of files the app sees read-only at /priv.
        #[arg(long = "priv")]
        priv_dir: Option<PathBuf>,
        /// Output file (default: <name>-<version>.rbp).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Write the bundle even if the component imports undeclared interfaces.
        #[arg(long)]
        skip_import_check: bool,
    },
    /// Create a minisign key pair: <prefix>.key and <prefix>.pub.
    Keygen {
        /// Path prefix for the two files.
        prefix: PathBuf,
        /// Leave the secret key unencrypted.
        #[arg(long)]
        no_password: bool,
    },
    /// Deploy a signed bundle.
    Deploy {
        /// The `.rbp` file.
        bundle: PathBuf,
    },
    /// List deployed apps.
    #[command(alias = "ls")]
    List,
    /// Show one app.
    Status {
        /// App name.
        app: String,
    },
    /// Upgrade an app to a bundle.
    Upgrade {
        /// App name.
        app: String,
        /// The new version's `.rbp` file.
        bundle: PathBuf,
        #[command(flatten)]
        rollout: Rollout,
    },
    /// Return an app to its previous version.
    Rollback {
        /// App name.
        app: String,
        #[command(flatten)]
        rollout: Rollout,
    },
    /// Stop an app (it stays deployed).
    Stop {
        /// App name.
        app: String,
    },
    /// Start a stopped app.
    Start {
        /// App name.
        app: String,
    },
    /// Stop and undeploy an app.
    Remove {
        /// App name.
        app: String,
    },
    /// Kill an instance; its supervisor restarts it.
    Kill {
        /// `node.id.incarnation`.
        pid: String,
    },
    /// List running instances.
    Ps,
    /// Show an instance's state (through its save-snapshot; an instance that
    /// traps there restarts).
    State {
        /// `node.id.incarnation`.
        pid: String,
    },
    /// Set an app's log level (trace, debug, info, warn, error, critical), or
    /// `reset` to its manifest's level.
    LogLevel {
        /// App name.
        app: String,
        /// Level, or `reset`.
        level: String,
    },
    /// Cast to an app.
    Cast {
        /// App name.
        app: String,
        /// The message body.
        body: String,
        /// The message content type.
        #[arg(long, default_value = "text/plain")]
        content_type: String,
    },
    /// Show an app's recent log records.
    Logs {
        /// App name.
        app: String,
        /// How many records.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Call an app and print the reply.
    Call {
        /// App name.
        app: String,
        /// The message body.
        body: String,
        /// The message content type.
        #[arg(long, default_value = "text/plain")]
        content_type: String,
        /// How long to wait for the reply.
        #[arg(long, default_value_t = 5_000)]
        timeout_ms: u64,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    #[cfg(feature = "node")]
    if let Command::Node { config } = &args.command {
        return cli::node::run(config);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(run(&Client::new(&args.socket), args.json, args.command))
}

fn print<T: Serialize>(
    json: bool,
    value: &T,
    text: impl FnOnce(&T) -> String,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", text(value));
    }
    Ok(())
}

async fn read(path: &PathBuf) -> anyhow::Result<Vec<u8>> {
    tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))
}

async fn run(client: &Client, json: bool, command: Command) -> anyhow::Result<()> {
    match command {
        #[cfg(feature = "node")]
        Command::Node { .. } => unreachable!("handled before the client runtime starts"),
        Command::Bundle {
            manifest,
            component,
            key,
            priv_dir,
            out,
            skip_import_check,
        } => {
            let b = bundle::bundle(&bundle::BundleRequest {
                manifest: &manifest,
                component: &component,
                key: &key,
                priv_dir: priv_dir.as_deref(),
                out: out.as_deref(),
                skip_import_check,
            })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": b.path, "app": b.name, "version": b.version,
                        "signer": b.public_key, "imports": b.imports, "priv_files": b.priv_files,
                    }))?
                );
            } else {
                println!(
                    "wrote {} ({} {}, {} priv file(s), signed by {})",
                    b.path.display(),
                    b.name,
                    b.version,
                    b.priv_files,
                    b.public_key
                );
            }
            Ok(())
        }
        Command::Keygen {
            prefix,
            no_password,
        } => {
            let public = bundle::keygen(&prefix, no_password)?;
            println!("{public}");
            Ok(())
        }
        Command::Deploy { bundle } => {
            let r = client.deploy(read(&bundle).await?).await?;
            print(json, &r, |r| {
                let mut out = format!(
                    "deployed {} {} ({})",
                    r.app.name, r.app.version, r.app.status
                );
                for w in &r.warnings {
                    out.push_str(&format!("\nwarning: {w}"));
                }
                out
            })
        }
        Command::List => print(json, &client.list().await?, |a| format::apps(a)),
        Command::Status { app } => print(json, &client.status(&app).await?, format::detail),
        Command::Upgrade {
            app,
            bundle,
            rollout,
        } => {
            let r = client
                .upgrade(&app, read(&bundle).await?, &rollout.params())
                .await?;
            print(json, &r, format::upgrade)
        }
        Command::Rollback { app, rollout } => print(
            json,
            &client.rollback(&app, &rollout.params()).await?,
            format::upgrade,
        ),
        Command::Stop { app } => print(json, &client.stop(&app).await?, |d| d.message.clone()),
        Command::Start { app } => print(json, &client.start(&app).await?, |d| d.message.clone()),
        Command::Remove { app } => print(json, &client.remove(&app).await?, |d| d.message.clone()),
        Command::Kill { pid } => print(json, &client.kill(&pid).await?, |d| d.message.clone()),
        Command::Ps => print(json, &client.ps().await?, |i| format::instances(i)),
        Command::State { pid } => print(json, &client.state(&pid).await?, format::state),
        Command::LogLevel { app, level } => {
            let level = (level != "reset").then_some(level.as_str());
            print(json, &client.set_log_level(&app, level).await?, |d| {
                d.message.clone()
            })
        }
        Command::Cast {
            app,
            body,
            content_type,
        } => print(
            json,
            &client.cast(&app, &content_type, body.into_bytes()).await?,
            |d| d.message.clone(),
        ),
        Command::Logs { app, lines } => {
            print(json, &client.logs(&app, lines).await?, |l| format::logs(l))
        }
        Command::Call {
            app,
            body,
            content_type,
            timeout_ms,
        } => {
            let (_, reply) = client
                .call(&app, &content_type, body.into_bytes(), timeout_ms)
                .await?;
            println!("{}", String::from_utf8_lossy(&reply));
            Ok(())
        }
    }
}
