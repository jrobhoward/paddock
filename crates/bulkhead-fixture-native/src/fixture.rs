//! The fixture's commands, on `bulkhead_native::app` (Unix only).

use std::io::Read;
use std::process::ExitCode;

use bulkhead_native::app::{self, App, CallRef, Ctx};
use bulkhead_native::protocol::{
    Envelope, ExitReason, FdRef, Info, Level, NetAnswer, NetRequest, Pid, SqlAnswer, SqlRequest,
    SqlValue,
};

#[derive(Default)]
struct Fixture {
    infos: Vec<String>,
    waiting: Option<CallRef>,
    /// The state its snapshot carries, one note per line.
    notes: Vec<String>,
    /// The version whose snapshot it started from.
    loaded_from: Option<String>,
    /// Fail the next `save_snapshot`.
    fail_save: bool,
    /// The SQL transaction `sql-begin` opened.
    tx: Option<u32>,
    /// Streams `tcp-connect` made, by the node's socket number.
    streams: std::collections::BTreeMap<u32, std::net::TcpStream>,
}

fn text(s: impl Into<String>) -> Envelope {
    Envelope::new("text/plain", s.into().into_bytes())
}

fn pid_text(p: Pid) -> String {
    format!("{}.{}.{}", p.node, p.id, p.incarnation)
}

fn parse_pid(s: &str) -> Option<Pid> {
    let mut it = s.split('.');
    Some(Pid {
        node: it.next()?.parse().ok()?,
        id: it.next()?.parse().ok()?,
        incarnation: it.next()?.parse().ok()?,
    })
}

fn outcome<T, E: std::fmt::Display>(r: Result<T, E>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => format!("err:{e}"),
    }
}

/// A SQL answer as text: rows as `col=value` cells joined by `,`, rows
/// by `;`; a count; or `ok`.
fn sql_text(answer: SqlAnswer) -> String {
    match answer {
        SqlAnswer::Rows(rows) => rows
            .rows
            .iter()
            .map(|row| {
                rows.columns
                    .iter()
                    .zip(row)
                    .map(|(c, v)| {
                        let v = match v {
                            SqlValue::Null => "null".to_string(),
                            SqlValue::Bool(b) => b.to_string(),
                            SqlValue::Int(i) => i.to_string(),
                            SqlValue::Float(f) => f.to_string(),
                            SqlValue::Text(s) => s.clone(),
                            SqlValue::Bytes(b) => format!("{:?}", b.0),
                        };
                        format!("{c}={v}")
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(";"),
        SqlAnswer::Changed(n) => n.to_string(),
        SqlAnswer::Begun(_) | SqlAnswer::Ended => "ok".into(),
    }
}

impl Fixture {
    /// Sockets through the node: `udp-bind ADDR` (its socket),
    /// `udp-send S ADDR TEXT`, `udp-recv S` (`FROM TEXT` or `none`),
    /// `net-watch S TAG`; `tcp-connect ADDR` keeps the passed stream (its
    /// socket), `tcp-write S TEXT`, `tcp-close S` drops it and tells the
    /// node, `tcp-close-keep S` tells the node and keeps it.
    fn net(&mut self, ctx: &mut Ctx, verb: &str, first: &str, tail: &str) -> String {
        use std::io::Write;
        let socket = || first.parse::<u32>().unwrap_or(u32::MAX);
        let addr = |s: &str| s.parse::<std::net::SocketAddr>();
        let request = match verb {
            "udp-bind" | "tcp-connect" => match addr(first) {
                Ok(a) if verb == "udp-bind" => NetRequest::UdpBind(a),
                Ok(a) => NetRequest::Connect(a),
                Err(e) => return format!("err:{e}"),
            },
            "udp-send" => {
                let (to, text) = tail.split_once(' ').unwrap_or((tail, ""));
                match addr(to) {
                    Ok(to) => NetRequest::UdpSend {
                        socket: socket(),
                        to,
                        data: bulkhead_native::protocol::Bytes(text.as_bytes().to_vec()),
                    },
                    Err(e) => return format!("err:{e}"),
                }
            }
            "udp-recv" => NetRequest::UdpRecv(socket()),
            "net-watch" => NetRequest::Notify {
                socket: socket(),
                tag: tail.parse().unwrap_or(0),
            },
            "tcp-write" => {
                return match self.streams.get_mut(&socket()) {
                    Some(s) => outcome(s.write_all(tail.as_bytes())),
                    None => "err:no stream".into(),
                };
            }
            "tcp-close" => {
                self.streams.remove(&socket());
                NetRequest::Close(socket())
            }
            _ => NetRequest::Close(socket()),
        };
        match ctx.net(request) {
            Ok(NetAnswer::Udp { socket, .. }) => socket.to_string(),
            Ok(NetAnswer::Stream(s)) => match app::take_fd(s.fd) {
                Ok(fd) => {
                    self.streams.insert(s.socket, std::net::TcpStream::from(fd));
                    s.socket.to_string()
                }
                Err(e) => format!("err:{e}"),
            },
            Ok(NetAnswer::Datagram(Some((from, data)))) => {
                format!("{from} {}", String::from_utf8_lossy(&data.0))
            }
            Ok(NetAnswer::Datagram(None)) => "none".into(),
            Ok(_) => "ok".into(),
            Err(e) => format!("err:{e:?}"),
        }
    }

    /// `sql-q DS STMT`, `sql-x DS STMT`, `sql-begin DS`; inside the open
    /// transaction `sql-tq STMT`, `sql-tx STMT`, `sql-commit`,
    /// `sql-rollback`; `sql-drop` forgets it as a dropped handle does
    /// (`end_transaction_quietly`). Errors as `err:` and the error.
    fn sql(&mut self, ctx: &mut Ctx, verb: &str, first: &str, tail: &str, rest: &str) -> String {
        let request = match verb {
            "sql-q" => SqlRequest::Query {
                datasource: first.into(),
                statement: tail.into(),
                params: Vec::new(),
            },
            "sql-x" => SqlRequest::Execute {
                datasource: first.into(),
                statement: tail.into(),
                params: Vec::new(),
            },
            "sql-begin" => SqlRequest::Begin(first.into()),
            "sql-drop" => {
                if let Some(tx) = self.tx.take() {
                    app::end_transaction_quietly(tx);
                }
                return "ok".into();
            }
            _ => {
                let Some(tx) = self.tx else {
                    return "err:no transaction".into();
                };
                match verb {
                    "sql-tq" => SqlRequest::TxQuery {
                        tx,
                        statement: rest.into(),
                        params: Vec::new(),
                    },
                    "sql-tx" => SqlRequest::TxExecute {
                        tx,
                        statement: rest.into(),
                        params: Vec::new(),
                    },
                    _ => {
                        self.tx = None;
                        SqlRequest::TxEnd {
                            tx,
                            commit: verb == "sql-commit",
                        }
                    }
                }
            }
        };
        match ctx.sql(request) {
            Ok(SqlAnswer::Begun(tx)) => {
                self.tx = Some(tx);
                "ok".into()
            }
            Ok(answer) => sql_text(answer),
            Err(e) => format!("err:{e:?}"),
        }
    }

    fn run(&mut self, ctx: &mut Ctx, call: CallRef, command: &str) -> Option<String> {
        let (verb, rest) = command.split_once(' ').unwrap_or((command, ""));
        let (first, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let pid = || parse_pid(first).unwrap_or_else(|| panic!("not a pid: {first}"));
        Some(match verb {
            "echo" => rest.to_string(),
            "self" => pid_text(ctx.self_pid()),
            "selfs" => {
                let started = std::time::Instant::now();
                for _ in 0..first.parse::<u64>().unwrap_or(0) {
                    std::hint::black_box(ctx.self_pid());
                }
                started.elapsed().as_nanos().to_string()
            }
            "call" => match ctx.call(pid(), text(tail), 5_000) {
                Ok(reply) => String::from_utf8_lossy(&reply.body).into_owned(),
                Err(e) => format!("err:{e:?}"),
            },
            "cast" => {
                ctx.cast(pid(), text(tail));
                "ok".into()
            }
            "after" => ctx
                .send_after(first.parse().unwrap_or(0), text(tail))
                .to_string(),
            "link" => {
                ctx.link(pid());
                "ok".into()
            }
            "monitor" => ctx.monitor(pid()).to_string(),
            "trap" => {
                ctx.trap_exits(true);
                "ok".into()
            }
            "infos" => self.infos.join("\n"),
            "note" => {
                self.notes.push(rest.to_string());
                "ok".into()
            }
            "notes" => self.notes.join(","),
            "loaded-from" => self.loaded_from.clone().unwrap_or_else(|| "none".into()),
            "os-pid" => std::process::id().to_string(),
            "fail-save" => {
                self.fail_save = true;
                "ok".into()
            }
            "register" => match ctx.register(first) {
                Ok(()) => "ok".into(),
                Err(e) => format!("err:{e:?}"),
            },
            "whereis" => ctx.whereis(first).map_or_else(|| "none".into(), pid_text),
            "exit" => {
                let reason = match tail {
                    "normal" => ExitReason::Normal,
                    "kill" => ExitReason::Killed,
                    other => ExitReason::Crashed(other.to_string()),
                };
                ctx.exit(pid(), reason);
                "ok".into()
            }
            "noreply" => {
                self.waiting = Some(call);
                return None;
            }
            "log" => {
                app::log(Level::Warn, rest);
                "ok".into()
            }
            "print" => {
                println!("{rest}");
                "ok".into()
            }
            "udp-bind" | "udp-send" | "udp-recv" | "net-watch" | "tcp-connect" | "tcp-write"
            | "tcp-close" | "tcp-close-keep" => self.net(ctx, verb, first, tail),
            "sql-q" | "sql-x" | "sql-begin" | "sql-tq" | "sql-tx" | "sql-commit"
            | "sql-rollback" | "sql-drop" => self.sql(ctx, verb, first, tail, rest),
            "panic" => panic!("asked to"),
            "abort" => std::process::abort(),
            "break-channel" => {
                app::break_channel_for_tests();
                "ok".into()
            }
            "published" => {
                app::published_revision(first).map_or_else(|| "none".into(), |r| r.to_string())
            }
            "fd" => match first.parse() {
                Ok(id) => read_passed(FdRef(id)),
                Err(_) => "err:fd ID".into(),
            },
            "spin" => loop {
                std::hint::spin_loop();
            },
            "alloc" => {
                let mib: usize = rest.parse().unwrap_or(0);
                let v = vec![1u8; mib << 20];
                format!("ok {}", std::hint::black_box(v).len() >> 20)
            }
            "read" => outcome(std::fs::read(rest)),
            "write" => outcome(std::fs::write(rest, b"x")),
            "connect" => outcome(std::net::TcpStream::connect(rest)),
            "exec" => outcome(std::process::Command::new("/bin/true").status()),
            #[cfg(target_os = "linux")]
            "io-uring" => {
                let mut params = rustix::io_uring::io_uring_params::default();
                // SAFETY: one entry and zeroed parameters; the ring's
                // descriptor, if one came, is dropped without being mapped.
                outcome(unsafe { rustix::io_uring::io_uring_setup(1, &mut params) })
            }
            "vread" => outcome(std::fs::read(bulkhead_native::preopens::path(rest))),
            "vwrite" => outcome(std::fs::write(bulkhead_native::preopens::path(rest), b"x")),
            "priv" => match std::env::var(bulkhead_native::protocol::PRIV_ENV) {
                Ok(dir) => outcome(std::fs::read(std::path::Path::new(&dir).join(rest))),
                Err(_) => "err:no priv".into(),
            },
            "dumpable" => dumpable(),
            "kv" => kv(ctx, rest),
            "group" => group(ctx, first, tail),
            "config" => match ctx.config(first) {
                Ok(v) => v.unwrap_or_else(|| "none".into()),
                Err(e) => format!("err:{e:?}"),
            },
            "config-all" => match ctx.config_all() {
                Ok(all) => all
                    .into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
                Err(e) => format!("err:{e:?}"),
            },
            "secret" => match ctx.secret(first) {
                Ok(v) => String::from_utf8_lossy(&v).into_owned(),
                Err(e) => format!("err:{e:?}"),
            },
            "ext" => extension(ctx, first, tail),
            "metric" => metric(ctx, first, tail),
            "kill" => {
                let target: i32 = rest.parse().unwrap_or(0);
                // SAFETY: `kill` takes no pointers; any pid and signal 0
                // (an existence check) is defined.
                let r = unsafe { libc_kill(target, 0) };
                if r == 0 {
                    "ok".into()
                } else {
                    format!("err:{}", std::io::Error::last_os_error())
                }
            }
            other => format!("err:unknown command {other}"),
        })
    }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

impl App for Fixture {
    fn init(_ctx: &mut Ctx, args: Vec<u8>) -> Result<Self, String> {
        if args == b"fail" {
            return Err("asked to fail".into());
        }
        Ok(Self::default())
    }

    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        if self.fail_save {
            return Err("asked to fail".into());
        }
        Ok(self.notes.join("\n").into_bytes())
    }

    fn load_snapshot(
        _ctx: &mut Ctx,
        from_version: &str,
        snapshot: Vec<u8>,
    ) -> Result<Self, String> {
        let snapshot = String::from_utf8_lossy(&snapshot).into_owned();
        if snapshot.lines().any(|note| note == "refuse") {
            return Err("asked to refuse".into());
        }
        Ok(Self {
            notes: snapshot.lines().map(str::to_string).collect(),
            loaded_from: Some(from_version.to_string()),
            ..Self::default()
        })
    }

    fn handle_call(&mut self, ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Option<Envelope> {
        let command = String::from_utf8_lossy(&msg.body).into_owned();
        self.run(ctx, call, &command).map(text)
    }

    fn handle_cast(&mut self, ctx: &mut Ctx, msg: Envelope) {
        let body = String::from_utf8_lossy(&msg.body).into_owned();
        if let Some(rest) = body.strip_prefix("reply ")
            && let Some(call) = self.waiting.take()
        {
            ctx.reply(call, text(rest));
        } else {
            self.infos.push(format!("cast {body}"));
        }
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        self.infos.push(match info {
            Info::Exit { from, reason } => format!("exit {} {reason:?}", pid_text(from)),
            Info::Down {
                reference,
                pid,
                reason,
            } => format!("down {reference} {} {reason:?}", pid_text(pid)),
            Info::Timer { reference, msg } => {
                format!("timer {reference} {}", String::from_utf8_lossy(&msg.body))
            }
            Info::HostEvent { name, msg } if name == "io-ready" => {
                format!("event {name} {}", String::from_utf8_lossy(&msg.body))
            }
            Info::HostEvent { name, .. } => format!("event {name}"),
            Info::KvChanged {
                watch,
                bucket,
                key,
                entry,
                ..
            } => match entry {
                Some((value, revision)) => format!(
                    "kv {watch} {bucket} {key} {}@{revision}",
                    String::from_utf8_lossy(&value.0)
                ),
                None => format!("kv {watch} {bucket} {key} deleted"),
            },
        });
    }
}

/// Serve the fixture on stdin and stdout.
pub(crate) fn main() -> ExitCode {
    app::serve::<Fixture>()
}

/// Whether the kernel would dump this process's core: `yes` or `no`.
#[cfg(target_os = "linux")]
fn dumpable() -> String {
    use rustix::process::{DumpableBehavior, dumpable_behavior};
    match dumpable_behavior() {
        Ok(DumpableBehavior::NotDumpable) => "no".into(),
        Ok(_) => "yes".into(),
        Err(e) => format!("err:{e}"),
    }
}

#[cfg(not(target_os = "linux"))]
fn dumpable() -> String {
    "err:linux only".into()
}

/// `kv OP BUCKET ...`: the keyvalue requests, answered as text (`err:` and
/// the error for a failure).
fn kv(ctx: &mut Ctx, command: &str) -> String {
    let words: Vec<&str> = command.split(' ').collect();
    let (op, bucket) = (words[0], words.get(1).copied().unwrap_or(""));
    let arg = |i: usize| words.get(i).copied().unwrap_or("");
    let text = |v: Vec<u8>| String::from_utf8_lossy(&v).into_owned();
    let out = if op == "unwatch" {
        ctx.unwatch(bucket.parse().unwrap_or(0))
            .map(|()| "ok".to_string())
    } else if op == "handle-get" {
        // `kv handle-get HANDLE KEY`: a get on a bucket handle as given.
        app::get_on_handle_for_tests(bucket.parse().unwrap_or(u32::MAX), arg(2))
            .map(|v| v.map_or_else(|| "none".into(), text))
    } else {
        let mut b = ctx.bucket(bucket);
        match op {
            "get" => b.get(arg(2)).map(|v| v.map_or_else(|| "none".into(), text)),
            "set" => b.set(arg(2), arg(3).as_bytes()).map(|()| "ok".into()),
            "del" => b.delete(arg(2)).map(|()| "ok".into()),
            "incr" => b
                .increment(arg(2), arg(3).parse().unwrap_or(1))
                .map(|n| n.to_string()),
            "many" => {
                let keys: Vec<String> = arg(2).split(',').map(str::to_string).collect();
                b.get_many(&keys).map(|vs| {
                    vs.into_iter()
                        .map(|v| v.map_or_else(|| "-".into(), text))
                        .collect::<Vec<_>>()
                        .join(",")
                })
            }
            "setmany" => {
                let entries: Vec<(String, Vec<u8>)> = arg(2)
                    .split(',')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                    .collect();
                b.set_many(&entries).map(|()| "ok".into())
            }
            "entry" => b
                .entry(arg(2))
                .map(|e| e.map_or_else(|| "none".into(), |(v, r)| format!("{}@{r}", text(v)))),
            "rev" => b.revision().map(|r| r.to_string()),
            "scan" => b
                .scan(bulkhead_native::protocol::ScanQuery {
                    prefix: Some(arg(2).to_string()),
                    ..Default::default()
                })
                .map(|page| {
                    page.entries
                        .into_iter()
                        .map(|(k, v, r)| format!("{k}={}@{r}", text(v.0)))
                        .collect::<Vec<_>>()
                        .join(",")
                }),
            "cas" => {
                let expected = arg(3).parse().ok();
                b.compare_and_swap(arg(2), expected, arg(4).as_bytes())
                    .map(|r| r.to_string())
            }
            "watch" => b.watch(arg(2)).map(|w| w.id.to_string()),
            "gets" => {
                let n: u64 = arg(2).parse().unwrap_or(0);
                let started = std::time::Instant::now();
                let mut out = Ok(String::new());
                for _ in 0..n {
                    if let Err(e) = b.get("t") {
                        out = Err(e);
                        break;
                    }
                }
                out.map(|_| started.elapsed().as_nanos().to_string())
            }
            "watchp" => b.watch_prefix(arg(2)).map(|w| w.id.to_string()),
            other => return format!("err:unknown kv op {other}"),
        }
    };
    out.unwrap_or_else(|e| format!("err:{e:?}"))
}

/// `group OP GROUP [TEXT]`: `join`, `leave`, `members` (how many),
/// `publish` (to how many).
fn group(ctx: &mut Ctx, op: &str, rest: &str) -> String {
    let (name, text_part) = rest.split_once(' ').unwrap_or((rest, ""));
    let out = match op {
        "join" => ctx.join_group(name).map(|()| "ok".to_string()),
        "leave" => ctx.leave_group(name).map(|()| "ok".to_string()),
        "members" => ctx.group_members(name).map(|m| m.len().to_string()),
        "publish" => ctx.publish(name, text(text_part)).map(|n| n.to_string()),
        other => return format!("err:unknown group op {other}"),
    };
    out.unwrap_or_else(|e| format!("err:{e:?}"))
}

/// `metric register KIND NAME`, the id; `metric add ID N`,
/// `metric set ID N`, `metric observe ID N`, queued until the callback
/// ends.
fn metric(ctx: &mut Ctx, op: &str, rest: &str) -> String {
    use bulkhead_native::protocol::MetricKind;
    let (a, b) = rest.split_once(' ').unwrap_or((rest, ""));
    match op {
        "register" => {
            let kind = match a {
                "gauge" => MetricKind::Gauge,
                "histogram" => MetricKind::Histogram,
                _ => MetricKind::Counter,
            };
            ctx.register_metric(kind, b, "a fixture metric", &[], &[])
                .map_or_else(|e| format!("err:{e:?}"), |id| id.to_string())
        }
        "add" | "set" | "observe" => {
            let (id, value) = (a.parse().unwrap_or(0), b.parse().unwrap_or(0.0));
            match op {
                "add" => ctx.metric_add(id, value),
                "set" => ctx.metric_set(id, value),
                _ => ctx.metric_observe(id, value),
            }
            "queued".into()
        }
        other => format!("err:unknown metric op {other}"),
    }
}

/// What the descriptor the node passed as `fd` reads, up to 1 KiB.
fn read_passed(fd: FdRef) -> String {
    let mut file = match app::take_fd(fd) {
        Ok(owned) => std::fs::File::from(owned),
        Err(e) => return format!("err:{e}"),
    };
    let mut buf = vec![0; 1024];
    match file.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned(),
        Err(e) => format!("err:{e}"),
    }
}

/// `ext PACKAGE FUNCTION JSON`: the extension call with the JSON
/// arguments in CBOR, answered with its result as JSON.
fn extension(ctx: &mut Ctx, package: &str, rest: &str) -> String {
    let (function, json) = rest.split_once(' ').unwrap_or((rest, "null"));
    let args: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return format!("err:json {e}"),
    };
    let mut encoded = Vec::new();
    if let Err(e) = ciborium::into_writer(&args, &mut encoded) {
        return format!("err:cbor {e}");
    }
    match ctx.extension(package, function, encoded) {
        Ok(result) => match ciborium::from_reader::<serde_json::Value, _>(result.as_slice()) {
            Ok(v) => v.to_string(),
            Err(e) => format!("err:result {e}"),
        },
        Err(e) => format!("err:{e:?}"),
    }
}
