//! `paddock release upgrade` and `paddock release rollback`: an app's
//! release across an HA pair (ROADMAP U1; DESIGN.md, "Releases across the
//! pair"), driven from here over each node's admin API.
//!
//! Each step reads both nodes first and does the next thing left, so a run
//! stopped by a node out of reach or a change of roles continues where it
//! stopped when run again. The standby goes first, then the primary, each
//! with the node's own upgrade or rollback, which moves running instances
//! in place and reverts them if the new version cannot load their
//! snapshot. After each step a gate waits for the app to run at the new
//! version on the node changed. An upgrade whose step or gate fails rolls
//! back what the run changed, newest first, unless asked not to.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use paddock::api::{Client, RolloutParams};
use serde::{Deserialize, Serialize};

/// The pair file: how to reach each node's admin API.
///
/// ```toml
/// [[node]]
/// id = 1
/// address = "10.0.0.1:7443"   # the node's [admin] address
/// key = "ed25519:..."         # the node's public key
///
/// [[node]]
/// id = 2
/// socket = "/var/lib/paddock/admin.sock"   # or its admin socket
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PairFile {
    pub(crate) node: Vec<PairNode>,
}

/// One node of the pair file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PairNode {
    pub(crate) id: u32,
    /// The admin socket, for a node on this machine.
    pub(crate) socket: Option<PathBuf>,
    /// The remote admin API's `host:port`, with `key`.
    pub(crate) address: Option<String>,
    /// The node's public key, which the remote admin API must present.
    pub(crate) key: Option<String>,
}

impl PairFile {
    /// Read and check the pair file at `path`.
    pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("pair file {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| anyhow::anyhow!("pair file {}: {e}", path.display()))
    }

    /// Parse a pair file: two nodes, each with a socket, or an address and
    /// a key.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let file: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        if file.node.len() != 2 || file.node[0].id == file.node[1].id {
            return Err("it names two nodes, by different ids".into());
        }
        for n in &file.node {
            match (&n.socket, &n.address, &n.key) {
                (Some(_), None, None) | (None, Some(_), Some(_)) => {}
                _ => {
                    return Err(format!(
                        "node {}: give either `socket`, or `address` and `key`",
                        n.id
                    ));
                }
            }
        }
        Ok(file)
    }
}

/// A node of the pair and its admin client.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub(crate) id: u32,
    pub(crate) client: Client,
}

/// What a step needs of one node: its failover standing and the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct View {
    pub(crate) id: u32,
    /// `primary`, `standby` or `down`.
    pub(crate) role: String,
    pub(crate) may_run: bool,
    /// On the primary: the standby lacks replicated writes.
    pub(crate) partner_outdated: bool,
    /// The app's version.
    pub(crate) version: String,
    /// The version its rollback returns to.
    pub(crate) previous: Option<String>,
    /// The app's status (`running`, `standby` for a held singleton, ...).
    pub(crate) status: String,
}

/// The next step towards a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Change the app on this node.
    Change(u32),
    /// Both nodes run the version.
    Done,
    /// Nothing is done now, and why: rerun once it is resolved.
    Blocked(String),
}

/// The next step to bring both nodes to `target`: the standby first, then
/// the primary; none unless the pair has a primary that may run and a
/// standby that is current.
pub(crate) fn plan(views: &[View; 2], target: &str) -> Step {
    let primary = views.iter().find(|v| v.role == "primary" && v.may_run);
    let standby = views.iter().find(|v| v.role == "standby");
    let (Some(p), Some(s)) = (primary, standby) else {
        let roles: Vec<String> = views
            .iter()
            .map(|v| format!("node {} is {}", v.id, v.role))
            .collect();
        return Step::Blocked(format!(
            "the pair needs a running primary and a standby, and {}",
            roles.join(", ")
        ));
    };
    if p.partner_outdated {
        return Step::Blocked(format!("node {} is catching up on replicated writes", s.id));
    }
    if s.version != target {
        return Step::Change(s.id);
    }
    if p.version != target {
        return Step::Change(p.id);
    }
    Step::Done
}

/// The version a rollback of both nodes returns to: their common previous
/// version, or, for a rollback stopped half way, the version of the node
/// already rolled back.
pub(crate) fn rollback_target(views: &[View; 2]) -> Result<String, String> {
    let [a, b] = views;
    if a.version == b.version {
        return match (&a.previous, &b.previous) {
            (Some(x), Some(y)) if x == y => Ok(x.clone()),
            _ => Err(format!(
                "nodes {} and {} have no previous version in common",
                a.id, b.id
            )),
        };
    }
    if a.previous.as_deref() == Some(b.version.as_str()) {
        return Ok(b.version.clone());
    }
    if b.previous.as_deref() == Some(a.version.as_str()) {
        return Ok(a.version.clone());
    }
    Err(format!(
        "node {} runs {} and node {} runs {}, and neither is the other's previous version",
        a.id, a.version, b.id, b.version
    ))
}

/// How a release runs.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) params: RolloutParams,
    /// Roll back what the run changed if a step or gate fails.
    pub(crate) abort: bool,
    /// How long a gate waits.
    pub(crate) timeout: Duration,
}

/// What a release did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Report {
    pub(crate) app: String,
    pub(crate) to: String,
    /// Each step, as done.
    pub(crate) steps: Vec<String>,
    /// `done`, `stopped` (rerun to continue) or `aborted` (rolled back).
    pub(crate) outcome: String,
    /// Why it stopped or was aborted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

impl Report {
    pub(crate) fn ok(&self) -> bool {
        self.outcome == "done"
    }

    /// The report for people, a line per step.
    pub(crate) fn text(&self) -> String {
        let mut lines = self.steps.clone();
        lines.push(match &self.reason {
            Some(why) => format!("{} {} {}: {why}", self.outcome, self.app, self.to),
            None => format!("{} {} {}", self.outcome, self.app, self.to),
        });
        lines.join("\n")
    }
}

async fn view(m: &Member, app: &str) -> Result<View, String> {
    let f = m
        .client
        .failover()
        .await
        .map_err(|e| format!("node {}: {e}", m.id))?;
    let d = m
        .client
        .status(app)
        .await
        .map_err(|e| format!("node {}: {e}", m.id))?;
    Ok(View {
        id: m.id,
        role: f.role,
        may_run: f.may_run,
        partner_outdated: f.partner_outdated,
        version: d.summary.version,
        previous: d.summary.previous,
        status: d.summary.status,
    })
}

async fn views(members: &[Member; 2], app: &str) -> Result<[View; 2], String> {
    Ok([view(&members[0], app).await?, view(&members[1], app).await?])
}

fn member(members: &[Member; 2], id: u32) -> &Member {
    if members[0].id == id {
        &members[0]
    } else {
        &members[1]
    }
}

/// Wait until node `id` runs `app` at `version` in its role as it was
/// (`running` on a primary, also `standby` on a standby holding a
/// singleton). A failed app ends the wait at once.
async fn gate(
    members: &[Member; 2],
    id: u32,
    app: &str,
    version: &str,
    role: &str,
    timeout: Duration,
) -> Result<(), String> {
    let m = member(members, id);
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < timeout {
        match view(m, app).await {
            Ok(v) if v.status == "failed" => {
                return Err(format!("node {id}: {app} {} failed", v.version));
            }
            Ok(v) if v.role != role => {
                return Err(format!("node {id} became {} during the release", v.role));
            }
            Ok(v)
                if v.version == version
                    && (v.status == "running" || (role == "standby" && v.status == "standby")) =>
            {
                return Ok(());
            }
            Ok(v) => last = format!("{app} {} is {}", v.version, v.status),
            Err(e) => last = e,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "node {id}: {app} not running at {version} after {}s ({last})",
        timeout.as_secs()
    ))
}

/// Upgrade `app` on both nodes to the bundle `bytes`, of version `target`.
pub(crate) async fn upgrade(
    members: &[Member; 2],
    app: &str,
    bytes: &[u8],
    target: &str,
    opts: &Options,
) -> Report {
    let mut report = Report {
        app: app.to_string(),
        to: target.to_string(),
        steps: Vec::new(),
        outcome: "done".into(),
        reason: None,
    };
    // The nodes this run changed, in order.
    let mut changed: Vec<u32> = Vec::new();
    let failed = loop {
        let views = match views(members, app).await {
            Ok(v) => v,
            Err(e) => return stopped(report, format!("{e}; run again once it can be reached")),
        };
        let id = match plan(&views, target) {
            Step::Done => return report,
            Step::Blocked(why) => return stopped(report, why),
            Step::Change(id) => id,
        };
        // Each node is changed once; a second time means it did not stay.
        if changed.contains(&id) {
            break format!("node {id} is not at {target} after its upgrade");
        }
        let role = views
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.role.clone())
            .unwrap_or_default();
        match member(members, id)
            .client
            .upgrade(app, bytes.to_vec(), &opts.params)
            .await
        {
            Ok(r) => {
                changed.push(id);
                report.steps.push(format!(
                    "node {id} ({role}): upgraded {app} {} -> {} ({})",
                    r.from, r.to, r.mode
                ));
            }
            Err(e) => break format!("node {id} ({role}): upgrade refused: {e}"),
        }
        if let Err(e) = gate(members, id, app, target, &role, opts.timeout).await {
            break e;
        }
    };
    if !opts.abort || changed.is_empty() {
        let outcome = if changed.is_empty() {
            "aborted"
        } else {
            "stopped"
        };
        report.outcome = outcome.into();
        report.reason = Some(failed);
        return report;
    }
    for &id in changed.iter().rev() {
        let line = match member(members, id).client.rollback(app, &opts.params).await {
            Ok(r) => format!(
                "node {id}: rolled back {app} {} -> {} ({})",
                r.from, r.to, r.mode
            ),
            Err(e) => format!("node {id}: rollback failed: {e}"),
        };
        report.steps.push(line);
    }
    report.outcome = "aborted".into();
    report.reason = Some(failed);
    report
}

/// Roll `app` back on both nodes to the version before.
pub(crate) async fn rollback(members: &[Member; 2], app: &str, opts: &Options) -> Report {
    let mut report = Report {
        app: app.to_string(),
        to: String::new(),
        steps: Vec::new(),
        outcome: "done".into(),
        reason: None,
    };
    let target = match views(members, app).await.and_then(|v| rollback_target(&v)) {
        Ok(t) => t,
        Err(e) => return stopped(report, e),
    };
    report.to.clone_from(&target);
    let mut changed: Vec<u32> = Vec::new();
    loop {
        let views = match views(members, app).await {
            Ok(v) => v,
            Err(e) => return stopped(report, format!("{e}; run again once it can be reached")),
        };
        let id = match plan(&views, &target) {
            Step::Done => return report,
            Step::Blocked(why) => return stopped(report, why),
            Step::Change(id) if changed.contains(&id) => {
                return stopped(
                    report,
                    format!("node {id} is not at {target} after its rollback"),
                );
            }
            Step::Change(id) => id,
        };
        let role = views
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.role.clone())
            .unwrap_or_default();
        match member(members, id).client.rollback(app, &opts.params).await {
            Ok(r) => {
                changed.push(id);
                report.steps.push(format!(
                    "node {id} ({role}): rolled back {app} {} -> {} ({})",
                    r.from, r.to, r.mode
                ));
            }
            Err(e) => return stopped(report, format!("node {id} ({role}): rollback refused: {e}")),
        }
        if let Err(e) = gate(members, id, app, &target, &role, opts.timeout).await {
            return stopped(report, e);
        }
    }
}

fn stopped(mut report: Report, why: String) -> Report {
    report.outcome = "stopped".into();
    report.reason = Some(why);
    report
}

#[cfg(test)]
#[path = "release_tests.rs"]
mod release_tests;
