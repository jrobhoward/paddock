//! The chaos fixture as an in-process test app (ROADMAP Z9c): the
//! commands and replies of `bulkhead-fixture-chaos`, whose crate docs list
//! them, for tests and the simulation.
//!
//! `spin` (a call, `init`, or `terminate` with `terminate-spin`) waits
//! forever without finishing its callback: as for a native app, whose
//! deadline is wall time, the node kills it at the deadline or on a kill;
//! unlike a process, it yields while it waits, so it holds no worker.
//! `sleep:<ms>` waits on a timer, which counts against the deadline as a
//! native app's wait does. `alloc` needs real memory and answers `needs a
//! process`. `trap` ends the callback with an error, which the instance sees
//! as a native app's panic (`crashed: panic: asked to trap`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{CallError, Envelope, ExitReason, Info, Pid, RegisterError, TestApp, TestCtx};

/// Ends the deferred callers in a snapshot; the ballast follows it.
const BALLAST_MARK: u64 = u64::MAX;

/// The chaos app.
#[derive(Debug, Default)]
pub struct Chaos {
    count: u64,
    terminate_spin: bool,
    deferred: Vec<u64>,
    fired: Vec<String>,
    infos: Vec<String>,
    health_checks: u64,
    health: Option<String>,
    due: HashMap<u64, Instant>,
    late: Vec<u64>,
    ballast: Vec<u8>,
}

pub(super) fn text(body: impl Into<String>) -> Envelope {
    Envelope::new("text/plain", body.into().into_bytes())
}

/// `node.serial.incarnation@creation`, as the host prints pids.
#[must_use]
pub fn show_pid(pid: Pid) -> String {
    format!(
        "{}.{}.{}@{}",
        pid.node,
        pid.id & ((1 << 40) - 1),
        pid.incarnation,
        pid.id >> 40
    )
}

/// `node.serial.incarnation@creation`, or `node.id.incarnation` with the
/// whole id.
#[must_use]
pub fn parse_pid(s: &str) -> Pid {
    let (fields, creation) = match s.split_once('@') {
        Some((fields, creation)) => (fields, creation.parse::<u64>().ok()),
        None => (s, None),
    };
    let mut parts = fields.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    let node = u32::try_from(next()).unwrap_or(0);
    let id = next();
    Pid {
        node,
        id: creation.map_or(id, |c| (c << 40) | id),
        incarnation: u32::try_from(next()).unwrap_or(0),
    }
}

pub(super) fn show_exit(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Normal => "normal".into(),
        ExitReason::Shutdown => "shutdown".into(),
        ExitReason::Killed => "killed".into(),
        ExitReason::Noproc => "noproc".into(),
        ExitReason::Noconnection => "noconnection".into(),
        ExitReason::Timeout => "timeout".into(),
        ExitReason::Crashed(why) => format!("crashed({why})"),
    }
}

pub(super) fn show_call_error(err: &CallError) -> String {
    match err {
        CallError::Noproc => "noproc".into(),
        CallError::Noconnection => "noconnection".into(),
        CallError::Timeout => "timeout".into(),
        CallError::Overloaded => "overloaded".into(),
        CallError::TooLarge => "too-large".into(),
        CallError::AccessDenied => "access-denied".into(),
        CallError::Exited(reason) => format!("exited:{}", show_exit(reason)),
    }
}

fn parse_exit(s: &str) -> ExitReason {
    match s {
        "normal" => ExitReason::Normal,
        "shutdown" => ExitReason::Shutdown,
        "killed" => ExitReason::Killed,
        other => ExitReason::Crashed(other.strip_prefix("crashed-").unwrap_or(other).to_string()),
    }
}

pub(super) async fn register(cx: &mut TestCtx, name: &str) -> String {
    match cx.register(name).await {
        Ok(()) => "ok".into(),
        Err(RegisterError::AccessDenied) => "err:access-denied".into(),
        Err(RegisterError::Taken(pid)) => format!("err:taken:{}", show_pid(pid)),
    }
}

pub(super) const NEEDS_A_PROCESS: &str = "needs a process";

impl Chaos {
    /// Run `msg`'s command; `None` for no reply now.
    // One arm per command, as the fixture's own dispatcher.
    #[allow(clippy::cognitive_complexity)]
    async fn run(
        &mut self,
        cx: &mut TestCtx,
        caller: Option<u64>,
        msg: &Envelope,
    ) -> Result<Option<Envelope>, String> {
        let command = String::from_utf8_lossy(&msg.body).into_owned();
        let (verb, arg) = command.split_once(':').unwrap_or((&command, ""));
        let reply = |s: String| Ok(Some(text(s)));
        match verb {
            "echo" => reply(arg.to_string()),
            "mirror" => Ok(Some(msg.clone())),
            "count" => {
                self.count += 1;
                reply(self.count.to_string())
            }
            "trap" => Err("asked to trap".into()),
            "spin" => std::future::pending().await,
            "sleep" => {
                tokio::time::sleep(Duration::from_millis(arg.parse().unwrap_or(0))).await;
                reply("slept".into())
            }
            "alloc" => reply(NEEDS_A_PROCESS.into()),
            "busy" => {
                let rounds: u64 = arg.parse().unwrap_or(0);
                let mut x: u64 = 1;
                for _ in 0..rounds {
                    x = std::hint::black_box(
                        x.wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1_442_695_040_888_963_407),
                    );
                }
                reply(x.to_string())
            }
            "noreply" => Ok(None),
            "defer" => {
                if let Some(caller) = caller {
                    self.deferred.push(caller);
                }
                Ok(None)
            }
            "answer" => {
                for caller in std::mem::take(&mut self.deferred) {
                    cx.reply(caller, text(arg)).await;
                }
                reply("answered".into())
            }
            "answer-now" => {
                if let Some(caller) = caller {
                    cx.reply(caller, text(arg)).await;
                }
                Ok(None)
            }
            "self" => {
                let me = cx.self_pid().await;
                reply(show_pid(me))
            }
            "call" => {
                let mut parts = arg.splitn(3, ':');
                let pid = parse_pid(parts.next().unwrap_or(""));
                let timeout: u32 = parts.next().unwrap_or("1000").parse().unwrap_or(1000);
                let inner = text(parts.next().unwrap_or(""));
                reply(match cx.call(pid, inner, timeout).await {
                    Ok(env) => format!("ok:{}", String::from_utf8_lossy(&env.body)),
                    Err(err) => format!("err:{}", show_call_error(&err)),
                })
            }
            "call-self" => {
                let me = cx.self_pid().await;
                let result = cx.call(me, text("echo:me"), 1000).await;
                reply(format!("{}", result.is_ok()))
            }
            "cast" => {
                let (pid, inner) = arg.split_once(':').unwrap_or((arg, ""));
                let _ = cx.cast(parse_pid(pid), text(inner)).await;
                reply("sent".into())
            }
            // `cast-wait:<pid>:<ms>:<command>`: as the conformance app's.
            "cast-wait" => {
                let mut parts = arg.splitn(3, ':');
                let pid = parse_pid(parts.next().unwrap_or(""));
                let ms = parts.next().unwrap_or("").parse().unwrap_or(u32::MAX);
                let inner = text(parts.next().unwrap_or(""));
                let result = super::conformance::cast_wait(cx, pid, ms, inner).await;
                reply(result)
            }
            "register" => {
                let result = register(cx, arg).await;
                reply(result)
            }
            "whereis" => {
                let found = cx.whereis(arg).await;
                reply(found.map_or("none".into(), show_pid))
            }
            "timer" => {
                let (ms, body) = arg.split_once(':').unwrap_or((arg, ""));
                let reference = cx.send_after(ms.parse().unwrap_or(0), text(body)).await;
                reply(reference.to_string())
            }
            "cancel" => {
                cx.cancel_timer(arg.parse().unwrap_or(0)).await;
                reply("cancelled".into())
            }
            "timers" => reply(self.fired.join(",")),
            "timed" => {
                let ms: u32 = arg.parse().unwrap_or(0);
                let due = Instant::now() + Duration::from_millis(ms.into());
                let reference = cx.send_after(ms, text("timed")).await;
                self.due.insert(reference, due);
                reply(reference.to_string())
            }
            "lateness" => {
                let late: Vec<String> = self.late.drain(..).map(|us| us.to_string()).collect();
                reply(late.join(","))
            }
            "clock" => {
                let reads: u64 = arg.parse().unwrap_or(0);
                let started = Instant::now();
                for _ in 0..reads {
                    std::hint::black_box(Instant::now());
                }
                reply(started.elapsed().as_nanos().to_string())
            }
            "ballast" => {
                let kib: usize = arg.parse().unwrap_or(0);
                self.ballast = vec![7u8; kib << 10];
                reply("ok".into())
            }
            "link" => {
                cx.link(parse_pid(arg)).await;
                reply("ok".into())
            }
            "unlink" => {
                cx.unlink(parse_pid(arg)).await;
                reply("ok".into())
            }
            "trap-exits" => {
                cx.trap_exits(arg == "on").await;
                reply("ok".into())
            }
            "monitor" => {
                let reference = cx.monitor(parse_pid(arg)).await;
                reply(reference.to_string())
            }
            "demonitor" => {
                cx.demonitor(arg.parse().unwrap_or(0)).await;
                reply("ok".into())
            }
            "exit" => {
                let (pid, reason) = arg.split_once(':').unwrap_or((arg, "normal"));
                cx.exit(parse_pid(pid), parse_exit(reason)).await;
                reply("ok".into())
            }
            "infos" => reply(self.infos.join(";")),
            "health" => {
                self.health = (arg != "ok").then(|| arg.to_string());
                reply("ok".into())
            }
            "health-checks" => reply(self.health_checks.to_string()),
            _ => reply(format!("unknown command '{command}'")),
        }
    }
}

impl TestApp for Chaos {
    async fn init(cx: &mut TestCtx, args: Vec<u8>) -> Result<Self, String> {
        let args = String::from_utf8_lossy(&args).into_owned();
        // Manifest `start_args` arrive as JSON; accept a JSON string too.
        let args = args.trim().trim_matches('"').to_string();
        let mut terminate_spin = false;
        for word in args.split_whitespace() {
            match word.split_once(':') {
                Some(("register", name)) => {
                    let result = register(cx, name).await;
                    if result != "ok" {
                        return Err(result);
                    }
                }
                _ => match word {
                    "fail" => return Err("asked to fail".to_string()),
                    "trap" => return Err("panic: asked to trap in init".to_string()),
                    "spin" => std::future::pending::<()>().await,
                    "terminate-spin" => terminate_spin = true,
                    "trap-exits" => cx.trap_exits(true).await,
                    _ => {}
                },
            }
        }
        Ok(Self {
            terminate_spin,
            ..Self::default()
        })
    }

    async fn load_snapshot(
        _cx: &mut TestCtx,
        _from_version: String,
        snapshot: Vec<u8>,
    ) -> Result<Self, String> {
        let chunks = snapshot.as_chunks::<8>().0;
        let mark = chunks
            .iter()
            .position(|c| u64::from_le_bytes(*c) == BALLAST_MARK);
        let numbers_end = mark.map_or(snapshot.len(), |i| i * 8);
        if numbers_end < 8 || !numbers_end.is_multiple_of(8) {
            return Err("snapshot must be whole 8-byte numbers".to_string());
        }
        let ballast = mark.map_or_else(Vec::new, |i| snapshot[(i + 1) * 8..].to_vec());
        let mut numbers = chunks[..numbers_end / 8]
            .iter()
            .map(|c| u64::from_le_bytes(*c));
        let count = numbers.next().unwrap_or(0);
        Ok(Self {
            count,
            deferred: numbers.collect(),
            ballast,
            ..Self::default()
        })
    }

    async fn handle_call(
        &mut self,
        cx: &mut TestCtx,
        call_ref: u64,
        msg: Envelope,
    ) -> Result<Option<Envelope>, String> {
        // A host health check: `ok`, or what `health:<text>` set.
        if msg.schema.as_deref() == Some("bulkhead/health") {
            self.health_checks += 1;
            return Ok(Some(text(
                self.health.clone().unwrap_or_else(|| "ok".to_string()),
            )));
        }
        self.run(cx, Some(call_ref), &msg).await
    }

    async fn handle_cast(&mut self, cx: &mut TestCtx, msg: Envelope) -> Result<(), String> {
        self.run(cx, None, &msg).await.map(|_| ())
    }

    async fn handle_info(&mut self, _cx: &mut TestCtx, info: Info) -> Result<(), String> {
        match info {
            Info::Timer { reference, msg } => {
                let now = Instant::now();
                if let Some(due) = self.due.remove(&reference) {
                    let late = now.saturating_duration_since(due).as_micros();
                    self.late.push(u64::try_from(late).unwrap_or(u64::MAX));
                }
                self.fired
                    .push(String::from_utf8_lossy(&msg.body).into_owned());
            }
            Info::Exit { from, reason } => {
                self.infos
                    .push(format!("exit:{}:{}", show_pid(from), show_exit(&reason)));
            }
            Info::HostEvent { name, msg } => {
                let body = String::from_utf8_lossy(&msg.body).into_owned();
                self.infos.push(format!("event:{name}:{body}"));
            }
            Info::Down {
                reference,
                pid,
                reason,
            } => {
                self.infos.push(format!(
                    "down:{reference}:{}:{}",
                    show_pid(pid),
                    show_exit(&reason)
                ));
            }
            Info::KvChanged { .. } => {}
        }
        Ok(())
    }

    async fn terminate(&mut self, _cx: &mut TestCtx, _reason: ExitReason) {
        if self.terminate_spin {
            std::future::pending::<()>().await;
        }
    }

    fn save_snapshot(&mut self) -> Result<Vec<u8>, String> {
        let mut out = self.count.to_le_bytes().to_vec();
        for caller in &self.deferred {
            out.extend_from_slice(&caller.to_le_bytes());
        }
        if !self.ballast.is_empty() {
            out.extend_from_slice(&BALLAST_MARK.to_le_bytes());
            out.extend_from_slice(&self.ballast);
        }
        Ok(out)
    }
}
