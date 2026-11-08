//! The conformance suite's app as an in-process test app (ROADMAP Z9d): the
//! commands and replies of `bulkhead-fixture-conformance`, whose crate docs
//! list them, on the requests that fixture's native build makes. Errors
//! other than `err:access-denied` and `err:not-found` are written as the
//! protocol's, which no case reads.
//!
//! `trap` ends the callback with an error, which the instance sees as a
//! native app's panic. In `init` it is an error reading `panic: asked to
//! trap in init`, so the instance exits with `init failed: ` before it.
//! `spin` waits forever without finishing its callback, as the chaos app's
//! does. `alloc` needs real memory and answers `needs a process`.

use bulkhead_native::protocol::{
    Bytes, ConfigError, FromApp, GroupsAnswer, GroupsError, GroupsRequest, Level, MetricError,
    MetricKind, MetricUpdate, ScanPage, ScanQuery, SecretError,
};
use serde::{Deserialize, Serialize};

use super::chaos::{self, NEEDS_A_PROCESS, parse_pid, show_call_error, show_exit, show_pid, text};
use super::{
    Answer, CastError, Envelope, ExitReason, Info, KvAnswer, KvError, KvRequest, Pid, Request,
    TestApp, TestCtx, emit,
};

/// The snapshot format this app writes and reads.
const SNAPSHOT_VERSION: u32 = 1;

/// What a snapshot carries, in GUEST_FORMATS.md's layout.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Kept {
    count: u64,
    notes: Vec<String>,
    deferred: Vec<u64>,
    refuse: bool,
}

/// The conformance app.
#[derive(Debug, Default)]
pub struct Conformance {
    kept: Kept,
    args: String,
    fired: Vec<String>,
    infos: Vec<String>,
    changes: Vec<String>,
    health: Option<String>,
    health_checks: u64,
    loaded_from: Option<String>,
    on_terminate: Option<Pid>,
    /// From the last `log-level` host event; every level before any.
    log_level: Option<Level>,
}

const LEVELS: [(Level, &str); 6] = [
    (Level::Trace, "trace"),
    (Level::Debug, "debug"),
    (Level::Info, "info"),
    (Level::Warn, "warn"),
    (Level::Error, "error"),
    (Level::Critical, "critical"),
];

fn number(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

fn milliseconds(s: &str) -> u32 {
    u32::try_from(number(s)).unwrap_or(u32::MAX)
}

fn parse_exit(s: &str) -> ExitReason {
    match s {
        "normal" => ExitReason::Normal,
        "shutdown" => ExitReason::Shutdown,
        "killed" => ExitReason::Killed,
        other => ExitReason::Crashed(other.to_string()),
    }
}

fn show_cast(result: Result<(), CastError>) -> String {
    let error = match result {
        Ok(()) => return "ok".into(),
        Err(CastError::Noproc) => "noproc",
        Err(CastError::Noconnection) => "noconnection",
        Err(CastError::Overloaded) => "overloaded",
        Err(CastError::TooLarge) => "too-large",
        Err(CastError::AccessDenied) => "access-denied",
        Err(CastError::Timeout) => "timeout",
    };
    format!("err:{error}")
}

fn kv_error(e: &KvError) -> String {
    match e {
        KvError::AccessDenied => "err:access-denied".into(),
        other => format!("err:{other:?}"),
    }
}

fn unexpected(answer: &KvAnswer) -> KvError {
    KvError::Other(format!("unexpected answer {answer:?}"))
}

/// A `Bytes` value as text.
fn lossy(value: &Bytes) -> String {
    String::from_utf8_lossy(&value.0).into_owned()
}

/// Every key of `bucket`, sorted.
async fn keys(cx: &mut TestCtx, bucket: &str) -> Result<String, KvError> {
    let mut all = Vec::new();
    let mut cursor = None;
    loop {
        let request = KvRequest::Keys {
            bucket: bucket.to_string(),
            cursor,
        };
        match cx.kv(request).await? {
            KvAnswer::Keys(page, next) => {
                all.extend(page);
                cursor = next;
                if cursor.is_none() {
                    all.sort();
                    return Ok(all.join(","));
                }
            }
            other => return Err(unexpected(&other)),
        }
    }
}

async fn kv_command(cx: &mut TestCtx, verb: &str, arg: &str) -> Result<String, KvError> {
    let mut parts = arg.splitn(3, ':');
    let bucket = parts.next().unwrap_or("").to_string();
    let key = parts.next().unwrap_or("").to_string();
    let request = match verb {
        "kv-set" => KvRequest::Set {
            bucket,
            key,
            value: Bytes(parts.next().unwrap_or("").as_bytes().to_vec()),
        },
        "kv-get" => KvRequest::Get { bucket, key },
        "kv-delete" => KvRequest::Delete { bucket, key },
        _ => return keys(cx, &bucket).await,
    };
    Ok(match cx.kv(request).await? {
        KvAnswer::Value(value) => value.map_or_else(|| "none".into(), |v| lossy(&v)),
        _ => "ok".into(),
    })
}

async fn scan_page(cx: &mut TestCtx, bucket: &str, query: ScanQuery) -> Result<ScanPage, KvError> {
    let request = KvRequest::Scan {
        bucket: bucket.to_string(),
        query,
    };
    match cx.kv(request).await? {
        KvAnswer::Page(page) => Ok(page),
        other => Err(unexpected(&other)),
    }
}

/// `scan`: every entry, read in pages of the command's limit.
async fn scan_command(cx: &mut TestCtx, arg: &str) -> Result<String, KvError> {
    let fields: Vec<&str> = arg.splitn(6, ':').collect();
    let field = |i: usize| fields.get(i).copied().filter(|s| !s.is_empty());
    let bucket = field(0).unwrap_or("");
    let mut query = ScanQuery {
        prefix: field(1).map(str::to_string),
        start: field(2).map(str::to_string),
        end: field(3).map(str::to_string),
        reverse: field(4) == Some("down"),
        limit: field(5).and_then(|n| n.parse().ok()).unwrap_or(0),
        cursor: None,
    };
    let mut found = Vec::new();
    loop {
        let page = scan_page(cx, bucket, query.clone()).await?;
        found.extend(
            page.entries
                .iter()
                .map(|(key, value, _)| format!("{key}={}", lossy(value))),
        );
        if page.next.is_none() {
            return Ok(found.join(","));
        }
        query.cursor = page.next;
    }
}

/// `first`, `last`, `next` and `prev`, as the SDK's `watch` reads them.
async fn neighbour(cx: &mut TestCtx, verb: &str, arg: &str) -> Result<String, KvError> {
    let (bucket, key) = arg.split_once(':').unwrap_or((arg, ""));
    let query = match verb {
        "first" => ScanQuery {
            limit: 1,
            ..ScanQuery::default()
        },
        "last" => ScanQuery {
            reverse: true,
            limit: 1,
            ..ScanQuery::default()
        },
        "next" => ScanQuery {
            start: Some(key.to_string()),
            limit: 2,
            ..ScanQuery::default()
        },
        _ => ScanQuery {
            end: Some(key.to_string()),
            reverse: true,
            limit: 1,
            ..ScanQuery::default()
        },
    };
    let page = scan_page(cx, bucket, query).await?;
    let found = page
        .entries
        .into_iter()
        .map(|(key, _, _)| key)
        // `start` is inclusive: `next` skips `key` itself.
        .find(|found| verb != "next" || found.as_str() > key);
    Ok(found.unwrap_or_else(|| "none".into()))
}

async fn watch(cx: &mut TestCtx, arg: &str) -> Result<String, KvError> {
    let (bucket, key) = arg.split_once(':').unwrap_or((arg, ""));
    let request = KvRequest::Watch {
        bucket: bucket.to_string(),
        key: key.to_string(),
        prefix: false,
        after: None,
    };
    match cx.kv(request).await? {
        KvAnswer::Started { id, .. } => Ok(id.to_string()),
        other => Err(unexpected(&other)),
    }
}

async fn group_command(cx: &mut TestCtx, verb: &str, arg: &str) -> String {
    let (group, rest) = arg.split_once(':').unwrap_or((arg, ""));
    let group = group.to_string();
    let request = match verb {
        "join" => GroupsRequest::Join(group),
        "leave" => GroupsRequest::Leave(group),
        "members" => GroupsRequest::Members(group),
        _ => GroupsRequest::Publish {
            group,
            msg: text(rest),
        },
    };
    match cx.groups(request).await {
        Ok(GroupsAnswer::Unit) => "ok".into(),
        Ok(GroupsAnswer::Members(pids)) => {
            let mut members: Vec<String> = pids.into_iter().map(show_pid).collect();
            members.sort();
            members.join(",")
        }
        Ok(GroupsAnswer::Delivered(n)) => n.to_string(),
        Err(GroupsError::AccessDenied) => "err:access-denied".into(),
        Err(other) => format!("err:{other:?}"),
    }
}

async fn config(cx: &mut TestCtx, key: &str) -> String {
    match cx.request(Request::ConfigGet(key.to_string())).await {
        Answer::Config(Ok(value)) => value.unwrap_or_else(|| "none".into()),
        Answer::Config(Err(ConfigError::AccessDenied)) => "err:access-denied".into(),
        other => format!("err:{other:?}"),
    }
}

async fn secret(cx: &mut TestCtx, name: &str) -> String {
    match cx.request(Request::Secret(name.to_string())).await {
        Answer::Secret(Ok(value)) => {
            let value = lossy(&value);
            value.strip_suffix('\n').unwrap_or(&value).to_string()
        }
        Answer::Secret(Err(SecretError::AccessDenied)) => "err:access-denied".into(),
        Answer::Secret(Err(SecretError::NotFound)) => "err:not-found".into(),
        other => format!("err:{other:?}"),
    }
}

/// `counter`, `gauge` and `histogram`: register the metric, then update it.
async fn metric(cx: &mut TestCtx, verb: &str, arg: &str) -> String {
    let (name, n) = arg.split_once(':').unwrap_or((arg, "0"));
    let n: f64 = n.parse().unwrap_or(0.0);
    let (kind, buckets) = match verb {
        "counter" => (MetricKind::Counter, vec![]),
        "gauge" => (MetricKind::Gauge, vec![]),
        _ => (MetricKind::Histogram, vec![1.0, 10.0]),
    };
    let registered = cx
        .request(Request::MetricRegister {
            kind,
            name: name.to_string(),
            help: "A conformance metric.".into(),
            labels: vec![],
            buckets,
        })
        .await;
    let id = match registered {
        Answer::Metric(Ok(id)) => id,
        Answer::Metric(Err(MetricError::AccessDenied)) => return "err:access-denied".into(),
        other => return format!("err:{other:?}"),
    };
    let update = match verb {
        "counter" => MetricUpdate::Add(id, n),
        "gauge" => MetricUpdate::Set(id, n),
        _ => MetricUpdate::Observe(id, n),
    };
    emit(&cx.pipe, FromApp::Metrics(vec![update]));
    "ok".into()
}

async fn cast_wait(cx: &mut TestCtx, to: Pid, timeout_ms: u32, msg: Envelope) -> String {
    let answer = cx
        .request(Request::CastWait {
            to,
            msg,
            timeout_ms,
        })
        .await;
    show_cast(match answer {
        Answer::CastFailed(e) => Err(e),
        _ => Ok(()),
    })
}

impl Conformance {
    fn log(&self, cx: &TestCtx, arg: &str) {
        let (level, message) = arg.split_once(':').unwrap_or((arg, ""));
        let level = LEVELS
            .iter()
            .find(|(_, name)| *name == level)
            .map_or(Level::Info, |(l, _)| *l);
        if self.log_level.is_none_or(|lowest| level >= lowest) {
            emit(
                &cx.pipe,
                FromApp::Log {
                    level,
                    message: message.to_string(),
                },
            );
        }
    }

    fn log_level(&self) -> String {
        LEVELS
            .iter()
            .find(|(l, _)| self.log_level.is_none_or(|lowest| *l >= lowest))
            .map_or("critical", |(_, name)| name)
            .to_string()
    }

    /// Run `msg`'s command; `None` for no reply now.
    // One arm per command, as the fixture's own dispatcher.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
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
            "args" => reply(self.args.clone()),
            "count" => {
                self.kept.count += 1;
                reply(self.kept.count.to_string())
            }
            "note" => {
                self.kept.notes.push(arg.to_string());
                reply("ok".into())
            }
            "notes" => reply(self.kept.notes.join(",")),
            "noreply" => Ok(None),
            "defer" => {
                if let Some(caller) = caller {
                    self.kept.deferred.push(caller);
                }
                Ok(None)
            }
            "answer" => {
                let deferred = std::mem::take(&mut self.kept.deferred);
                for caller in &deferred {
                    cx.reply(*caller, text(arg)).await;
                }
                reply(format!("answered:{}", deferred.len()))
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
                let timeout = milliseconds(parts.next().unwrap_or(""));
                let inner = text(parts.next().unwrap_or(""));
                reply(match cx.call(pid, inner, timeout).await {
                    Ok(env) => format!("ok:{}", String::from_utf8_lossy(&env.body)),
                    Err(err) => format!("err:{}", show_call_error(&err)),
                })
            }
            "cast" => {
                let (pid, inner) = arg.split_once(':').unwrap_or((arg, ""));
                let _ = cx.cast(parse_pid(pid), text(inner)).await;
                reply("sent".into())
            }
            "try-cast" => {
                let (pid, inner) = arg.split_once(':').unwrap_or((arg, ""));
                let result = cx.cast(parse_pid(pid), text(inner)).await;
                reply(show_cast(result))
            }
            "cast-wait" | "cast-wait-n" => {
                let mut parts = arg.splitn(3, ':');
                let pid = parse_pid(parts.next().unwrap_or(""));
                let ms = milliseconds(parts.next().unwrap_or(""));
                let rest = parts.next().unwrap_or("");
                let inner = if verb == "cast-wait-n" {
                    let n = usize::try_from(number(rest)).unwrap_or(0);
                    format!("note:{}", "x".repeat(n))
                } else {
                    rest.to_string()
                };
                let result = cast_wait(cx, pid, ms, text(inner)).await;
                reply(result)
            }
            "register" => {
                let result = chaos::register(cx, arg).await;
                reply(result)
            }
            "whereis" => {
                let found = cx.whereis(arg).await;
                reply(found.map_or("none".into(), show_pid))
            }
            "timer" => {
                let (ms, body) = arg.split_once(':').unwrap_or((arg, ""));
                let reference = cx.send_after(milliseconds(ms), text(body)).await;
                reply(reference.to_string())
            }
            "cancel" => {
                cx.cancel_timer(number(arg)).await;
                reply("cancelled".into())
            }
            "timers" => reply(self.fired.join(",")),
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
                cx.demonitor(number(arg)).await;
                reply("ok".into())
            }
            "exit" => {
                let (pid, reason) = arg.split_once(':').unwrap_or((arg, "normal"));
                cx.exit(parse_pid(pid), parse_exit(reason)).await;
                reply("ok".into())
            }
            "infos" => reply(self.infos.join(";")),
            "on-terminate" => {
                self.on_terminate = Some(parse_pid(arg));
                reply("ok".into())
            }
            "trap" => Err("asked to trap".into()),
            "spin" => std::future::pending().await,
            "alloc" => reply(NEEDS_A_PROCESS.into()),
            "refuse-upgrade" => {
                self.kept.refuse = true;
                reply("ok".into())
            }
            "loaded-from" => reply(self.loaded_from.clone().unwrap_or_else(|| "none".into())),
            "health" => {
                self.health = (arg != "ok").then(|| arg.to_string());
                reply("ok".into())
            }
            "health-checks" => reply(self.health_checks.to_string()),
            "log" => {
                self.log(cx, arg);
                reply("ok".into())
            }
            "log-level" => reply(self.log_level()),
            "kv-set" | "kv-get" | "kv-delete" | "kv-keys" => {
                let result = kv_command(cx, verb, arg).await;
                reply(result.unwrap_or_else(|e| kv_error(&e)))
            }
            "watch" => {
                let result = watch(cx, arg).await;
                reply(result.unwrap_or_else(|e| kv_error(&e)))
            }
            "changes" => reply(self.changes.join(",")),
            "scan" => {
                let result = scan_command(cx, arg).await;
                reply(result.unwrap_or_else(|e| kv_error(&e)))
            }
            "first" | "last" | "next" | "prev" => {
                let result = neighbour(cx, verb, arg).await;
                reply(result.unwrap_or_else(|e| kv_error(&e)))
            }
            "join" | "leave" | "members" | "publish" => {
                let result = group_command(cx, verb, arg).await;
                reply(result)
            }
            "config" => {
                let result = config(cx, arg).await;
                reply(result)
            }
            "secret" => {
                let result = secret(cx, arg).await;
                reply(result)
            }
            "counter" | "gauge" | "histogram" => {
                let result = metric(cx, verb, arg).await;
                reply(result)
            }
            _ => reply(format!("unknown command '{command}'")),
        }
    }
}

impl TestApp for Conformance {
    async fn init(cx: &mut TestCtx, args: Vec<u8>) -> Result<Self, String> {
        let raw = String::from_utf8_lossy(&args).into_owned();
        // Manifest `start_args` arrive as JSON; accept a JSON string too.
        for word in raw.trim().trim_matches('"').split_whitespace() {
            match word.split_once(':') {
                Some(("register", name)) => {
                    let result = chaos::register(cx, name).await;
                    if result != "ok" {
                        return Err(result);
                    }
                }
                _ => match word {
                    "fail" => return Err("asked to fail".to_string()),
                    "trap" => return Err("panic: asked to trap in init".to_string()),
                    "trap-exits" => cx.trap_exits(true).await,
                    _ => {}
                },
            }
        }
        Ok(Self {
            args: raw,
            ..Self::default()
        })
    }

    async fn load_snapshot(
        _cx: &mut TestCtx,
        from_version: String,
        snapshot: Vec<u8>,
    ) -> Result<Self, String> {
        let (version, kept): (u32, Kept) = ciborium::from_reader(snapshot.as_slice())
            .map_err(|e| format!("not a snapshot: {e}"))?;
        if version != SNAPSHOT_VERSION {
            return Err(format!("snapshot format {version} is not known"));
        }
        if kept.refuse {
            return Err("asked to refuse".to_string());
        }
        Ok(Self {
            kept,
            loaded_from: Some(from_version),
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
            Info::KvChanged { key, revision, .. } => {
                self.changes.push(format!("{key}@{revision}"));
            }
            Info::Timer { msg, .. } => {
                self.fired
                    .push(String::from_utf8_lossy(&msg.body).into_owned());
            }
            Info::Exit { from, reason } => {
                self.infos
                    .push(format!("exit:{}:{}", show_pid(from), show_exit(&reason)));
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
            Info::HostEvent { name, msg } if name == "log-level" => {
                self.log_level = LEVELS
                    .iter()
                    .find(|(_, level)| level.as_bytes() == msg.body)
                    .map(|(l, _)| *l);
            }
            Info::HostEvent { name, msg } => {
                let body = String::from_utf8_lossy(&msg.body).into_owned();
                self.infos.push(format!("event:{name}:{body}"));
            }
        }
        Ok(())
    }

    async fn terminate(&mut self, cx: &mut TestCtx, reason: ExitReason) {
        if let Some(to) = self.on_terminate {
            let note = format!("note:terminate:{}", show_exit(&reason));
            let _ = cx.cast(to, text(note)).await;
        }
    }

    fn save_snapshot(&mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        ciborium::into_writer(&(SNAPSHOT_VERSION, &self.kept), &mut out)
            .map_err(|e| format!("snapshot encode: {e}"))?;
        Ok(out)
    }
}
