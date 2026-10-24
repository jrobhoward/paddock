//! A native-process app for tests: each call's body is a command (text),
//! answered with text.
//!
//! | Command | Reply |
//! | --- | --- |
//! | `echo TEXT` | `TEXT` |
//! | `self` | this instance's pid, `node.id.incarnation` |
//! | `call PID TEXT` | the reply's body, or `err:` and the call error |
//! | `cast PID TEXT` | `ok` |
//! | `after MS TEXT` | the timer's reference |
//! | `link PID`, `monitor PID`, `trap` | `ok`, or the monitor's reference |
//! | `infos` | each info received so far, one per line |
//! | `register NAME`, `whereis NAME` | `ok` or `err:...`, the pid or `none` |
//! | `exit PID REASON` | `ok` (`normal`, `kill` or any text as `crashed`) |
//! | `noreply` | nothing now: the reply comes from a cast `reply TEXT` |
//! | `log TEXT`, `print TEXT` | `ok`, after a warn record or a line on stdout |
//! | `panic`, `abort`, `spin` | never |
//! | `alloc MIB` | `ok` once allocated and touched |
//! | `read PATH`, `write PATH`, `connect ADDR`, `exec`, `kill PID` | `ok` or `err:` and the OS error |
//!
//! Init arguments `fail` make `init` fail.

use std::process::ExitCode;

use paddock_native::app::{self, App, CallRef, Ctx};
use paddock_native::protocol::{Envelope, ExitReason, Info, Level, Pid};

#[derive(Default)]
struct Fixture {
    infos: Vec<String>,
    waiting: Option<CallRef>,
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

impl Fixture {
    fn run(&mut self, ctx: &mut Ctx, call: CallRef, command: &str) -> Option<String> {
        let (verb, rest) = command.split_once(' ').unwrap_or((command, ""));
        let (first, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let pid = || parse_pid(first).unwrap_or_else(|| panic!("not a pid: {first}"));
        Some(match verb {
            "echo" => rest.to_string(),
            "self" => pid_text(ctx.self_pid()),
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
            "panic" => panic!("asked to"),
            "abort" => std::process::abort(),
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
            Info::HostEvent { name, .. } => format!("event {name}"),
        });
    }
}

fn main() -> ExitCode {
    app::serve::<Fixture>()
}
