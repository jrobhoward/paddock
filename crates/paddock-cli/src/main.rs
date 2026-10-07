//! `paddock`: manage a Paddock node through its admin API.

mod format;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args as ClapArgs, Parser, Subcommand};
use paddock_api::{Client, RolloutParams};
use serde::Serialize;

/// Manage a Paddock node.
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = Client::new(&cli.socket);
    run(&client, cli.json, cli.command).await
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
