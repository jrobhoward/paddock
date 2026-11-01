//! A native instance's process: `bulkhead-runner`, its pipes and its
//! limits.
//!
//! The instance's task writes frames to the app's stdin itself while the
//! pipe has room, and queues them for a writer task when it has not (or
//! while earlier frames still wait there), so a stalled app never blocks
//! the instance's task. It also reads and decodes the app's frames itself,
//! through a buffer, so a call costs no hand-off between tasks on the
//! node's side (Z1, spike S30). stderr becomes log lines, as a
//! component's does. Between fork and exec
//! the child gets the parent-death signal (`SIGKILL`), no core dumps, and
//! its memory limit: the cgroup it starts in, or `RLIMIT_DATA` without
//! one. It runs in a process group of its own, with an environment of
//! `BULKHEAD_PRIV` alone (the bundle's `priv/` files, when it has any).
//!
//! Native apps run on Linux only (`bulkhead-runner` is Linux only); elsewhere
//! [`Port::spawn`] refuses, and what only it uses is dead code.

#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_native::protocol::{self, FromApp, ToApp};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;

use super::{NativeCode, cgroup};
use crate::config::NativeConfig;
use crate::message::ExitReason;
use crate::output::CapturedOutput;
use crate::spec::InstanceSpec;

/// How long a process that closed its stdout has to exit before it is
/// killed.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// Bytes read from the app at a time.
const READ_CHUNK: usize = 16 << 10;

/// Something from the app.
#[derive(Debug)]
pub(crate) enum Incoming {
    Frame(FromApp),
    /// A frame that could not be read; the app is broken.
    Broken(String),
}

/// The app's stdin: written directly while nothing waits in the queue.
#[derive(Debug)]
struct Stdin {
    pipe: Arc<pipe::Sender>,
    /// Frames for the writer task, written in order once the pipe has room.
    queue: mpsc::UnboundedSender<Vec<u8>>,
    /// Frames queued and not yet written.
    queued: Arc<AtomicUsize>,
}

/// The app's stdout, read by the instance's task.
#[derive(Debug)]
struct Stdout {
    pipe: ChildStdout,
    /// Bytes read and not yet decoded, from `start`.
    buf: Vec<u8>,
    start: usize,
    /// No more frames: the pipe closed, or a frame was broken.
    done: bool,
}

/// A running native instance's process.
#[derive(Debug)]
pub(crate) struct Port {
    child: Child,
    stdin: Option<Stdin>,
    stdout: Stdout,
    cgroup: Option<PathBuf>,
}

/// The runner the node starts native apps through.
fn runner(config: &NativeConfig) -> PathBuf {
    config.runner.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join("bulkhead-runner")))
            .unwrap_or_else(|| PathBuf::from("bulkhead-runner"))
    })
}

impl Port {
    /// Start `program` for the instance `name` (its pid, as text).
    ///
    /// # Errors
    ///
    /// Why the process could not start: no runner, or a cgroup that could
    /// not be made.
    #[cfg(target_os = "linux")]
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
        cmd.env_clear();
        if let Some(p) = spec
            .preopens
            .iter()
            .find(|p| p.guest == crate::spec::PRIV_PATH)
        {
            cmd.env(protocol::PRIV_ENV, &p.host);
        }
        cmd.stdin(Stdio::piped())
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
        let (Some(stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err("pipes not created".into());
        };
        let pipe = stdin
            .into_owned_fd()
            .and_then(pipe::Sender::from_owned_fd)
            .map(Arc::new)
            .map_err(|e| format!("stdin: {e}"))?;
        let (queue, mut queued_frames) = mpsc::unbounded_channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let (writer, left) = (pipe.clone(), queued.clone());
        tokio::spawn(async move {
            while let Some(frame) = queued_frames.recv().await {
                if write_all(&writer, &frame).await.is_err() {
                    return;
                }
                left.fetch_sub(1, Ordering::AcqRel);
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
            stdin: Some(Stdin {
                pipe,
                queue,
                queued,
            }),
            stdout: Stdout {
                pipe: stdout,
                buf: Vec::with_capacity(READ_CHUNK),
                start: 0,
                done: false,
            },
            cgroup,
        })
    }

    /// Native apps run on Linux only.
    ///
    /// # Errors
    ///
    /// Always.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn spawn(
        _config: &NativeConfig,
        _program: &NativeCode,
        _spec: &InstanceSpec,
        _name: &str,
        _output: CapturedOutput,
    ) -> Result<Self, String> {
        Err("native apps run on Linux only".into())
    }

    /// Send a frame to the app: written now if the pipe has room and
    /// nothing is queued before it, else queued for the writer task.
    /// Dropped once stdin is closed.
    pub(crate) fn send(&self, frame: &ToApp) {
        let Some(stdin) = &self.stdin else {
            return;
        };
        let bytes = protocol::encode(frame);
        let mut written = 0;
        if stdin.queued.load(Ordering::Acquire) == 0 {
            match stdin.pipe.try_write(&bytes) {
                Ok(n) if n == bytes.len() => return,
                Ok(n) => written = n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                // The app has gone; its exit is read from stdout.
                Err(_) => return,
            }
        }
        stdin.queued.fetch_add(1, Ordering::AcqRel);
        let _ = stdin.queue.send(bytes[written..].to_vec());
    }

    /// The next thing from the app; `None` once its stdout has closed.
    /// Cancel safe: bytes read stay buffered for the next call.
    pub(crate) async fn recv(&mut self) -> Option<Incoming> {
        let out = &mut self.stdout;
        loop {
            if out.done {
                return None;
            }
            if let Some(next) = out.take() {
                return Some(next);
            }
            if out.start > 0 {
                out.buf.drain(..out.start);
                out.start = 0;
            }
            out.buf.reserve(READ_CHUNK);
            match out.pipe.read_buf(&mut out.buf).await {
                Ok(0) | Err(_) => out.done = true,
                Ok(_) => {}
            }
        }
    }

    /// Close the app's stdin: it exits once it has read what is queued.
    pub(crate) fn close(&mut self) {
        self.stdin = None;
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
        #[cfg(unix)]
        let signal = |s: &std::process::ExitStatus| s.signal();
        #[cfg(not(unix))]
        let signal = |_: &std::process::ExitStatus| None::<i32>;
        ExitReason::Crashed(match status {
            Some(s) => match (s.code(), signal(&s)) {
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

impl Stdout {
    /// A whole frame from the buffer, if one is there.
    fn take(&mut self) -> Option<Incoming> {
        let have = &self.buf[self.start..];
        let prefix: [u8; 4] = have.get(..4)?.try_into().ok()?;
        let len = match protocol::frame_len(prefix) {
            Ok(len) => len,
            Err(e) => {
                self.done = true;
                return Some(Incoming::Broken(e.to_string()));
            }
        };
        let body = have.get(4..4 + len)?;
        let next = protocol::decode(body)
            .map(Incoming::Frame)
            .unwrap_or_else(|e| Incoming::Broken(e.to_string()));
        self.start += 4 + len;
        if matches!(next, Incoming::Broken(_)) {
            self.done = true;
        }
        Some(next)
    }
}

/// Write all of `bytes` to the app's stdin, waiting for room.
async fn write_all(pipe: &pipe::Sender, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        pipe.writable().await?;
        match pipe.try_write(bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
