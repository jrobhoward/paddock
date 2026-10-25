//! Workloads of ordinary app code for `paddock-host`'s `app_cost`
//! benchmark (ROADMAP B11): the same source compiled natively, into an app
//! (`paddock-fixture-workload`) and into a core module with no imports
//! (feature `exports`), so the three can be compared run for run.
//!
//! Each workload works on input generated from a fixed seed, so nothing is
//! vendored and every build sees the same bytes, and returns a checksum that
//! must match across the builds:
//!
//! - `json-typed`: parse a list of orders into structs (serde derive),
//!   recompute each total, and serialize the list back.
//! - `json-value`: the same document through `serde_json::Value`, walked
//!   and serialized back.
//! - `regex`: scan an access log with a capturing pattern.
//! - `markdown`: render a Markdown document to HTML (`pulldown-cmark`).
//! - `map`: count keys in a `BTreeMap<String, u64>`, look them up and take
//!   the largest.
//!
//! Input generation and the regex's compilation happen once, in
//! [`Inputs::new`], outside the timed runs.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The workloads, by name; [`run`] takes these.
pub const WORKLOADS: [&str; 5] = ["json-typed", "json-value", "regex", "markdown", "map"];

/// Orders in the JSON document (about 190 KiB).
const ORDERS: usize = 400;
/// Lines in the access log (about 270 KiB).
const LOG_LINES: usize = 2000;
/// Sections in the Markdown document (about 120 KiB).
const SECTIONS: usize = 200;
/// Keys counted per `map` run.
const MAP_KEYS: usize = 10_000;

/// Fixed-seed generator for the inputs (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[usize::try_from(self.below(items.len() as u64)).unwrap_or(0)]
    }
}

const WORDS: [&str; 16] = [
    "valve", "pump", "sensor", "station", "north", "south", "pressure", "flow", "alarm", "reset",
    "manual", "remote", "level", "tank", "line", "relay",
];
const COUNTRIES: [&str; 6] = ["NZ", "DE", "US", "JP", "BR", "KE"];
const TAGS: [&str; 5] = ["priority", "gift", "repeat", "wholesale", "export"];

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    Paid,
    Shipped,
    Cancelled,
}

#[derive(Serialize, Deserialize)]
struct Customer {
    id: u64,
    name: String,
    email: String,
    country: String,
}

#[derive(Serialize, Deserialize)]
struct Item {
    sku: String,
    qty: u32,
    price_cents: u64,
    weight_kg: f64,
}

#[derive(Serialize, Deserialize)]
struct Order {
    id: u64,
    created: String,
    status: Status,
    customer: Customer,
    items: Vec<Item>,
    total_cents: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    tags: Vec<String>,
}

fn words(rng: &mut Rng, n: u64) -> String {
    let mut out = String::new();
    for i in 0..n {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(rng.pick(&WORDS));
    }
    out
}

fn orders(rng: &mut Rng) -> String {
    let statuses = [
        Status::Pending,
        Status::Paid,
        Status::Shipped,
        Status::Cancelled,
    ];
    let mut list = Vec::with_capacity(ORDERS);
    for id in 0..ORDERS as u64 {
        let items = (0..1 + rng.below(5))
            .map(|_| Item {
                sku: format!(
                    "{}-{:05}",
                    rng.pick(&WORDS).to_uppercase(),
                    rng.below(100_000)
                ),
                qty: u32::try_from(1 + rng.below(20)).unwrap_or(1),
                price_cents: 100 + rng.below(50_000),
                #[allow(clippy::cast_precision_loss)]
                weight_kg: rng.below(100_000) as f64 / 1000.0,
            })
            .collect();
        let name = words(rng, 2);
        list.push(Order {
            id,
            created: format!(
                "2026-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                1 + rng.below(12),
                1 + rng.below(28),
                rng.below(24),
                rng.below(60),
                rng.below(60)
            ),
            status: statuses[usize::try_from(rng.below(4)).unwrap_or(0)],
            customer: Customer {
                id: rng.below(10_000),
                email: format!("{}@example.com", name.replace(' ', ".")),
                name,
                country: rng.pick(&COUNTRIES).to_string(),
            },
            items,
            total_cents: 0,
            note: (rng.below(3) == 0).then(|| {
                let n = 3 + rng.below(8);
                words(rng, n)
            }),
            tags: (0..rng.below(3))
                .map(|_| rng.pick(&TAGS).to_string())
                .collect(),
        });
    }
    serde_json::to_string(&list).unwrap_or_default()
}

fn access_log(rng: &mut Rng) -> String {
    let methods = ["GET", "GET", "GET", "POST", "PUT", "DELETE"];
    let statuses = ["200", "200", "200", "201", "304", "404", "500"];
    let mut out = String::new();
    for _ in 0..LOG_LINES {
        let _ = writeln!(
            out,
            "203.0.{}.{} - - [25/Oct/2026:{:02}:{:02}:{:02} +0000] \"{} /{}/{}/{}?page={} HTTP/1.1\" {} {} \"-\" \"Mozilla/5.0 (X11; Linux x86_64) {}\"",
            rng.below(256),
            rng.below(256),
            rng.below(24),
            rng.below(60),
            rng.below(60),
            rng.pick(&methods),
            if rng.below(2) == 0 { "api" } else { "static" },
            rng.pick(&WORDS),
            rng.below(10_000),
            rng.below(20),
            rng.pick(&statuses),
            rng.below(100_000),
            rng.pick(&WORDS),
        );
    }
    out
}

fn markdown(rng: &mut Rng) -> String {
    let mut out = String::new();
    for s in 0..SECTIONS {
        let _ = writeln!(out, "## {} {s}\n", words(rng, 3));
        for _ in 0..1 + rng.below(3) {
            let _ = writeln!(
                out,
                "{} *{}* {} **{}** [{}](https://example.com/{}) `{}` {}.\n",
                words(rng, 8),
                words(rng, 2),
                words(rng, 6),
                words(rng, 1),
                words(rng, 2),
                rng.below(1000),
                words(rng, 1),
                words(rng, 10)
            );
        }
        for _ in 0..rng.below(5) {
            let _ = writeln!(out, "- {} `{}`", words(rng, 5), rng.below(100));
        }
        let _ = writeln!(out, "\n> {}\n", words(rng, 12));
    }
    out
}

/// Each workload's input, generated once.
#[derive(Debug)]
pub struct Inputs {
    orders: String,
    log: String,
    log_line: Regex,
    markdown: String,
    keys: Vec<String>,
}

impl Inputs {
    /// Generate every input from the fixed seed.
    ///
    /// # Panics
    ///
    /// Never: the regex is a constant that compiles.
    #[must_use]
    pub fn new() -> Self {
        let mut rng = Rng(0x5EED);
        Self {
            orders: orders(&mut rng),
            log: access_log(&mut rng),
            #[allow(clippy::unwrap_used)]
            log_line: Regex::new(
                r#"(?m)^(\S+) \S+ \S+ \[([^\]]+)\] "([A-Z]+) (/[^ "?]*)(?:\?[^ "]*)? HTTP/[0-9.]+" ([0-9]{3}) ([0-9]+)"#,
            )
            .unwrap(),
            markdown: markdown(&mut rng),
            keys: (0..MAP_KEYS)
                .map(|_| format!("{}-{:04}", rng.pick(&WORDS), rng.below(2000)))
                .collect(),
        }
    }

    /// The sizes of the inputs in bytes: orders, log, Markdown.
    #[must_use]
    pub fn sizes(&self) -> (usize, usize, usize) {
        (self.orders.len(), self.log.len(), self.markdown.len())
    }
}

impl Default for Inputs {
    fn default() -> Self {
        Self::new()
    }
}

fn json_typed(inputs: &Inputs) -> u64 {
    let Ok(mut orders) = serde_json::from_str::<Vec<Order>>(&inputs.orders) else {
        return 0;
    };
    let mut sum = 0u64;
    for order in &mut orders {
        order.total_cents = order
            .items
            .iter()
            .map(|i| i.price_cents * u64::from(i.qty))
            .sum();
        if matches!(order.status, Status::Cancelled) {
            order.tags.push("void".into());
        }
        sum = sum.wrapping_add(order.total_cents);
    }
    let text = serde_json::to_string(&orders).unwrap_or_default();
    sum.wrapping_add(text.len() as u64)
}

fn walk(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Array(items) => items.iter().map(walk).fold(1, u64::wrapping_add),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.len() as u64).wrapping_add(walk(v)))
            .fold(1, u64::wrapping_add),
        serde_json::Value::String(s) => s.len() as u64,
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(1),
        _ => 1,
    }
}

fn json_value(inputs: &Inputs) -> u64 {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&inputs.orders) else {
        return 0;
    };
    let text = serde_json::to_string(&value).unwrap_or_default();
    walk(&value).wrapping_add(text.len() as u64)
}

fn regex_scan(inputs: &Inputs) -> u64 {
    let mut sum = 0u64;
    for caps in inputs.log_line.captures_iter(&inputs.log) {
        let status: u64 = caps[5].parse().unwrap_or(0);
        let bytes: u64 = caps[6].parse().unwrap_or(0);
        if caps[4].starts_with("/api/") {
            sum = sum.wrapping_add(1 << 32);
        }
        sum = sum.wrapping_add(status * 100_000 + bytes);
    }
    sum
}

fn markdown_html(inputs: &Inputs) -> u64 {
    let parser = pulldown_cmark::Parser::new(&inputs.markdown);
    let mut html = String::with_capacity(inputs.markdown.len() * 2);
    pulldown_cmark::html::push_html(&mut html, parser);
    html.len() as u64
}

fn map_count(inputs: &Inputs) -> u64 {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for key in &inputs.keys {
        *counts.entry(key.clone()).or_default() += 1;
    }
    let mut hits = 0u64;
    for (i, key) in inputs.keys.iter().enumerate() {
        let probe = if i % 2 == 0 {
            key.clone()
        } else {
            format!("{key}-x")
        };
        hits += counts.get(&probe).copied().unwrap_or(0);
    }
    let mut top: Vec<(&String, &u64)> = counts.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let first = top.first().map_or(0, |(k, v)| k.len() as u64 * 1000 + **v);
    hits.wrapping_mul(31)
        .wrapping_add(counts.len() as u64)
        .wrapping_add(first)
}

/// Run workload `name` `n` times; the sum of its checksums, or `None` for
/// an unknown name.
#[must_use]
pub fn run(inputs: &Inputs, name: &str, n: u32) -> Option<u64> {
    let once: fn(&Inputs) -> u64 = match name {
        "json-typed" => json_typed,
        "json-value" => json_value,
        "regex" => regex_scan,
        "markdown" => markdown_html,
        "map" => map_count,
        _ => return None,
    };
    Some(
        (0..n)
            .map(|_| once(std::hint::black_box(inputs)))
            .fold(0, u64::wrapping_add),
    )
}

#[cfg(feature = "exports")]
mod exports {
    use std::sync::OnceLock;

    use super::{Inputs, WORKLOADS, run};

    static INPUTS: OnceLock<Inputs> = OnceLock::new();

    /// Generate the inputs; call once before timing `workload_run`.
    #[unsafe(no_mangle)]
    pub extern "C" fn workload_prepare() {
        INPUTS.get_or_init(Inputs::new);
    }

    /// Run workload number `id` (in [`WORKLOADS`] order) `n` times.
    #[unsafe(no_mangle)]
    pub extern "C" fn workload_run(id: u32, n: u32) -> u64 {
        let inputs = INPUTS.get_or_init(Inputs::new);
        usize::try_from(id)
            .ok()
            .and_then(|i| WORKLOADS.get(i))
            .and_then(|name| run(inputs, name, n))
            .unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
