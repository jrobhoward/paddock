//! Human-readable output.

use paddock_api::{AppDetail, AppSummary, InstanceInfo, LogLine, UpgradeResponse};

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
            ]
        })
        .collect();
    table(&["PID", "APP", "VERSION", "STATE", "QUEUED"], &rows)
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

/// `paddock logs <app>`.
pub fn logs(lines: &[LogLine]) -> String {
    lines
        .iter()
        .map(|l| {
            format!(
                "{} {:<5} {} [{}] {}",
                l.time_ms, l.level, l.pid, l.source, l.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod format_tests;
