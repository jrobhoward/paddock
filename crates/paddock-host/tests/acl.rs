//! Messaging ACLs on one node: calls, casts, exit signals and
//! links between apps pass only when the sender's `may_call` and the
//! target's `accept_from` both allow them; monitors, the host and an app's
//! own instances are not checked.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{ask, pid_text, shared, spec, text};
use paddock_host::InstanceHandle;
use paddock_host::acl::Acl;

/// An instance of `app` with the given routes.
fn up(app: &str, init: &str, accept_from: &[&str], may_call: &[&str]) -> InstanceHandle {
    let (rt, loaded) = shared();
    let mut s = spec(init);
    s.app = app.into();
    s.acl = Acl {
        accept_from: accept_from.iter().map(|a| (*a).to_string()).collect(),
        may_call: may_call.iter().map(|a| (*a).to_string()).collect(),
    };
    rt.spawn(loaded, s).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____accept_from_omits_the_sender____access_denied_until_listed() {
    let orders = up("acl1-orders", "", &["acl1-web"], &[]);
    let web = up("acl1-web", "", &[], &[]);
    let admin = up("acl1-admin", "", &[], &[]);
    let to = pid_text(orders.pid());

    let refused = ask(&admin, &format!("call:{to}:1000:echo:x")).await;
    let allowed = ask(&web, &format!("call:{to}:1000:echo:x")).await;

    assert_eq!(refused, "err:access-denied");
    assert_eq!(allowed, "ok:x");
    for h in [orders, web, admin] {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____may_call_omits_the_target____access_denied() {
    let orders = up("acl2-orders", "", &[], &[]);
    let web = up("acl2-web", "", &[], &["acl2-billing"]);

    let refused = ask(
        &web,
        &format!("call:{}:1000:echo:x", pid_text(orders.pid())),
    )
    .await;

    assert_eq!(refused, "err:access-denied");
    orders.kill();
    web.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast_and_exit____refused_sender____dropped() {
    let orders = up("acl3-orders", "", &["acl3-web"], &[]);
    let admin = up("acl3-admin", "", &[], &[]);
    let to = pid_text(orders.pid());

    ask(&admin, &format!("cast:{to}:count")).await;
    ask(&admin, &format!("exit:{to}:kill")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let count = ask(&orders, "count").await;

    assert_eq!(count, "1");
    assert_eq!(orders.exit_reason(), None);
    orders.kill();
    admin.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____refused_sender____noproc_at_once_and_the_target_untouched() {
    let (rt, _) = shared();
    let orders = up("acl4-orders", "", &["acl4-web"], &[]);
    let admin = up("acl4-admin", "trap-exits", &[], &[]);
    let to = pid_text(orders.pid());

    ask(&admin, &format!("link:{to}")).await;
    let infos = ask(&admin, "infos").await;

    assert_eq!(infos, format!("exit:{to}:noproc"));
    assert!(rt.links(orders.pid()).links.is_empty());
    orders.kill();
    admin.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exemptions____same_app_monitor_and_host____not_checked() {
    let (rt, _) = shared();
    let a = up("acl5-orders", "", &["acl5-web"], &["acl5-web"]);
    let b = up("acl5-orders", "", &["acl5-web"], &["acl5-web"]);
    let admin = up("acl5-admin", "", &[], &[]);
    let to_b = pid_text(b.pid());

    let same_app = ask(&a, &format!("call:{to_b}:1000:echo:x")).await;
    ask(&admin, &format!("monitor:{to_b}")).await;
    let monitors = rt.links(b.pid()).monitored_by.len();
    let host = rt.call(b.pid(), text("echo:host"), common::LONG).await;

    assert_eq!(same_app, "ok:x");
    assert_eq!(monitors, 1);
    assert!(host.is_ok());
    for h in [a, b, admin] {
        h.kill();
    }
}
