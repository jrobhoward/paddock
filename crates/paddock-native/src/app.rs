//! A native app as an executable: implement [`App`] and call [`serve`]
//! from `main`.
//!
//! ```no_run
//! use paddock_native::app::{App, CallRef, Ctx, serve};
//! use paddock_native::protocol::Envelope;
//!
//! struct Echo;
//!
//! impl App for Echo {
//!     fn init(_ctx: &mut Ctx, _args: Vec<u8>) -> Result<Self, String> {
//!         Ok(Echo)
//!     }
//!
//!     fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Option<Envelope> {
//!         Some(msg)
//!     }
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     serve::<Echo>()
//! }
//! ```
//!
//! The node runs one callback at a time, as for a component. The process
//! functions are on [`Ctx`], valid during a callback; [`log`] works from
//! any thread at any time. Frames go over the stdout the node gave the
//! process, which [`serve`] moves to a descriptor of its own, pointing
//! stdout at stderr: what the app prints becomes log lines, never a
//! corrupt frame. The app must not read stdin.
//!
//! A panic in a callback crashes the instance with the panic's message.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock, PoisonError, mpsc};

use crate::protocol::{
    self, Answer, CallError, Envelope, ExitReason, FromApp, Info, Level, Pid, RegisterError,
    Request, ToApp, VERSION,
};

/// A call's reference, for a later [`Ctx::reply`].
pub type CallRef = u64;

/// A native app's callbacks.
pub trait App: Sized {
    /// Start, from the app's init arguments (a supervisor child's
    /// `start_args` as JSON, or empty). An error ends the instance.
    ///
    /// # Errors
    ///
    /// Why the app cannot start.
    fn init(ctx: &mut Ctx, args: Vec<u8>) -> Result<Self, String>;

    /// Answer a call: `Some` replies now, `None` replies later with
    /// [`Ctx::reply`].
    fn handle_call(&mut self, ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Option<Envelope>;

    /// Handle a cast.
    fn handle_cast(&mut self, _ctx: &mut Ctx, _msg: Envelope) {}

    /// Handle an exit signal, a monitor's notice, a timer or a host event.
    fn handle_info(&mut self, _ctx: &mut Ctx, _info: Info) {}

    /// Stop gracefully.
    fn terminate(&mut self, _ctx: &mut Ctx, _reason: ExitReason) {}
}

/// The descriptor frames go out on.
static OUT: OnceLock<Mutex<File>> = OnceLock::new();

/// Send one frame to the node. Without a node (before [`serve`], or once
/// the node has gone) a log record goes to stderr and anything else is
/// dropped.
fn send(frame: &FromApp) {
    let Some(out) = OUT.get() else {
        if let FromApp::Log { level, message } = frame {
            eprintln!("{level:?}: {message}");
        }
        return;
    };
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    // A failed write means the node has gone; the reader sees the end of
    // stdin and the process exits.
    let _ = protocol::write(&mut *out, frame);
}

/// Log a record through the node's logging, from any thread.
pub fn log(level: Level, message: impl Into<String>) {
    send(&FromApp::Log {
        level,
        message: message.into(),
    });
}

/// The process functions, during a callback.
#[derive(Debug)]
pub struct Ctx {
    frames: mpsc::Receiver<ToApp>,
    next_id: u64,
}

impl Ctx {
    /// Ask the node and wait for its answer. Anything else from the node
    /// here breaks the protocol, and the end of stdin means the node has
    /// gone: the process exits either way.
    fn request(&mut self, request: Request) -> Answer {
        self.next_id += 1;
        let id = self.next_id;
        send(&FromApp::Request { id, request });
        match self.frames.recv() {
            Ok(ToApp::Answer { id: got, answer }) if got == id => answer,
            Ok(other) => protocol_error(&format!("expected answer {id}, got {other:?}")),
            Err(_) => gone(),
        }
    }

    /// This instance's pid.
    pub fn self_pid(&mut self) -> Pid {
        match self.request(Request::SelfPid) {
            Answer::Pid(pid) => pid,
            other => unexpected(&other),
        }
    }

    /// Send `msg` to `to` without waiting; dropped if `to` is gone.
    pub fn cast(&mut self, to: Pid, msg: Envelope) {
        self.request(Request::Cast { to, msg });
    }

    /// Call `to` and wait up to `timeout_ms` for its reply.
    ///
    /// # Errors
    ///
    /// The [`CallError`] if there is no reply.
    pub fn call(&mut self, to: Pid, msg: Envelope, timeout_ms: u32) -> Result<Envelope, CallError> {
        match self.request(Request::Call {
            to,
            msg,
            timeout_ms,
        }) {
            Answer::Called(result) => result,
            other => unexpected(&other),
        }
    }

    /// Reply to a call taken with no reply.
    pub fn reply(&mut self, call_ref: CallRef, msg: Envelope) {
        self.request(Request::Reply { call_ref, msg });
    }

    /// Link to `other`: if either exits abnormally, the other gets an
    /// exit signal.
    pub fn link(&mut self, other: Pid) {
        self.request(Request::Link(other));
    }

    /// Remove a link.
    pub fn unlink(&mut self, other: Pid) {
        self.request(Request::Unlink(other));
    }

    /// Receive exit signals as [`Info::Exit`] instead of exiting.
    pub fn trap_exits(&mut self, on: bool) {
        self.request(Request::TrapExits(on));
    }

    /// Monitor `other`: its exit arrives as [`Info::Down`] with the
    /// returned reference.
    pub fn monitor(&mut self, other: Pid) -> u64 {
        match self.request(Request::Monitor(other)) {
            Answer::Ref(r) => r,
            other => unexpected(&other),
        }
    }

    /// Remove a monitor.
    pub fn demonitor(&mut self, reference: u64) {
        self.request(Request::Demonitor(reference));
    }

    /// Register `name` (one the manifest lists) for this instance.
    ///
    /// # Errors
    ///
    /// The [`RegisterError`] if the name is not listed or is taken.
    pub fn register(&mut self, name: impl Into<String>) -> Result<(), RegisterError> {
        match self.request(Request::Register(name.into())) {
            Answer::Registered(result) => result,
            other => unexpected(&other),
        }
    }

    /// The pid registered under `name`.
    pub fn whereis(&mut self, name: impl Into<String>) -> Option<Pid> {
        match self.request(Request::Whereis(name.into())) {
            Answer::MaybePid(pid) => pid,
            other => unexpected(&other),
        }
    }

    /// Deliver `msg` as [`Info::Timer`] after `ms`; returns the timer's
    /// reference.
    pub fn send_after(&mut self, ms: u32, msg: Envelope) -> u64 {
        match self.request(Request::SendAfter { ms, msg }) {
            Answer::Ref(r) => r,
            other => unexpected(&other),
        }
    }

    /// Cancel a timer.
    pub fn cancel_timer(&mut self, reference: u64) {
        self.request(Request::CancelTimer(reference));
    }

    /// Send an exit signal to `to`; to this instance's own pid, stop it.
    pub fn exit(&mut self, to: Pid, reason: ExitReason) {
        self.request(Request::Exit { to, reason });
    }
}

fn unexpected(answer: &Answer) -> ! {
    protocol_error(&format!("unexpected answer {answer:?}"))
}

fn protocol_error(what: &str) -> ! {
    eprintln!("paddock-native: protocol error: {what}");
    std::process::exit(3)
}

/// The node closed stdin: it has gone, or stopped this instance.
fn gone() -> ! {
    std::process::exit(0)
}

/// Run a callback, turning a panic into [`FromApp::Failed`] and an exit.
fn guarded<R>(f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(panic) => {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_string());
            send(&FromApp::Failed(format!("panic: {why}")));
            std::process::exit(101)
        }
    }
}

/// Take stdout for frames, pointing stdout itself at stderr.
fn take_stdout() -> io::Result<File> {
    let stdout = io::stdout();
    let mut locked = stdout.lock();
    locked.flush()?;
    let frames = rustix::io::dup(locked.as_fd())?;
    rustix::stdio::dup2_stdout(io::stderr().as_fd())?;
    Ok(File::from(frames))
}

/// Serve `A` to the node over stdin and stdout until the node stops it.
#[must_use]
pub fn serve<A: App>() -> ExitCode {
    match take_stdout() {
        Ok(out) => {
            let _ = OUT.set(Mutex::new(out));
        }
        Err(e) => {
            eprintln!("paddock-native: stdout: {e}");
            return ExitCode::from(2);
        }
    }
    let (tx, frames) = mpsc::channel();
    // Pings are answered here, even while a callback runs.
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            match protocol::read::<ToApp>(&mut stdin) {
                Ok(Some(ToApp::Ping { id })) => send(&FromApp::Pong { id }),
                Ok(Some(frame)) => {
                    if tx.send(frame).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => protocol_error(&e.to_string()),
            }
        }
    });
    send(&FromApp::Hello { version: VERSION });
    let mut ctx = Ctx { frames, next_id: 0 };
    let mut app = match ctx.frames.recv() {
        Ok(ToApp::Init { args }) => match guarded(|| A::init(&mut ctx, args)) {
            Ok(app) => {
                send(&FromApp::Init(Ok(())));
                app
            }
            Err(why) => {
                send(&FromApp::Init(Err(why)));
                return ExitCode::from(1);
            }
        },
        Ok(other) => protocol_error(&format!("expected init, got {other:?}")),
        Err(_) => gone(),
    };
    loop {
        let frame = match ctx.frames.recv() {
            Ok(frame) => frame,
            Err(_) => gone(),
        };
        match frame {
            ToApp::Call { call_ref, msg } => {
                let reply = guarded(|| app.handle_call(&mut ctx, call_ref, msg));
                send(&FromApp::Reply(reply));
            }
            ToApp::Cast { msg } => {
                guarded(|| app.handle_cast(&mut ctx, msg));
                send(&FromApp::Done);
            }
            ToApp::Info { info } => {
                guarded(|| app.handle_info(&mut ctx, info));
                send(&FromApp::Done);
            }
            ToApp::Terminate { reason } => {
                guarded(|| app.terminate(&mut ctx, reason));
                send(&FromApp::Done);
                return ExitCode::SUCCESS;
            }
            other => protocol_error(&format!("unexpected {other:?} between callbacks")),
        }
    }
}
