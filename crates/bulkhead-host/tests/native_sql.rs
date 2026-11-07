//! `bulkhead:sql` for native-process apps (ROADMAP Z8d): the fixture
//! executable's SQL commands through `bulkhead-runner`, on the terms a
//! component gets (`tests/sql.rs`). Access checks run anywhere; the rest
//! run against a scratch Postgres and are skipped, saying so, when none is
//! installed.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use bulkhead_bundle::manifest::{Capability, NativeProgram};
use bulkhead_host::{
    InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
    SqlDatasource,
};
use common::pg::Scratch;
use common::{ask, native_fixtures};

/// A native instance spec for app `app` using datasource `ds` with cap
/// `cap`, granted or not.
fn spec(app: &str, ds: &str, cap: u32, granted: bool) -> InstanceSpec {
    let mut s = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(10),
        init_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(1),
        ..InstanceSpec::new(app, "1.0.0")
    };
    if granted {
        s.granted.insert(Capability::Sql);
    }
    s.services.sql.datasources = BTreeMap::from([(ds.to_string(), cap)]);
    s
}

fn runtime(datasources: BTreeMap<String, SqlDatasource>) -> (Runtime, LoadedApp) {
    let mut native = NativeConfig::default();
    native.runner = Some(native_fixtures().0.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        datasources,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    (rt, app)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql____not_granted_unlisted_or_unbound____access_denied_or_not_found() {
    let (rt, app) = runtime(BTreeMap::new());
    let not_granted = rt.spawn(&app, spec("a", "main", 1, false)).unwrap();
    let listed = rt.spawn(&app, spec("b", "main", 1, true)).unwrap();

    let denied = ask(&not_granted, "sql-q main SELECT 1").await;
    let unlisted = ask(&listed, "sql-q other SELECT 1").await;
    let unbound = ask(&listed, "sql-x main SELECT 1").await;
    let tx = ask(&not_granted, "sql-begin main").await;
    let no_tx = ask(&listed, "sql-tx INSERT INTO t VALUES (1)").await;

    assert_eq!(denied, "err:AccessDenied");
    assert_eq!(unlisted, "err:AccessDenied");
    assert_eq!(unbound, "err:NotFound");
    assert_eq!(tx, "err:AccessDenied");
    assert_eq!(no_tx, "err:no transaction");
}

// ------------------------------------------------------------- with Postgres

/// A runtime with datasource `main` on `pg`, the fixture loaded.
fn with(pg: &Scratch) -> (Runtime, LoadedApp) {
    let mut ds = SqlDatasource::new(
        pg.socket_dir().display().to_string(),
        "postgres",
        "bulkhead",
    );
    ds.port = common::pg::PORT;
    ds.pool_size = 4;
    ds.acquire_timeout = Duration::from_millis(500);
    ds.statement_timeout = Duration::from_millis(300);
    runtime(BTreeMap::from([("main".to_string(), ds)]))
}

fn up(rt: &Runtime, app: &LoadedApp, name: &str, cap: u32) -> InstanceHandle {
    rt.spawn(app, spec(name, "main", cap, true)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____statements_against_postgres____rows_counts_and_errors() {
    let Some(pg) = Scratch::start() else { return };
    let (rt, app) = with(&pg);
    let a = up(&rt, &app, "a", 2);

    let created = ask(
        &a,
        "sql-x main CREATE TABLE t (id int PRIMARY KEY, name text, ok bool, data bytea)",
    )
    .await;
    let inserted = ask(
        &a,
        "sql-x main INSERT INTO t VALUES (1, 'héllo', true, '\\x00ff'), (2, NULL, false, NULL)",
    )
    .await;
    let rows = ask(&a, "sql-q main SELECT * FROM t ORDER BY id").await;
    let duplicate = ask(&a, "sql-x main INSERT INTO t (id) VALUES (1)").await;
    let slow = ask(&a, "sql-q main SELECT pg_sleep(2)").await;

    assert_eq!(created, "0");
    assert_eq!(inserted, "2");
    assert_eq!(
        rows,
        "id=1,name=héllo,ok=true,data=[0, 255];id=2,name=null,ok=false,data=null"
    );
    assert!(
        duplicate.starts_with("err:Query { code: \"23505\""),
        "{duplicate}"
    );
    assert_eq!(slow, "err:Timeout");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____transactions____commit_seen_rollback_and_drop_not() {
    let Some(pg) = Scratch::start() else { return };
    let (rt, app) = with(&pg);
    let a = up(&rt, &app, "a", 2);
    ask(&a, "sql-x main CREATE TABLE n (v int)").await;
    let count = "sql-q main SELECT count(*)::int AS c FROM n";

    ask(&a, "sql-begin main").await;
    ask(&a, "sql-tx INSERT INTO n VALUES (1)").await;
    let inside = ask(&a, "sql-tq SELECT count(*)::int AS c FROM n").await;
    let outside_before = ask(&a, count).await;
    let committed = ask(&a, "sql-commit").await;
    let after_commit = ask(&a, count).await;
    ask(&a, "sql-begin main").await;
    ask(&a, "sql-tx INSERT INTO n VALUES (2)").await;
    let rolled_back = ask(&a, "sql-rollback").await;
    ask(&a, "sql-begin main").await;
    ask(&a, "sql-tx INSERT INTO n VALUES (3)").await;
    let dropped = ask(&a, "sql-drop").await;
    let after_rollback_and_drop = ask(&a, count).await;

    assert_eq!(inside, "c=1");
    assert_eq!(outside_before, "c=0");
    assert_eq!(committed, "ok");
    assert_eq!(after_commit, "c=1");
    assert_eq!(rolled_back, "ok");
    assert_eq!(dropped, "ok");
    assert_eq!(after_rollback_and_drop, "c=1");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____transaction_open_at_exit____rolled_back_and_connection_freed() {
    let Some(pg) = Scratch::start() else { return };
    let (rt, app) = with(&pg);
    let holder = up(&rt, &app, "capped", 1);
    let other = up(&rt, &app, "capped", 1);
    ask(&holder, "sql-x main CREATE TABLE c (v int)").await;
    ask(&holder, "sql-begin main").await;
    ask(&holder, "sql-tx INSERT INTO c VALUES (1)").await;

    let while_held = ask(&other, "sql-q main SELECT 1::int AS one").await;
    holder.kill();
    holder.wait().await;
    let after_exit = ask(&other, "sql-q main SELECT count(*)::int AS c FROM c").await;

    assert_eq!(while_held, "err:LimitReached");
    assert_eq!(
        after_exit, "c=0",
        "the exited instance's transaction rolled back"
    );
    other.kill();
}
