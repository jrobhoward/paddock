//! `paddock`: run a Paddock node, build bundles, and manage a node through
//! its admin API.

mod cli;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args as ClapArgs, Parser, Subcommand};
use cli::{bundle, format};
use paddock::api::{Client, RolloutParams, Target};
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
    /// Move instances by restart, whatever the manifest's upgrade mode: each
    /// starts the other version from init, and in-memory state is lost.
    #[arg(long)]
    restart: bool,
}

impl Rollout {
    fn params(&self) -> RolloutParams {
        RolloutParams {
            approve: self.approve,
            batch: self.batch,
            observe_ms: self.observe_ms,
            max_restarts: self.max_restarts,
            restart: self.restart,
        }
    }
}

/// `paddock cluster` subcommands.
#[cfg(feature = "node")]
#[derive(Debug, Subcommand)]
enum ClusterCommand {
    /// Make this node's key: write the private key to PATH and print the
    /// public key, for every other node's peers file.
    Keygen {
        /// Where to write the private key (must not exist).
        path: PathBuf,
    },
    /// Print the public key of the private key at PATH.
    Pubkey {
        /// The private key.
        path: PathBuf,
    },
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
    /// A node's key for the cluster transport.
    #[cfg(feature = "node")]
    Cluster {
        #[command(subcommand)]
        action: ClusterCommand,
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
    /// Print the node's metrics (Prometheus text).
    Metrics,
    /// Reload the HTTPS ingress certificates and the cluster key and peers
    /// files.
    ReloadTls,
    /// List the cluster's members as the node knows them.
    Members,
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
    /// Cast to an app, or to one instance by pid or registered name.
    Cast {
        #[command(flatten)]
        message: Message,
    },
    /// Show an app's recent log records.
    Logs {
        /// App name.
        app: String,
        /// How many records.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Call an app, or one instance by pid or registered name, and print
    /// the reply.
    Call {
        #[command(flatten)]
        message: Message,
        /// How long to wait for the reply.
        #[arg(long, default_value_t = 5_000)]
        timeout_ms: u64,
    },
}

/// A message for `call` and `cast`, and where it goes.
#[derive(Debug, ClapArgs)]
struct Message {
    /// The app and the message body; only the body with --pid or --name.
    #[arg(value_name = "[APP] BODY", num_args = 1..=2, required = true)]
    args: Vec<String>,
    /// Send to one instance by pid (`node.serial.incarnation@creation`, as `ps` shows it) instead of an app.
    #[arg(long, conflicts_with = "name")]
    pid: Option<String>,
    /// Send to the instance holding this registered name.
    #[arg(long)]
    name: Option<String>,
    /// The message content type.
    #[arg(long, default_value = "text/plain")]
    content_type: String,
    /// Read BODY as JSON and send it as CBOR (`application/cbor`); a CBOR
    /// reply prints as JSON.
    #[arg(long, conflicts_with = "content_type")]
    cbor: bool,
}

impl Message {
    /// The target, content type and body bytes.
    fn into_parts(self) -> anyhow::Result<(Target, String, Vec<u8>)> {
        let (target, body) = match (self.pid, self.name, self.args.as_slice()) {
            (Some(pid), None, [body]) => (Target::Pid(pid), body.clone()),
            (None, Some(name), [body]) => (Target::Name(name), body.clone()),
            (None, None, [app, body]) => (Target::App(app.clone()), body.clone()),
            (None, None, _) => anyhow::bail!("give an app and a body, or --pid/--name and a body"),
            _ => anyhow::bail!("with --pid or --name, give only the body"),
        };
        if !self.cbor {
            return Ok((target, self.content_type, body.into_bytes()));
        }
        let bytes = format::cbor_from_json(&body).map_err(|e| anyhow::anyhow!("--cbor: {e}"))?;
        Ok((target, "application/cbor".into(), bytes))
    }
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    #[cfg(feature = "node")]
    if let Command::Node { config } = &args.command {
        return paddock::node::run(config, paddock::node::builtin_extensions());
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

/// `paddock bundle`: build, sign and report a bundle.
fn make_bundle(json: bool, request: &bundle::BundleRequest<'_>) -> anyhow::Result<()> {
    let b = bundle::bundle(request)?;
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

/// Print an admin action's outcome.
fn done(
    json: bool,
    result: Result<paddock::api::Done, paddock::api::ClientError>,
) -> anyhow::Result<()> {
    print(json, &result?, |d| d.message.clone())
}

/// `paddock cluster ...`.
#[cfg(feature = "node")]
fn cluster_command(action: ClusterCommand) -> anyhow::Result<()> {
    let public = match action {
        ClusterCommand::Keygen { path } => cli::cluster::keygen(&path)?,
        ClusterCommand::Pubkey { path } => cli::cluster::pubkey(&path)?,
    };
    println!("{public}");
    Ok(())
}

/// `paddock metrics`: the Prometheus text as the node renders it.
async fn print_metrics(client: &Client) -> anyhow::Result<()> {
    print!("{}", client.metrics().await?);
    Ok(())
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
        } => make_bundle(
            json,
            &bundle::BundleRequest {
                manifest: &manifest,
                component: &component,
                key: &key,
                priv_dir: priv_dir.as_deref(),
                out: out.as_deref(),
                skip_import_check,
            },
        ),
        Command::Keygen {
            prefix,
            no_password,
        } => {
            let public = bundle::keygen(&prefix, no_password)?;
            println!("{public}");
            Ok(())
        }
        #[cfg(feature = "node")]
        Command::Cluster { action } => cluster_command(action),
        Command::Deploy { .. }
        | Command::List
        | Command::Status { .. }
        | Command::Upgrade { .. }
        | Command::Rollback { .. }
        | Command::Stop { .. }
        | Command::Start { .. }
        | Command::Remove { .. }
        | Command::LogLevel { .. }
        | Command::Logs { .. } => run_apps(client, json, command).await,
        _ => run_runtime(client, json, command).await,
    }
}

/// The commands that manage apps.
async fn run_apps(client: &Client, json: bool, command: Command) -> anyhow::Result<()> {
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
        Command::Stop { app } => done(json, client.stop(&app).await),
        Command::Start { app } => done(json, client.start(&app).await),
        Command::Remove { app } => done(json, client.remove(&app).await),
        Command::LogLevel { app, level } => {
            let level = (level != "reset").then_some(level.as_str());
            done(json, client.set_log_level(&app, level).await)
        }
        Command::Logs { app, lines } => {
            print(json, &client.logs(&app, lines).await?, |l| format::logs(l))
        }
        _ => anyhow::bail!("not an app command"),
    }
}

/// The commands on instances, messages and the node.
async fn run_runtime(client: &Client, json: bool, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Kill { pid } => done(json, client.kill(&pid).await),
        Command::Ps => print(json, &client.ps().await?, |i| format::instances(i)),
        Command::Metrics => print_metrics(client).await,
        Command::ReloadTls => done(json, client.reload_tls().await),
        Command::Members => print(json, &client.members().await?, |m| format::members(m)),
        Command::State { pid } => print(json, &client.state(&pid).await?, format::state),
        Command::Cast { message } => {
            let (target, content_type, body) = message.into_parts()?;
            done(json, client.cast_to(&target, &content_type, body).await)
        }
        Command::Call {
            message,
            timeout_ms,
        } => {
            let (target, content_type, body) = message.into_parts()?;
            let (reply_type, reply) = client
                .call_to(&target, &content_type, body, timeout_ms)
                .await?;
            println!("{}", format::reply(&reply_type, &reply));
            Ok(())
        }
        _ => anyhow::bail!("not a runtime command"),
    }
}
