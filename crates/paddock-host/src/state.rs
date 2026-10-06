//! Per-instance store data: WASI context, resource limits, deadline
//! accounting and the `paddock:runtime` host functions.

use std::time::{Duration, Instant};

use wasmtime::component::ResourceTable;
use wasmtime::{Error, ResourceLimiter, Result};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings::paddock::runtime::{process, types};
use crate::pid::Pid;

/// Store data for one instance.
pub(crate) struct InstanceState {
    pub(crate) pid: Pid,
    wasi: WasiCtx,
    table: ResourceTable,
    pub(crate) limiter: Limiter,
    pub(crate) clock: CallbackClock,
}

impl InstanceState {
    pub(crate) fn new(pid: Pid, memory_max: u64, table_max: Option<u32>) -> Self {
        Self {
            pid,
            // No preopens, no environment, no inherited stdio, no sockets.
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            limiter: Limiter::new(memory_max, table_max),
            clock: CallbackClock::default(),
        }
    }
}

impl WasiView for InstanceState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Enforces `limits.memory_max` and `limits.table_max`, and remembers which
/// one was hit so the exit reason can say so.
pub(crate) struct Limiter {
    memory_max: usize,
    table_max: Option<usize>,
    pub(crate) memory_exceeded: bool,
    pub(crate) table_exceeded: bool,
}

impl Limiter {
    fn new(memory_max: u64, table_max: Option<u32>) -> Self {
        Self {
            memory_max: usize::try_from(memory_max).unwrap_or(usize::MAX),
            table_max: table_max.map(|t| usize::try_from(t).unwrap_or(usize::MAX)),
            memory_exceeded: false,
            table_exceeded: false,
        }
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        if desired > self.memory_max {
            self.memory_exceeded = true;
            return Err(Error::msg("memory_limit"));
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        if self.table_max.is_some_and(|max| desired > max) {
            self.table_exceeded = true;
            return Err(Error::msg("table_limit"));
        }
        Ok(true)
    }
}

/// Guest execution time of the current callback.
///
/// The deadline counts guest execution only: host imports add the time they
/// spend suspended to `host_time`, which is subtracted, so an instance waiting
/// on another is not runaway code (spike S1).
#[derive(Debug)]
pub(crate) struct CallbackClock {
    started: Instant,
    host_time: Duration,
    limit: Duration,
    pub(crate) timed_out: bool,
}

impl Default for CallbackClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            host_time: Duration::ZERO,
            limit: Duration::MAX,
            timed_out: false,
        }
    }
}

impl CallbackClock {
    /// Start timing a callback allowed `limit` of guest execution.
    pub(crate) fn start(&mut self, limit: Duration) {
        self.started = Instant::now();
        self.host_time = Duration::ZERO;
        self.limit = limit;
    }

    /// Whether the callback has used up its guest execution time; records a
    /// timeout if so.
    pub(crate) fn expired(&mut self) -> bool {
        let running = self.started.elapsed().saturating_sub(self.host_time);
        if running > self.limit {
            self.timed_out = true;
        }
        self.timed_out
    }

    /// Exclude time spent in a host import from the deadline. Used by the
    /// blocking `process` imports that land in M3.
    #[allow(dead_code)]
    pub(crate) fn add_host_time(&mut self, spent: Duration) {
        self.host_time += spent;
    }
}

impl types::Host for InstanceState {}

/// A `process` function that M3 implements; trapping makes a guest that
/// calls it early crash with a clear reason instead of misbehaving.
fn not_yet<T>(name: &str) -> Result<T> {
    Err(Error::msg(format!(
        "process.{name} is not implemented yet (Phase 1 milestone M3)"
    )))
}

impl process::Host for InstanceState {
    async fn self_(&mut self) -> Result<types::Pid> {
        Ok(self.pid.into())
    }

    async fn cast(&mut self, _to: types::Pid, _msg: types::Envelope) -> Result<()> {
        not_yet("cast")
    }

    async fn call(
        &mut self,
        _to: types::Pid,
        _msg: types::Envelope,
        _timeout_ms: u32,
    ) -> Result<std::result::Result<types::Envelope, types::CallError>> {
        not_yet("call")
    }

    async fn reply(&mut self, _to: types::CallRef, _msg: types::Envelope) -> Result<()> {
        not_yet("reply")
    }

    async fn link(&mut self, _other: types::Pid) -> Result<()> {
        not_yet("link")
    }

    async fn unlink(&mut self, _other: types::Pid) -> Result<()> {
        not_yet("unlink")
    }

    async fn trap_exits(&mut self, _on: bool) -> Result<()> {
        not_yet("trap-exits")
    }

    async fn monitor(&mut self, _other: types::Pid) -> Result<u64> {
        not_yet("monitor")
    }

    async fn demonitor(&mut self, _monitor_ref: u64) -> Result<()> {
        not_yet("demonitor")
    }

    async fn register(
        &mut self,
        _name: String,
    ) -> Result<std::result::Result<(), types::RegisterError>> {
        not_yet("register")
    }

    async fn whereis(&mut self, _name: String) -> Result<Option<types::Pid>> {
        not_yet("whereis")
    }

    async fn send_after(&mut self, _ms: u32, _msg: types::Envelope) -> Result<u64> {
        not_yet("send-after")
    }

    async fn cancel_timer(&mut self, _timer_ref: u64) -> Result<()> {
        not_yet("cancel-timer")
    }

    async fn exit(&mut self, _other: types::Pid, _reason: types::ExitReason) -> Result<()> {
        not_yet("exit")
    }
}
