//! `paddock:secrets`: names the manifest lists and the
//! operator grants, read from the node's file or environment sources,
//! through `paddock-sdk`'s `secrets` module and the SDK fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use common::{ask, fixture};
use paddock_bundle::manifest::Capability;
use paddock_host::{InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig, SecretSource};

const ENV_VAR: &str = "PADDOCK_TEST_SECRET_E3";
/// In secret values only: no log record or error may carry it.
const MARKER: &str = "s3cr3t-marker";

struct Setup {
    rt: Runtime,
    app: LoadedApp,
    file: PathBuf,
    _dir: tempfile::TempDir,
}

fn setup() -> &'static Setup {
    static SETUP: OnceLock<Setup> = OnceLock::new();
    SETUP.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("db-password");
        std::fs::write(&file, "hunter2\n").unwrap();
        let marked = dir.path().join("marked");
        std::fs::write(&marked, MARKER).unwrap();
        let binary = dir.path().join("binary");
        std::fs::write(&binary, [b"\xff".as_slice(), MARKER.as_bytes()].concat()).unwrap();
        // SAFETY: set once, before any thread of this test binary reads the
        // environment through the secrets service.
        unsafe { std::env::set_var(ENV_VAR, "from-env") };
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 8,
            secrets: BTreeMap::from([
                ("db".to_string(), SecretSource::File(file.clone())),
                ("api".to_string(), SecretSource::Env(ENV_VAR.to_string())),
                (
                    "gone".to_string(),
                    SecretSource::File(dir.path().join("missing")),
                ),
                ("marked".to_string(), SecretSource::File(marked)),
                ("binary".to_string(), SecretSource::File(binary)),
            ]),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
        Setup {
            rt,
            app,
            file,
            _dir: dir,
        }
    })
}

fn up(names: &[&str], granted: bool) -> InstanceHandle {
    let s = setup();
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        ..InstanceSpec::new("secrets", "1.0.0")
    };
    if granted {
        spec.granted = BTreeSet::from([Capability::Secrets]);
    }
    spec.services.secrets.names = names.iter().map(ToString::to_string).collect();
    s.rt.spawn(&s.app, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get____file_and_env_sources____values_read_each_time() {
    let s = setup();
    let a = up(&["db", "api"], true);

    let db = ask(&a, "secret:db").await;
    let api = ask(&a, "secret:api").await;
    std::fs::write(&s.file, "rotated").unwrap();
    let rotated = ask(&a, "secret:db").await;
    std::fs::write(&s.file, "hunter2\n").unwrap();

    assert_eq!(db, "hunter2");
    assert_eq!(api, "from-env");
    assert_eq!(rotated, "rotated");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get____unlisted_ungranted_unbound_or_unreadable____errors() {
    let listed = up(&["db", "nowhere", "gone"], true);
    let ungranted = up(&["db"], false);
    let narrowed = {
        let s = setup();
        let mut spec = InstanceSpec {
            granted: BTreeSet::from([Capability::Secrets]),
            ..InstanceSpec::new("narrowed", "1.0.0")
        };
        spec.services.secrets.names = BTreeSet::from(["db".to_string()]);
        s.rt.spawn(&s.app, spec.with_granted_secrets(&BTreeSet::new()))
            .unwrap()
    };

    let unlisted = ask(&listed, "secret:api").await;
    let unbound = ask(&listed, "secret:nowhere").await;
    let unreadable = ask(&listed, "secret:gone").await;
    let not_granted = ask(&ungranted, "secret:db").await;
    let not_in_policy = ask(&narrowed, "secret:db").await;

    assert_eq!(unlisted, "err:access denied");
    assert_eq!(unbound, "err:not found");
    assert!(
        unreadable.starts_with("err:unavailable: secret 'gone': file:"),
        "{unreadable}"
    );
    assert!(
        !unreadable.contains("missing"),
        "the path leaked: {unreadable}"
    );
    assert_eq!(not_granted, "err:access denied");
    assert_eq!(not_in_policy, "err:access denied");
    for h in [listed, ungranted, narrowed] {
        h.kill();
    }
}

/// Every log record of this test binary, any target and level, as text.
static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Everything;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Everything {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!(" {}={value:?}", field.name()));
            }
        }
        let mut fields = Fields(event.metadata().target().to_string());
        event.record(&mut fields);
        LOGGED.lock().unwrap().push(fields.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get____values_and_failures____never_in_a_log_record_or_error() {
    use tracing_subscriber::layer::SubscriberExt;
    let _ =
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Everything));
    let a = up(&["marked", "binary"], true);

    let value = ask(&a, "secret:marked").await;
    // The guest decodes the bytes as UTF-8 and fails.
    let binary = ask(&a, "secret:binary").await;
    a.kill();
    a.wait().await;

    assert_eq!(value, MARKER);
    assert!(binary.starts_with("err:"), "{binary}");
    assert!(!binary.contains(MARKER), "{binary}");
    let logged = LOGGED.lock().unwrap();
    assert!(!logged.is_empty(), "no records captured");
    let leaked: Vec<&String> = logged.iter().filter(|r| r.contains(MARKER)).collect();
    assert!(leaked.is_empty(), "{leaked:?}");
}
