//! A native instance's process: `paddock-runner`, its pipes and its
//! limits.
//!
//! Frames to the app go through a writer task, so a stalled app never
//! blocks the instance's task; frames from it come through a reader task.
//! stderr becomes log lines, as a component's does. Between fork and exec
//! the child gets the parent-death signal (`SIGKILL`), no core dumps, and
//! its memory limit: the cgroup it starts in, or `RLIMIT_DATA` without
//! one. It runs in a process group of its own, with an empty environment.

use std::ffi::CString;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use paddock_bundle::manifest::NativeProgram;
use paddock_native::protocol::{self, FromApp, ToApp};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::{NativeCode, cgroup};
use crate::config::NativeConfig;
use crate::message::ExitReason;
use crate::output::CapturedOutput;
use crate::spec::InstanceSpec;

/// How long a process that closed its stdout has to exit before it is
/// killed.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// Frames read ahead of the instance's task.
const INCOMING: usize = 256;

/// Something from the app.
#[derive(Debug)]
pub(crate) enum Incoming {
    Frame(FromApp),
    /// A frame that could not be read; the app is broken.
    Broken(String),
}

/// A running native instance's process.
#[derive(Debug)]
pub(crate) struct Port {
    child: Child,
    frames: Option<mpsc::UnboundedSender<Vec<u8>>>,
    incoming: mpsc::Receiver<Incoming>,
    cgroup: Option<PathBuf>,
}

/// The runner the node starts native apps through.
fn runner(config: &NativeConfig) -> PathBuf {
    config.runner.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join("paddock-runner")))
            .unwrap_or_else(|| PathBuf::from("paddock-runner"))
    })
}

impl Port {
    /// Start `program` for the instance `name` (its pid, as text).
    ///
    /// # Errors
    ///
    /// Why the process could not start: no runner, or a cgroup that could
    /// not be made.
    pub(crate) fn spawn(
        config: &NativeConfig,
        program: &NativeCode,
        spec: &InstanceSpec,
        name: &str,
        output: CapturedOutput,
    ) -> Result<Self, String> {
        let runner = runner(config);
        let mut cmd = Command::new(&runner);
        cmd.arg(match program.kind {
            NativeProgram::Library => "--library",
            NativeProgram::Executable => "--executable",
        })
        .arg(&program.path);
        for p in &spec.preopens {
            cmd.arg("--read").arg(&p.host);
        }
        if config.landlock_optional {
            cmd.args(["--landlock", "best-effort"]);
        }
        cmd.env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let cgroup = config
            .cgroup
            .as_ref()
            .map(|root| cgroup::create(root, name, spec.memory_max, config.pids_max))
            .transpose()
            .map_err(|e| format!("cgroup: {e}"))?;
        let procs = cgroup
            .as_ref()
            .map(|dir| {
                CString::new(
                    dir.join("cgroup.procs")
                        .into_os_string()
                        .into_encoded_bytes(),
                )
            })
            .transpose()
            .map_err(|e| format!("cgroup: {e}"))?;
        let data_limit = cgroup.is_none().then_some(spec.memory_max);
        // SAFETY: between fork and exec the closure only makes system calls
        // through rustix, which neither allocate nor take locks; `procs` was
        // made before the fork.
        unsafe {
            cmd.pre_exec(move || {
                use rustix::process::{
                    Resource, Rlimit, Signal, set_parent_process_death_signal, setrlimit,
                };
                set_parent_process_death_signal(Some(Signal::KILL))?;
                let none = Rlimit {
                    current: Some(0),
                    maximum: Some(0),
                };
                setrlimit(Resource::Core, none)?;
                if let Some(bytes) = data_limit {
                    setrlimit(
                        Resource::Data,
                        Rlimit {
                            current: Some(bytes),
                            maximum: Some(bytes),
                        },
                    )?;
                }
                if let Some(procs) = &procs {
                    let fd = rustix::fs::open(
                        procs.as_c_str(),
                        rustix::fs::OFlags::WRONLY,
                        rustix::fs::Mode::empty(),
                    )?;
                    rustix::io::write(&fd, b"0")?;
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("{}: {e}", runner.display()))?;
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err("pipes not created".into());
        };
        let (frames, mut out) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(frame) = out.recv().await {
                if stdin.write_all(&frame).await.is_err() {
                    return;
                }
            }
        });
        let (tx, incoming) = mpsc::channel(INCOMING);
        tokio::spawn(async move {
            loop {
                let mut prefix = [0u8; 4];
                if stdout.read_exact(&mut prefix).await.is_err() {
                    return;
                }
                let next = match protocol::frame_len(prefix) {
                    Ok(len) => {
                        let mut body = vec![0; len];
                        match stdout.read_exact(&mut body).await {
                            Ok(_) => protocol::decode(&body)
                                .map(Incoming::Frame)
                                .unwrap_or_else(|e| Incoming::Broken(e.to_string())),
                            Err(_) => return,
                        }
                    }
                    Err(e) => Incoming::Broken(e.to_string()),
                };
                let broken = matches!(next, Incoming::Broken(_));
                if tx.send(next).await.is_err() || broken {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            use wasmtime_wasi::cli::StdoutStream;
            let mut lines = std::pin::Pin::from(output.async_stream());
            let _ = tokio::io::copy(&mut stderr, &mut lines).await;
            let _ = lines.shutdown().await;
        });
        Ok(Self {
            child,
            frames: Some(frames),
            incoming,
            cgroup,
        })
    }

    /// Queue a frame for the app; dropped once stdin is closed.
    pub(crate) fn send(&self, frame: &ToApp) {
        if let Some(frames) = &self.frames {
            let _ = frames.send(protocol::encode(frame));
        }
    }

    /// The next thing from the app; `None` once its stdout has closed.
    pub(crate) async fn recv(&mut self) -> Option<Incoming> {
        self.incoming.recv().await
    }

    /// Close the app's stdin: it exits once it has read what is queued.
    pub(crate) fn close(&mut self) {
        self.frames = None;
    }

    /// Kill the process now.
    pub(crate) fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Why the process ended, once it has: waited for within a grace
    /// period, then killed. Over its cgroup's `memory.max` is
    /// `memory_limit`.
    pub(crate) async fn exit_reason(&mut self) -> ExitReason {
        let status = match tokio::time::timeout(EXIT_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => {
                self.kill();
                self.child.wait().await.ok()
            }
        };
        if self.cgroup.as_deref().is_some_and(cgroup::oom_killed) {
            return ExitReason::Crashed("memory_limit".into());
        }
        ExitReason::Crashed(match status {
            Some(s) => match (s.code(), s.signal()) {
                (Some(code), _) => format!("exited with status {code}"),
                (_, Some(signal)) => format!("killed by signal {signal}"),
                _ => "exited".into(),
            },
            None => "exited".into(),
        })
    }

    /// Wait for the process to exit after a stop (closing stdin, then
    /// killing it after the grace period), and remove its cgroup.
    pub(crate) async fn finish(mut self) {
        self.close();
        if tokio::time::timeout(EXIT_GRACE, self.child.wait())
            .await
            .is_err()
        {
            self.kill();
            let _ = self.child.wait().await;
        }
        if let Some(dir) = &self.cgroup {
            cgroup::remove(dir).await;
        }
    }
}
