//! The conformance suite: what an app built with any guest SDK must do, as
//! cases against an app speaking `paddock-fixture-conformance`'s command
//! protocol (its crate documentation). Runs against the Rust reference app
//! by default; `PADDOCK_CONFORMANCE_APP=<component.wasm>` runs it against
//! another language's:
//!
//! ```text
//! PADDOCK_CONFORMANCE_APP=sdks/go/conformance/app.wasm \
//!     cargo test -p paddock-host --test conformance
//! ```
//!
//! Each test is one case; the case names are the rows of WRITING_APPS.md's
//! table of SDKs.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::{LONG, app_manifest, ask, ask_until, body, fixture, text};
use paddock_bundle::manifest::{Bucket, Capability, Durability, GroupsScope, LogLevel};
use paddock_host::health::HealthCheck;
use paddock_host::{
    AppDeployment, CallError, Envelope, ExitReason, InstanceHandle, InstanceSpec, LoadedApp,
    Runtime, RuntimeConfig, SecretSource, UpgradeError, UpgradeOptions,
};
use serde_json::json;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// The component under test: `PADDOCK_CONFORMANCE_APP` (a relative path
/// from the workspace root), or the Rust reference app.
fn component() -> Vec<u8> {
    match std::env::var_os("PADDOCK_CONFORMANCE_APP") {
        Some(path) => std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(&path),
        )
        .unwrap_or_else(|e| panic!("PADDOCK_CONFORMANCE_APP {}: {e}", path.display())),
        None => fixture("paddock_fixture_conformance"),
    }
}

/// The secret `token` the runtime binds, from a file written by `echo`.
const TOKEN: &str = "s3cr3t";

/// The directory holding the secrets' files, for the whole run.
fn secrets_dir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), format!("{TOKEN}\n")).unwrap();
        dir
    })
    .path()
}

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let dir = secrets_dir();
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 64,
            // componentize-py's apps hold 14 core instances.
            max_core_instances_per_component: 16,
            secrets: BTreeMap::from([
                ("token".to_string(), SecretSource::File(dir.join("token"))),
                (
                    "unbound".to_string(),
                    SecretSource::File(dir.join("missing")),
                ),
            ]),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&component()).unwrap();
        (rt, app)
    })
}

/// Room for a language runtime's own heap, under the default pool.
const MEMORY_MAX: u64 = 128 << 20;

/// A spec for app `app` with every service the protocol uses granted: a
/// memory bucket `data`, the group `<app>-group` to join and publish to,
/// logging at `info`, config `mode=fast`, the secrets `token` and
/// `unbound`, and metrics under the prefix `conformance`. Tests share a runtime, so each uses its own app name.
fn spec(app: &str, init_args: &str) -> InstanceSpec {
    let mut s = InstanceSpec {
        memory_max: MEMORY_MAX,
        callback_deadline: Duration::from_millis(500),
        mailbox_max: 64,
        init_timeout: Duration::from_secs(2),
        shutdown_timeout: Duration::from_millis(500),
        init_args: init_args.as_bytes().to_vec(),
        registered_names: vec![format!("{app}-name")],
        granted: [
            Capability::KeyValue,
            Capability::Groups,
            Capability::Logging,
            Capability::Config,
            Capability::Secrets,
            Capability::Metrics,
        ]
        .into(),
        ..InstanceSpec::new(app, "1.0.0")
    };
    s.services.keyvalue.buckets = BTreeMap::from([(
        "data".to_string(),
        Bucket {
            durability: Durability::Memory,
            quota: None,
        },
    )]);
    s.services.groups.scope = GroupsScope {
        join: vec![format!("{app}-group")],
        publish: vec![format!("{app}-group")],
    };
    s.services.config.values = BTreeMap::from([("mode".to_string(), "fast".to_string())]);
    s.services.secrets.names = ["token".to_string(), "unbound".to_string()].into();
    s.services.metrics.prefix = "conformance".into();
    s
}

fn start(app: &str, init_args: &str) -> InstanceHandle {
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec(app, init_args)).unwrap()
}

/// A started instance whose `init` has finished: it has answered a call.
async fn up(app: &str, init_args: &str) -> InstanceHandle {
    let h = start(app, init_args);
    assert_eq!(ask(&h, "echo:up").await, "up");
    h
}

fn pid(h: &InstanceHandle) -> String {
    h.pid().text()
}

/// An app-level deployment of the component under test at `version`.
fn deployment(name: &str, version: &str, appserver: serde_json::Value) -> AppDeployment {
    let mut manifest = app_manifest(name, appserver);
    manifest.app.version = version.parse().unwrap();
    AppDeployment::new(manifest, shared().1.clone())
}

// --- init --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init____arguments____passed_as_given() {
    let h = up("c-args", "\"alpha beta\"").await;

    let args = ask(&h, "args").await;

    assert_eq!(args, "\"alpha beta\"");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init____returns_an_error____exits_crashed_with_it() {
    let h = start("c-init-fail", "fail");

    let reason = h.wait().await;

    assert_eq!(
        reason,
        ExitReason::Crashed("init failed: asked to fail".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init____traps____exits_crashed_with_a_trap() {
    let h = start("c-init-trap", "trap");

    let reason = h.wait().await;

    let ExitReason::Crashed(why) = reason else {
        panic!("expected crashed, got {reason:?}");
    };
    assert!(why.starts_with("trap:"), "{why}");
}

// --- messages ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____echo_and_count____replies_and_keeps_state() {
    let h = up("c-call", "").await;

    let echo = ask(&h, "echo:hello").await;
    let counts = [ask(&h, "count").await, ask(&h, "count").await];

    assert_eq!(echo, "hello");
    assert_eq!(counts, ["1".to_string(), "2".to_string()]);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____envelope____content_type_schema_and_bytes_kept() {
    let h = up("c-mirror", "").await;
    let sent = Envelope {
        content_type: "application/x-conformance".into(),
        schema: Some("conformance.mirror.v1".into()),
        body: b"mirror".to_vec(),
    };

    let back = h.call(sent.clone(), LONG).await.unwrap();

    assert_eq!(back, sent);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast____then_call____handled_in_order() {
    let h = up("c-cast", "").await;

    h.cast(text("note:a")).unwrap();
    h.cast(text("note:b")).unwrap();
    let notes = ask(&h, "notes").await;

    assert_eq!(notes, "a,b");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____noreply____caller_times_out_and_instance_keeps_running() {
    let h = up("c-noreply", "").await;

    let result = h.call(text("noreply"), Duration::from_millis(100)).await;
    let after = ask(&h, "echo:still").await;

    assert_eq!(result, Err(CallError::Timeout));
    assert_eq!(after, "still");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply____deferred_then_answered____caller_gets_the_answer() {
    let h = up("c-defer", "").await;
    let pending = tokio::spawn({
        let h = h.clone();
        async move { h.call(text("defer"), LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let answered = ask(&h, "answer:later").await;
    let deferred = pending.await.unwrap().unwrap();

    assert_eq!(answered, "answered:1");
    assert_eq!(body(&deferred), "later");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply____during_the_callback____caller_gets_it() {
    let h = up("c-answer-now", "").await;

    let reply = ask(&h, "answer-now:now").await;

    assert_eq!(reply, "now");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_call_and_cast____to_another_instance____reply_and_delivery() {
    let a = up("c-guest-call", "").await;
    let b = up("c-guest-call", "").await;

    let called = ask(&a, &format!("call:{}:1000:echo:hi", pid(&b))).await;
    let sent = ask(&a, &format!("cast:{}:note:from-a", pid(&b))).await;
    let notes = ask_until(&b, "notes", |n| !n.is_empty()).await;
    let dead = ask(&a, "call:0.999999.1@0:100:echo:x").await;

    assert_eq!(called, "ok:hi");
    assert_eq!(sent, "sent");
    assert_eq!(notes, "from-a");
    assert_eq!(dead, "err:noproc");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self____pid____as_the_host_writes_it() {
    let h = up("c-self", "").await;

    let me = ask(&h, "self").await;

    assert_eq!(me, pid(&h));
    h.kill();
}

// --- timers ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timers____two_set_one_cancelled____the_other_fires() {
    let h = up("c-timers", "").await;

    ask(&h, "timer:20:first").await;
    let cancelled = ask(&h, "timer:40:never").await;
    ask(&h, "timer:60:second").await;
    ask(&h, &format!("cancel:{cancelled}")).await;
    let fired = ask_until(&h, "timers", |t| t.contains("second")).await;

    assert_eq!(fired, "first,second");
    h.kill();
}

// --- registered names --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register____listed_name____whereis_finds_it() {
    let h = up("c-register", "register:c-register-name").await;
    let other = up("c-register", "").await;

    let found = ask(&other, "whereis:c-register-name").await;
    let taken = ask(&other, "register:c-register-name").await;
    let unlisted = ask(&other, "register:not-listed").await;
    let missing = ask(&other, "whereis:nobody").await;

    assert_eq!(found, pid(&h));
    assert_eq!(taken, format!("err:taken:{}", pid(&h)));
    assert_eq!(unlisted, "err:access-denied");
    assert_eq!(missing, "none");
    h.kill();
    other.kill();
}

// --- links, monitors and exits -----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____partner_crashes____non_trapping_instance_exits_too() {
    let a = up("c-link", "").await;
    let b = up("c-link", "").await;
    ask(&a, &format!("link:{}", pid(&b))).await;

    let _ = b.call(text("trap"), LONG).await;
    let reason = a.wait().await;

    assert!(
        matches!(&reason, ExitReason::Crashed(why) if why.starts_with("trap:")),
        "{reason:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____partner_crashes____trapping_instance_gets_an_exit_info() {
    let a = up("c-link-trap", "trap-exits").await;
    let b = up("c-link-trap", "").await;
    ask(&a, &format!("link:{}", pid(&b))).await;

    let _ = b.call(text("trap"), LONG).await;
    let infos = ask_until(&a, "infos", |i| !i.is_empty()).await;

    let prefix = format!("exit:{}:crashed(trap:", pid(&b));
    assert!(infos.starts_with(&prefix), "{infos}");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink____then_partner_crashes____no_exit() {
    let a = up("c-unlink", "").await;
    let b = up("c-unlink", "").await;
    ask(&a, &format!("link:{}", pid(&b))).await;
    ask(&a, &format!("unlink:{}", pid(&b))).await;

    let _ = b.call(text("trap"), LONG).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let alive = ask(&a, "echo:alive").await;

    assert_eq!(alive, "alive");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor____target_exits____down_with_reference_and_reason() {
    let watcher = up("c-monitor", "").await;
    let target = up("c-monitor", "").await;
    let quiet = up("c-monitor", "").await;
    let reference = ask(&watcher, &format!("monitor:{}", pid(&target))).await;
    let dropped = ask(&watcher, &format!("monitor:{}", pid(&quiet))).await;
    ask(&watcher, &format!("demonitor:{dropped}")).await;

    target.kill();
    quiet.kill();
    let infos = ask_until(&watcher, "infos", |i| !i.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let later = ask(&watcher, "infos").await;

    assert_eq!(infos, format!("down:{reference}:{}:killed", pid(&target)));
    assert_eq!(later, infos);
    watcher.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_signal____to_trapping_and_non_trapping____info_or_exit() {
    let sender = up("c-exit", "").await;
    let trapping = up("c-exit", "trap-exits").await;
    let plain = up("c-exit", "").await;

    ask(&sender, &format!("exit:{}:stop-now", pid(&trapping))).await;
    ask(&sender, &format!("exit:{}:stop-now", pid(&plain))).await;
    let infos = ask_until(&trapping, "infos", |i| !i.is_empty()).await;
    let reason = plain.wait().await;

    assert_eq!(infos, format!("exit:{}:crashed(stop-now)", pid(&sender)));
    assert_eq!(reason, ExitReason::Crashed("stop-now".into()));
    sender.kill();
    trapping.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate____graceful_shutdown____runs_with_the_reason() {
    let h = up("c-terminate", "").await;
    let witness = up("c-terminate", "").await;
    ask(&h, &format!("on-terminate:{}", pid(&witness))).await;

    h.shutdown(ExitReason::Shutdown);
    let reason = h.wait().await;
    let notes = ask_until(&witness, "notes", |n| !n.is_empty()).await;

    assert_eq!(reason, ExitReason::Shutdown);
    assert_eq!(notes, "terminate:shutdown");
    witness.kill();
}

// --- snapshots and upgrades ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot____state____in_the_guest_formats_layout() {
    let h = up("c-snapshot", "").await;
    ask(&h, "count").await;
    ask(&h, "count").await;
    ask(&h, "note:kept").await;

    let bytes = h.snapshot().await.unwrap();

    let expected: (u32, ciborium::Value) = (
        1,
        ciborium::Value::Map(vec![
            ("count".into(), 2.into()),
            ("notes".into(), ciborium::Value::Array(vec!["kept".into()])),
            ("deferred".into(), ciborium::Value::Array(vec![])),
            ("refuse".into(), false.into()),
        ]),
    );
    let read: (u32, ciborium::Value) = ciborium::from_reader(bytes.as_slice()).unwrap();
    assert_eq!(read, expected);
    let mut written = Vec::new();
    ciborium::into_writer(&expected, &mut written).unwrap();
    assert_eq!(bytes, written);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade____snapshot_mode____state_deferred_callers_and_pid_kept() {
    let (_, app) = shared();
    let h = up("c-upgrade", "").await;
    ask(&h, "count").await;
    ask(&h, "note:before").await;
    let pending = tokio::spawn({
        let h = h.clone();
        async move { h.call(text("defer"), LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut next = spec("c-upgrade", "");
    next.version = "2.0.0".into();

    h.upgrade(app, next, "1.0.0").await.unwrap();
    let count = ask(&h, "count").await;
    let notes = ask(&h, "notes").await;
    let from = ask(&h, "loaded-from").await;
    let answered = ask(&h, "answer:after").await;
    let deferred = pending.await.unwrap().unwrap();

    assert_eq!(count, "2");
    assert_eq!(notes, "before");
    assert_eq!(from, "1.0.0");
    assert_eq!(answered, "answered:1");
    assert_eq!(body(&deferred), "after");
    assert_eq!(ask(&h, "self").await, pid(&h));
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade____snapshot_refused____old_version_keeps_running() {
    let (_, app) = shared();
    let h = up("c-refuse", "").await;
    ask(&h, "count").await;
    ask(&h, "refuse-upgrade").await;
    let mut next = spec("c-refuse", "");
    next.version = "2.0.0".into();

    let result = h.upgrade(app, next, "1.0.0").await;
    let count = ask(&h, "count").await;

    assert_eq!(
        result,
        Err(UpgradeError::LoadFailed("asked to refuse".into()))
    );
    assert_eq!(count, "2");
    assert_eq!(ask(&h, "loaded-from").await, "none");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback____after_snapshot_upgrade____previous_version_with_state() {
    let (rt, _) = shared();
    let name = "c-rollback";
    let base = json!({
        "limits": { "callback_deadline_ms": 1000, "memory_max": "128MiB" },
        "upgrade": { "mode": "snapshot", "snapshot_version": 1, "from_versions": ["^1", "^2"] }
    });
    let apps = rt
        .start_apps(vec![deployment(name, "1.0.0", base.clone())])
        .await
        .unwrap();
    let app = apps.app(name).unwrap();
    let h = app.instances()[0].clone();
    ask(&h, "count").await;
    apps.upgrade(
        name,
        deployment(name, "2.0.0", base),
        &UpgradeOptions::default(),
    )
    .await
    .unwrap();
    ask(&h, "count").await;

    let report = apps
        .rollback(name, &UpgradeOptions::default())
        .await
        .unwrap();
    let count = ask(&h, "count").await;
    let from = ask(&h, "loaded-from").await;

    assert_eq!(report.to.to_string(), "1.0.0");
    assert_eq!(count, "3");
    assert_eq!(from, "2.0.0");
    apps.stop().await;
}

// --- limits --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trap____in_a_call____caller_gets_crashed_and_instance_exits() {
    let h = up("c-trap", "").await;

    let result = h.call(text("trap"), LONG).await;
    let reason = h.wait().await;

    let Err(CallError::Exited(ExitReason::Crashed(why))) = &result else {
        panic!("expected exited(crashed), got {result:?}");
    };
    assert!(why.starts_with("trap:"), "{why}");
    assert_eq!(reason, ExitReason::Crashed(why.clone()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_max____allocation_beyond____crashes_with_memory_limit() {
    let h = up("c-memory", "").await;

    let small = ask(&h, "alloc:4").await;
    let big = h.call(text("alloc:256"), LONG).await;

    assert_eq!(small, "ok");
    assert_eq!(
        big,
        Err(CallError::Exited(ExitReason::Crashed(
            "memory_limit".into()
        )))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_deadline____guest_spins____exits_timeout() {
    let h = up("c-spin", "").await;
    let started = Instant::now();

    let result = h.call(text("spin"), LONG).await;

    assert_eq!(result, Err(CallError::Exited(ExitReason::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory____many_calls_and_casts____does_not_grow() {
    // What the host passes each callback must be freed after it: an SDK
    // whose runtime keeps it (TinyGo, spike S21) leaks 4 KiB a message here.
    let h = up("c-leak", "").await;
    let command = format!("echo:{}", "x".repeat(4096));
    for _ in 0..1_000 {
        ask(&h, &command).await;
    }
    let warm = h.memory_bytes();

    for _ in 0..10_000 {
        ask(&h, &command).await;
        h.cast(text(&command)).unwrap();
    }
    ask(&h, "echo:done").await;
    let after = h.memory_bytes();

    assert!(
        after <= warm + (2 << 20),
        "grew from {warm} to {after} bytes"
    );
    h.kill();
}

// --- health checks -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health____healthy_then_sick____answered_then_killed_unhealthy() {
    let (rt, app) = shared();
    let mut s = spec("c-health", "");
    s.health = Some(HealthCheck {
        interval: Duration::from_millis(30),
        failure_threshold: 2,
    });
    let h = rt.spawn(app, s).unwrap();
    let checks = ask_until(&h, "health-checks", |n| n.parse::<u32>().unwrap() >= 3).await;

    ask(&h, "health:out of disk").await;
    let reason = tokio::time::timeout(Duration::from_secs(5), h.wait())
        .await
        .unwrap();

    assert!(checks.parse::<u32>().unwrap() >= 3, "{checks}");
    assert_eq!(reason, ExitReason::Crashed("unhealthy".into()));
}

// --- logging -------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Record {
    level: String,
    fields: HashMap<String, String>,
}

impl Visit for Record {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().into(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().into(), value.into());
    }
}

static RECORDS: Mutex<Vec<Record>> = Mutex::new(Vec::new());

struct Capture;

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != "paddock::guest" {
            return;
        }
        let mut record = Record {
            level: event.metadata().level().to_string(),
            ..Record::default()
        };
        event.record(&mut record);
        RECORDS.lock().unwrap().push(record);
    }
}

fn capture() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::registry().with(Capture);
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
}

fn records(app: &str) -> Vec<Record> {
    RECORDS
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.fields.get("app").map(String::as_str) == Some(app))
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_level____host_event_then_operator_change____app_follows() {
    capture();
    let (rt, app) = shared();
    let mut s = spec("c-log", "");
    s.services.logging.level = LogLevel::Warn;
    let h = rt.spawn(app, s).unwrap();

    let first = ask(&h, "log-level").await;
    ask(&h, "log:warn:kept").await;
    ask(&h, "log:info:dropped").await;
    rt.set_log_level("c-log", Some(LogLevel::Debug));
    let changed = ask_until(&h, "log-level", |l| l == "debug").await;
    ask(&h, "log:debug:now-kept").await;

    assert_eq!(first, "warn");
    assert_eq!(changed, "debug");
    let deadline = Instant::now() + Duration::from_secs(2);
    while records("c-log").len() < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let got: Vec<(String, String)> = records("c-log")
        .into_iter()
        .map(|r| {
            (
                r.level,
                r.fields.get("message").cloned().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("WARN".to_string(), "kept".to_string()),
            ("DEBUG".to_string(), "now-kept".to_string())
        ]
    );
    h.kill();
}

// --- keyvalue and watches --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyvalue____set_get_keys_delete____round_trip() {
    let h = up("c-kv", "").await;

    let set = [
        ask(&h, "kv-set:data:b:2").await,
        ask(&h, "kv-set:data:a:1").await,
    ];
    let got = ask(&h, "kv-get:data:a").await;
    let keys = ask(&h, "kv-keys:data").await;
    ask(&h, "kv-delete:data:a").await;
    let gone = ask(&h, "kv-get:data:a").await;
    let unlisted = ask(&h, "kv-get:elsewhere:a").await;

    assert_eq!(set, ["ok".to_string(), "ok".to_string()]);
    assert_eq!(got, "1");
    assert_eq!(keys, "a,b");
    assert_eq!(gone, "none");
    assert_eq!(unlisted, "err:access-denied");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch____key_written____change_notice_handled() {
    let h = up("c-watch", "").await;

    let id = ask(&h, "watch:data:w").await;
    ask(&h, "kv-set:data:other:x").await;
    ask(&h, "kv-set:data:w:x").await;
    let changes = ask_until(&h, "changes", |c| !c.is_empty()).await;

    assert!(id.parse::<u64>().is_ok(), "{id}");
    assert!(changes.starts_with("w@"), "{changes}");
    assert!(!changes.contains("other"), "{changes}");
    h.kill();
}

// --- groups ----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn groups____join_publish_leave____members_receive_casts() {
    let a = up("c-groups", "").await;
    let b = up("c-groups", "").await;

    ask(&a, "join:c-groups-group").await;
    ask(&b, "join:c-groups-group").await;
    let members = ask(&a, "members:c-groups-group").await;
    let took = ask(&a, "publish:c-groups-group:note:hello").await;
    let notes = ask_until(&b, "notes", |n| !n.is_empty()).await;
    ask(&b, "leave:c-groups-group").await;
    let after = ask(&a, "members:c-groups-group").await;
    let denied = ask(&a, "join:not-listed").await;

    let mut both = [pid(&a), pid(&b)];
    both.sort();
    assert_eq!(members, both.join(","));
    assert_eq!(took, "2");
    assert_eq!(notes, "hello");
    assert_eq!(after, pid(&a));
    assert_eq!(denied, "err:access-denied");
    a.kill();
    b.kill();
}

// --- config, secrets and metrics ---------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config____set_and_missing_keys____value_or_none() {
    let h = up("c-config", "").await;

    let set = ask(&h, "config:mode").await;
    let missing = ask(&h, "config:nope").await;

    assert_eq!(set, "fast");
    assert_eq!(missing, "none");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets____bound_unbound_and_unlisted____value_or_error() {
    let h = up("c-secret", "").await;

    let token = ask(&h, "secret:token").await;
    let unbound = ask(&h, "secret:unbound").await;
    let unlisted = ask(&h, "secret:other").await;

    assert_eq!(token, TOKEN);
    assert!(unbound.starts_with("err:"), "{unbound}");
    assert_eq!(unlisted, "err:access-denied");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics____counter_gauge_histogram____rendered_for_the_app() {
    let (rt, _) = shared();
    let h = up("c-metrics", "").await;

    let made = [
        ask(&h, "counter:c_orders_total:2").await,
        ask(&h, "counter:c_orders_total:3").await,
        ask(&h, "gauge:c_queue:7").await,
        ask(&h, "histogram:c_latency:5").await,
    ];
    let text = rt.render_metrics();

    assert_eq!(made, ["ok", "ok", "ok", "ok"].map(String::from));
    for line in [
        r#"conformance_c_orders_total{app="c-metrics"} 5"#,
        r#"conformance_c_queue{app="c-metrics"} 7"#,
        r#"conformance_c_latency_bucket{app="c-metrics",le="10"} 1"#,
        r#"conformance_c_latency_bucket{app="c-metrics",le="1"} 0"#,
    ] {
        assert!(text.contains(line), "{line} not in {text}");
    }
    h.kill();
}
