//! Native instances started by the privileged helper, `bulkhead-priv`
//! (ROADMAP PS1): each app under a user of its own, in its cgroup,
//! namespaces and minimal root on Linux, its process group and `rctl`
//! rules on FreeBSD.
//!
//! Root only: each test starts a helper of its own and does nothing
//! without `BULKHEAD_PRIV_TESTS=1` and root (on the test machines, after
//! building as an ordinary user).

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{Capability, NativeProgram};
use bulkhead_host::{
    ExitReason, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use common::privhelper::{self, Started};
use common::{LONG, ask, native_fixtures, text};

fn runtime(started: &Started) -> Runtime {
    let mut native = NativeConfig::default();
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    native.landlock_optional = common::NO_SANDBOX;
    Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// The native fixture, copied where the helper may share it.
fn program(started: &Started, rt: &Runtime) -> LoadedApp {
    let dir = privhelper::dir_owned(&started.dir.path().join("prog"), started.node_uid, 0o755);
    let path = privhelper::copy_owned(&native_fixtures().1, &dir, started.node_uid);
    rt.load_native(NativeCode {
        path,
        kind: NativeProgram::Executable,
    })
}

fn spec(app: &str) -> InstanceSpec {
    InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_millis(500),
        init_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(1),
        ..InstanceSpec::new(app, "1.0.0")
    }
}

/// The helper, started for one test, or `None` (and a note) when the
/// helper's tests do not run here.
fn helper(name: &str) -> Option<Started> {
    if !privhelper::enabled() {
        privhelper::skip(name);
        return None;
    }
    Some(privhelper::start(&native_fixtures().0))
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____two_apps____each_under_a_user_of_its_own() {
    let Some(started) = helper("helper____two_apps") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let a = rt.spawn(&app, spec("alpha")).unwrap();
    let a2 = rt.spawn(&app, spec("alpha")).unwrap();
    let b = rt.spawn(&app, spec("beta")).unwrap();

    let ids = (
        ask(&a, "ids").await,
        ask(&a2, "ids").await,
        ask(&b, "ids").await,
    );
    let echo = ask(&a, "echo hello").await;
    a.shutdown(ExitReason::Shutdown);
    let reason = a.wait().await;

    // Which app starts first, and gets the first user, is a race.
    let users = [started.first_uid, started.first_uid + 1].map(|u| format!("{u} {u}"));
    assert!(users.contains(&ids.0), "{ids:?}");
    assert_eq!(ids.0, ids.1, "an app's instances share its user");
    assert!(users.contains(&ids.2) && ids.2 != ids.0, "{ids:?}");
    assert_eq!(echo, "hello");
    assert_eq!(reason, ExitReason::Shutdown);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn helper____minimal_root____the_system_hidden() {
    let Some(started) = helper("helper____minimal_root") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();

    let passwd = ask(&i, "read /etc/passwd").await;
    let pid = ask(&i, "os-pid").await;
    let node = ask(
        &i,
        &format!("read {}", started.dir.path().join("priv.conf").display()),
    )
    .await;

    assert!(passwd.starts_with("err:"), "{passwd}");
    assert_eq!(pid, "1", "the instance's PID namespace");
    assert!(node.starts_with("err:"), "{node}");
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____spins_past_deadline____killed_with_timeout() {
    let Some(started) = helper("helper____spins_past_deadline") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();
    let _ = ask(&i, "echo up").await;
    let at = Instant::now();

    let _ = i.call(text("spin"), LONG).await;
    let reason = i.wait().await;

    assert_eq!(reason, ExitReason::Timeout);
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____allocates_over_memory_max____crashed() {
    let Some(started) = helper("helper____allocates_over_memory_max") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();

    let small = ask(&i, "alloc 8").await;
    let _ = i.call(text("alloc 256"), LONG).await;
    let reason = i.wait().await;

    assert_eq!(small, "ok 8");
    if cfg!(target_os = "linux") {
        assert_eq!(reason, ExitReason::Crashed("memory_limit".into()));
    } else {
        assert!(matches!(reason, ExitReason::Crashed(_)), "{reason:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____volumes____written_where_writable_read_where_not() {
    let Some(started) = helper("helper____volumes") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let uid = started.node_uid;
    let base = started.dir.path();
    let rw = privhelper::dir_owned(&base.join("rw"), uid, 0o700);
    let ro = privhelper::dir_owned(&base.join("ro"), uid, 0o700);
    let table = ro.join("table.csv");
    std::fs::write(&table, b"a,b\n").unwrap();
    std::os::unix::fs::chown(&table, Some(uid), None).unwrap();
    let preopen = |host: &PathBuf, guest: &str, writable| bulkhead_host::Preopen {
        host: host.clone(),
        guest: guest.into(),
        writable,
    };
    let spec = InstanceSpec {
        granted: [Capability::Filesystem].into(),
        preopens: vec![preopen(&rw, "/data", true), preopen(&ro, "/ref", false)],
        ..spec("alpha")
    };
    // As the node does when it installs an app (ROADMAP PS2): once.
    rt.share_with_app("alpha", &rw, true).await.unwrap();
    rt.share_with_app("alpha", &ro, false).await.unwrap();
    let i = rt.spawn(&app, spec).unwrap();

    let wrote = ask(&i, "vwrite /data/new.txt").await;
    let read = ask(&i, "vcat /ref/table.csv").await;
    let write_ro = ask(&i, "vwrite /ref/table.csv").await;

    assert_eq!(wrote, "ok");
    assert_eq!(read, "a,b\n");
    assert!(write_ro.starts_with("err:"), "{write_ro}");
    let made = std::fs::metadata(rw.join("new.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(made.uid(), started.first_uid, "the app's file is the app's");
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____killed____ends_promptly() {
    let Some(started) = helper("helper____killed") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();
    let pid: i32 = ask(&i, "os-pid").await.parse().unwrap_or(0);

    i.kill();
    let reason = tokio::time::timeout(Duration::from_secs(5), i.wait()).await;

    assert!(pid > 0);
    assert!(reason.is_ok(), "the kill reached the app");
}

/// A directory the node wrote, shared with `app` as the node shares an
/// app's `priv/` files, holding `secret.txt`.
async fn shared_dir(started: &Started, rt: &Runtime, name: &str, app: &str) -> PathBuf {
    let uid = started.node_uid;
    let dir = privhelper::dir_owned(&started.dir.path().join(name), uid, 0o700);
    let file = dir.join("secret.txt");
    std::fs::write(&file, b"alpha's").unwrap();
    std::os::unix::fs::chown(&file, Some(uid), None).unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    rt.share_with_app(app, &dir, false).await.unwrap();
    dir
}

fn reading(app: &str, dir: &std::path::Path) -> InstanceSpec {
    InstanceSpec {
        granted: [Capability::Filesystem].into(),
        preopens: vec![bulkhead_host::Preopen {
            host: dir.to_path_buf(),
            guest: "/ref".into(),
            writable: false,
        }],
        ..spec(app)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn share____one_apps_files____another_apps_user_reads_nothing() {
    let Some(started) = helper("share____one_apps_files") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let dir = shared_dir(&started, &rt, "alpha-priv", "alpha").await;
    let alpha = rt.spawn(&app, reading("alpha", &dir)).unwrap();
    let beta = rt.spawn(&app, reading("beta", &dir)).unwrap();

    let own = ask(&alpha, "vcat /ref/secret.txt").await;
    let other = ask(&beta, "vcat /ref/secret.txt").await;

    assert_eq!(own, "alpha's");
    assert!(other.starts_with("err:"), "{other}");
}

#[tokio::test(flavor = "multi_thread")]
async fn share____then_starts____a_start_shares_nothing_again() {
    use std::os::unix::fs::MetadataExt;
    let Some(started) = helper("share____then_starts") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let dir = shared_dir(&started, &rt, "once", "alpha").await;
    let first = rt.spawn(&app, reading("alpha", &dir)).unwrap();
    ask(&first, "echo up").await;
    // Taken back from the app: were a start to share again, it would undo
    // this.
    let file = dir.join("secret.txt");
    std::os::unix::fs::chown(&file, None, Some(0)).unwrap();

    let second = rt.spawn(&app, reading("alpha", &dir)).unwrap();
    let read = ask(&second, "vcat /ref/secret.txt").await;

    assert_eq!(std::fs::metadata(&file).unwrap().gid(), 0);
    assert!(read.starts_with("err:"), "{read}");
}

#[tokio::test(flavor = "multi_thread")]
async fn share____a_volume____becomes_the_apps_and_sharing_again_changes_nothing() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(started) = helper("share____a_volume") else {
        return;
    };
    let rt = runtime(&started);
    let vol = privhelper::dir_owned(&started.dir.path().join("vol"), started.node_uid, 0o755);

    rt.share_with_app("alpha", &vol, true).await.unwrap();
    rt.share_with_app("alpha", &vol, true).await.unwrap();

    let meta = std::fs::metadata(&vol).unwrap();
    assert_eq!(meta.uid(), started.first_uid);
    assert_eq!(meta.gid(), started.first_uid);
    assert_eq!(meta.permissions().mode() & 0o7777, 0o770);
}
