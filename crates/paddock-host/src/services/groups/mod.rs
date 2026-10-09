//! `paddock:groups`: process groups on one node, with publish.
//!
//! The table is node-wide and keyed by pid, so an upgrade (same pid, new
//! Store) keeps an instance's groups, and an exit removes them
//! (the services' exit hook). `publish` takes the message from the guest once
//! and casts a copy into each member's mailbox, outside the table's lock;
//! a full mailbox drops it and counts a dropped cast on that member, as any
//! cast does. Joins count against `limits.groups_max`
//! ([`HostObjects::groups`](crate::HostObjects)). Cluster-wide groups are
//! ROADMAP D4.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use paddock_bundle::manifest::{AppSpec, Capability, GroupsScope};
use wasmtime::Result;
use wasmtime::component::Linker;

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::instance::InstanceHandle;
use crate::message::Envelope;
use crate::pid::Pid;

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    wasmtime::component::bindgen!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "../../wit/groups",
            "../../wit/watch",
            "wit",
        ],
        world: "paddock:host/groups",
        imports: { default: async | trappable },
        with: { "paddock:runtime/types": crate::bindings::paddock::runtime::types },
    });
}

use bindings::paddock::groups::groups;

/// The groups service.
pub(crate) struct Groups;

/// What the manifest asks of groups for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct GroupsSpec {
    /// The groups the app may join and publish to.
    pub scope: GroupsScope,
}

/// Every group on the node.
#[derive(Default)]
pub(crate) struct Shared {
    tables: Mutex<Tables>,
}

#[derive(Default)]
struct Tables {
    /// Members of each group, by pid.
    members: HashMap<String, BTreeMap<Pid, InstanceHandle>>,
    /// The groups each instance is in.
    joined: HashMap<Pid, BTreeSet<String>>,
}

impl Shared {
    fn join(&self, me: &InstanceHandle, group: &str, max: usize) -> Result<(), groups::Error> {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        let pid = me.pid();
        let joined = t.joined.entry(pid).or_default();
        if joined.contains(group) {
            return Ok(());
        }
        if joined.len() >= max {
            if joined.is_empty() {
                t.joined.remove(&pid);
            }
            return Err(groups::Error::LimitReached);
        }
        joined.insert(group.to_string());
        t.members
            .entry(group.to_string())
            .or_default()
            .insert(pid, me.clone());
        Ok(())
    }

    fn leave(&self, pid: Pid, group: &str) {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(joined) = t.joined.get_mut(&pid) {
            joined.remove(group);
            if joined.is_empty() {
                t.joined.remove(&pid);
            }
        }
        remove_member(&mut t.members, group, pid);
    }

    /// The group's members, ordered by pid.
    fn members(&self, group: &str) -> Vec<InstanceHandle> {
        let t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        t.members
            .get(group)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    fn exited(&self, pid: Pid) {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        for group in t.joined.remove(&pid).unwrap_or_default() {
            remove_member(&mut t.members, &group, pid);
        }
    }
}

fn remove_member(
    members: &mut HashMap<String, BTreeMap<Pid, InstanceHandle>>,
    group: &str,
    pid: Pid,
) {
    if let Some(m) = members.get_mut(group) {
        m.remove(&pid);
        if m.is_empty() {
            members.remove(group);
        }
    }
}

/// Groups state for one instance.
pub(crate) struct Instance {
    shared: Arc<Shared>,
    scope: GroupsScope,
    granted: bool,
    me: InstanceHandle,
    max: usize,
}

impl Instance {
    fn may_join(&self, group: &str) -> Result<(), groups::Error> {
        if self.granted && self.scope.may_join(group) {
            Ok(())
        } else {
            Err(groups::Error::AccessDenied)
        }
    }
}

impl Service for Groups {
    const CAPABILITY: Capability = Capability::Groups;
    const INTERFACES: &'static [&'static str] = &["paddock:groups/groups"];
    type Spec = GroupsSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> GroupsSpec {
        GroupsSpec {
            scope: manifest.capabilities.groups.clone().unwrap_or_default(),
        }
    }

    fn shared(_config: &RuntimeConfig) -> Result<Shared, HostError> {
        Ok(Shared::default())
    }

    fn instance(shared: &Arc<Shared>, spec: &GroupsSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            shared: shared.clone(),
            scope: spec.scope.clone(),
            granted: info.granted,
            me: info.handle.clone(),
            max: usize::try_from(info.host_objects.groups).unwrap_or(usize::MAX),
        }
    }

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        groups::add_to_linker::<T, Ctx<Self>>(linker, T::service)
    }

    fn exited(shared: &Shared, pid: Pid) {
        shared.exited(pid);
    }
}

impl groups::Host for ServiceCtx<'_, Instance> {
    async fn join(&mut self, group: String) -> Result<std::result::Result<(), groups::Error>> {
        let state = &*self.state;
        Ok(state
            .may_join(&group)
            .and_then(|()| state.shared.join(&state.me, &group, state.max)))
    }

    async fn leave(&mut self, group: String) -> Result<std::result::Result<(), groups::Error>> {
        let state = &*self.state;
        Ok(state
            .may_join(&group)
            .map(|()| state.shared.leave(state.me.pid(), &group)))
    }

    async fn members(
        &mut self,
        group: String,
    ) -> Result<std::result::Result<Vec<groups::Pid>, groups::Error>> {
        let state = &*self.state;
        let scope = &state.scope;
        if !state.granted || !(scope.may_join(&group) || scope.may_publish(&group)) {
            return Ok(Err(groups::Error::AccessDenied));
        }
        Ok(Ok(state
            .shared
            .members(&group)
            .iter()
            .map(|m| m.pid().into())
            .collect()))
    }

    async fn publish(
        &mut self,
        group: String,
        msg: groups::Envelope,
    ) -> Result<std::result::Result<u32, groups::Error>> {
        let state = &*self.state;
        if !state.granted || !state.scope.may_publish(&group) {
            return Ok(Err(groups::Error::AccessDenied));
        }
        let msg = Envelope::from(msg);
        let from = Some(state.me.pid());
        let delivered = state
            .shared
            .members(&group)
            .iter()
            .filter(|member| member.cast_from(from, msg.clone()).is_ok())
            .count();
        Ok(Ok(u32::try_from(delivered).unwrap_or(u32::MAX)))
    }
}
