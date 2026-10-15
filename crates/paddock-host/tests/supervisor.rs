//! Host-native supervisors, against the chaos fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{LONG, ask, shared, spec, text};
use paddock_host::{
    Backoff, ChildSpec, ChildStart, ExitReason, InstanceHandle, Pid, RestartType, Strategy,
    SupervisorHandle, SupervisorSpec,
};

fn child(id: &str, restart: RestartType, init_args: &str) -> ChildSpec {
    let (_, app) = shared();
    ChildSpec {
        id: id.into(),
        restart,
        start: ChildStart::Instance {
            app: app.clone(),
            spec: spec(init_args),
        },
    }
}

fn sup(strategy: Strategy, children: Vec<ChildSpec>) -> SupervisorSpec {
    SupervisorSpec {
        name: "test".into(),
        strategy,
        intensity: 5,
        period: Duration::from_secs(10),
        backoff: Backoff::none(),
        children,
    }
}

fn pids(s: &SupervisorHandle) -> Vec<Option<Pid>> {
    s.children().iter().map(paddock_host::Child::pid).collect()
}

fn instance(s: &SupervisorHandle, index: usize) -> InstanceHandle {
    s.children()[index].instance().unwrap().clone()
}

/// Wait until the children's pids differ from `before` in `changed` places.
async fn wait_for_pids(
    s: &SupervisorHandle,
    done: impl Fn(&[Option<Pid>]) -> bool,
) -> Vec<Option<Pid>> {
    let mut rx = s.subscribe_children();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now: Vec<Option<Pid>> = rx
            .borrow_and_update()
            .iter()
            .map(paddock_host::Child::pid)
            .collect();
        if done(&now) || Instant::now() > deadline {
            return now;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
    }
}

fn next(p: Option<Pid>) -> Option<Pid> {
    p.map(|p| Pid {
        incarnation: p.incarnation + 1,
        ..p
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_for_one____child_killed____only_it_restarts_with_next_incarnation() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::OneForOne,
            vec![
                child("a", RestartType::Permanent, ""),
                child("b", RestartType::Permanent, ""),
            ],
        ))
        .unwrap();
    let before = pids(&s);

    instance(&s, 0).kill();
    let after = wait_for_pids(&s, |p| p[0] != before[0] && p[0].is_some()).await;

    assert_eq!(after[0], next(before[0]));
    assert_eq!(after[1], before[1]);
    assert_eq!(ask(&instance(&s, 0), "count").await, "1");
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_for_all____child_crashes____every_child_restarts() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::OneForAll,
            vec![
                child("a", RestartType::Permanent, ""),
                child("b", RestartType::Permanent, ""),
            ],
        ))
        .unwrap();
    let before = pids(&s);
    let b = instance(&s, 1);
    ask(&b, "count").await;

    instance(&s, 0).call(text("trap"), LONG).await.unwrap_err();
    let after = wait_for_pids(&s, |p| {
        p[0] != before[0] && p[1] != before[1] && p.iter().all(Option::is_some)
    })
    .await;

    assert_eq!(after, vec![next(before[0]), next(before[1])]);
    assert_eq!(b.wait().await, ExitReason::Shutdown);
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_for_one____middle_child_crashes____it_and_later_children_restart() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::RestForOne,
            vec![
                child("a", RestartType::Permanent, ""),
                child("b", RestartType::Permanent, ""),
                child("c", RestartType::Permanent, ""),
            ],
        ))
        .unwrap();
    let before = pids(&s);

    instance(&s, 1).kill();
    let after = wait_for_pids(&s, |p| {
        p[1] != before[1] && p[2] != before[2] && p.iter().all(Option::is_some)
    })
    .await;

    assert_eq!(after, vec![before[0], next(before[1]), next(before[2])]);
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simple_one_for_one____start_children_then_one_crashes____only_it_restarts() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::SimpleOneForOne,
            vec![child("worker", RestartType::Permanent, "")],
        ))
        .unwrap();

    for _ in 0..3 {
        s.start_child().await.unwrap();
    }
    let before = pids(&s);
    instance(&s, 2).kill();
    let after = wait_for_pids(&s, |p| p.len() == 3 && p[2] != before[2] && p[2].is_some()).await;

    assert_eq!(after, vec![before[0], before[1], next(before[2])]);
    let ids: Vec<String> = s.children().iter().map(|c| c.id.clone()).collect();
    assert_eq!(ids, vec!["worker-1", "worker-2", "worker-3"]);
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_child____not_simple_one_for_one____refused() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::OneForOne,
            vec![child("a", RestartType::Permanent, "")],
        ))
        .unwrap();

    let result = s.start_child().await;

    assert!(matches!(
        result,
        Err(paddock_host::HostError::InvalidSupervisor(_))
    ));
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary____child_crashes____removed_not_restarted() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::OneForOne,
            vec![
                child("t", RestartType::Temporary, ""),
                child("p", RestartType::Permanent, ""),
            ],
        ))
        .unwrap();

    instance(&s, 0).kill();
    let after = wait_for_pids(&s, |p| p.len() == 1).await;

    assert_eq!(after.len(), 1);
    assert_eq!(s.children()[0].id, "p");
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_child____normal_exit_then_crash____only_the_crash_restarted() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::OneForOne,
            vec![
                child("n", RestartType::Transient, ""),
                child("c", RestartType::Transient, ""),
            ],
        ))
        .unwrap();
    let before = pids(&s);
    let n = instance(&s, 0);

    // The instance stops itself; whether its reply gets out first is a race,
    // and either is fine here.
    let _ = n
        .call(
            text(&format!(
                "exit:{}.{}.{}:normal",
                n.pid().node,
                n.pid().id,
                n.pid().incarnation
            )),
            LONG,
        )
        .await;
    instance(&s, 1).kill();
    let after = wait_for_pids(&s, |p| {
        p[0].is_none() && p[1] != before[1] && p[1].is_some()
    })
    .await;

    assert_eq!(after, vec![None, next(before[1])]);
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intensity____exceeded____supervisor_stops_children_and_escalates() {
    let (rt, _) = shared();
    let mut spec = sup(
        Strategy::OneForOne,
        vec![
            child("a", RestartType::Permanent, ""),
            child("b", RestartType::Permanent, ""),
        ],
    );
    spec.intensity = 2;
    let s = rt.start_supervisor(spec).unwrap();
    let b = instance(&s, 1);

    for _ in 0..3 {
        let before = pids(&s)[0];
        instance(&s, 0).kill();
        wait_for_pids(&s, |p| {
            p[0] != before && (p[0].is_some() || s.exit_reason().is_some())
        })
        .await;
    }
    let reason = s.wait().await;

    let ExitReason::Crashed(why) = reason else {
        panic!("expected crashed, got {reason:?}");
    };
    assert!(
        why.contains("max restart intensity (2 in 10000 ms)"),
        "{why}"
    );
    assert!(why.contains("last exit: a killed"), "{why}");
    assert_eq!(b.wait().await, ExitReason::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intensity____child_traps_in_init____restart_loop_escalates() {
    let (rt, _) = shared();
    let mut spec = sup(
        Strategy::OneForOne,
        vec![child("bad", RestartType::Permanent, "trap")],
    );
    spec.intensity = 3;

    let s = rt.start_supervisor(spec).unwrap();
    let reason = tokio::time::timeout(LONG, s.wait()).await.unwrap();

    let ExitReason::Crashed(why) = reason else {
        panic!("expected crashed, got {reason:?}");
    };
    assert!(why.contains("max restart intensity"), "{why}");
    assert!(why.contains("last exit: bad crashed: trap"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown____running_children____stopped_gracefully_in_reverse_order() {
    let (rt, _) = shared();
    let s = rt
        .start_supervisor(sup(
            Strategy::OneForOne,
            vec![
                child("a", RestartType::Permanent, ""),
                child("b", RestartType::Permanent, ""),
            ],
        ))
        .unwrap();
    let (a, b) = (instance(&s, 0), instance(&s, 1));
    // `a` is stopped only once `b` has exited, so `b` has exited by the time
    // `a` has. (Two waiters recording their own order would race.)
    let b_gone_when_a_exits = tokio::spawn({
        let (a, b) = (a.clone(), b.clone());
        async move {
            a.wait().await;
            b.exit_reason().is_some()
        }
    });

    s.shutdown();
    let reason = s.wait().await;

    assert_eq!(reason, ExitReason::Shutdown);
    assert_eq!(a.exit_reason(), Some(ExitReason::Shutdown));
    assert_eq!(b.exit_reason(), Some(ExitReason::Shutdown));
    assert!(b_gone_when_a_exits.await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested____child_supervisor_escalates____parent_restarts_it() {
    let (rt, _) = shared();
    let mut inner = sup(
        Strategy::OneForOne,
        vec![child("w", RestartType::Permanent, "")],
    );
    inner.name = "inner".into();
    inner.intensity = 0;
    let outer = rt
        .start_supervisor(sup(
            Strategy::OneForOne,
            vec![ChildSpec {
                id: "inner".into(),
                restart: RestartType::Permanent,
                start: ChildStart::Supervisor(Box::new(inner)),
            }],
        ))
        .unwrap();
    let Some(paddock_host::ChildHandle::Supervisor(first)) = outer.children()[0].handle.clone()
    else {
        panic!("expected a supervisor child");
    };
    let worker = first.children()[0].instance().unwrap().clone();

    worker.kill();
    let reason = first.wait().await;
    let mut rx = outer.subscribe_children();
    let second = loop {
        if let Some(paddock_host::ChildHandle::Supervisor(h)) =
            rx.borrow_and_update()[0].handle.clone()
            && h.exit_reason().is_none()
        {
            break h;
        }
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .unwrap()
            .unwrap();
    };

    assert!(matches!(reason, ExitReason::Crashed(_)), "{reason:?}");
    let new_worker = second.children()[0].instance().unwrap().clone();
    assert_ne!(new_worker.pid().id, worker.pid().id);
    assert_eq!(ask(&new_worker, "echo:back").await, "back");
    outer.shutdown();
    outer.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backoff____child_crashes____restart_waits_the_initial_delay() {
    let (rt, _) = shared();
    let mut spec = sup(
        Strategy::OneForOne,
        vec![child("a", RestartType::Permanent, "")],
    );
    spec.backoff = Backoff {
        initial: Duration::from_millis(300),
        max: Duration::from_secs(1),
        jitter: false,
    };
    let s = rt.start_supervisor(spec).unwrap();
    let before = pids(&s);
    let started = Instant::now();

    instance(&s, 0).kill();
    wait_for_pids(&s, |p| p[0] != before[0] && p[0].is_some()).await;

    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    s.shutdown();
    s.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_supervisor____simple_one_for_one_without_one_template____refused() {
    let (rt, _) = shared();

    let result = rt.start_supervisor(sup(
        Strategy::SimpleOneForOne,
        vec![
            child("a", RestartType::Permanent, ""),
            child("b", RestartType::Permanent, ""),
        ],
    ));

    assert!(matches!(
        result,
        Err(paddock_host::HostError::InvalidSupervisor(_))
    ));
}
