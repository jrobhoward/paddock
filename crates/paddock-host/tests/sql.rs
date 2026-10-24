//! `paddock:sql` through `paddock-sdk`'s `sql` wrapper and the
//! SDK fixture. Access checks run anywhere; the rest run against a scratch
//! Postgres this test starts as the current user (`initdb` and `pg_ctl` in
//! a temporary directory, on a Unix socket only), and are skipped, saying
//! so, when no Postgres is installed. `PADDOCK_PG_BIN` names the directory
//! holding `initdb` and `pg_ctl`; otherwise `/usr/lib/postgresql/*/bin` and
//! `PATH` are searched.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::pg::Scratch;
use common::{ask, fixture};
use paddock_bundle::manifest::Capability;
use paddock_host::{
    InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig, SqlDatasource,
};

/// The SQL command `fields` for the SDK fixture.
fn sql(fields: &[&str]) -> String {
    format!("sql\x1f{}", fields.join("\x1f"))
}

/// A spec for app `app` using datasource `ds` with cap `cap`, granted or
/// not.
fn spec(app: &str, ds: &str, cap: u32, granted: bool) -> InstanceSpec {
    let mut s = InstanceSpec {
        callback_deadline: Duration::from_secs(10),
        ..InstanceSpec::new(app, "1.0.0")
    };
    if granted {
        s.granted.insert(Capability::Sql);
    }
    s.services.sql.datasources = BTreeMap::from([(ds.to_string(), cap)]);
    s
}

fn runtime(datasources: BTreeMap<String, SqlDatasource>) -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        datasources,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
    (rt, app)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql____not_granted_unlisted_or_unbound____access_denied_or_not_found() {
    let (rt, app) = runtime(BTreeMap::new());
    let not_granted = rt.spawn(&app, spec("a", "main", 1, false)).unwrap();
    let listed = rt.spawn(&app, spec("b", "main", 1, true)).unwrap();

    let denied = ask(&not_granted, &sql(&["query", "main", "SELECT 1"])).await;
    let unlisted = ask(&listed, &sql(&["query", "other", "SELECT 1"])).await;
    let unbound = ask(&listed, &sql(&["exec", "main", "SELECT 1"])).await;
    let tx = ask(&not_granted, &sql(&["begin", "main"])).await;

    assert_eq!(denied, "err:AccessDenied");
    assert_eq!(unlisted, "err:AccessDenied");
    assert_eq!(unbound, "err:NotFound");
    assert_eq!(tx, "err:AccessDenied");
}

// ------------------------------------------------------------- with Postgres

/// A datasource on `pg`, database `postgres` as `paddock`.
fn datasource(pg: &Scratch) -> SqlDatasource {
    let mut ds = SqlDatasource::new(pg.socket_dir().display().to_string(), "postgres", "paddock");
    ds.port = common::pg::PORT;
    ds.pool_size = 4;
    ds.acquire_timeout = Duration::from_millis(500);
    ds
}

/// A runtime with datasource `main` on `pg` (adjusted by `tune`), the
/// fixture loaded.
fn with(pg: &Scratch, tune: impl FnOnce(&mut SqlDatasource)) -> (Runtime, LoadedApp) {
    let mut ds = datasource(pg);
    tune(&mut ds);
    runtime(BTreeMap::from([("main".to_string(), ds)]))
}

fn up(rt: &Runtime, app: &LoadedApp, name: &str, cap: u32) -> InstanceHandle {
    rt.spawn(app, spec(name, "main", cap, true)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____statements_against_postgres____values_errors_and_limits() {
    let Some(pg) = Scratch::start() else { return };
    let (rt, app) = with(&pg, |ds| {
        ds.statement_timeout = Duration::from_millis(300);
        ds.max_result_bytes = 4_096;
    });
    let a = up(&rt, &app, "a", 2);
    let created = ask(
        &a,
        &sql(&[
            "exec",
            "main",
            "CREATE TABLE t (id int PRIMARY KEY, name text, ok bool, score float8, \
             amount numeric(10,2), at timestamptz, data bytea, note text)",
        ]),
    )
    .await;

    let inserted = ask(
        &a,
        &sql(&[
            "exec",
            "main",
            "INSERT INTO t VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            "i:1",
            "t:héllo",
            "b:true",
            "f:2.5",
            "t:12.30",
            "t:2026-10-15 12:00:00+00",
            "x:00ff",
            "null",
        ]),
    )
    .await;
    let row = ask(
        &a,
        &sql(&["query", "main", "SELECT * FROM t WHERE id = $1", "i:1"]),
    )
    .await;
    let duplicate = ask(&a, &sql(&["exec", "main", "INSERT INTO t (id) VALUES (1)"])).await;
    let interval = ask(&a, &sql(&["query", "main", "SELECT interval '1 day' AS d"])).await;
    let cast = ask(
        &a,
        &sql(&["query", "main", "SELECT (interval '1 day')::text AS d"]),
    )
    .await;
    let slow = ask(&a, &sql(&["query", "main", "SELECT pg_sleep(2)"])).await;
    let big = ask(
        &a,
        &sql(&[
            "query",
            "main",
            "SELECT repeat('x', 1000) FROM generate_series(1, 10)",
        ]),
    )
    .await;

    assert_eq!(created, "0");
    assert_eq!(inserted, "1");
    assert_eq!(
        row,
        "[\"id\", \"name\", \"ok\", \"score\", \"amount\", \"at\", \"data\", \"note\"]|\
         [[Value::Int(1), Value::Text(\"héllo\"), Value::Boolean(true), Value::Float(2.5), \
         Value::Text(\"12.30\"), Value::Text(\"2026-10-15 12:00:00+00\"), \
         Value::Bytes([0, 255]), Value::Null]]"
    );
    assert!(
        duplicate.starts_with("err:Query { code: \"23505\""),
        "{duplicate}"
    );
    assert!(
        interval.starts_with("err:Unsupported(\"column d is of type interval"),
        "{interval}"
    );
    assert_eq!(cast, "[\"d\"]|[[Value::Text(\"1 day\")]]");
    assert_eq!(slow, "err:Timeout");
    assert_eq!(big, "err:TooLarge");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____transactions____commit_seen_rollback_and_drop_not() {
    let Some(pg) = Scratch::start() else { return };
    let (rt, app) = with(&pg, |_| {});
    let a = up(&rt, &app, "a", 2);
    ask(&a, &sql(&["exec", "main", "CREATE TABLE n (v int)"])).await;
    let count = || sql(&["query", "main", "SELECT count(*)::int AS c FROM n"]);

    ask(&a, &sql(&["begin", "main"])).await;
    ask(&a, &sql(&["tx-exec", "INSERT INTO n VALUES ($1)", "i:1"])).await;
    let inside = ask(&a, &sql(&["tx-query", "SELECT count(*)::int AS c FROM n"])).await;
    let outside_before = ask(&a, &count()).await;
    let committed = ask(&a, &sql(&["commit"])).await;
    let after_commit = ask(&a, &count()).await;
    ask(&a, &sql(&["begin", "main"])).await;
    ask(&a, &sql(&["tx-exec", "INSERT INTO n VALUES (2)"])).await;
    ask(&a, &sql(&["rollback"])).await;
    ask(&a, &sql(&["begin", "main"])).await;
    ask(&a, &sql(&["tx-exec", "INSERT INTO n VALUES (3)"])).await;
    ask(&a, &sql(&["drop"])).await;
    let after_rollback_and_drop = ask(&a, &count()).await;

    assert_eq!(inside, "[\"c\"]|[[Value::Int(1)]]");
    assert_eq!(outside_before, "[\"c\"]|[[Value::Int(0)]]");
    assert_eq!(committed, "ok");
    assert_eq!(after_commit, "[\"c\"]|[[Value::Int(1)]]");
    assert_eq!(after_rollback_and_drop, "[\"c\"]|[[Value::Int(1)]]");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____app_cap_reached____limit_reached_until_the_holder_exits() {
    let Some(pg) = Scratch::start() else { return };
    let (rt, app) = with(&pg, |_| {});
    let holder = up(&rt, &app, "capped", 1);
    let other = up(&rt, &app, "capped", 1);
    let another_app = up(&rt, &app, "free", 1);
    ask(&holder, &sql(&["exec", "main", "CREATE TABLE c (v int)"])).await;
    ask(&holder, &sql(&["begin", "main"])).await;
    ask(&holder, &sql(&["tx-exec", "INSERT INTO c VALUES (1)"])).await;

    let while_held = ask(&other, &sql(&["query", "main", "SELECT 1::int AS one"])).await;
    let other_app = ask(
        &another_app,
        &sql(&["query", "main", "SELECT 1::int AS one"]),
    )
    .await;
    holder.kill();
    holder.wait().await;
    let after_exit = ask(
        &other,
        &sql(&["query", "main", "SELECT count(*)::int AS c FROM c"]),
    )
    .await;

    assert_eq!(while_held, "err:LimitReached");
    assert_eq!(
        other_app, "[\"one\"]|[[Value::Int(1)]]",
        "the cap is the app's"
    );
    assert_eq!(
        after_exit, "[\"c\"]|[[Value::Int(0)]]",
        "the exited instance's transaction rolled back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____session_state_set_by_one_call____not_seen_by_the_next() {
    let Some(pg) = Scratch::start() else { return };
    let (rt, app) = with(&pg, |ds| ds.pool_size = 1);
    let a = up(&rt, &app, "a", 1);
    let b = up(&rt, &app, "b", 1);

    ask(&a, &sql(&["exec", "main", "SET application_name = 'leak'"])).await;
    ask(&a, &sql(&["exec", "main", "BEGIN"])).await;
    let name = ask(&b, &sql(&["query", "main", "SHOW application_name"])).await;
    // SAVEPOINT works only inside a transaction: outside one it is refused
    // with SQLSTATE 25P01, which shows no transaction carried over.
    let savepoint = ask(&b, &sql(&["exec", "main", "SAVEPOINT probe"])).await;

    assert_eq!(name, "[\"application_name\"]|[[Value::Text(\"paddock\")]]");
    assert!(
        savepoint.starts_with("err:Query { code: \"25P01\""),
        "no transaction carried over: {savepoint}"
    );
}
