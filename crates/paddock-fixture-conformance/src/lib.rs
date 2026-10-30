//! Test fixture: the Rust SDK's conformance app. Every guest SDK builds an
//! app that speaks this protocol, and `paddock-host`'s
//! `tests/conformance.rs` runs the same cases against each
//! (`PADDOCK_CONFORMANCE_APP=<component.wasm>`; this one by default).
//!
//! `init` arguments are UTF-8 words separated by spaces, optionally inside
//! a JSON string (as manifest `start_args` arrive). The whole argument
//! bytes are kept for `args`.
//! - `fail`: `init` returns the error `asked to fail`.
//! - `trap`: `init` traps.
//! - `register:<name>`: register `<name>` during `init`; an error fails
//!   `init`.
//! - `trap-exits`: trap exits from `init` on.
//!
//! Calls and casts carry UTF-8 commands; a call is answered with a
//! `text/plain` envelope holding the reply below, a cast runs the command
//! and drops the reply. Pids are written `node.serial.incarnation@creation`
//! (the id's high 24 bits are the creation, the low 40 the serial), exit
//! reasons `normal`, `shutdown`, `killed`, `noproc`, `noconnection`,
//! `timeout` or `crashed(<text>)`, and call errors `noproc`,
//! `noconnection`, `timeout`, `overloaded`, `too-large`, `access-denied`
//! or `exited:<reason>`. A service that refuses for want of a grant or a
//! manifest entry replies `err:access-denied`; other service errors reply
//! `err:<text>`.
//!
//! Messages:
//! - `echo:<text>`: `<text>`.
//! - `mirror`: the received envelope unchanged (content type, schema,
//!   body).
//! - `args`: the `init` arguments as received.
//! - `count`: add one to the counter; its value.
//! - `note:<text>`: keep `<text>`; `ok`. `notes`: the notes kept, joined
//!   by `,`.
//! - `noreply`: no answer, ever.
//! - `defer`: no answer now; remember the caller. `answer:<text>`: answer
//!   every remembered caller `<text>`; `answered:<n>`.
//! - `answer-now:<text>`: answer this call `<text>` with `process.reply`
//!   during the callback, then return `noreply`.
//! - `self`: this instance's pid.
//! - `call:<pid>:<timeout-ms>:<command>`: call `<pid>` with `<command>`;
//!   `ok:<reply body>` or `err:<call error>`.
//! - `cast:<pid>:<command>`: cast `<command>`; `sent`.
//! - `register:<name>`: `ok`, `err:access-denied` or `err:taken:<pid>`.
//!   `whereis:<name>`: the pid or `none`.
//! - `timer:<ms>:<text>`: a timer carrying `<text>`; its reference.
//!   `cancel:<ref>`: `cancelled`. `timers`: the texts of the timers
//!   handled, in order, joined by `,`.
//! - `link:<pid>`, `unlink:<pid>`, `trap-exits:<on|off>`,
//!   `demonitor:<ref>`: `ok`. `monitor:<pid>`: the monitor's reference.
//! - `exit:<pid>:<reason>`: send an exit signal (`normal`, `shutdown`,
//!   `killed`, or any other text as `crashed(<text>)`); `ok`.
//! - `infos`: the exits, downs and host events handled, as
//!   `exit:<pid>:<reason>`, `down:<ref>:<pid>:<reason>` and
//!   `event:<name>:<body>`, joined by `;`. `log-level` and `kv-changed`
//!   events are not listed.
//! - `on-terminate:<pid>`: when `terminate` runs, cast
//!   `note:terminate:<reason>` to `<pid>`; `ok`.
//! - `trap`: trap. `spin`: loop forever. `alloc:<mib>`: allocate and keep
//!   `<mib>` MiB; `ok`.
//! - `refuse-upgrade`: the next `load-snapshot` of this state refuses; `ok`.
//! - `loaded-from`: the version `load-snapshot` was given, or `none`.
//! - `health:<text>`: health checks answer `<text>` from now on (`ok` is
//!   healthy). `health-checks`: how many have been answered.
//! - `log:<level>:<text>`: a `wasi:logging` record; `ok`. `log-level`: the
//!   lowest level whose records the app sends to the host, from the
//!   `log-level` host event (`trace` before any).
//! - `kv-set:<bucket>:<key>:<value>`, `kv-delete:<bucket>:<key>`: `ok`.
//!   `kv-get:<bucket>:<key>`: the value or `none`. `kv-keys:<bucket>`:
//!   the keys, sorted, joined by `,`.
//! - `watch:<bucket>:<key>`: start a watch; its id. `changes`: each
//!   `kv-changed` notice handled, `<key>@<revision>`, joined by `,`.
//! - `join:<group>`, `leave:<group>`: `ok`. `members:<group>`: the
//!   members' pids, sorted, joined by `,`. `publish:<group>:<command>`:
//!   cast `<command>` to every member; how many took it.
//!
//! Health checks (`paddock/health`) are answered as GUEST_FORMATS.md says.
//! The snapshot is GUEST_FORMATS.md's `[1, state]`, the state a CBOR map
//! `{"count": uint, "notes": [text], "deferred": [uint], "refuse": bool}`
//! in that order, so the suite can read it and an app in one language can
//! take over another's state. `load-snapshot` refuses another format
//! version, and a state whose `refuse` is true.

use std::time::Duration;

use paddock_sdk::log::{self, Level};
use paddock_sdk::watch::{self, Change, WatchError};
use paddock_sdk::{
    App, CallError, CallRef, Ctx, Envelope, ExitReason, Info, MonitorRef, Pid, RegisterError,
    Reply, Snapshot, TimerRef, groups, kv,
};
use serde::{Deserialize, Serialize};

/// What a snapshot carries.
#[derive(Debug, Default, Serialize, Deserialize, Snapshot)]
#[snapshot(version = 1)]
struct Kept {
    count: u64,
    notes: Vec<String>,
    deferred: Vec<u64>,
    refuse: bool,
}

/// The conformance app.
#[derive(Default)]
struct Conformance {
    kept: Kept,
    args: String,
    fired: Vec<String>,
    infos: Vec<String>,
    changes: Vec<String>,
    health: Option<String>,
    health_checks: u64,
    loaded_from: Option<String>,
    hoard: Vec<Vec<u8>>,
    on_terminate: Option<Pid>,
}

fn spin() -> ! {
    let mut n = 0u64;
    loop {
        n = std::hint::black_box(n.wrapping_add(1));
    }
}

fn show_pid(pid: Pid) -> String {
    format!(
        "{}.{}.{}@{}",
        pid.node,
        pid.id & ((1 << 40) - 1),
        pid.incarnation,
        pid.id >> 40
    )
}

fn parse_pid(s: &str) -> Pid {
    let (fields, creation) = s.split_once('@').unwrap_or((s, "0"));
    let mut parts = fields.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    let node = u32::try_from(next()).unwrap_or(0);
    let serial = next();
    let incarnation = u32::try_from(next()).unwrap_or(0);
    Pid {
        node,
        id: (creation.parse::<u64>().unwrap_or(0) << 40) | serial,
        incarnation,
    }
}

fn show_exit(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Crashed(why) => format!("crashed({why})"),
        other => other.to_string(),
    }
}

fn parse_exit(s: &str) -> ExitReason {
    match s {
        "normal" => ExitReason::Normal,
        "shutdown" => ExitReason::Shutdown,
        "killed" => ExitReason::Killed,
        other => ExitReason::Crashed(other.to_string()),
    }
}

fn show_call_error(err: &CallError) -> String {
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

fn register(ctx: &mut Ctx, name: &str) -> String {
    match ctx.register(name) {
        Ok(()) => "ok".into(),
        Err(RegisterError::AccessDenied) => "err:access-denied".into(),
        Err(RegisterError::Taken(pid)) => format!("err:taken:{}", show_pid(pid)),
    }
}

fn number(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

const LEVELS: [(Level, &str); 6] = [
    (Level::Trace, "trace"),
    (Level::Debug, "debug"),
    (Level::Info, "info"),
    (Level::Warn, "warn"),
    (Level::Error, "error"),
    (Level::Critical, "critical"),
];

fn kv_command(verb: &str, arg: &str) -> Result<String, kv::KvError> {
    let mut parts = arg.splitn(3, ':');
    let bucket = kv::Bucket::open(parts.next().unwrap_or(""))?;
    let key = parts.next().unwrap_or("");
    Ok(match verb {
        "kv-set" => {
            bucket.set(key, parts.next().unwrap_or("").as_bytes())?;
            "ok".into()
        }
        "kv-get" => bucket.get(key)?.map_or_else(
            || "none".into(),
            |v| String::from_utf8_lossy(&v).into_owned(),
        ),
        "kv-delete" => {
            bucket.delete(key)?;
            "ok".into()
        }
        _ => {
            let mut keys = bucket.keys()?;
            keys.sort();
            keys.join(",")
        }
    })
}

fn kv_error(e: &kv::KvError) -> String {
    match e {
        kv::KvError::AccessDenied => "err:access-denied".into(),
        other => format!("err:{other}"),
    }
}

fn watch_error(e: &WatchError) -> String {
    match e {
        WatchError::AccessDenied => "err:access-denied".into(),
        other => format!("err:{other}"),
    }
}

fn group_command(verb: &str, arg: &str) -> Result<String, groups::GroupError> {
    let (group, rest) = arg.split_once(':').unwrap_or((arg, ""));
    Ok(match verb {
        "join" => groups::join(group).map(|()| "ok".to_string())?,
        "leave" => groups::leave(group).map(|()| "ok".to_string())?,
        "members" => {
            let mut members: Vec<String> =
                groups::members(group)?.into_iter().map(show_pid).collect();
            members.sort();
            members.join(",")
        }
        _ => groups::publish(group, Envelope::text(rest))?.to_string(),
    })
}

fn group_error(e: &groups::GroupError) -> String {
    match e {
        groups::GroupError::AccessDenied => "err:access-denied".into(),
        other => format!("err:{other}"),
    }
}

/// What a command does with the call.
enum Outcome {
    Reply(Envelope),
    NoReply,
}

impl Conformance {
    #[allow(clippy::too_many_lines)] // one arm per command, as the protocol lists them
    fn run(&mut self, ctx: &mut Ctx, caller: Option<CallRef>, msg: &Envelope) -> Outcome {
        let command = String::from_utf8_lossy(&msg.body).into_owned();
        let (verb, arg) = command.split_once(':').unwrap_or((&command, ""));
        let reply = |s: String| Outcome::Reply(Envelope::text(s));
        match verb {
            "echo" => reply(arg.to_string()),
            "mirror" => Outcome::Reply(msg.clone()),
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
            "noreply" => Outcome::NoReply,
            "defer" => {
                if let Some(caller) = caller {
                    self.kept.deferred.push(caller.0);
                }
                Outcome::NoReply
            }
            "answer" => {
                let deferred = std::mem::take(&mut self.kept.deferred);
                for caller in &deferred {
                    ctx.reply(CallRef(*caller), Envelope::text(arg));
                }
                reply(format!("answered:{}", deferred.len()))
            }
            "answer-now" => {
                if let Some(caller) = caller {
                    ctx.reply(caller, Envelope::text(arg));
                }
                Outcome::NoReply
            }
            "self" => reply(show_pid(ctx.pid())),
            "call" => {
                let mut parts = arg.splitn(3, ':');
                let pid = parse_pid(parts.next().unwrap_or(""));
                let timeout = Duration::from_millis(number(parts.next().unwrap_or("")));
                let inner = Envelope::text(parts.next().unwrap_or(""));
                reply(match ctx.call(pid, inner, timeout) {
                    Ok(env) => format!("ok:{}", String::from_utf8_lossy(&env.body)),
                    Err(err) => format!("err:{}", show_call_error(&err)),
                })
            }
            "cast" => {
                let (pid, inner) = arg.split_once(':').unwrap_or((arg, ""));
                ctx.cast(parse_pid(pid), Envelope::text(inner));
                reply("sent".into())
            }
            "register" => reply(register(ctx, arg)),
            "whereis" => reply(ctx.whereis(arg).map_or("none".into(), show_pid)),
            "timer" => {
                let (ms, text) = arg.split_once(':').unwrap_or((arg, ""));
                let timer = ctx.send_after(Duration::from_millis(number(ms)), Envelope::text(text));
                reply(timer.0.to_string())
            }
            "cancel" => {
                ctx.cancel_timer(TimerRef(number(arg)));
                reply("cancelled".into())
            }
            "timers" => reply(self.fired.join(",")),
            "link" => {
                ctx.link(parse_pid(arg));
                reply("ok".into())
            }
            "unlink" => {
                ctx.unlink(parse_pid(arg));
                reply("ok".into())
            }
            "trap-exits" => {
                ctx.trap_exits(arg == "on");
                reply("ok".into())
            }
            "monitor" => reply(ctx.monitor(parse_pid(arg)).0.to_string()),
            "demonitor" => {
                ctx.demonitor(MonitorRef(number(arg)));
                reply("ok".into())
            }
            "exit" => {
                let (pid, reason) = arg.split_once(':').unwrap_or((arg, "normal"));
                ctx.exit(parse_pid(pid), &parse_exit(reason));
                reply("ok".into())
            }
            "infos" => reply(self.infos.join(";")),
            "on-terminate" => {
                self.on_terminate = Some(parse_pid(arg));
                reply("ok".into())
            }
            "trap" => panic!("asked to trap"),
            "spin" => spin(),
            "alloc" => {
                let block = vec![1u8; usize::try_from(number(arg)).unwrap_or(0) << 20];
                self.hoard.push(std::hint::black_box(block));
                reply("ok".into())
            }
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
                let (level, text) = arg.split_once(':').unwrap_or((arg, ""));
                let level = LEVELS
                    .iter()
                    .find(|(_, name)| *name == level)
                    .map_or(Level::Info, |(l, _)| *l);
                log::write(level, text);
                reply("ok".into())
            }
            "log-level" => reply(
                LEVELS
                    .iter()
                    .find(|(l, _)| log::enabled(*l))
                    .map_or("critical", |(_, name)| name)
                    .to_string(),
            ),
            "kv-set" | "kv-get" | "kv-delete" | "kv-keys" => {
                reply(kv_command(verb, arg).unwrap_or_else(|e| kv_error(&e)))
            }
            "watch" => {
                let (bucket, key) = arg.split_once(':').unwrap_or((arg, ""));
                reply(
                    watch::watch(bucket, key)
                        .map_or_else(|e| watch_error(&e), |started| started.id.to_string()),
                )
            }
            "changes" => reply(self.changes.join(",")),
            "join" | "leave" | "members" | "publish" => {
                reply(group_command(verb, arg).unwrap_or_else(|e| group_error(&e)))
            }
            _ => reply(format!("unknown command '{command}'")),
        }
    }
}

impl App for Conformance {
    fn init(ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let raw = String::from_utf8_lossy(args).into_owned();
        // Manifest `start_args` arrive as JSON; accept a JSON string too.
        for word in raw.trim().trim_matches('"').split_whitespace() {
            match word.split_once(':') {
                Some(("register", name)) => {
                    let result = register(ctx, name);
                    if result != "ok" {
                        return Err(result);
                    }
                }
                _ => match word {
                    "fail" => return Err("asked to fail".into()),
                    "trap" => panic!("asked to trap in init"),
                    "trap-exits" => ctx.trap_exits(true),
                    _ => {}
                },
            }
        }
        Ok(Self {
            args: raw,
            ..Self::default()
        })
    }

    fn handle_call(&mut self, ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        match self.run(ctx, Some(call), &msg) {
            Outcome::Reply(env) => Reply::Now(env),
            Outcome::NoReply => Reply::Later,
        }
    }

    fn handle_cast(&mut self, ctx: &mut Ctx, msg: Envelope) {
        self.run(ctx, None, &msg);
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        if let Some(change) = Change::from_info(&info) {
            self.changes
                .push(format!("{}@{}", change.key, change.revision));
            return;
        }
        match info {
            Info::Timer { msg, .. } => self
                .fired
                .push(String::from_utf8_lossy(&msg.body).into_owned()),
            Info::Exit { from, reason } => {
                self.infos
                    .push(format!("exit:{}:{}", show_pid(from), show_exit(&reason)))
            }
            Info::Down {
                monitor,
                pid,
                reason,
            } => self.infos.push(format!(
                "down:{}:{}:{}",
                monitor.0,
                show_pid(pid),
                show_exit(&reason)
            )),
            Info::Event { name, msg } => self.infos.push(format!(
                "event:{name}:{}",
                String::from_utf8_lossy(&msg.body)
            )),
        }
    }

    fn terminate(&mut self, ctx: &mut Ctx, reason: ExitReason) {
        if let Some(to) = self.on_terminate {
            ctx.cast(
                to,
                Envelope::text(format!("note:terminate:{}", show_exit(&reason))),
            );
        }
    }

    fn health(&mut self, _ctx: &mut Ctx) -> Result<(), String> {
        self.health_checks += 1;
        self.health.clone().map_or(Ok(()), Err)
    }

    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        self.kept.to_snapshot()
    }

    fn load_snapshot(_ctx: &mut Ctx, from_version: &str, snapshot: &[u8]) -> Result<Self, String> {
        let kept = Kept::from_snapshot(snapshot)?;
        if kept.refuse {
            return Err("asked to refuse".into());
        }
        Ok(Self {
            kept,
            loaded_from: Some(from_version.to_string()),
            ..Self::default()
        })
    }
}

paddock_sdk::app!(Conformance);
