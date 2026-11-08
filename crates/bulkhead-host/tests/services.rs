//! Host services against a native app (`bulkhead-fixture-native`, ROADMAP
//! Z9d): logging, stdout capture, config, keyvalue and the filesystem.
//! Covers Phase 1 exit criterion 8. Deny stubs, and a component's
//! filesystem grant, still run on the component services fixture.

// The native fixture runs on Linux only.
#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{
    Bucket, ByteSize, Capability, Durability, LogLevel, NativeProgram,
};
use bulkhead_host::{
    CallError, ExitReason, HostError, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, Preopen,
    Runtime, RuntimeConfig,
};
use common::{LONG, ask, fixture, text};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

// --- capturing guest log records -------------------------------------------

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
        if event.metadata().target() != "bulkhead::guest" {
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

/// Records for `app`, waiting up to 2 s for at least `n`.
async fn records(app: &str, n: usize) -> Vec<Record> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let found: Vec<Record> = RECORDS
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.fields.get("app").map(String::as_str) == Some(app))
            .cloned()
            .collect();
        if found.len() >= n || Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// --- setup -----------------------------------------------------------------

fn load(rt: &Runtime) -> LoadedApp {
    rt.load_native(NativeCode {
        path: common::native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    })
}

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 64,
            native: common::native_config(),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = load(&rt);
        (rt, app)
    })
}

/// The component services fixture, for what only a component has.
fn component() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 8,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&fixture("bulkhead_fixture_services")).unwrap();
        (rt, app)
    })
}

fn granted(caps: &[Capability]) -> BTreeSet<Capability> {
    caps.iter().copied().collect()
}

fn spec(app: &str, caps: &[Capability]) -> InstanceSpec {
    InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(2),
        shutdown_timeout: Duration::from_millis(300),
        granted: granted(caps),
        ..InstanceSpec::new(app, "1.0.0")
    }
}

fn bucket(durability: Durability, quota: Option<u64>) -> Bucket {
    Bucket {
        durability,
        quota: quota.map(ByteSize),
        read_concurrency: false,
    }
}

fn with_buckets(mut s: InstanceSpec, buckets: &[(&str, Bucket)]) -> InstanceSpec {
    s.services.keyvalue.buckets = buckets
        .iter()
        .map(|(n, b)| ((*n).to_string(), b.clone()))
        .collect();
    s
}

async fn up(rt: &Runtime, app: &LoadedApp, spec: InstanceSpec) -> InstanceHandle {
    let h = rt.spawn(app, spec).unwrap();
    ask(&h, "echo up").await;
    h
}

// --- logging -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log____granted____record_carries_level_tags_and_context() {
    capture();
    let (rt, app) = shared();
    let h = up(rt, app, spec("log-tags", &[Capability::Logging])).await;

    ask(&h, "log-at warn disk almost full").await;
    let found = records("log-tags", 1).await;

    assert_eq!(found.len(), 1);
    let r = &found[0];
    assert_eq!(r.level, "WARN");
    assert_eq!(r.fields["message"], "disk almost full");
    // The native protocol's records carry no context.
    assert_eq!(r.fields["context"], "");
    assert_eq!(r.fields["version"], "1.0.0");
    assert_eq!(r.fields["pid"], h.pid().to_string());
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log____below_max_level____dropped() {
    capture();
    let (rt, app) = shared();
    let mut s = spec("log-level", &[Capability::Logging]);
    s.services.logging.level = LogLevel::Warn;
    let h = up(rt, app, s).await;

    ask(&h, "log-at info chatter").await;
    ask(&h, "log-at error kept").await;
    let found = records("log-level", 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let messages: Vec<&str> = found.iter().map(|r| r.fields["message"].as_str()).collect();
    assert_eq!(messages, vec!["kept"]);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log____not_granted____dropped_without_error() {
    capture();
    let (rt, app) = shared();
    let h = up(rt, app, spec("log-denied", &[])).await;

    let reply = ask(&h, "log-at error hidden").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(reply, "ok");
    assert!(records("log-denied", 0).await.is_empty());
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log____rate_limited____excess_dropped_and_count_reported_later() {
    capture();
    let (rt, app) = shared();
    let mut s = spec("log-rate", &[Capability::Logging]);
    s.services.logging.rate_limit = Some(2);
    let h = up(rt, app, s).await;

    for i in 0..5 {
        ask(&h, &format!("log-at info burst-{i}")).await;
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    ask(&h, "log-at info after").await;
    let found = records("log-rate", 4).await;

    let summary: Vec<String> = found
        .iter()
        .map(|r| {
            r.fields
                .get("dropped")
                .map_or_else(|| r.fields["message"].clone(), |d| format!("dropped={d}"))
        })
        .collect();
    assert_eq!(summary, vec!["burst-0", "burst-1", "dropped=3", "after"]);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log____newline_in_message____one_record_with_it_escaped() {
    capture();
    let (rt, app) = shared();
    let h = up(rt, app, spec("log-inject", &[Capability::Logging])).await;

    ask(
        &h,
        "log-at info first\n2026-10-10T12:00:00Z  INFO bulkhead::node::gate: forged",
    )
    .await;
    let found = records("log-inject", 1).await;

    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].fields["message"],
        "first\\n2026-10-10T12:00:00Z  INFO bulkhead::node::gate: forged"
    );
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout____over_the_apps_log_rate____excess_dropped_and_counted() {
    capture();
    let (rt, app) = shared();
    let mut s = spec("stdout-rate", &[]);
    s.services.logging.rate_limit = Some(2);
    let h = up(rt, app, s).await;

    for i in 0..5 {
        ask(&h, &format!("print burst-{i}")).await;
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    ask(&h, "print after").await;
    let found = records("stdout-rate", 4).await;

    let summary: Vec<String> = found
        .iter()
        .map(|r| {
            r.fields
                .get("dropped")
                .map_or_else(|| r.fields["message"].clone(), |d| format!("dropped={d}"))
        })
        .collect();
    assert_eq!(summary, vec!["burst-0", "burst-1", "dropped=3", "after"]);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_and_stderr____written____captured_as_stderr_records() {
    capture();
    let (rt, app) = shared();
    let h = up(rt, app, spec("stdio", &[])).await;

    // A native app's two streams share one pipe, logged as stderr.
    ask(&h, "print hello from stdout").await;
    ask(&h, "eprint trouble on stderr").await;
    let found = records("stdio", 2).await;

    assert_eq!(found.len(), 2);
    assert_eq!(found[0].fields["stream"], "stderr");
    assert_eq!(found[0].fields["message"], "hello from stdout");
    assert_eq!(found[0].level, "WARN");
    assert_eq!(found[1].fields["stream"], "stderr");
    assert_eq!(found[1].fields["message"], "trouble on stderr");
    assert_eq!(found[1].fields["pid"], h.pid().to_string());
    h.kill();
}

// --- config ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config____granted____returns_values() {
    let (rt, app) = shared();
    let mut s = spec("cfg", &[Capability::Config]);
    s.services.config.values = BTreeMap::from([
        ("db.host".into(), "pg.internal".into()),
        ("mode".into(), "fast".into()),
    ]);
    let h = up(rt, app, s).await;

    let one = ask(&h, "config db.host").await;
    let missing = ask(&h, "config nope").await;
    let all = ask(&h, "config-all").await;

    assert_eq!(one, "pg.internal");
    assert_eq!(missing, "none");
    assert_eq!(all, "db.host=pg.internal,mode=fast");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config____not_granted____access_denied() {
    let (rt, app) = shared();
    let mut s = spec("cfg-denied", &[]);
    s.services.config.values = BTreeMap::from([("k".into(), "v".into())]);
    let h = rt.spawn(app, s).unwrap();

    let reply = ask(&h, "config k").await;

    assert_eq!(reply, "err:AccessDenied");
    h.kill();
}

// --- keyvalue ----------------------------------------------------------------

fn kv_spec(app: &str) -> InstanceSpec {
    with_buckets(
        spec(app, &[Capability::KeyValue]),
        &[("data", bucket(Durability::Memory, None))],
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyvalue____store_atomics_and_batch____round_trip() {
    let (rt, app) = shared();
    let h = up(rt, app, kv_spec("kv-ops")).await;

    let replies = [
        ask(&h, "kv set data a 1").await,
        ask(&h, "kv get data a").await,
        ask(&h, "kv exists data a").await,
        ask(&h, "kv exists data z").await,
        ask(&h, "kv incr data n 5").await,
        ask(&h, "kv incr data n 2").await,
        ask(&h, "kv setmany data b=2,c=3").await,
        ask(&h, "kv many data a,b,z").await,
        ask(&h, "kv keys data").await,
        ask(&h, "kv delmany data b,c").await,
        ask(&h, "kv del data a").await,
        ask(&h, "kv get data a").await,
        ask(&h, "kv keys data").await,
    ];

    assert_eq!(
        replies,
        [
            "ok", "1", "true", "false", "5", "7", "ok", "1,2,-", "a,b,c,n", "ok", "ok", "none", "n"
        ]
    );
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyvalue____many_keys____list_keys_pages_through_all() {
    let (rt, app) = shared();
    let h = up(rt, app, kv_spec("kv-pages")).await;

    ask(&h, "kv fill data 600").await;
    let keys = ask(&h, "kv keys data").await;

    assert_eq!(keys.split(',').count(), 600);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyvalue____memory_bucket____survives_instance_crash() {
    let (rt, app) = shared();
    let first = up(rt, app, kv_spec("kv-crash")).await;
    ask(&first, "kv set data k kept").await;
    first.call(text("panic"), LONG).await.unwrap_err();
    first.wait().await;

    let second = rt.respawn(app, kv_spec("kv-crash"), first.pid()).unwrap();
    let kept = ask(&second, "kv get data k").await;

    assert_eq!(kept, "kept");
    second.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyvalue____same_bucket_two_apps____isolated() {
    let (rt, app) = shared();
    let a = up(rt, app, kv_spec("kv-iso-a")).await;
    let b = up(rt, app, kv_spec("kv-iso-b")).await;

    ask(&a, "kv set data k from-a").await;
    let seen = ask(&b, "kv get data k").await;

    assert_eq!(seen, "none");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyvalue____undeclared_bucket_or_not_granted____access_denied() {
    let (rt, app) = shared();
    let declared = up(rt, app, kv_spec("kv-deny")).await;
    let ungranted = up(
        rt,
        app,
        with_buckets(
            spec("kv-deny2", &[]),
            &[("data", bucket(Durability::Memory, None))],
        ),
    )
    .await;

    let other_bucket = ask(&declared, "kv get other k").await;
    let not_granted = ask(&ungranted, "kv get data k").await;

    assert_eq!(other_bucket, "err:AccessDenied");
    assert_eq!(not_granted, "err:AccessDenied");
    declared.kill();
    ungranted.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyvalue____quota_exceeded____set_refused() {
    let (rt, app) = shared();
    let s = with_buckets(
        spec("kv-quota", &[Capability::KeyValue]),
        &[("data", bucket(Durability::Memory, Some(16)))],
    );
    let h = up(rt, app, s).await;

    let fits = ask(&h, "kv set data a 1234567").await;
    let over = ask(&h, "kv set data b 1234567890").await;

    assert_eq!(fits, "ok");
    assert_eq!(over, "err:Other(\"bucket quota of 16 bytes exceeded\")");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_8____disk_bucket____survives_crash_and_node_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = RuntimeConfig {
        max_instances: 4,
        data_dir: Some(dir.path().to_path_buf()),
        native: common::native_config(),
        ..RuntimeConfig::default()
    };
    let disk = || {
        with_buckets(
            spec("kv-disk", &[Capability::KeyValue]),
            &[("orders", bucket(Durability::Disk, None))],
        )
    };
    {
        let rt = Runtime::new(config.clone()).unwrap();
        let app = load(&rt);
        let h = up(&rt, &app, disk()).await;
        ask(&h, "kv set orders o-1 paid").await;
        h.call(text("panic"), LONG).await.unwrap_err();
        h.wait().await;
        let again = rt.respawn(&app, disk(), h.pid()).unwrap();
        assert_eq!(ask(&again, "kv get orders o-1").await, "paid");
        again.kill();
        again.wait().await;
    }

    // The old instance tasks release the database as they finish.
    let deadline = Instant::now() + Duration::from_secs(5);
    let rt = loop {
        match Runtime::new(config.clone()) {
            Ok(rt) => break rt,
            Err(HostError::Storage(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    let app = load(&rt);
    let h = up(&rt, &app, disk()).await;
    let after_restart = ask(&h, "kv get orders o-1").await;

    assert_eq!(after_restart, "paid");
    h.kill();
}

// --- filesystem --------------------------------------------------------------

struct Dirs {
    _root: tempfile::TempDir,
    priv_dir: PathBuf,
    data_dir: PathBuf,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let priv_dir = root.path().join("priv");
    let data_dir = root.path().join("data");
    std::fs::create_dir_all(&priv_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(priv_dir.join("hello.txt"), "from the bundle").unwrap();
    Dirs {
        _root: root,
        priv_dir,
        data_dir,
    }
}

fn fs_spec(app: &str, d: &Dirs, caps: &[Capability]) -> InstanceSpec {
    let mut s = spec(app, caps);
    s.preopens = vec![
        Preopen {
            host: d.priv_dir.clone(),
            guest: "/priv".into(),
            writable: false,
        },
        Preopen {
            host: d.data_dir.clone(),
            guest: "/data".into(),
            writable: true,
        },
    ];
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_8____filesystem____priv_read_only_data_writable_escapes_fail() {
    let (rt, app) = shared();
    let d = dirs();
    let h = up(rt, app, fs_spec("fs", &d, &[Capability::Filesystem])).await;

    let read_priv = ask(&h, "vcat /priv/hello.txt").await;
    let write_priv = ask(&h, "vwrite /priv/new.txt").await;
    let write_data = ask(&h, "vwrite /data/out.txt").await;
    let read_data = ask(&h, "vcat /data/out.txt").await;
    let escape = ask(&h, "vcat /data/../priv/hello.txt").await;
    let deep_escape = ask(&h, "vcat /data/../../../../etc/hostname").await;
    let ambient = ask(&h, "vcat /etc/hostname").await;

    assert_eq!(read_priv, "from the bundle");
    assert!(write_priv.starts_with("err:"), "{write_priv}");
    assert_eq!(write_data, "ok");
    assert_eq!(read_data, "x");
    assert_eq!(
        std::fs::read_to_string(d.data_dir.join("out.txt")).unwrap(),
        "x"
    );
    assert!(escape.starts_with("err:"), "{escape}");
    assert!(deep_escape.starts_with("err:"), "{deep_escape}");
    assert!(ambient.starts_with("err:"), "{ambient}");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filesystem____not_granted____no_preopens() {
    let (rt, app) = component();
    let d = dirs();
    let h = up(rt, app, fs_spec("fs-denied", &d, &[])).await;

    let reply = ask(&h, "read:/priv/hello.txt").await;

    assert!(reply.starts_with("err:"), "{reply}");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filesystem____native_app_not_granted____no_preopens() {
    let (rt, app) = shared();
    let d = dirs();
    let h = up(rt, app, fs_spec("fs-denied-native", &d, &[])).await;

    let reply = ask(&h, "vcat /priv/hello.txt").await;

    assert!(reply.starts_with("err:"), "{reply}");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn____preopen_directory_missing____refused() {
    let (rt, app) = shared();
    let mut s = spec("fs-missing", &[Capability::Filesystem]);
    s.preopens = vec![Preopen {
        host: PathBuf::from("/nonexistent/bulkhead-test"),
        guest: "/data".into(),
        writable: true,
    }];

    let result = rt.spawn(app, s);

    assert!(matches!(result, Err(HostError::Preopen(_))), "{result:?}");
}

// --- deny stubs --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_stubs____unimplemented_interface____error_channel_returns_access_denied() {
    let (rt, app) = component();
    let h = up(rt, app, spec("deny", &[])).await;

    let reply = ask(&h, "denied-ping").await;

    assert_eq!(app.stubbed_imports(), ["bulkhead:fixture/denied"]);
    assert_eq!(reply, "err:access-denied");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_stubs____no_error_channel____traps_naming_the_function() {
    let (rt, app) = component();
    let h = up(rt, app, spec("deny-boom", &[])).await;

    let result = h.call(text("denied-boom"), LONG).await;

    let Err(CallError::Exited(ExitReason::Crashed(why))) = &result else {
        panic!("expected exited(crashed), got {result:?}");
    };
    assert!(why.contains("bulkhead:fixture/denied#boom"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_level____operator_override____applies_at_once_and_resets() {
    capture();
    let (rt, app) = shared();
    let h = up(rt, app, spec("log-override", &[Capability::Logging])).await;

    rt.set_log_level("log-override", Some(LogLevel::Error));
    ask(&h, "log-at warn hidden").await;
    rt.set_log_level("log-override", Some(LogLevel::Debug));
    ask(&h, "log-at debug shown").await;
    rt.set_log_level("log-override", None);
    ask(&h, "log-at debug hidden-again").await;
    ask(&h, "log-at info back-to-info").await;
    let found = records("log-override", 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let messages: Vec<&str> = found.iter().map(|r| r.fields["message"].as_str()).collect();
    assert_eq!(messages, vec!["shown", "back-to-info"]);
    assert_eq!(rt.log_level("log-override"), None);
    h.kill();
}
