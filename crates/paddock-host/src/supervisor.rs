//! Host-native supervisors.
//!
//! A supervisor is a host task, not Wasm: trusted, cheap, and immune to guest
//! bugs. It starts its children in order, watches each one, and restarts them
//! by OTP's rules:
//!
//! - Strategies: `one_for_one` restarts the child that exited; `one_for_all`
//!   restarts every child; `rest_for_one` restarts the child and every child
//!   started after it; `simple_one_for_one` is a dynamic pool of children
//!   from one template, each restarted on its own.
//! - Restart types: `permanent` always, `transient` after an abnormal exit
//!   (anything but `normal` or `shutdown`), `temporary` never.
//! - More than `intensity` restarts within `period` and the supervisor gives
//!   up: it stops its children and exits, escalating to its parent.
//! - Restarts wait a backoff that doubles with each restart in the window, up
//!   to a cap, with optional jitter (an addition beyond OTP).
//!
//! Siblings stopped as part of a group restart are stopped gracefully, in
//! reverse start order, and restarted in start order. A restarted instance
//! keeps its id with the next incarnation.

use std::collections::VecDeque;
use std::hash::BuildHasher;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{RestartType, Strategy};
use tokio::sync::{mpsc, oneshot, watch};

use crate::error::HostError;
use crate::instance::{InstanceEvent, InstanceHandle};
use crate::message::ExitReason;
use crate::pid::Pid;
use crate::probe;
use crate::runtime::{LoadedApp, Runtime};
use crate::spec::InstanceSpec;

/// Delay before a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    /// Delay before the first restart in a window.
    pub initial: Duration,
    /// Cap on the delay, which doubles with each restart in the window.
    pub max: Duration,
    /// Randomize each delay to between half and all of it.
    pub jitter: bool,
}

impl Backoff {
    /// No delay.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            initial: Duration::ZERO,
            max: Duration::ZERO,
            jitter: false,
        }
    }

    /// The delay before restart number `n` (1-based) in the current window.
    /// `unit` is a random number in `[0, 1)`, used when jitter is on.
    #[must_use]
    pub fn delay(&self, n: usize, unit: f64) -> Duration {
        if self.initial.is_zero() {
            return Duration::ZERO;
        }
        let doublings = u32::try_from(n.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(30);
        let delay = self
            .initial
            .saturating_mul(1u32 << doublings)
            .min(self.max.max(self.initial));
        if self.jitter {
            delay.mul_f64(0.5 + 0.5 * unit.clamp(0.0, 1.0))
        } else {
            delay
        }
    }
}

impl From<&paddock_bundle::manifest::Backoff> for Backoff {
    fn from(b: &paddock_bundle::manifest::Backoff) -> Self {
        Self {
            initial: Duration::from_millis(b.initial_ms),
            max: Duration::from_millis(b.max_ms),
            jitter: b.jitter,
        }
    }
}

/// What a supervisor runs.
#[derive(Debug, Clone)]
pub struct SupervisorSpec {
    /// Name, for log records and escalation reasons.
    pub name: String,
    /// Restart strategy.
    pub strategy: Strategy,
    /// Most restarts allowed within `period`.
    pub intensity: u32,
    /// Window for `intensity`.
    pub period: Duration,
    /// Delay before each restart.
    pub backoff: Backoff,
    /// Children in start order. For `simple_one_for_one`, exactly one: the
    /// template for [`SupervisorHandle::start_child`].
    pub children: Vec<ChildSpec>,
}

/// One child of a supervisor.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Identifies the child within its supervisor.
    pub id: String,
    /// When to restart it.
    pub restart: RestartType,
    /// What to start.
    pub start: ChildStart,
}

/// What a child is.
// An instance spec is a few hundred bytes and children are built once per
// start, so boxing it would only make the API clumsier.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ChildStart {
    /// An instance of an app.
    Instance {
        /// The compiled app.
        app: LoadedApp,
        /// Its limits and arguments.
        spec: InstanceSpec,
    },
    /// A nested supervisor.
    Supervisor(Box<SupervisorSpec>),
}

/// A running child.
#[derive(Debug, Clone)]
pub enum ChildHandle {
    /// An instance.
    Instance(InstanceHandle),
    /// A nested supervisor.
    Supervisor(SupervisorHandle),
}

impl ChildHandle {
    /// The instance's pid; `None` for a supervisor.
    #[must_use]
    pub fn pid(&self) -> Option<Pid> {
        match self {
            Self::Instance(h) => Some(h.pid()),
            Self::Supervisor(_) => None,
        }
    }

    async fn wait(&self) -> ExitReason {
        match self {
            Self::Instance(h) => h.wait().await,
            Self::Supervisor(h) => h.wait().await,
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Instance(h) => h.shutdown(ExitReason::Shutdown),
            Self::Supervisor(h) => h.shutdown(),
        }
    }

    fn kill(&self) {
        match self {
            Self::Instance(h) => h.kill(),
            Self::Supervisor(h) => h.kill(),
        }
    }
}

/// A child as its supervisor sees it.
#[derive(Debug, Clone)]
pub struct Child {
    /// The child's id.
    pub id: String,
    /// For an instance child, the app it runs.
    pub app: Option<String>,
    /// The running child, or `None` while it is down.
    pub handle: Option<ChildHandle>,
    /// Down and about to be started again (waiting out a backoff, or part of
    /// a group restart). A child that is down and not restarting has
    /// finished for good.
    pub restarting: bool,
}

impl Child {
    /// The running instance's pid, if this child is a running instance.
    #[must_use]
    pub fn pid(&self) -> Option<Pid> {
        self.handle.as_ref().and_then(ChildHandle::pid)
    }

    /// The running instance, if this child is a running instance.
    #[must_use]
    pub fn instance(&self) -> Option<&InstanceHandle> {
        match &self.handle {
            Some(ChildHandle::Instance(h)) => Some(h),
            _ => None,
        }
    }
}

/// A handle to a running supervisor. Cheap to clone. When every handle is
/// dropped the supervisor shuts down.
#[derive(Debug, Clone)]
pub struct SupervisorHandle {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    name: String,
    strategy: Strategy,
    restarts: Arc<AtomicU64>,
    commands: mpsc::UnboundedSender<Command>,
    children: watch::Receiver<Vec<Child>>,
    exit: watch::Receiver<Option<ExitReason>>,
}

#[derive(Debug)]
enum Command {
    Shutdown,
    Kill,
    StartChild(oneshot::Sender<Result<Child, HostError>>),
    Replace {
        /// `None` for the `simple_one_for_one` template.
        id: Option<String>,
        start: Box<ChildStart>,
        restart: bool,
        reply: oneshot::Sender<Result<Option<Child>, HostError>>,
    },
}

impl SupervisorHandle {
    /// The supervisor's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// The children, in start order.
    #[must_use]
    pub fn children(&self) -> Vec<Child> {
        self.shared.children.borrow().clone()
    }

    /// A receiver that sees every change to the children.
    #[must_use]
    pub fn subscribe_children(&self) -> watch::Receiver<Vec<Child>> {
        self.shared.children.clone()
    }

    /// Start one more child from the template of a `simple_one_for_one`
    /// supervisor.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidSupervisor`] for another strategy or a supervisor
    /// that has exited, and the errors of [`Runtime::spawn`].
    pub async fn start_child(&self) -> Result<Child, HostError> {
        let gone = || HostError::InvalidSupervisor(format!("{} has exited", self.shared.name));
        let (reply, answer) = oneshot::channel();
        self.shared
            .commands
            .send(Command::StartChild(reply))
            .map_err(|_| gone())?;
        answer.await.map_err(|_| gone())?
    }

    /// The restart strategy.
    #[must_use]
    pub fn strategy(&self) -> Strategy {
        self.shared.strategy
    }

    /// Restarts this supervisor has made after child exits since it started.
    /// Restarts asked for with [`SupervisorHandle::replace_child`] are not
    /// counted.
    #[must_use]
    pub fn restart_count(&self) -> u64 {
        self.shared.restarts.load(Ordering::Relaxed)
    }

    /// Replace what child `id` runs. Later restarts use `start`. With
    /// `restart`, the child is also stopped gracefully and started again
    /// now from `start` (an instance keeps its id with the next
    /// incarnation); this does not count toward the restart intensity.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidSupervisor`] for an unknown child or a supervisor
    /// that has exited, and the errors of [`Runtime::spawn`] for a restart.
    pub async fn replace_child(
        &self,
        id: &str,
        start: ChildStart,
        restart: bool,
    ) -> Result<Child, HostError> {
        self.replace(Some(id.to_string()), start, restart)
            .await?
            .ok_or_else(|| {
                HostError::InvalidSupervisor(format!("{}: no child {id}", self.shared.name))
            })
    }

    /// Replace the template of a `simple_one_for_one` supervisor. Running
    /// children keep their own spec; see [`SupervisorHandle::replace_child`].
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidSupervisor`] for another strategy or a supervisor
    /// that has exited.
    pub async fn replace_template(&self, start: ChildStart) -> Result<(), HostError> {
        self.replace(None, start, false).await.map(|_| ())
    }

    async fn replace(
        &self,
        id: Option<String>,
        start: ChildStart,
        restart: bool,
    ) -> Result<Option<Child>, HostError> {
        let gone = || HostError::InvalidSupervisor(format!("{} has exited", self.shared.name));
        let (reply, answer) = oneshot::channel();
        self.shared
            .commands
            .send(Command::Replace {
                id,
                start: Box::new(start),
                restart,
                reply,
            })
            .map_err(|_| gone())?;
        answer.await.map_err(|_| gone())?
    }

    /// Stop every child gracefully, in reverse start order, then exit with
    /// `shutdown`.
    pub fn shutdown(&self) {
        let _ = self.shared.commands.send(Command::Shutdown);
    }

    /// Kill every child, then exit with `killed`.
    pub fn kill(&self) {
        let _ = self.shared.commands.send(Command::Kill);
    }

    /// Why the supervisor exited, or `None` while it runs.
    #[must_use]
    pub fn exit_reason(&self) -> Option<ExitReason> {
        self.shared.exit.borrow().clone()
    }

    /// Wait for the supervisor to exit and return why.
    pub async fn wait(&self) -> ExitReason {
        let mut exit = self.shared.exit.clone();
        match exit.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or(ExitReason::Killed),
            Err(_) => ExitReason::Killed,
        }
    }
}

/// Start a supervisor and its children.
pub(crate) fn start(rt: &Runtime, spec: SupervisorSpec) -> Result<SupervisorHandle, HostError> {
    validate(&spec)?;
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (children_tx, children_rx) = watch::channel(Vec::new());
    let (exit_tx, exit_rx) = watch::channel(None);
    let restarts = Arc::new(AtomicU64::new(0));
    let template = (spec.strategy == Strategy::SimpleOneForOne)
        .then(|| spec.children.first().cloned())
        .flatten();
    let mut task = Task {
        rt: rt.clone(),
        name: spec.name.clone(),
        strategy: spec.strategy,
        intensity: spec.intensity,
        period: spec.period,
        backoff: spec.backoff,
        template,
        slots: Vec::new(),
        next_key: 0,
        restarts: VecDeque::new(),
        last_exit: None,
        events_tx,
        events_rx,
        commands: commands_rx,
        children: children_tx,
        exit: exit_tx,
        jitter_seq: 0,
        restart_total: restarts.clone(),
    };
    if task.template.is_none() {
        for child in spec.children {
            task.add_slot(child.id.clone(), child);
        }
        for i in 0..task.slots.len() {
            if let Err(err) = task.start_slot(i) {
                for slot in &task.slots {
                    if let Some(child) = &slot.child {
                        child.kill();
                    }
                }
                return Err(err);
            }
        }
    }
    task.publish();
    tokio::spawn(task.run());
    Ok(SupervisorHandle {
        shared: Arc::new(Shared {
            name: spec.name,
            strategy: spec.strategy,
            restarts,
            commands: commands_tx,
            children: children_rx,
            exit: exit_rx,
        }),
    })
}

fn validate(spec: &SupervisorSpec) -> Result<(), HostError> {
    let invalid = |why: String| {
        Err(HostError::InvalidSupervisor(format!(
            "{}: {why}",
            spec.name
        )))
    };
    if spec.strategy == Strategy::SimpleOneForOne && spec.children.len() != 1 {
        return invalid("simple_one_for_one needs exactly one child spec".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for child in &spec.children {
        if !ids.insert(child.id.as_str()) {
            return invalid(format!("child id '{}' is used twice", child.id));
        }
    }
    if spec.period.is_zero() {
        return invalid("period must be greater than zero".into());
    }
    Ok(())
}

struct Slot {
    key: u64,
    id: String,
    spec: ChildSpec,
    child: Option<ChildHandle>,
    generation: u64,
    last_pid: Option<Pid>,
    /// Why the child last exited, for the next incarnation's `last_exit`.
    last_reason: Option<ExitReason>,
    restarting: bool,
}

struct ChildExit {
    key: u64,
    generation: u64,
    reason: ExitReason,
}

struct Task {
    rt: Runtime,
    name: String,
    strategy: Strategy,
    intensity: u32,
    period: Duration,
    backoff: Backoff,
    template: Option<ChildSpec>,
    slots: Vec<Slot>,
    next_key: u64,
    restarts: VecDeque<Instant>,
    last_exit: Option<(String, ExitReason)>,
    events_tx: mpsc::UnboundedSender<ChildExit>,
    events_rx: mpsc::UnboundedReceiver<ChildExit>,
    commands: mpsc::UnboundedReceiver<Command>,
    children: watch::Sender<Vec<Child>>,
    exit: watch::Sender<Option<ExitReason>>,
    jitter_seq: u64,
    restart_total: Arc<AtomicU64>,
}

impl Task {
    async fn run(mut self) {
        let span = tracing::info_span!("supervisor", name = %self.name);
        let reason = tracing::Instrument::instrument(self.serve(), span).await;
        self.publish();
        self.exit.send_replace(Some(reason));
    }

    async fn serve(&mut self) -> ExitReason {
        loop {
            tokio::select! {
                biased;
                command = self.commands.recv() => {
                    if let Err(reason) = self.command(command).await {
                        return reason;
                    }
                }
                Some(event) = self.events_rx.recv() => {
                    if let Err(reason) = self.child_exited(event).await {
                        return reason;
                    }
                }
            }
        }
    }

    /// Handle a command; `Err` means the supervisor exits with that reason.
    async fn command(&mut self, command: Option<Command>) -> Result<(), ExitReason> {
        match command {
            None | Some(Command::Shutdown) => {
                self.terminate_all().await;
                Err(ExitReason::Shutdown)
            }
            Some(Command::Kill) => {
                for slot in &self.slots {
                    if let Some(child) = &slot.child {
                        child.kill();
                    }
                }
                for slot in &mut self.slots {
                    slot.generation += 1;
                    if let Some(child) = slot.child.take() {
                        child.wait().await;
                    }
                }
                Err(ExitReason::Killed)
            }
            Some(Command::StartChild(reply)) => {
                let _ = reply.send(self.start_dynamic());
                Ok(())
            }
            Some(Command::Replace {
                id,
                start,
                restart,
                reply,
            }) => {
                let _ = reply.send(self.replace(id, *start, restart).await);
                Ok(())
            }
        }
    }

    async fn replace(
        &mut self,
        id: Option<String>,
        start: ChildStart,
        restart: bool,
    ) -> Result<Option<Child>, HostError> {
        let Some(id) = id else {
            let template = self.template.as_mut().ok_or_else(|| {
                HostError::InvalidSupervisor(format!(
                    "{}: no template; not simple_one_for_one",
                    self.name
                ))
            })?;
            template.start = start;
            return Ok(None);
        };
        let index =
            self.slots.iter().position(|s| s.id == id).ok_or_else(|| {
                HostError::InvalidSupervisor(format!("{}: no child {id}", self.name))
            })?;
        self.slots[index].spec.start = start;
        if restart {
            self.slots[index].restarting = true;
            self.terminate_slot(index).await;
            self.start_slot(index)?;
            self.publish();
        }
        Ok(Some(self.child_view(&self.slots[index])))
    }

    fn start_dynamic(&mut self) -> Result<Child, HostError> {
        let Some(template) = self.template.clone() else {
            return Err(HostError::InvalidSupervisor(format!(
                "{}: start_child needs a simple_one_for_one supervisor",
                self.name
            )));
        };
        let id = format!("{}-{}", template.id, self.next_key + 1);
        let index = self.add_slot(id, template);
        if let Err(err) = self.start_slot(index) {
            self.slots.remove(index);
            return Err(err);
        }
        self.publish();
        Ok(self.child_view(&self.slots[index]))
    }

    fn add_slot(&mut self, id: String, spec: ChildSpec) -> usize {
        self.next_key += 1;
        self.slots.push(Slot {
            key: self.next_key,
            id,
            spec,
            child: None,
            generation: 0,
            last_pid: None,
            last_reason: None,
            restarting: false,
        });
        self.slots.len() - 1
    }

    /// Start (or restart) the child in slot `index` and watch it.
    fn start_slot(&mut self, index: usize) -> Result<(), HostError> {
        let slot = &mut self.slots[index];
        slot.generation += 1;
        let handle = match &slot.spec.start {
            ChildStart::Instance { app, spec } => ChildHandle::Instance(match slot.last_pid {
                Some(previous) => self.rt.respawn(app, spec.clone(), previous)?,
                None => self.rt.spawn(app, spec.clone())?,
            }),
            ChildStart::Supervisor(spec) => {
                ChildHandle::Supervisor(self.rt.start_supervisor((**spec).clone())?)
            }
        };
        if let (Some(previous), ChildHandle::Instance(instance)) = (slot.last_pid, &handle) {
            probe::supervisor_restart(instance.pid(), &slot.spec.id);
            self.rt.instance_event(InstanceEvent::Restarted {
                pid: instance.pid(),
                previous,
                child: slot.spec.id.clone(),
            });
        }
        if let (ChildHandle::Instance(instance), Some(reason)) = (&handle, &slot.last_reason) {
            instance.set_last_exit(reason.clone());
        }
        slot.last_pid = handle.pid().or(slot.last_pid);
        slot.child = Some(handle.clone());
        slot.restarting = false;
        let (key, generation, events) = (slot.key, slot.generation, self.events_tx.clone());
        tokio::spawn(async move {
            let reason = handle.wait().await;
            let _ = events.send(ChildExit {
                key,
                generation,
                reason,
            });
        });
        Ok(())
    }

    /// React to a child's exit; `Err` means the supervisor exits.
    async fn child_exited(&mut self, event: ChildExit) -> Result<(), ExitReason> {
        let Some(index) = self.slots.iter().position(|s| s.key == event.key) else {
            return Ok(());
        };
        if self.slots[index].generation != event.generation {
            return Ok(()); // a child this supervisor stopped itself
        }
        let slot = &mut self.slots[index];
        slot.child = None;
        tracing::info!(child = %slot.id, reason = %event.reason, "child exited");
        self.last_exit = Some((slot.id.clone(), event.reason.clone()));
        slot.last_reason = Some(event.reason.clone());
        if !should_restart(slot.spec.restart, &event.reason) {
            if slot.spec.restart == RestartType::Temporary || self.template.is_some() {
                self.slots.remove(index);
            }
            self.publish();
            return Ok(());
        }
        slot.restarting = true;
        self.restart_total.fetch_add(1, Ordering::Relaxed);
        self.publish();

        let now = Instant::now();
        self.restarts.push_back(now);
        while self
            .restarts
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.period)
        {
            self.restarts.pop_front();
        }
        if self.restarts.len() > self.intensity as usize {
            let reason = self.escalation_reason();
            tracing::error!(%reason, "giving up");
            self.terminate_all().await;
            return Err(reason);
        }
        let unit = self.random_unit();
        let delay = self.backoff.delay(self.restarts.len(), unit);
        self.pause(delay).await?;
        self.restart_group(index).await;
        Ok(())
    }

    fn escalation_reason(&self) -> ExitReason {
        let last = self
            .last_exit
            .as_ref()
            .map(|(id, reason)| format!("; last exit: {id} {reason}"))
            .unwrap_or_default();
        ExitReason::Crashed(format!(
            "supervisor {} reached max restart intensity ({} in {} ms){last}",
            self.name,
            self.intensity,
            self.period.as_millis()
        ))
    }

    /// Wait out a backoff while still answering commands.
    async fn pause(&mut self, delay: Duration) -> Result<(), ExitReason> {
        if delay.is_zero() {
            return Ok(());
        }
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                command = self.commands.recv() => self.command(command).await?,
                () = &mut sleep => return Ok(()),
            }
        }
    }

    /// Restart the child in slot `index` and, depending on the strategy, its
    /// siblings.
    async fn restart_group(&mut self, index: usize) {
        let group: Vec<usize> = match self.strategy {
            Strategy::OneForOne | Strategy::SimpleOneForOne => vec![index],
            Strategy::OneForAll => (0..self.slots.len()).collect(),
            Strategy::RestForOne => (index..self.slots.len()).collect(),
        };
        for &j in group.iter().rev() {
            if j != index {
                self.slots[j].restarting = true;
                self.terminate_slot(j).await;
            }
        }
        let mut gone = Vec::new();
        for &j in &group {
            if j != index && self.slots[j].spec.restart == RestartType::Temporary {
                gone.push(self.slots[j].key);
                continue;
            }
            self.slots[j].restarting = true;
            if let Err(err) = self.start_slot(j) {
                // Counted like any other exit, so a child that cannot start
                // escalates once the intensity is used up.
                let slot = &self.slots[j];
                tracing::warn!(child = %slot.id, error = %err, "restart failed");
                let _ = self.events_tx.send(ChildExit {
                    key: slot.key,
                    generation: slot.generation,
                    reason: ExitReason::Crashed(format!("restart failed: {err}")),
                });
            }
        }
        self.slots.retain(|s| !gone.contains(&s.key));
        self.publish();
    }

    /// Stop one child gracefully and wait for it; its exit is not treated as
    /// a failure.
    async fn terminate_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.generation += 1;
        if let Some(child) = slot.child.take() {
            child.shutdown();
            child.wait().await;
        }
    }

    async fn terminate_all(&mut self) {
        for index in (0..self.slots.len()).rev() {
            self.slots[index].restarting = false;
            self.terminate_slot(index).await;
        }
        self.publish();
    }

    fn child_view(&self, slot: &Slot) -> Child {
        Child {
            id: slot.id.clone(),
            app: match &slot.spec.start {
                ChildStart::Instance { spec, .. } => Some(spec.app.clone()),
                ChildStart::Supervisor(_) => None,
            },
            handle: slot.child.clone(),
            restarting: slot.restarting,
        }
    }

    fn publish(&self) {
        let children = self.slots.iter().map(|s| self.child_view(s)).collect();
        self.children.send_replace(children);
    }

    fn random_unit(&mut self) -> f64 {
        self.jitter_seq += 1;
        let bits = std::hash::RandomState::new().hash_one(self.jitter_seq);
        (bits >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Whether a child with this restart type is restarted after exiting with
/// `reason`.
#[must_use]
pub(crate) fn should_restart(restart: RestartType, reason: &ExitReason) -> bool {
    match restart {
        RestartType::Permanent => true,
        RestartType::Transient => !reason.is_normal(),
        RestartType::Temporary => false,
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod supervisor_tests;
