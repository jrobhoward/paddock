//! Native-process apps through `bulkhead-runner`: the fixture
//! executable's callbacks and process functions, its failures as exit
//! reasons, its limits, and a rustbridge plugin.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{Capability, NativeProgram};
use bulkhead_host::{
    CallError, Envelope, ExitReason, InstanceHandle, InstanceSpec, LoadedApp, NativeCode,
    NativeConfig, Pid, Runtime, RuntimeConfig,
};
use common::{LONG, ask, body, native_fixtures, text};

fn runtime(tune: impl FnOnce(&mut NativeConfig)) -> Runtime {
    let mut native = NativeConfig::default();
    native.runner = Some(native_fixtures().0.clone());
    tune(&mut native);
    Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

fn executable(rt: &Runtime) -> LoadedApp {
    rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    })
}

fn spec(init_args: &str) -> InstanceSpec {
    InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_millis(500),
        init_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(1),
        init_args: init_args.as_bytes().to_vec(),
        ..InstanceSpec::new("native", "1.0.0")
    }
}

/// A pid as the fixture writes and reads it.
fn fixture_pid(pid: Pid) -> String {
    format!("{}.{}.{}", pid.node, pid.id, pid.incarnation)
}

async fn start(rt: &Runtime, app: &LoadedApp) -> InstanceHandle {
    rt.spawn(app, spec("")).unwrap()
}

#[tokio::test]
async fn executable____calls_then_shutdown____replies_and_stops_with_reason() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let echo = ask(&instance, "echo hello").await;
    let me = ask(&instance, "self").await;
    instance.shutdown(ExitReason::Shutdown);
    let reason = instance.wait().await;

    assert_eq!(echo, "hello");
    assert_eq!(me, fixture_pid(instance.pid()));
    assert_eq!(reason, ExitReason::Shutdown);
}

#[tokio::test]
async fn executable____call_larger_than_the_pipe____echoed_whole() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;
    // Over a pipe's 64 KiB both ways: the node's write goes part direct
    // and part through its writer task, and the reply arrives in many
    // reads.
    let big: String = (0..300_000)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();

    let echo = ask(&instance, &format!("echo {big}")).await;
    let after = ask(&instance, "echo small").await;

    assert_eq!(echo, big);
    assert_eq!(after, "small");
}

#[tokio::test]
async fn executable____call_monitor_and_timer____process_functions_work() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let a = start(&rt, &app).await;
    let b = start(&rt, &app).await;
    let b_text = fixture_pid(b.pid());

    let called = ask(&a, &format!("call {b_text} echo through")).await;
    let reference = ask(&a, &format!("monitor {b_text}")).await;
    let timer = ask(&a, "after 20 tick").await;
    b.shutdown(ExitReason::Normal);
    b.wait().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let infos = ask(&a, "infos").await;

    assert_eq!(called, "through");
    assert!(infos.contains(&format!("timer {timer} tick")), "{infos}");
    assert!(
        infos.contains(&format!("down {reference} {b_text} Normal")),
        "{infos}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executable____waits_on_a_call_past_its_deadline____not_killed() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let caller = rt
        .spawn(
            &app,
            InstanceSpec {
                callback_deadline: Duration::from_millis(300),
                ..spec("")
            },
        )
        .unwrap();
    let callee = start(&rt, &app).await;
    let to = fixture_pid(callee.pid());

    let waiting = {
        let caller = caller.clone();
        tokio::spawn(async move { ask(&caller, &format!("call {to} noreply")).await })
    };
    tokio::time::sleep(Duration::from_millis(600)).await;
    callee.cast(text("reply late")).unwrap();
    let reply = waiting.await.unwrap();
    let still = ask(&caller, "echo alive").await;

    assert_eq!(reply, "late");
    assert_eq!(still, "alive");
}

#[tokio::test]
async fn executable____noreply_then_cast____caller_gets_later_reply() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let waiting = {
        let instance = instance.clone();
        tokio::spawn(async move { instance.call(text("noreply"), LONG).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    instance.cast(text("reply later")).unwrap();
    let reply = waiting.await.unwrap().unwrap();

    assert_eq!(body(&reply), "later");
}

#[tokio::test]
async fn executable____init_fails____crashed_with_message() {
    let rt = runtime(|_| {});
    let app = executable(&rt);

    let instance = rt.spawn(&app, spec("fail")).unwrap();
    let reason = instance.wait().await;

    assert_eq!(
        reason,
        ExitReason::Crashed("init failed: asked to fail".into())
    );
}

#[rstest::rstest]
#[case("panic", "panic: asked to")]
#[case("abort", "killed by signal 6")]
#[tokio::test]
async fn executable____fails_in_callback____crashed_with_why(
    #[case] command: &str,
    #[case] why: &str,
) {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let call = instance.call(text(command), LONG).await;
    let reason = instance.wait().await;

    assert!(matches!(call, Err(CallError::Exited(_))), "{call:?}");
    assert_eq!(reason, ExitReason::Crashed(why.into()));
}

#[tokio::test]
async fn executable____spins_past_deadline____killed_with_timeout() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;
    let started = Instant::now();

    let _ = instance.call(text("spin"), LONG).await;
    let reason = instance.wait().await;

    assert_eq!(reason, ExitReason::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executable____breaks_its_channel____crashed_as_corrupt() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let _ = instance.call(text("break-channel"), LONG).await;
    let reason = instance.wait().await;

    let ExitReason::Crashed(why) = &reason else {
        panic!("expected a crash, got {reason:?}");
    };
    assert!(why.contains("corrupt"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executable____spin_zero____calls_and_requests_answered_sleeping() {
    let rt = runtime(|n| n.spin = Duration::ZERO);
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let echoed = ask(&instance, "echo slept").await;
    let me = ask(&instance, "self").await;
    let requests = ask(&instance, "selfs 200").await;

    assert_eq!(echoed, "slept");
    assert_eq!(me, fixture_pid(instance.pid()));
    assert!(requests.parse::<u64>().is_ok(), "{requests}");
}

#[tokio::test]
async fn executable____allocates_over_memory_max____crashed() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let small = ask(&instance, "alloc 8").await;
    let _ = instance.call(text("alloc 256"), LONG).await;
    let reason = instance.wait().await;

    assert_eq!(small, "ok 8");
    assert!(matches!(reason, ExitReason::Crashed(_)), "{reason:?}");
}

#[tokio::test]
async fn executable____idle_with_pings____stays_up() {
    let rt = runtime(|n| {
        n.ping_interval = Duration::from_millis(20);
        n.ping_timeout = Duration::from_millis(500);
    });
    let app = executable(&rt);
    let instance = start(&rt, &app).await;
    ask(&instance, "echo up").await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = ask(&instance, "echo still").await;

    assert_eq!(after, "still");
    assert_eq!(instance.exit_reason(), None);
}

/// The node reads the rings through a mapping of the memfd the app holds:
/// were the app able to shrink it, the node's next read would fault.
#[tokio::test]
async fn executable____shrinks_its_channel____refused_and_the_node_runs_on() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let shrunk = ask(&instance, "shrink-channel").await;
    let after = ask(&instance, "echo still here").await;

    assert!(shrunk.contains("Operation not permitted"), "{shrunk}");
    assert_eq!(after, "still here");
}

#[tokio::test]
async fn executable____call_outside_the_allow_list____crashed_naming_the_call() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let answer = instance.call(common::text("memfd"), LONG).await;
    let exit = instance.wait().await;

    assert!(answer.is_err(), "{answer:?}");
    assert_eq!(exit, ExitReason::Crashed("syscall memfd_create".into()));
}

#[tokio::test]
async fn executable____user_namespace____refused_and_the_app_runs_on() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let namespace = ask(&instance, "user-namespace").await;
    let after = ask(&instance, "echo still here").await;

    assert!(namespace.contains("Operation not permitted"), "{namespace}");
    assert_eq!(after, "still here");
}

#[tokio::test]
async fn executable____sandboxed____files_sockets_and_node_signals_refused() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let read = ask(&instance, "read /etc/hostname").await;
    let connect = ask(&instance, "connect 127.0.0.1:9").await;
    let signal = ask(&instance, &format!("kill {}", std::process::id())).await;
    let printed = ask(&instance, "print to stderr").await;
    let ring = ask(&instance, "io-uring").await;

    assert!(read.contains("Permission denied"), "{read}");
    assert!(ring.contains("Operation not permitted"), "{ring}");
    assert!(connect.contains("Operation not permitted"), "{connect}");
    assert!(signal.contains("Operation not permitted"), "{signal}");
    assert_eq!(printed, "ok");
}

#[tokio::test]
async fn library____json_call____plugin_answers() {
    let rt = runtime(|_| {});
    let app = rt.load_native(NativeCode {
        path: native_fixtures().2.clone(),
        kind: NativeProgram::Library,
    });
    let instance = rt.spawn(&app, spec(r#"{"k":1}"#)).unwrap();
    let json = |schema: &str, body: &str| Envelope {
        schema: Some(schema.into()),
        ..Envelope::new("application/json", body)
    };

    let echo = instance
        .call(json("echo", r#"{"a":1}"#), LONG)
        .await
        .unwrap();
    let config = instance.call(json("config", "{}"), LONG).await.unwrap();

    assert_eq!(body(&echo), r#"{"a":1}"#);
    assert_eq!(echo.schema.as_deref(), Some("echo"));
    assert_eq!(body(&config), r#"{"k":1}"#);
}

#[tokio::test]
async fn runner____missing____instance_crashed_at_start() {
    let rt = runtime(|n| n.runner = Some("/nonexistent/bulkhead-runner".into()));
    let app = executable(&rt);

    let instance = start(&rt, &app).await;
    let reason = instance.wait().await;

    let ExitReason::Crashed(why) = reason else {
        panic!("{reason:?}");
    };
    assert!(why.contains("/nonexistent/bulkhead-runner"), "{why}");
}

/// This process's own cgroup v2 directory.
fn own_cgroup() -> std::path::PathBuf {
    let line = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let rel = line.lines().find_map(|l| l.strip_prefix("0::")).unwrap();
    std::path::Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'))
}

/// Run alone under a delegated cgroup, as the only process in it (not
/// through `cargo test`, which would share it): build with
/// `cargo test -p bulkhead-host --test native --no-run`, then
/// `systemd-run --user --scope -p Delegate=yes <test binary> --ignored cgroup`.
#[tokio::test]
#[ignore = "needs a delegated cgroup (systemd-run --user --scope -p Delegate=yes)"]
async fn cgroup____allocates_over_memory_max____memory_limit_and_cgroup_removed() {
    let root = own_cgroup();
    let rt = runtime(|n| n.cgroup = Some(root.clone()));
    let app = executable(&rt);
    let instance = start(&rt, &app).await;
    let pid = instance.pid();
    let dir = root
        .join("native")
        .join(format!("{}-{}-{}", pid.node, pid.id, pid.incarnation));

    let small = ask(&instance, "alloc 8").await;
    let inside = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap();
    let before = instance.memory_bytes();
    ask(&instance, "hold 16").await;
    let held = instance.memory_bytes();
    let _ = instance.call(text("alloc 256"), LONG).await;
    let reason = instance.wait().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(small, "ok 8");
    assert!(!inside.trim().is_empty());
    assert!(held >= before + (14 << 20), "{before} then {held}");
    assert_eq!(reason, ExitReason::Crashed("memory_limit".into()));
    assert!(!dir.exists(), "{} left behind", dir.display());
}

#[tokio::test]
async fn executable____holding_memory____resident_set_reported() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;
    ask(&instance, "echo up").await;

    let before = instance.memory_bytes();
    ask(&instance, "hold 32").await;
    let held = instance.memory_bytes();

    assert!(before > 0);
    assert!(held >= before + (30 << 20), "{before} then {held}");
    instance.kill();
}

#[tokio::test]
async fn executable____after_the_runner_execs_it____not_dumpable() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let dumpable = ask(&instance, "dumpable").await;

    assert_eq!(dumpable, "no");
}

#[tokio::test]
async fn executable____volumes____read_write_where_granted_read_only_elsewhere() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let (rw, ro, priv_dir) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::write(ro.path().join("table.csv"), b"a,b\n").unwrap();
    std::fs::write(priv_dir.path().join("p.txt"), b"p").unwrap();
    let preopen = |dir: &tempfile::TempDir, guest: &str, writable| bulkhead_host::Preopen {
        host: dir.path().to_path_buf(),
        guest: guest.into(),
        writable,
    };
    let spec = InstanceSpec {
        granted: [Capability::Filesystem].into(),
        preopens: vec![
            preopen(&priv_dir, bulkhead_host::PRIV_PATH, false),
            preopen(&rw, "/data", true),
            preopen(&ro, "/ref", false),
        ],
        ..spec("")
    };
    let instance = rt.spawn(&app, spec).unwrap();

    let wrote = ask(&instance, "vwrite /data/new.txt").await;
    let read_back = ask(&instance, "vread /data/new.txt").await;
    let read_only = ask(&instance, "vread /ref/table.csv").await;
    let write_read_only = ask(&instance, "vwrite /ref/table.csv").await;
    let write_priv = ask(&instance, "vwrite /priv/p.txt").await;
    let escape = ask(&instance, "vread /data/../ref/table.csv").await;
    let outside = ask(&instance, "vwrite /elsewhere/f").await;

    assert_eq!(wrote, "ok");
    assert_eq!(std::fs::read(rw.path().join("new.txt")).unwrap(), b"x");
    assert_eq!(read_back, "ok");
    assert_eq!(read_only, "ok");
    assert!(
        write_read_only.contains("Permission denied"),
        "{write_read_only}"
    );
    assert!(write_priv.contains("Permission denied"), "{write_priv}");
    assert!(escape.starts_with("err:"), "{escape}");
    assert!(outside.starts_with("err:"), "{outside}");
}

#[tokio::test]
async fn executable____priv_files____readable_at_bulkhead_priv() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("table.csv"), b"a,b\n").unwrap();
    let spec = InstanceSpec {
        granted: [Capability::Filesystem].into(),
        preopens: vec![bulkhead_host::Preopen {
            host: dir.path().to_path_buf(),
            guest: bulkhead_host::PRIV_PATH.into(),
            writable: false,
        }],
        ..spec("")
    };
    let instance = rt.spawn(&app, spec).unwrap();

    let found = ask(&instance, "priv table.csv").await;
    let missing = ask(&instance, "priv nothing.csv").await;

    assert_eq!(found, "ok");
    assert!(missing.starts_with("err:"), "{missing}");
}
