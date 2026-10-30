//! `paddock:groups`: process groups across the cluster, with publish
//! (on one node and across nodes, as OTP's `pg`).
//!
//! Each node owns its own members. The table is keyed by pid, so an upgrade
//! (same pid, new Store) keeps an instance's groups, and an exit removes them
//! (the services' exit hook). Joins count against `limits.groups_max`
//! ([`HostObjects::groups`](crate::HostObjects)).
//!
//! Other nodes learn a node's members from it: a full snapshot
//! (`GroupSync`) once per session, sent from the heartbeat round when a
//! peer's session or creation changed, then each join and leave as it
//! happens (`GroupJoin`, `GroupLeave`, sent as notices, so one that cannot
//! be sent ends the session and the next session sends a fresh snapshot).
//! One lock orders every group frame this node sends, so a snapshot never
//! overtakes the updates after it. When a session ends, the node forgets
//! that peer's members; groups merge again when the next session's
//! snapshot arrives. Membership is eventual and never blocks.
//!
//! `publish` takes the message from the guest once, casts a copy into each
//! local member's mailbox the messaging ACL allows ([`crate::acl`]), and
//! sends one `GroupPublish` to each other node with members, which fans it
//! out to its own members under the same check against the publisher's
//! stamp. A full mailbox drops a copy and counts a dropped cast on that
//! member, as any cast does.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use paddock_bundle::manifest::{AppSpec, Capability, GroupsScope};
use wasmtime::Result;
use wasmtime::component::Linker;

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::acl::{self, Stamp};
use crate::config::RuntimeConfig;
use crate::delivery::{self, Origin};
use crate::error::HostError;
use crate::instance::InstanceHandle;
use crate::message::Envelope;
use crate::pid::Pid;
use crate::runtime::RuntimeInner;
use crate::transport::{self, Frame};

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
            "../../wit/secrets",
            "../../wit/metrics",
            "../../wit/sql",
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

/// Every group, as this node knows it.
#[derive(Default)]
pub(crate) struct Shared {
    tables: Mutex<Tables>,
    /// Held while computing and sending group frames to other nodes, so
    /// they leave in the order the table changed. Never taken under
    /// `tables`.
    sending: Mutex<()>,
    rt: OnceLock<Weak<RuntimeInner>>,
}

#[derive(Default)]
struct Tables {
    /// This node's members of each group, by pid.
    members: HashMap<String, BTreeMap<Pid, InstanceHandle>>,
    /// The groups each local instance is in.
    joined: HashMap<Pid, BTreeSet<String>>,
    /// Other nodes' members, by node and group.
    remote: BTreeMap<u32, BTreeMap<String, BTreeSet<Pid>>>,
    /// The peers holding this node's snapshot, and the (creation,
    /// session) it was sent in.
    synced: HashMap<u32, (u32, u64)>,
}

/// Each group and this node's members of it, as a snapshot carries them.
type Snapshot = Vec<(String, Vec<Pid>)>;

/// A peer's current start and session, as this node knows them.
fn session_of(rt: &RuntimeInner, node: u32) -> (u32, u64) {
    (rt.liveness.creation(node), rt.liveness.session(node))
}

impl Shared {
    /// The runtime the table sends through.
    pub(crate) fn attach(&self, rt: &Arc<RuntimeInner>) {
        let _ = self.rt.set(Arc::downgrade(rt));
    }

    fn runtime(&self) -> Option<Arc<RuntimeInner>> {
        self.rt.get().and_then(Weak::upgrade)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tables> {
        self.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Peers holding this node's snapshot in their current session; the
    /// others are dropped from `synced`, to be sent a fresh one.
    fn in_step(rt: &RuntimeInner, t: &mut Tables) -> Vec<u32> {
        t.synced.retain(|&node, at| session_of(rt, node) == *at);
        t.synced.keys().copied().collect()
    }

    /// Change the local table with `change` and tell every peer in step
    /// with the frames it returns.
    fn change(&self, change: impl FnOnce(&mut Tables) -> Vec<Frame>) {
        let _sending = self.sending.lock().unwrap_or_else(PoisonError::into_inner);
        let rt = self.runtime();
        let (frames, peers) = {
            let mut t = self.lock();
            let frames = change(&mut t);
            let peers = match &rt {
                Some(rt) if !frames.is_empty() => Self::in_step(rt, &mut t),
                _ => Vec::new(),
            };
            (frames, peers)
        };
        if let Some(rt) = rt {
            for node in peers {
                for frame in &frames {
                    transport::send_notice(&rt, node, frame.clone());
                }
            }
        }
    }

    fn join(&self, me: &InstanceHandle, group: &str, max: usize) -> Result<(), groups::Error> {
        let mut result = Ok(());
        let pid = me.pid();
        self.change(|t| {
            let joined = t.joined.entry(pid).or_default();
            if joined.contains(group) {
                return Vec::new();
            }
            if joined.len() >= max {
                if joined.is_empty() {
                    t.joined.remove(&pid);
                }
                result = Err(groups::Error::LimitReached);
                return Vec::new();
            }
            joined.insert(group.to_string());
            t.members
                .entry(group.to_string())
                .or_default()
                .insert(pid, me.clone());
            vec![Frame::GroupJoin {
                group: group.to_string(),
                pid,
            }]
        });
        result
    }

    fn leave(&self, pid: Pid, group: &str) {
        self.change(|t| {
            let was = t.joined.get_mut(&pid).is_some_and(|j| j.remove(group));
            if t.joined.get(&pid).is_some_and(BTreeSet::is_empty) {
                t.joined.remove(&pid);
            }
            remove_member(&mut t.members, group, pid);
            if was {
                vec![Frame::GroupLeave {
                    group: group.to_string(),
                    pid,
                }]
            } else {
                Vec::new()
            }
        });
    }

    fn exited(&self, pid: Pid) {
        self.change(|t| {
            let groups = t.joined.remove(&pid).unwrap_or_default();
            groups
                .into_iter()
                .map(|group| {
                    remove_member(&mut t.members, &group, pid);
                    Frame::GroupLeave { group, pid }
                })
                .collect()
        });
    }

    /// This node's members of `group`, ordered by pid.
    fn local_members(&self, group: &str) -> Vec<InstanceHandle> {
        self.lock()
            .members
            .get(group)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Every member of `group` on any node, ordered by pid.
    fn members(&self, group: &str) -> Vec<Pid> {
        let t = self.lock();
        let mut all: BTreeSet<Pid> = t
            .members
            .get(group)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default();
        for groups in t.remote.values() {
            if let Some(pids) = groups.get(group) {
                all.extend(pids.iter().copied());
            }
        }
        all.into_iter().collect()
    }

    /// The other nodes with members of `group`, and how many each has.
    fn remote_members(&self, group: &str) -> Vec<(u32, usize)> {
        self.lock()
            .remote
            .iter()
            .filter_map(|(&node, groups)| groups.get(group).map(|p| (node, p.len())))
            .collect()
    }

    /// Send this node's snapshot to every reachable peer whose session or
    /// creation changed since it last had one. Called every heartbeat
    /// round.
    ///
    /// A snapshot that cannot be sent (the peer is reachable but has no
    /// connection for the moment: lost or being replaced) is sent again the
    /// next round. It does not end the session as an unsent notice does
    /// (S12's R1): the peer replaces what it knows of this node's members
    /// with each snapshot, and forgets them when its own session ends.
    /// Ending it here would end every new session as it began, each round,
    /// until the connection was back.
    pub(crate) fn sync(&self, rt: &RuntimeInner) {
        let _sending = self.sending.lock().unwrap_or_else(PoisonError::into_inner);
        let due: Vec<(u32, Snapshot)> = {
            let mut t = self.lock();
            let snapshot: Snapshot = t
                .members
                .iter()
                .map(|(g, m)| (g.clone(), m.keys().copied().collect()))
                .collect();
            let mut due = Vec::new();
            for node in rt.liveness.watched() {
                let at = session_of(rt, node);
                if t.synced.get(&node) == Some(&at)
                    || !transport::contacted(rt, node)
                    || !rt.liveness.reachable(node)
                {
                    continue;
                }
                t.synced.insert(node, at);
                due.push((node, snapshot.clone()));
            }
            due
        };
        for (node, groups) in due {
            if transport::try_send(rt, node, Frame::GroupSync { groups }).is_err() {
                self.lock().synced.remove(&node);
            }
        }
    }

    /// The session with `node` ended: forget its members, and send it a
    /// fresh snapshot in the next.
    pub(crate) fn forget(&self, node: u32) {
        let mut t = self.lock();
        t.remote.remove(&node);
        t.synced.remove(&node);
    }

    /// `node`'s snapshot: replaces what was known of its members.
    pub(crate) fn synced_from(&self, node: u32, groups: Vec<(String, Vec<Pid>)>) {
        let groups: BTreeMap<String, BTreeSet<Pid>> = groups
            .into_iter()
            .map(|(g, pids)| {
                let pids: BTreeSet<Pid> = pids.into_iter().filter(|p| p.node == node).collect();
                (g, pids)
            })
            .filter(|(_, pids)| !pids.is_empty())
            .collect();
        self.lock().remote.insert(node, groups);
    }

    /// `pid` on `node` joined `group`.
    pub(crate) fn joined_on(&self, node: u32, group: String, pid: Pid) {
        self.lock()
            .remote
            .entry(node)
            .or_default()
            .entry(group)
            .or_default()
            .insert(pid);
    }

    /// `pid` on `node` left `group`.
    pub(crate) fn left_on(&self, node: u32, group: &str, pid: Pid) {
        let mut t = self.lock();
        if let Some(groups) = t.remote.get_mut(&node)
            && let Some(pids) = groups.get_mut(group)
        {
            pids.remove(&pid);
            if pids.is_empty() {
                groups.remove(group);
            }
        }
    }

    /// A publish from another node's `from`, with its `stamp`: cast to each
    /// local member of `group` the messaging ACL allows.
    pub(crate) fn published_from(
        &self,
        group: &str,
        from: Pid,
        stamp: Option<&Stamp>,
        msg: &Envelope,
    ) {
        let Some(stamp) = stamp else {
            return;
        };
        for member in self.local_members(group) {
            if acl::allowed(
                &stamp.app,
                &stamp.may_call,
                member.app(),
                &member.acl().accept_from,
            ) {
                let _ = member.cast_from(Some(from), msg.clone());
            }
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
            .into_iter()
            .map(Into::into)
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
        let me = Origin::Local(&state.me);
        let mut delivered = state
            .shared
            .local_members(&group)
            .iter()
            .filter(|member| {
                delivery::permits(me, member) && member.cast_from(me.pid(), msg.clone()).is_ok()
            })
            .count();
        // One copy per other node with members, counted as reaching them
        // all once queued: that node fans it out, under the same ACL.
        if let Some(rt) = state.shared.runtime() {
            for (node, count) in state.shared.remote_members(&group) {
                let frame = Frame::GroupPublish {
                    group: group.clone(),
                    from: state.me.pid(),
                    sender: delivery::stamp(me),
                    msg: msg.clone(),
                };
                if transport::try_send(&rt, node, frame).is_ok() {
                    delivered += count;
                }
            }
        }
        Ok(Ok(u32::try_from(delivered).unwrap_or(u32::MAX)))
    }
}
