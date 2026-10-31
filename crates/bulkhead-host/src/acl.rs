//! Messaging ACLs: which apps an app's instances may send to,
//! and which apps may send to them, by app name, from the manifest's
//! `messaging_acl` (`may_call`, `accept_from`).
//!
//! A call, cast, exit signal or link from one instance to another passes
//! when both lists allow it: the sender's `may_call` names the target's
//! app, and the target's `accept_from` names the sender's app, an empty
//! list allowing any. Instances of the same app always pass, so a pool's
//! members reach each other. A sender that is not an instance (the admin
//! API, the HTTP ingress, an extension) always passes: the operator
//! configured it. Monitors are not checked; they only observe.
//!
//! The sender's identity is the host's, never the guest's: on one node it
//! is the sending instance's app, and from another node it is the stamp the
//! sending node puts on the frame ([`Stamp`]), whose sender pid must belong
//! to that node. Every check happens on the target's node.

use serde::{Deserialize, Serialize};

/// An app's routes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Acl {
    /// Apps whose instances may send to this app's; empty: any.
    pub accept_from: Vec<String>,
    /// Apps this app's instances may send to; empty: any.
    pub may_call: Vec<String>,
}

/// Who sent a frame, as the sending node vouches: the sending instance's
/// app and its `may_call`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// The sender's app.
    pub app: String,
    /// The sender's app's `may_call`; empty: any.
    pub may_call: Vec<String>,
}

/// Whether an instance of `sender_app`, with `sender_may_call`, may send to
/// an instance of `target_app` that accepts from `target_accept`.
pub(crate) fn allowed(
    sender_app: &str,
    sender_may_call: &[String],
    target_app: &str,
    target_accept: &[String],
) -> bool {
    if sender_app == target_app {
        return true;
    }
    let listed = |list: &[String], app: &str| list.is_empty() || list.iter().any(|a| a == app);
    listed(sender_may_call, target_app) && listed(target_accept, sender_app)
}

#[cfg(test)]
#[path = "acl_tests.rs"]
mod acl_tests;
