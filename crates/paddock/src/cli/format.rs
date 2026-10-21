//! Human-readable output.

use paddock::api::{AppDetail, AppSummary, InstanceInfo, LogLine, StateResponse, UpgradeResponse};

/// Render rows as left-aligned columns under a header.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.len());
            }
        }
    }
    let line = |cells: Vec<&str>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths.get(i).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = vec![line(header.to_vec())];
    out.extend(
        rows.iter()
            .map(|r| line(r.iter().map(String::as_str).collect())),
    );
    out.join("\n")
}

/// `paddock list`.
pub fn apps(apps: &[AppSummary]) -> String {
    if apps.is_empty() {
        return "no apps deployed".to_string();
    }
    let rows: Vec<Vec<String>> = apps
        .iter()
        .map(|a| {
            vec![
                a.name.clone(),
                a.version.clone(),
                a.status.clone(),
                a.instances.to_string(),
                a.previous.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    table(
        &["APP", "VERSION", "STATUS", "INSTANCES", "PREVIOUS"],
        &rows,
    )
}

/// `paddock ps`.
pub fn instances(list: &[InstanceInfo]) -> String {
    if list.is_empty() {
        return "no instances running".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|i| {
            vec![
                i.pid.clone(),
                i.app.clone(),
                i.version.clone(),
                i.state.clone(),
                i.queued.to_string(),
                i.dropped.to_string(),
                memory(i.memory_bytes),
                i.restarts.to_string(),
                i.last_exit.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    table(
        &[
            "PID",
            "APP",
            "VERSION",
            "STATE",
            "QUEUED",
            "DROPPED",
            "MEMORY",
            "RESTARTS",
            "LAST EXIT",
        ],
        &rows,
    )
}

/// Bytes as KiB or MiB, for tables.
fn memory(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    if bytes >= MIB {
        #[allow(clippy::cast_precision_loss)]
        let mib = bytes as f64 / MIB as f64;
        format!("{mib:.1}MiB")
    } else {
        format!("{}KiB", bytes.div_ceil(1024))
    }
}

/// JSON text as CBOR bytes, for `--cbor`.
///
/// # Errors
///
/// A message if `json` is not JSON.
pub fn cbor_from_json(json: &str) -> Result<Vec<u8>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("BODY is not JSON: {e}"))?;
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// A reply body for printing: CBOR as JSON, anything else as text.
pub fn reply(content_type: &str, body: &[u8]) -> String {
    if content_type.starts_with("application/cbor")
        && let Ok(value) = ciborium::from_reader::<ciborium::Value, _>(body)
        && let Ok(json) = serde_json::to_string(&value)
    {
        return json;
    }
    String::from_utf8_lossy(body).into_owned()
}

/// `paddock status <app>`.
pub fn detail(d: &AppDetail) -> String {
    let s = &d.summary;
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.join(", ")
        }
    };
    let mut out = vec![
        format!("app:       {} {}", s.name, s.version),
        format!("status:    {}", s.status),
    ];
    if let Some(reason) = &s.reason {
        out.push(format!("reason:    {reason}"));
    }
    out.push(format!(
        "previous:  {}",
        s.previous.as_deref().unwrap_or("-")
    ));
    out.push(format!("restarts:  {}", d.restarts));
    out.push(format!(
        "signer:    {}",
        if d.signer.is_empty() { "-" } else { &d.signer }
    ));
    out.push(format!("granted:   {}", list(&d.granted)));
    out.push(format!("denied:    {}", list(&d.denied)));
    if !d.stubbed.is_empty() {
        out.push(format!("stubbed:   {}", list(&d.stubbed)));
    }
    for w in &d.warnings {
        out.push(format!("warning:   {w}"));
    }
    if !d.instance_list.is_empty() {
        out.push(String::new());
        out.push(instances(&d.instance_list));
    }
    if !d.listeners.is_empty() {
        let rows: Vec<Vec<String>> = d
            .listeners
            .iter()
            .map(|l| {
                vec![
                    l.name.clone(),
                    l.address.clone(),
                    l.active.to_string(),
                    l.accepted.to_string(),
                    l.refused.to_string(),
                    l.crashed.to_string(),
                    l.idle_closed.to_string(),
                ]
            })
            .collect();
        out.push(String::new());
        out.push(table(
            &[
                "LISTENER",
                "ADDRESS",
                "ACTIVE",
                "ACCEPTED",
                "REFUSED",
                "CRASHED",
                "IDLE CLOSED",
            ],
            &rows,
        ));
    }
    out.join("\n")
}

/// `paddock upgrade` and `paddock rollback`.
pub fn upgrade(u: &UpgradeResponse) -> String {
    let mut out = format!(
        "{} -> {} ({}), {} instance(s)",
        u.from,
        u.to,
        u.mode,
        u.instances.len()
    );
    if let Some(max) = u.pauses_us.iter().max() {
        out.push_str(&format!(", longest pause {max} µs"));
    }
    if !u.approved.is_empty() {
        out.push_str(&format!("\napproved: {}", u.approved.join(", ")));
    }
    out
}

/// `paddock state <pid>`.
pub fn state(s: &StateResponse) -> String {
    let mut head = format!("{} ({}, {} bytes", s.pid, s.format, s.bytes);
    if let Some(v) = s.format_version {
        head.push_str(&format!(", format {v}"));
    }
    head.push(')');
    let body = match (&s.value, &s.hex) {
        (Some(v), _) => serde_json::to_string_pretty(v).unwrap_or_default(),
        (None, Some(h)) => h.clone(),
        (None, None) => String::new(),
    };
    if body.is_empty() {
        head
    } else {
        format!("{head}\n{body}")
    }
}

/// `paddock logs <app>`.
pub fn logs(lines: &[LogLine]) -> String {
    lines.iter().map(log_line).collect::<Vec<_>>().join("\n")
}

/// One log record, as `paddock logs` prints it.
pub(crate) fn log_line(l: &LogLine) -> String {
    format!(
        "{} {:<5} {} [{}] {}",
        l.time_ms, l.level, l.pid, l.source, l.message
    )
}

/// One event, as `paddock events` prints it: time, kind and event, then
/// each field as `key=value` (strings with spaces or quotes quoted).
pub(crate) fn event(e: &paddock::api::Event) -> String {
    let mut out = format!("{} {} {}", e.time_ms, e.kind, e.event);
    for (key, value) in &e.fields {
        let text = match value {
            serde_json::Value::String(s) if !s.is_empty() && !s.contains([' ', '"', '\'', '=']) => {
                s.clone()
            }
            serde_json::Value::String(s) => format!("{s:?}"),
            other => other.to_string(),
        };
        out.push_str(&format!(" {key}={text}"));
    }
    out
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod format_tests;

/// `paddock failover`: the node's standing, one field per line.
pub(crate) fn release(r: &paddock::api::ReleaseInfo) -> String {
    let peers: Vec<String> = r
        .peers
        .iter()
        .map(|(node, version)| format!("{node} at {version}"))
        .collect();
    format!(
        "speaks:  versions {} to {}\nkeeps:   version {}'s formats\npeers:   {}",
        r.min_version,
        r.max_version,
        r.formats,
        if peers.is_empty() {
            "none connected".to_string()
        } else {
            peers.join(", ")
        }
    )
}

pub(crate) fn failover(f: &paddock::api::FailoverInfo) -> String {
    let yes = |b: bool| if b { "yes" } else { "no" };
    let mut out = format!(
        "role:       {}\nepoch:      {}\nmay run:    {}\npartner:    {} (reachable: {})\narbiter:    {}\nsingletons: {}",
        f.role,
        f.epoch,
        yes(f.may_run),
        f.partner,
        yes(f.partner_reachable),
        f.arbiter,
        if f.singletons.is_empty() {
            "none".to_string()
        } else {
            f.singletons.join(", ")
        }
    );
    if !f.paths.is_empty() {
        let paths: Vec<String> = f
            .paths
            .iter()
            .map(|(name, heard)| format!("{name} {}", if *heard { "heard" } else { "silent" }))
            .collect();
        out.push_str(&format!("\npaths:      {}", paths.join(", ")));
    }
    if f.awaiting_confirmation {
        out.push_str(
            "\nwaiting:    the partner looks gone and no witness can decide; `paddock failover confirm` promotes this node",
        );
    }
    if f.partner_outdated {
        out.push_str(
            "\noutdated:   the partner lacks replicated writes; it is not promoted until it catches up",
        );
    }
    if let Some(r) = &f.replication {
        out.push_str(&format!(
            "\nreplicated: {} (commit {}, last {}, log {} KiB",
            r.buckets.join(", "),
            r.commit,
            r.last,
            r.log_bytes.div_ceil(1024)
        ));
        if let Some(matched) = r.partner_matched {
            out.push_str(&format!(
                ", partner at {matched}, {} behind",
                r.last.saturating_sub(matched)
            ));
        }
        if r.alone {
            out.push_str(", committing alone");
        }
        out.push(')');
    }
    out
}

/// One line of `paddock failover --watch`: the leadership's fields, or
/// `unreachable` for a node that cannot be reached (`None`); with `json`,
/// one JSON object.
pub(crate) fn failover_watch(f: Option<&paddock::api::FailoverInfo>, json: bool) -> String {
    let (role, epoch, may_run, awaiting) = match f {
        Some(f) => (
            f.role.as_str(),
            Some(f.epoch),
            f.may_run,
            f.awaiting_confirmation,
        ),
        None => ("unreachable", None, false, false),
    };
    if json {
        let mut v = serde_json::json!({ "role": role, "may_run": may_run });
        if let Some(epoch) = epoch {
            v["epoch"] = epoch.into();
            v["awaiting_confirmation"] = awaiting.into();
        }
        return v.to_string();
    }
    let yes = |b: bool| if b { "yes" } else { "no" };
    match epoch {
        Some(epoch) => format!(
            "role={role} epoch={epoch} may_run={}{}",
            yes(may_run),
            if awaiting {
                " awaiting_confirmation=yes"
            } else {
                ""
            }
        ),
        None => format!("role={role} may_run=no"),
    }
}

/// `paddock members`: one line per member.
pub(crate) fn members(members: &[paddock::api::MemberInfo]) -> String {
    members_named(members, &std::collections::BTreeMap::new())
}

/// `members`, with a CONTACT column (the shell's contact for each node)
/// when `contacts` names any.
pub(crate) fn members_named(
    members: &[paddock::api::MemberInfo],
    contacts: &std::collections::BTreeMap<u32, String>,
) -> String {
    let mut out = String::from("NODE  CREATION  ROLE     REACHABLE  ");
    if !contacts.is_empty() {
        out.push_str("CONTACT     ");
    }
    out.push_str("ADDRESS");
    for m in members {
        out.push_str(&format!(
            "\n{:<5} {:<9} {:<8} {:<10} ",
            m.node,
            m.creation,
            m.role,
            if m.reachable { "yes" } else { "no" },
        ));
        if !contacts.is_empty() {
            let name = contacts.get(&m.node).map_or("-", String::as_str);
            out.push_str(&format!("{name:<11} "));
        }
        out.push_str(&m.address);
    }
    out
}
