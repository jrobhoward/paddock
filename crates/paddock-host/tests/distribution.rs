//! Two runtimes as nodes of an in-memory mesh: calls, casts, exit signals,
//! links and monitors across nodes, and what partitions and faults do to
//! them.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{LONG, ask, ask_until, body, fixture, pid_text, spec, text};
use paddock_host::transport::memory::{Faults, Mesh};
use paddock_host::{CallError, ExitReason, InstanceHandle, LoadedApp, Runtime, RuntimeConfig};

/// The chaos component's bytes, read once.
fn chaos() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| fixture("paddock_fixture_chaos"))
}

/// A node running the chaos app.
struct Node {
    rt: Runtime,
    app: LoadedApp,
}

impl Node {
    fn new(node: u32) -> Self {
        Self::with(RuntimeConfig {
            node,
            max_instances: 16,
            ..RuntimeConfig::default()
        })
    }

    fn with(config: RuntimeConfig) -> Self {
        let rt = Runtime::new(config).unwrap();
        let app = rt.load(chaos()).unwrap();
        Self { rt, app }
    }

    fn spawn(&self, init_args: &str) -> InstanceHandle {
        self.rt.spawn(&self.app, spec(init_args)).unwrap()
    }

    fn id(&self) -> u32 {
        self.rt.node()
    }
}

/// Nodes 1 and 2, joined by a fresh mesh.
fn two_nodes(seed: u64) -> (Node, Node, Mesh) {
    let (a, b) = (Node::new(1), Node::new(2));
    let mesh = Mesh::new(seed);
    mesh.join(&a.rt).unwrap();
    mesh.join(&b.rt).unwrap();
    (a, b, mesh)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote____call_and_cast____reach_the_other_node() {
    let (n1, n2, _mesh) = two_nodes(1);
    let a = n1.spawn("");
    let b = n2.spawn("");
    let to_b = pid_text(b.pid());

    let call = ask(&a, &format!("call:{to_b}:1000:echo:across")).await;
    let cast = ask(&a, &format!("cast:{to_b}:count")).await;
    // Same pair, so in order: the cast lands before this call.
    let count = ask(&a, &format!("call:{to_b}:1000:count")).await;
    let from_host = n1.rt.call(b.pid(), text("echo:host"), LONG).await.unwrap();
    let to_dead = ask(&a, &format!("call:{}.999.1@1:500:echo:x", n2.id())).await;

    assert_eq!(call, "ok:across");
    assert_eq!(cast, "sent");
    assert_eq!(count, "ok:2");
    assert_eq!(body(&from_host), "host");
    assert_eq!(to_dead, "err:noproc");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote____calls_from_a_peer_at_its_limit____overloaded_until_one_ends() {
    let n1 = Node::new(1);
    let n2 = Node::with(RuntimeConfig {
        node: 2,
        max_instances: 16,
        incoming_calls_per_peer: 2,
        ..RuntimeConfig::default()
    });
    let mesh = Mesh::new(11);
    mesh.join(&n1.rt).unwrap();
    mesh.join(&n2.rt).unwrap();
    let c = n2.spawn("");
    let held: Vec<_> = (0..2)
        .map(|_| {
            let (rt, pid) = (n1.rt.clone(), c.pid());
            // A probe below can take a place first; try again until held.
            tokio::spawn(async move {
                loop {
                    let r = rt.call(pid, text("defer"), LONG).await;
                    if r != Err(CallError::Overloaded) {
                        break r;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        })
        .collect();
    // Both held calls are in progress once a third is refused.
    let started = Instant::now();
    let over = loop {
        let r = n1.rt.call(c.pid(), text("echo:x"), LONG).await;
        if r.is_err() || started.elapsed() > LONG {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let local = n2.rt.call(c.pid(), text("echo:local"), LONG).await;
    n2.rt.cast(c.pid(), text("answer:done")).unwrap();
    let mut answered = Vec::new();
    for h in held {
        answered.push(h.await.unwrap());
    }
    let after = n1.rt.call(c.pid(), text("echo:after"), LONG).await;

    assert_eq!(over, Err(CallError::Overloaded));
    assert_eq!(body(&local.unwrap()), "local");
    for a in answered {
        assert_eq!(body(&a.unwrap()), "done");
    }
    assert_eq!(body(&after.unwrap()), "after");
    c.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote____link____exits_cross_both_ways() {
    let (n1, n2, _mesh) = two_nodes(2);
    let a = n1.spawn("trap-exits");
    let b = n2.spawn("trap-exits");
    let c = n2.spawn("");
    let (to_b, to_c) = (pid_text(b.pid()), pid_text(c.pid()));
    ask(&a, &format!("link:{to_b}")).await;
    ask(&a, &format!("link:{to_c}")).await;
    // The link frames are in flight; a round trip puts them behind us.
    ask(&a, &format!("call:{to_b}:1000:echo:sync")).await;

    c.kill();
    let a_infos = ask_until(&a, "infos", |s| s.contains("exit:")).await;
    a.kill();
    let dead_link = {
        let d = n1.spawn("trap-exits");
        ask(&d, &format!("link:{}.999.1@1", n2.id())).await;
        ask_until(&d, "infos", |s| s.contains("exit:")).await
    };
    let b_infos = ask_until(&b, "infos", |s| s.contains("exit:")).await;

    assert_eq!(a_infos, format!("exit:{to_c}:killed"));
    assert_eq!(dead_link, format!("exit:{}.999.1@1:noproc", n2.id()));
    assert_eq!(b_infos, format!("exit:{}:killed", pid_text(a.pid())));
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote____exit_signal____kills_target() {
    let (n1, n2, _mesh) = two_nodes(3);
    let a = n1.spawn("");
    let b = n2.spawn("");

    ask(&a, &format!("exit:{}:killed", pid_text(b.pid()))).await;
    let reason = tokio::time::timeout(LONG, b.wait()).await.unwrap();

    assert_eq!(reason, ExitReason::Killed);
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote____monitor____down_on_exit_none_after_demonitor() {
    let (n1, n2, _mesh) = two_nodes(4);
    let a = n1.spawn("");
    let b = n2.spawn("");
    let c = n2.spawn("");
    let (to_b, to_c) = (pid_text(b.pid()), pid_text(c.pid()));
    let ref_b = ask(&a, &format!("monitor:{to_b}")).await;
    let ref_c = ask(&a, &format!("monitor:{to_c}")).await;
    ask(&a, &format!("demonitor:{ref_c}")).await;
    ask(&a, &format!("call:{to_b}:1000:echo:sync")).await;

    c.kill();
    b.kill();
    let infos = ask_until(&a, "infos", |s| s.contains("down:")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let later = ask(&a, "infos").await;
    let dead = ask(&a, &format!("monitor:{}.999.1@1", n2.id())).await;
    let dead_down = ask_until(&a, "infos", |s| s.contains(&format!("down:{dead}:"))).await;

    assert_eq!(infos, format!("down:{ref_b}:{to_b}:killed"));
    assert_eq!(later, infos);
    assert!(
        dead_down.ends_with(&format!("down:{dead}:{}.999.1@1:noproc", n2.id())),
        "{dead_down}"
    );
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition____links_monitors_calls____end_with_noconnection() {
    let (n1, n2, mesh) = two_nodes(5);
    let a = n1.spawn("trap-exits");
    let b = n2.spawn("trap-exits");
    let c = n2.spawn("");
    let (to_a, to_b, to_c) = (pid_text(a.pid()), pid_text(b.pid()), pid_text(c.pid()));
    ask(&a, &format!("link:{to_b}")).await;
    let reference = ask(&a, &format!("monitor:{to_c}")).await;
    ask(&a, &format!("call:{to_b}:1000:echo:sync")).await;
    let rt1 = n1.rt.clone();
    let waiting = tokio::spawn(async move { rt1.call(c.pid(), text("noreply"), LONG).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    mesh.partition(n1.id(), n2.id());
    let started = Instant::now();
    let call = waiting.await.unwrap();
    let a_infos = ask_until(&a, "infos", |s| s.contains("down:")).await;
    let b_infos = ask_until(&b, "infos", |s| s.contains("exit:")).await;
    let cut_call = ask(&a, &format!("call:{to_b}:1000:echo:x")).await;
    let cut_cast = n1.rt.cast(b.pid(), text("count"));

    assert_eq!(call, Err(CallError::Noconnection));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        a_infos,
        format!("exit:{to_b}:noconnection;down:{reference}:{to_c}:noconnection")
    );
    assert_eq!(b_infos, format!("exit:{to_a}:noconnection"));
    assert_eq!(cut_call, "err:noconnection");
    assert_eq!(cut_cast, Err(CallError::Noconnection));
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heal____after_partition____traffic_flows_old_links_stay_gone() {
    let (n1, n2, mesh) = two_nodes(6);
    let a = n1.spawn("trap-exits");
    let b = n2.spawn("trap-exits");
    let to_b = pid_text(b.pid());
    ask(&a, &format!("link:{to_b}")).await;
    ask(&a, &format!("call:{to_b}:1000:echo:sync")).await;
    mesh.partition(n1.id(), n2.id());
    ask_until(&a, "infos", |s| s.contains("exit:")).await;

    mesh.heal(n1.id(), n2.id());
    let call = ask(&a, &format!("call:{to_b}:1000:echo:back")).await;
    b.kill();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let infos = ask(&a, "infos").await;

    assert_eq!(call, "ok:back");
    assert_eq!(infos, format!("exit:{to_b}:noconnection"));
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn faults____delay_and_jitter____slower_but_in_order() {
    let (n1, n2, mesh) = two_nodes(7);
    let faults = Faults {
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(20),
        ..Faults::default()
    };
    mesh.set_faults(n1.id(), n2.id(), faults);
    mesh.set_faults(n2.id(), n1.id(), faults);
    let b = n2.spawn("");

    for _ in 0..10 {
        n1.rt.cast(b.pid(), text("count")).unwrap();
    }
    let started = Instant::now();
    let count = n1.rt.call(b.pid(), text("count"), LONG).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(body(&count), "11");
    assert!(elapsed >= Duration::from_millis(80), "{elapsed:?}");
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn faults____every_frame_dropped____casts_lost_calls_time_out() {
    let (n1, n2, mesh) = two_nodes(8);
    mesh.set_default_faults(Faults {
        drop: 1.0,
        ..Faults::default()
    });
    let b = n2.spawn("");

    let cast = n1.rt.cast(b.pid(), text("count"));
    let call = n1
        .rt
        .call(b.pid(), text("echo:x"), Duration::from_millis(200))
        .await;
    let count = ask(&b, "count").await;

    assert_eq!(cast, Ok(()));
    assert_eq!(call, Err(CallError::Timeout));
    assert_eq!(count, "1");
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mesh____same_node_twice____refused() {
    let (a, b) = (Node::new(1), Node::new(1));
    let mesh = Mesh::new(9);
    mesh.join(&a.rt).unwrap();

    let refused = mesh.join(&b.rt);

    assert!(refused.is_err());
}
