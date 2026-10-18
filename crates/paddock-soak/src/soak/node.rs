//! A `paddock node` process: started, killed, stopped and continued, with
//! each signal recorded in the event log.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};

use super::events::{EventLog, What};
use super::time::now_us;

/// How long an admin call may take before the node counts as unresponsive.
pub(crate) const ADMIN_TIMEOUT: Duration = Duration::from_secs(5);

/// One node's process and files.
#[derive(Debug)]
pub(crate) struct Node {
    pub n: u32,
    paddock: PathBuf,
    config: PathBuf,
    log: PathBuf,
    socket: PathBuf,
    child: Option<Child>,
    frozen: bool,
    events: Arc<EventLog>,
}

impl Node {
    /// A node run from `dir/node-<n>.toml`, logging to `dir/node-<n>.log`,
    /// with its admin socket at `socket`.
    pub(crate) fn new(
        n: u32,
        paddock: &Path,
        dir: &Path,
        socket: PathBuf,
        events: Arc<EventLog>,
    ) -> Self {
        Self {
            n,
            paddock: paddock.to_path_buf(),
            config: dir.join(format!("node-{n}.toml")),
            log: dir.join(format!("node-{n}.log")),
            socket,
            child: None,
            frozen: false,
            events,
        }
    }

    pub(crate) fn config_path(&self) -> &Path {
        &self.config
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn client(&self) -> paddock::api::Client {
        paddock::api::Client::new(&self.socket)
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Start the process and wait for its admin API to answer.
    pub(crate) async fn spawn(&mut self) -> anyhow::Result<()> {
        let _ = std::fs::remove_file(&self.socket);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)?;
        let child = Command::new(&self.paddock)
            .arg("node")
            .arg("--config")
            .arg(&self.config)
            .env("NO_COLOR", "1")
            // `PADDOCK_SOAK_LOG` overrides the nodes' filter (it must keep
            // `paddock::lease=debug`, which the checker reads).
            .env(
                "RUST_LOG",
                std::env::var("PADDOCK_SOAK_LOG")
                    .unwrap_or_else(|_| "info,paddock::lease=debug".into()),
            )
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("starting {}", self.paddock.display()))?;
        self.child = Some(child);
        self.frozen = false;
        self.events.record(self.n, What::Spawn);
        for _ in 0..400 {
            if self.socket.exists()
                && tokio::time::timeout(ADMIN_TIMEOUT, self.client().list())
                    .await
                    .is_ok_and(|r| r.is_ok())
            {
                return Ok(());
            }
            if !self.alive() {
                bail!(
                    "node {} exited while starting; see {}",
                    self.n,
                    self.log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!(
            "node {} did not answer within 20 s; see {}",
            self.n,
            self.log.display()
        )
    }

    /// Whether the process is running; records an exit found here.
    pub(crate) fn alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            _ => {
                self.child = None;
                self.frozen = false;
                self.events.record(self.n, What::Exited);
                false
            }
        }
    }

    /// `SIGKILL`, and wait until the process is gone.
    pub(crate) fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            self.events.record(self.n, What::Kill);
        }
        self.frozen = false;
    }

    /// `SIGSTOP`, and wait until the process shows as stopped.
    pub(crate) async fn freeze(&mut self) -> anyhow::Result<()> {
        let Some(pid) = self.pid() else {
            return Ok(());
        };
        signal("STOP", pid)?;
        self.frozen = true;
        for _ in 0..100 {
            match stopped(pid) {
                Some(true) => break,
                Some(false) => tokio::time::sleep(Duration::from_millis(2)).await,
                None => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    break;
                }
            }
        }
        self.events.record(self.n, What::Freeze);
        Ok(())
    }

    /// `SIGCONT`, recorded just before it is sent.
    pub(crate) fn thaw(&mut self) -> anyhow::Result<()> {
        let Some(pid) = self.pid() else {
            return Ok(());
        };
        if !self.frozen {
            return Ok(());
        }
        self.events.record_at(now_us(), self.n, What::Thaw);
        signal("CONT", pid)?;
        self.frozen = false;
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// `kill -<name> <pid>`: no `unsafe` for a signal the soak sends rarely.
fn signal(name: &str, pid: u32) -> anyhow::Result<()> {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
        .context("running kill")?;
    if !status.success() {
        bail!("kill -{name} {pid} failed");
    }
    Ok(())
}

/// Whether `pid` is stopped, from `/proc` (Linux); `None` where there is no
/// `/proc`, in which case the caller's short wait stands in.
fn stopped(pid: u32) -> Option<bool> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The state follows the command name, which is in parentheses.
    let state = stat.rsplit_once(')')?.1.split_whitespace().next()?;
    Some(state == "T" || state == "t")
}

/// A free UDP port on loopback. Another process could take it before the
/// node binds it; on loopback in a soak run that is rare enough.
pub(crate) fn free_udp() -> anyhow::Result<std::net::SocketAddr> {
    Ok(std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?)
}

/// The paddock binary's output for `args`, run in `dir`.
pub(crate) fn paddock(paddock: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(paddock)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running {}", paddock.display()))?;
    if !out.status.success() {
        bail!(
            "paddock {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A short admin socket path: Unix socket paths are limited to about 107
/// bytes, and run directories can be deep.
pub(crate) fn socket_path(tag: &str, n: u32) -> PathBuf {
    std::env::temp_dir().join(format!("pdsoak-{}-{tag}{n}.sock", std::process::id()))
}

/// Sign `manifest` and `wasm` into `dir/<name>.rbp` with a new signer key;
/// returns the bundle and the signer's public key for `trusted_keys`.
pub(crate) fn bundle(
    paddock_bin: &Path,
    dir: &Path,
    name: &str,
    manifest: &str,
    wasm: &Path,
) -> anyhow::Result<(Vec<u8>, String)> {
    if !dir.join("signer.key").exists() {
        paddock(paddock_bin, dir, &["keygen", "signer", "--no-password"])?;
    }
    let public = std::fs::read_to_string(dir.join("signer.pub"))?
        .lines()
        .last()
        .unwrap_or_default()
        .to_string();
    std::fs::write(dir.join(format!("{name}.toml")), manifest)?;
    let wasm = wasm
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("not UTF-8: {}", wasm.display()))?;
    paddock(
        paddock_bin,
        dir,
        &[
            "bundle",
            &format!("{name}.toml"),
            wasm,
            "--key",
            "signer.key",
            "--out",
            &format!("{name}.rbp"),
        ],
    )?;
    Ok((std::fs::read(dir.join(format!("{name}.rbp")))?, public))
}
