//! `paddock:groups` (ROADMAP H2): join, leave, members and publish through
//! `paddock-sdk`'s `groups` wrapper and the SDK fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use common::{ask, ask_until, fixture};
use paddock_bundle::manifest::{Capability, GroupsScope};
use paddock_host::{
    HostObjectRequest, InstanceHandle, InstanceSpec, LoadedApp, Pid, Runtime, RuntimeConfig,
};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 32,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
        (rt, app)
    })
}

/// A spec granted `paddock:groups` with this scope. Each test uses its own
/// group names, since tests share one runtime.
fn spec(join: &[&str], publish: &[&str]) -> InstanceSpec {
    let mut s = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: [Capability::Groups].into(),
        ..InstanceSpec::new("groups", "1.0.0")
    };
    s.services.groups.scope = GroupsScope {
        join: join.iter().map(ToString::to_string).collect(),
        publish: publish.iter().map(ToString::to_string).collect(),
    };
    s
}

fn up(spec: InstanceSpec) -> InstanceHandle {
    let (rt, app) = shared();
    rt.spawn(app, spec).unwrap()
}

fn pids(handles: &[&InstanceHandle]) -> String {
    let mut pids: Vec<Pid> = handles.iter().map(|h| h.pid()).collect();
    pids.sort();
    pids.iter()
        .map(|p| format!("<{}>", p.text()))
        .collect::<Vec<_>>()
        .join(",")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish____two_members____each_receives_one_cast() {
    let a = up(spec(&["g-pub"], &[]));
    let b = up(spec(&["g-pub"], &[]));
    let producer = up(spec(&[], &["g-pub"]));
    ask(&a, "join:g-pub").await;
    ask(&b, "join:g-pub").await;

    let reached = ask(&producer, "publish:g-pub:tick").await;

    assert_eq!(reached, "2");
    assert_eq!(ask_until(&a, "received", |r| r == "tick").await, "tick");
    assert_eq!(ask_until(&b, "received", |r| r == "tick").await, "tick");
    assert_eq!(ask(&producer, "received").await, "");
    for h in [a, b, producer] {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish____publisher_is_a_member____receives_its_own() {
    let a = up(spec(&["g-self"], &["g-self"]));
    ask(&a, "join:g-self").await;

    let reached = ask(&a, "publish:g-self:me").await;

    assert_eq!(reached, "1");
    assert_eq!(ask_until(&a, "received", |r| r == "me").await, "me");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members____join_twice_and_leave____listed_once_then_gone() {
    let a = up(spec(&["g-members"], &[]));
    let b = up(spec(&["g-members"], &[]));
    ask(&a, "join:g-members").await;
    ask(&a, "join:g-members").await;
    ask(&b, "join:g-members").await;

    let both = ask(&a, "members:g-members").await;
    ask(&a, "leave:g-members").await;
    ask(&a, "leave:g-members").await;
    let one = ask(&a, "members:g-members").await;

    assert_eq!(both, pids(&[&a, &b]));
    assert_eq!(one, pids(&[&b]));
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members____member_exits____removed() {
    let a = up(spec(&["g-exit"], &[]));
    let b = up(spec(&["g-exit"], &[]));
    ask(&a, "join:g-exit").await;
    ask(&b, "join:g-exit").await;

    b.kill();
    b.wait().await;
    let left = ask(&a, "members:g-exit").await;

    assert_eq!(left, pids(&[&a]));
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope____not_granted_or_not_listed____access_denied() {
    let mut ungranted = spec(&["g-deny"], &["g-deny"]);
    ungranted.granted.clear();
    let none = up(ungranted);
    let publisher = up(spec(&[], &["g-deny"]));

    let denied = [
        ask(&none, "join:g-deny").await,
        ask(&none, "members:g-deny").await,
        ask(&none, "publish:g-deny:x").await,
        ask(&publisher, "join:g-deny").await,
        ask(&publisher, "publish:g-other:x").await,
        ask(&publisher, "members:g-other").await,
    ];
    let allowed = [
        ask(&publisher, "publish:g-deny:x").await,
        ask(&publisher, "members:g-deny").await,
    ];

    assert!(
        denied.iter().all(|r| r == "err:access denied"),
        "{denied:?}"
    );
    assert_eq!(allowed, ["0".to_string(), String::new()]);
    none.kill();
    publisher.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join____over_groups_max____limit_reached() {
    let mut limited = spec(&["g-lim-1", "g-lim-2"], &[]);
    limited.host_objects = HostObjectRequest {
        groups: Some(1),
        ..HostObjectRequest::default()
    };
    let a = up(limited);
    ask(&a, "join:g-lim-1").await;

    let again = ask(&a, "join:g-lim-1").await;
    let second = ask(&a, "join:g-lim-2").await;
    ask(&a, "leave:g-lim-1").await;
    let after_leave = ask(&a, "join:g-lim-2").await;

    assert_eq!(again, "ok");
    assert_eq!(second, "err:group limit reached");
    assert_eq!(after_leave, "ok");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish____body_above_members_message_max____that_member_misses_it() {
    let mut small = spec(&["g-size"], &[]);
    // Room for the test's commands ("join:g-size"), not the published body.
    small.message_max_bytes = 12;
    let a = up(small);
    let b = up(spec(&["g-size"], &[]));
    let producer = up(spec(&[], &["g-size"]));
    ask(&a, "join:g-size").await;
    ask(&b, "join:g-size").await;

    let reached = ask(&producer, "publish:g-size:longer-than-twelve").await;

    assert_eq!(reached, "1");
    assert_eq!(
        ask_until(&b, "received", |r| !r.is_empty()).await,
        "longer-than-twelve"
    );
    assert_eq!(ask(&a, "received").await, "");
    for h in [a, b, producer] {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade____member_upgraded____stays_in_its_groups() {
    let (_, app) = shared();
    let a = up(spec(&["g-upgrade"], &["g-upgrade"]));
    ask(&a, "join:g-upgrade").await;

    a.upgrade(app, spec(&["g-upgrade"], &["g-upgrade"]), "1.0.0")
        .await
        .unwrap();
    let members = ask(&a, "members:g-upgrade").await;
    let reached = ask(&a, "publish:g-upgrade:after").await;

    assert_eq!(members, pids(&[&a]));
    assert_eq!(reached, "1");
    a.kill();
}
