//! Passivation (spike S11): an idle instance drops its Store and keeps its
//! pid, mailbox, links, names and timers; a call, a cast, a timer or an exit
//! signal wakes it with its state, through `save-snapshot` and
//! `load-snapshot`. The runtime has fewer Store slots than instances.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use common::{LONG, ask, counter, spec, text};
use paddock_host::{
    ExitReason, InstanceHandle, InstanceSpec, LifecycleState, LoadedApp, Runtime, RuntimeConfig,
};

/// Store slots: fewer than the instances these tests start at once.
const SLOTS: u32 = 4;

/// A runtime with [`SLOTS`] Store slots and room for 200 instances, and the
/// counter fixture.
fn runtime() -> &'static (Runtime, LoadedApp) {
    static RT: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    RT.get_or_init(|| {
        let rt = Runtime::new(RuntimeConfig {
            max_instances: SLOTS,
            max_processes: Some(200),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&counter(1)).unwrap();
        (rt, app)
    })
}

fn passivating(names: &[&str]) -> InstanceSpec {
    let mut s = spec(
        &names
            .iter()
            .map(|n| format!("register:{n} "))
            .collect::<String>(),
    );
    s.app = "counter".into();
    s.version = "1.0.0".into();
    s.registered_names = names.iter().map(ToString::to_string).collect();
    s.passivate_after = Some(Duration::from_millis(30));
    s
}

fn spawn(names: &[&str]) -> InstanceHandle {
    let (rt, app) = runtime();
    rt.spawn(app, passivating(names)).unwrap()
}

/// Wait up to 5 s for `h` to reach `state`.
async fn until_state(h: &InstanceHandle, state: LifecycleState) {
    let mut watch = h.subscribe_state();
    tokio::time::timeout(Duration::from_secs(5), watch.wait_for(|s| *s == state))
        .await
        .unwrap_or_else(|_| panic!("{} never {state:?}, now {:?}", h.pid(), h.state()))
        .unwrap();
}

async fn incs(h: &InstanceHandle, n: usize) {
    for _ in 0..n {
        ask(h, "inc").await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____idle____store_dropped_and_a_call_wakes_it_with_its_state() {
    let h = spawn(&["p-call"]);
    incs(&h, 3).await;
    until_state(&h, LifecycleState::Passive).await;
    let memory_passive = h.memory_bytes();
    let found = runtime().0.whereis("p-call").map(|w| w.pid());

    let got = ask(&h, "get").await;

    assert_eq!(memory_passive, 0);
    assert_eq!(found, Some(h.pid()), "the name stays registered");
    assert_eq!(got, "1.0.0 count=3");
    assert_eq!(h.restarts(), 0, "the same incarnation");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____cast_or_timer_while_passive____wakes_and_handles_it() {
    let h = spawn(&[]);
    ask(&h, "after:300").await;
    until_state(&h, LifecycleState::Passive).await;

    // The timer fires while passivated: it wakes the instance, which adds
    // one and is passivated again.
    tokio::time::sleep(Duration::from_millis(400)).await;
    until_state(&h, LifecycleState::Passive).await;
    h.cast(text("inc")).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    until_state(&h, LifecycleState::Passive).await;
    let got = ask(&h, "get").await;

    assert_eq!(got, "1.0.0 count=2");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____linked_instance_exits____trapped_exit_wakes_it() {
    let (rt, app) = runtime();
    let mut other = passivating(&["p-linked"]);
    other.passivate_after = None;
    let linked = rt.spawn(app, other).unwrap();
    until_state(&linked, LifecycleState::Active).await;
    let h = spawn(&[]);
    assert_eq!(ask(&h, "trap-link:p-linked").await, "ok");
    until_state(&h, LifecycleState::Passive).await;

    linked.kill();
    linked.wait().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = ask(&h, "get").await;

    assert_eq!(got, "1.0.0 count=1", "the exit arrived as info");
    assert!(h.exit_reason().is_none());
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____deferred_call_unanswered____instance_stays_live() {
    let h = spawn(&[]);
    let held = {
        let h = h.clone();
        tokio::spawn(async move { h.call(text("hold"), LONG).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let while_held = h.state();

    ask(&h, "release").await;
    let answer = held.await.unwrap().unwrap();
    until_state(&h, LifecycleState::Passive).await;

    assert_eq!(while_held, LifecycleState::Active);
    assert_eq!(answer.body, b"released");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____snapshot_while_passive____answered_without_waking() {
    let h = spawn(&[]);
    incs(&h, 2).await;
    until_state(&h, LifecycleState::Passive).await;

    let snapshot = h.snapshot().await.unwrap();

    assert_eq!(snapshot, b"2");
    assert_eq!(h.state(), LifecycleState::Passive);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____stopped_while_passive____kill_ends_it_and_shutdown_wakes_it_first() {
    let killed = spawn(&[]);
    let stopped = spawn(&[]);
    until_state(&killed, LifecycleState::Passive).await;
    until_state(&stopped, LifecycleState::Passive).await;

    killed.kill();
    stopped.shutdown(ExitReason::Shutdown);
    let killed_reason = killed.wait().await;
    let stopped_reason = stopped.wait().await;

    assert_eq!(killed_reason, ExitReason::Killed);
    assert_eq!(stopped_reason, ExitReason::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____more_instances_than_store_slots____each_keeps_its_count() {
    let (rt, _) = runtime();
    let many: Vec<_> = (0..16).map(|_| spawn(&[])).collect();
    for (i, h) in many.iter().enumerate() {
        incs(h, i).await;
    }

    let mut counts = Vec::new();
    for h in &many {
        counts.push(ask(h, "get").await);
    }

    let want: Vec<_> = (0..16).map(|i| format!("1.0.0 count={i}")).collect();
    assert_eq!(counts, want);
    assert!(rt.store_count() <= SLOTS as usize);
    for h in &many {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____every_slot_held_by_an_idle_instance____one_passivates_early() {
    let (rt, app) = runtime();
    let mut slow = passivating(&[]);
    slow.passivate_after = Some(Duration::from_secs(60));
    let many: Vec<_> = (0..2 * SLOTS)
        .map(|_| rt.spawn(app, slow.clone()).unwrap())
        .collect();

    let started = std::time::Instant::now();
    for h in &many {
        ask(h, "inc").await;
    }
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(5),
        "waited for idle timers: {took:?}"
    );
    for h in &many {
        h.kill();
    }
}

/// A runtime of its own with one Store slot, so a test can hold it.
fn one_slot(component: &[u8]) -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 1,
        max_processes: Some(8),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(component).unwrap();
    (rt, app)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____every_slot_held_by_an_instance_that_never_passivates____start_fails_no_slot()
 {
    let (rt, app) = one_slot(&counter(1));
    let mut never = passivating(&[]);
    never.passivate_after = None;
    let holder = rt.spawn(&app, never).unwrap();
    until_state(&holder, LifecycleState::Active).await;
    let mut waiting = passivating(&[]);
    waiting.init_timeout = Duration::from_millis(300);

    let starved = rt.spawn(&app, waiting).unwrap();
    let reason = starved.wait().await;
    let metrics = rt.render_metrics();

    assert_eq!(reason, ExitReason::Crashed("no_slot".into()));
    assert!(
        metrics.contains("paddock_slot_timeouts_total{app=\"counter\"} 1"),
        "{metrics}"
    );
    holder.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade____no_slot_for_the_new_store____reverts_and_the_old_version_runs() {
    let (rt, app) = one_slot(&counter(1));
    let mut never = passivating(&[]);
    never.passivate_after = None;
    let h = rt.spawn(&app, never.clone()).unwrap();
    incs(&h, 2).await;
    let mut next = never;
    next.init_timeout = Duration::from_millis(300);

    let result = h.upgrade(&app, next, "1.0.0").await;

    assert_eq!(
        result,
        Err(paddock_host::UpgradeError::Instantiate(
            "no instance slot".into()
        ))
    );
    assert_eq!(ask(&h, "get").await, "1.0.0 count=2");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____passivated_and_woken____counted_in_metrics() {
    let (rt, _) = runtime();
    let h = spawn(&[]);
    until_state(&h, LifecycleState::Passive).await;

    let gauge_while_passive = rt.render_metrics();
    ask(&h, "get").await;
    let after_wake = rt.render_metrics();

    let value = |text: &str, prefix: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map_or(0, |v| v.trim().parse().unwrap())
    };
    assert!(
        value(
            &gauge_while_passive,
            "paddock_instances_passive{app=\"counter\"}"
        ) >= 1
    );
    assert!(value(&after_wake, "paddock_passivations_total{app=\"counter\"}") >= 1);
    assert!(value(&after_wake, "paddock_wake_seconds_count{app=\"counter\"}") >= 1);
    h.kill();
}

/// An SDK app with a watch and a group membership, passivated: a change
/// notice and a publish each wake it, and its state, kept by its own
/// `save_snapshot`, carries the notices across.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passivation____sdk_app_watching_and_in_a_group____woken_by_notice_and_publish() {
    use paddock_bundle::manifest::{AccessMode, Capability, Durability, GroupsScope};
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 4,
        max_processes: Some(8),
        buckets: std::collections::BTreeMap::from([(
            "board".to_string(),
            paddock_host::SharedBucket {
                durability: Durability::Memory,
                quota: None,
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&common::fixture("paddock_fixture_sdk")).unwrap();
    let spec = |passivate: bool| {
        let mut s = InstanceSpec {
            callback_deadline: Duration::from_secs(5),
            granted: [Capability::KeyValue, Capability::Groups].into(),
            ..InstanceSpec::new("sdk", "1.0.0")
        };
        s.services.keyvalue.shared =
            std::collections::BTreeMap::from([("board".to_string(), AccessMode::Rw)]);
        s.services.groups.scope = GroupsScope {
            join: vec!["g".into()],
            publish: vec!["g".into()],
        };
        s.passivate_after = passivate.then_some(Duration::from_millis(30));
        s
    };
    let sleeper = rt.spawn(&app, spec(true)).unwrap();
    let writer = rt.spawn(&app, spec(false)).unwrap();
    ask(&sleeper, "watch:board:k").await;
    ask(&sleeper, "join:g").await;
    until_state(&sleeper, LifecycleState::Passive).await;

    let members = ask(&writer, "members:g").await;
    ask(&writer, "set:board:k:1").await;
    until_state(&sleeper, LifecycleState::Passive).await;
    ask(&writer, "publish:g:hello").await;
    until_state(&sleeper, LifecycleState::Passive).await;
    let changes = ask(&sleeper, "changes").await;
    let received = ask(&sleeper, "received").await;

    assert!(members.contains(&sleeper.pid().text()), "{members}");
    assert!(changes.starts_with("k@"), "{changes}");
    assert_eq!(received, "hello");
    sleeper.kill();
    writer.kill();
}
