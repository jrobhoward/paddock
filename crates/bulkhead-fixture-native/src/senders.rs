//! The fixture's `sender` commands (ROADMAP AP1): sender channels, used
//! from threads of the fixture's own while the callback thread waits for
//! them, so each request is answered during a callback that is running.
//!
//! | Command | Reply |
//! | --- | --- |
//! | `sender open` | the new sender's number, or `err:` and the error |
//! | `sender clone N` | a sender opened through sender `N`: its number, or `err:` |
//! | `sender drop N` | `ok`, once sender `N` is dropped |
//! | `sender N CMD` | `CMD` run on a thread with sender `N` |
//! | `sender busy MS N CMD` | as `sender N CMD`, while the callback thread spins `MS` ms: the microseconds `CMD` took, a space, its reply |
//!
//! `CMD` is `self`; `selfs N` and `gets B N` (the nanoseconds `N`
//! requests for its own pid, or `N` gets of key `t` in bucket `B`, took);
//! `cast PID TEXT`; `call PID TEXT`; `call-app APP KEY CMD`;
//! `calls-app APP N` (`N` calls to the app started together, keys `0` to
//! `N-1`, each `echo I`, checked against its id: `OK/N`);
//! `calls PID,PID,.. N`
//! (`N` calls of `echo I` started together, each answer checked against
//! its id: `OK/N`); `kv get B K`, `kv set B K V`, `kv incr B K N`,
//! `kv watch B K`; `metric register NAME` (a counter's id), `metric add
//! ID N`; `bad` (a `link` request, which a sender may not make).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bulkhead_native::app::Ctx;
use bulkhead_native::protocol::{KvAnswer, KvRequest, MetricKind, Request};
use bulkhead_native::sender::Sender;

use crate::fixture::{keyed, parse_pid, pid_text, text};

/// The fixture's senders, by number; a dropped one leaves its place.
#[derive(Debug, Default)]
pub(crate) struct Senders(Vec<Option<Sender>>);

impl Senders {
    pub(crate) fn command(&mut self, ctx: &mut Ctx, rest: &str) -> String {
        let (first, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        match first {
            "open" => match ctx.sender() {
                Ok(s) => self.keep(s),
                Err(e) => format!("err:{e:?}"),
            },
            "clone" => match self.get(tail).map(Sender::try_clone) {
                Some(Ok(s)) => self.keep(s),
                Some(Err(e)) => format!("err:{e:?}"),
                None => "err:no such sender".into(),
            },
            "drop" => {
                if let Some(slot) = tail.parse::<usize>().ok().and_then(|n| self.0.get_mut(n)) {
                    *slot = None;
                }
                "ok".into()
            }
            "busy" => {
                let (ms, rest) = tail.split_once(' ').unwrap_or((tail, ""));
                let (n, cmd) = rest.split_once(' ').unwrap_or((rest, ""));
                let busy = Duration::from_millis(ms.parse().unwrap_or(0));
                match self.on_thread(n, cmd, busy) {
                    Some((took, reply)) => format!("{} {reply}", took.as_micros()),
                    None => "err:no such sender".into(),
                }
            }
            n => match self.on_thread(n, tail, Duration::ZERO) {
                Some((_, reply)) => reply,
                None => "err:no such sender".into(),
            },
        }
    }

    fn keep(&mut self, s: Sender) -> String {
        self.0.push(Some(s));
        (self.0.len() - 1).to_string()
    }

    fn get(&mut self, n: &str) -> Option<&mut Sender> {
        self.0.get_mut(n.parse::<usize>().ok()?)?.as_mut()
    }

    /// Run `cmd` with sender `n` on a thread of its own, the callback
    /// thread spinning for `busy` first, then waiting for it.
    fn on_thread(&mut self, n: &str, cmd: &str, busy: Duration) -> Option<(Duration, String)> {
        let slot = self.0.get_mut(n.parse::<usize>().ok()?)?;
        let mut sender = slot.take()?;
        let cmd = cmd.to_string();
        let thread = std::thread::spawn(move || {
            let started = Instant::now();
            let reply = run(&mut sender, &cmd);
            (sender, started.elapsed(), reply)
        });
        let until = Instant::now() + busy;
        while Instant::now() < until {
            std::hint::spin_loop();
        }
        let (sender, took, reply) = thread.join().ok()?;
        *slot = Some(sender);
        Some((took, reply))
    }
}

fn run(s: &mut Sender, cmd: &str) -> String {
    let words: Vec<&str> = cmd.split(' ').collect();
    let arg = |i: usize| words.get(i).copied().unwrap_or("");
    let rest_from = |i: usize| words.get(i..).map(|w| w.join(" ")).unwrap_or_default();
    let pid = |w: &str| {
        parse_pid(w).unwrap_or(bulkhead_native::protocol::Pid {
            node: 0,
            id: 0,
            incarnation: 0,
        })
    };
    match arg(0) {
        "self" => pid_text(s.self_pid()),
        "selfs" => {
            let n: u64 = arg(1).parse().unwrap_or(0);
            let started = Instant::now();
            for _ in 0..n {
                s.self_pid();
            }
            started.elapsed().as_nanos().to_string()
        }
        "gets" => {
            let n: u64 = arg(2).parse().unwrap_or(0);
            let started = Instant::now();
            for _ in 0..n {
                if let Err(e) = s.kv_op(arg(1), bulkhead_native::hot::KvOp::Get, "t", &[]) {
                    return format!("err:{e:?}");
                }
            }
            started.elapsed().as_nanos().to_string()
        }
        "cast" => match s.try_cast(pid(arg(1)), text(rest_from(2))) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err:{e:?}"),
        },
        "call" => match s.call(pid(arg(1)), text(rest_from(2)), 5_000) {
            Ok(reply) => String::from_utf8_lossy(&reply.body).into_owned(),
            Err(e) => format!("err:{e:?}"),
        },
        "call-app" => match s.call_app(arg(1), keyed(arg(2), &rest_from(3)), 5_000) {
            Ok(reply) => String::from_utf8_lossy(&reply.body).into_owned(),
            Err(e) => format!("err:{e:?}"),
        },
        "calls-app" => {
            let n: usize = arg(2).parse().unwrap_or(0);
            let mut expected = HashMap::new();
            for i in 0..n {
                let id =
                    s.start_call_app(arg(1), keyed(&i.to_string(), &format!("echo {i}")), 5_000);
                expected.insert(id, i.to_string());
            }
            let mut ok = 0;
            while let Some((id, result)) = s.next_answer() {
                if let Ok(reply) = result
                    && expected.remove(&id).as_deref()
                        == Some(&*String::from_utf8_lossy(&reply.body))
                {
                    ok += 1;
                }
            }
            format!("{ok}/{n}")
        }
        "calls" => {
            let targets: Vec<_> = arg(1).split(',').map(pid).collect();
            let n: usize = arg(2).parse().unwrap_or(0);
            let mut expected = HashMap::new();
            for i in 0..n {
                let id = s.start_call(targets[i % targets.len()], text(format!("echo {i}")), 5_000);
                expected.insert(id, i.to_string());
            }
            let mut ok = 0;
            while let Some((id, result)) = s.next_answer() {
                if let Ok(reply) = result
                    && expected.remove(&id).as_deref()
                        == Some(&*String::from_utf8_lossy(&reply.body))
                {
                    ok += 1;
                }
            }
            format!("{ok}/{n}")
        }
        "kv" => {
            let (bucket, key) = (arg(2).to_string(), arg(3).to_string());
            let answer = match arg(1) {
                "get" => s.kv(KvRequest::Get { bucket, key }),
                "set" => s.kv(KvRequest::Set {
                    bucket,
                    key,
                    value: arg(4).as_bytes().into(),
                }),
                "incr" => s.kv(KvRequest::Increment {
                    bucket,
                    key,
                    delta: arg(4).parse().unwrap_or(1),
                }),
                "watch" => s.kv(KvRequest::Watch {
                    bucket,
                    key,
                    prefix: false,
                    after: None,
                }),
                other => return format!("err:unknown kv op {other}"),
            };
            match answer {
                Ok(KvAnswer::Value(Some(v))) => String::from_utf8_lossy(&v.0).into_owned(),
                Ok(KvAnswer::Value(None)) => "none".into(),
                Ok(KvAnswer::Unit) => "ok".into(),
                Ok(KvAnswer::Count(n)) => n.to_string(),
                Ok(other) => format!("{other:?}"),
                Err(e) => format!("err:{e:?}"),
            }
        }
        "metric" => match arg(1) {
            "register" => s
                .register_metric(MetricKind::Counter, arg(2), "a fixture metric", &[], &[])
                .map_or_else(|e| format!("err:{e:?}"), |id| id.to_string()),
            "add" => {
                s.metric_add(arg(2).parse().unwrap_or(0), arg(3).parse().unwrap_or(0.0));
                "ok".into()
            }
            other => format!("err:unknown metric op {other}"),
        },
        "bad" => {
            let me = s.self_pid();
            s.send_for_tests(Request::Link(me));
            // The node ends the instance; nothing answers.
            std::thread::sleep(Duration::from_secs(10));
            "err:still running".into()
        }
        other => format!("err:unknown sender command {other}"),
    }
}
