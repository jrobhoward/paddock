//! Groups, config, secrets and metrics for native-process apps: the
//! fixture executable's commands through `bulkhead-runner`.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::{Capability, GroupsScope, NativeProgram};
use bulkhead_host::{
    HostObjectRequest, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime,
    RuntimeConfig, SecretSource,
};
use common::{ask, ask_until, native_fixtures};

/// The secret file `token` reads, kept for the tests' run.
fn secret_dir() -> &'static tempfile::TempDir {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), b"s3cret").unwrap();
        dir
    })
}

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let mut native = NativeConfig::default();
        native.runner = Some(native_fixtures().0.clone());
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 32,
            native,
            secrets: BTreeMap::from([(
                "token".to_string(),
                SecretSource::File(secret_dir().path().join("token")),
            )]),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load_native(NativeCode {
            path: native_fixtures().1.clone(),
            kind: NativeProgram::Executable,
        });
        (rt, app)
    })
}

/// A native instance of `app` with `granted`: groups `g-<app>`,
/// `shared-g` and `exits` to join and publish to, config `mode = fast`, the secrets
/// `token` (bound on the node) and `absent` (not bound), and metrics with
/// the prefix `nat`.
fn spec(app: &str, granted: &[Capability]) -> InstanceSpec {
    let mut spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        granted: granted.iter().copied().collect(),
        ..InstanceSpec::new(app, "1.0.0")
    };
    let groups = vec![
        format!("g-{app}"),
        "shared-g".to_string(),
        "exits".to_string(),
    ];
    spec.services.groups.scope = GroupsScope {
        join: groups.clone(),
        publish: groups,
    };
    spec.services.config.values = BTreeMap::from([("mode".to_string(), "fast".to_string())]);
    spec.services.secrets.names = BTreeSet::from(["token".to_string(), "absent".to_string()]);
    spec.services.metrics.prefix = "nat".into();
    spec.services.metrics.max_series = 2;
    spec
}

fn up(app: &str, granted: &[Capability]) -> InstanceHandle {
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec(app, granted)).unwrap()
}

const ALL: &[Capability] = &[
    Capability::Groups,
    Capability::Config,
    Capability::Secrets,
    Capability::Metrics,
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_groups____joined_and_published____members_get_the_cast() {
    let a = up("ng-a", ALL);
    let b = up("ng-b", ALL);
    let publisher = up("ng-pub", ALL);

    let joined = [
        ask(&a, "group join shared-g").await,
        ask(&b, "group join shared-g").await,
    ];
    let members = ask(&publisher, "group members shared-g").await;
    let delivered = ask(&publisher, "group publish shared-g hello").await;
    let a_got = ask_until(&a, "infos", |i| i.contains("cast hello")).await;
    let b_got = ask_until(&b, "infos", |i| i.contains("cast hello")).await;

    assert_eq!(joined, ["ok", "ok"]);
    assert_eq!(members, "2");
    assert_eq!(delivered, "2");
    assert!(a_got.contains("cast hello"), "{a_got}");
    assert!(b_got.contains("cast hello"), "{b_got}");
    for h in [a, b, publisher] {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_groups____not_granted_unlisted_or_over_groups_max____refused() {
    let (rt, loaded) = shared();
    let ungranted = up("ng-denied", &[]);
    let mut limited = spec("ng-limit", ALL);
    limited.host_objects = HostObjectRequest {
        groups: Some(1),
        ..HostObjectRequest::default()
    };
    let limited = rt.spawn(loaded, limited).unwrap();

    let not_granted = ask(&ungranted, "group join shared-g").await;
    let unlisted = ask(&limited, "group join elsewhere").await;
    let first = ask(&limited, "group join g-ng-limit").await;
    let second = ask(&limited, "group join shared-g").await;

    assert_eq!(not_granted, "err:AccessDenied");
    assert_eq!(unlisted, "err:AccessDenied");
    assert_eq!(first, "ok");
    assert_eq!(second, "err:LimitReached");
    ungranted.kill();
    limited.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_groups____member_exits____no_longer_a_member() {
    let member = up("ng-exit", ALL);
    let watcher = up("ng-exit-watch", ALL);
    ask(&member, "group join exits").await;
    let before = ask(&watcher, "group members exits").await;

    member.kill();
    member.wait().await;
    let after = ask_until(&watcher, "group members exits", |m| m == "0").await;

    assert_eq!(before, "1");
    assert_eq!(after, "0");
    watcher.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_config____granted_or_not____values_or_access_denied() {
    let a = up("nc-a", ALL);
    let ungranted = up("nc-denied", &[]);

    let mode = ask(&a, "config mode").await;
    let missing = ask(&a, "config other").await;
    let all = ask(&a, "config-all").await;
    let denied = ask(&ungranted, "config mode").await;

    assert_eq!(mode, "fast");
    assert_eq!(missing, "none");
    assert_eq!(all, "mode=fast");
    assert_eq!(denied, "err:AccessDenied");
    a.kill();
    ungranted.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_secrets____listed_unbound_unlisted_or_not_granted____value_not_found_or_denied() {
    let a = up("ns-a", ALL);
    let ungranted = up("ns-denied", &[]);

    let token = ask(&a, "secret token").await;
    let unbound = ask(&a, "secret absent").await;
    let unlisted = ask(&a, "secret other").await;
    let denied = ask(&ungranted, "secret token").await;

    assert_eq!(token, "s3cret");
    assert_eq!(unbound, "err:NotFound");
    assert_eq!(unlisted, "err:AccessDenied");
    assert_eq!(denied, "err:AccessDenied");
    a.kill();
    ungranted.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_metrics____updates_sent_with_the_callback____counted_once_each() {
    let (rt, _) = shared();
    let a = up("nm-a", ALL);
    let id = ask(&a, "metric register counter jobs_total").await;

    ask(&a, &format!("metric add {id} 2")).await;
    ask(&a, &format!("metric add {id} 3")).await;
    // A counter cannot go down: refused and logged, the rest unchanged.
    ask(&a, &format!("metric add {id} -1")).await;
    let text = rt.render_metrics();

    assert!(text.contains(r#"nat_jobs_total{app="nm-a"} 5"#), "{text}");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_metrics____not_granted_or_over_max_series____refused() {
    let a = up("nm-limit", ALL);
    let ungranted = up("nm-denied", &[]);

    let denied = ask(&ungranted, "metric register counter x_total").await;
    let first = ask(&a, "metric register counter one_total").await;
    let second = ask(&a, "metric register gauge two").await;
    let third = ask(&a, "metric register gauge three").await;

    assert_eq!(denied, "err:AccessDenied");
    assert!(first.parse::<u32>().is_ok(), "{first}");
    assert!(second.parse::<u32>().is_ok(), "{second}");
    assert_eq!(third, "err:LimitReached");
    a.kill();
    ungranted.kill();
}
