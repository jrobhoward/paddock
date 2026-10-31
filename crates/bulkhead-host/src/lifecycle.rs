//! Lifecycle states, from rustbridge's plugin lifecycle plus `Upgrading` and
//! `Restarting`.

/// Where an instance (or app) is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LifecycleState {
    /// Deployed, not started.
    #[default]
    Installed,
    /// Instantiating and running `init`.
    Starting,
    /// Handling messages.
    Active,
    /// Swapping to a new version between messages.
    Upgrading,
    /// Idle, with its Store dropped and its state in a snapshot; the next
    /// message wakes it (spike S11).
    Passive,
    /// Running `terminate` before a graceful stop.
    Stopping,
    /// Stopped normally.
    Stopped,
    /// Stopped abnormally (crash, deadline, kill, failed `init`).
    Failed,
    /// Waiting for its supervisor to start it again.
    Restarting,
}

impl LifecycleState {
    /// Whether moving from this state to `target` is allowed.
    #[must_use]
    pub fn can_transition_to(self, target: Self) -> bool {
        use LifecycleState::{
            Active, Failed, Installed, Passive, Restarting, Starting, Stopped, Stopping, Upgrading,
        };
        matches!(
            (self, target),
            (Installed | Stopped | Restarting, Starting)
                | (Starting | Upgrading, Active)
                | (Starting | Active, Stopping)
                | (Active, Upgrading | Passive)
                | (Passive, Active)
                | (Stopping, Stopped)
                | (Failed | Stopped, Restarting)
                | (
                    Installed | Starting | Active | Upgrading | Passive | Stopping | Restarting,
                    Failed
                )
        )
    }

    /// Whether an instance in this state takes messages from its mailbox
    /// (a passivated one wakes for them).
    #[must_use]
    pub fn handles_messages(self) -> bool {
        matches!(self, Self::Active | Self::Passive)
    }

    /// Whether this state is final for one incarnation.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
