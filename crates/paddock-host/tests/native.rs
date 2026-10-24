//! Native-process apps through `paddock-runner`: the fixture
//! executable's callbacks and process functions, its failures as exit
//! reasons, its limits, and a rustbridge plugin.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{LONG, ask, body, native_fixtures, text};
use paddock_bundle::manifest::NativeProgram;
use paddock_host::{
    CallError, Envelope, ExitReason, InstanceHandle, InstanceSpec, LoadedApp, NativeCode,
    NativeConfig, Pid, Runtime, RuntimeConfig,
};

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

#[tokio::test]
async fn executable____sandboxed____files_sockets_and_node_signals_refused() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let instance = start(&rt, &app).await;

    let read = ask(&instance, "read /etc/hostname").await;
    let connect = ask(&instance, "connect 127.0.0.1:9").await;
    let signal = ask(&instance, &format!("kill {}", std::process::id())).await;
    let printed = ask(&instance, "print to stderr").await;

    assert!(read.contains("Permission denied"), "{read}");
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
    let rt = runtime(|n| n.runner = Some("/nonexistent/paddock-runner".into()));
    let app = executable(&rt);

    let instance = start(&rt, &app).await;
    let reason = instance.wait().await;

    let ExitReason::Crashed(why) = reason else {
        panic!("{reason:?}");
    };
    assert!(why.contains("/nonexistent/paddock-runner"), "{why}");
}

/// This process's own cgroup v2 directory.
fn own_cgroup() -> std::path::PathBuf {
    let line = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let rel = line.lines().find_map(|l| l.strip_prefix("0::")).unwrap();
    std::path::Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'))
}

/// Run alone under a delegated cgroup, as the only process in it (not
/// through `cargo test`, which would share it): build with
/// `cargo test -p paddock-host --test native --no-run`, then
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
    let _ = instance.call(text("alloc 256"), LONG).await;
    let reason = instance.wait().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(small, "ok 8");
    assert!(!inside.trim().is_empty());
    assert_eq!(reason, ExitReason::Crashed("memory_limit".into()));
    assert!(!dir.exists(), "{} left behind", dir.display());
}

#[tokio::test]
async fn executable____priv_files____readable_at_paddock_priv() {
    let rt = runtime(|_| {});
    let app = executable(&rt);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("table.csv"), b"a,b\n").unwrap();
    let spec = InstanceSpec {
        preopens: vec![paddock_host::Preopen {
            host: dir.path().to_path_buf(),
            guest: paddock_host::PRIV_PATH.into(),
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
