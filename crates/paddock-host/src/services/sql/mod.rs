//! `paddock:sql`: SQL datasources the node binds (ROADMAP E5), Postgres.
//!
//! An instance uses only datasources its manifest lists and the operator's
//! policy grants ([`SqlSpec::datasources`], narrowed by
//! [`InstanceSpec::with_granted_datasources`](crate::InstanceSpec::with_granted_datasources)),
//! with the capability granted; the node binds each name
//! ([`SqlDatasource`](crate::SqlDatasource)). A datasource's pool is built
//! on first use, so a node whose apps never call it opens no connection.
//!
//! Each app has a connection cap per datasource (its manifest's
//! `max_connections`), shared by its instances: a call or a transaction
//! holds one of the app's connections while it runs. A call waits up to the
//! datasource's acquire timeout for one, then fails with `limit-reached`.
//! A transaction is a resource holding its connection; dropping it without
//! `commit`, or the instance exiting, rolls it back.
//!
//! Without the `sql` feature the interface is still linked and every call
//! answers `unavailable`.

#[cfg(feature = "sql")]
mod pool;
#[cfg(feature = "sql")]
mod values;

use std::collections::BTreeMap;
use std::sync::Arc;

use paddock_bundle::manifest::{AppSpec, Capability};
use wasmtime::Result;
use wasmtime::component::{Linker, Resource};

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    wasmtime::component::bindgen!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "../../wit/groups",
            "../../wit/watch",
            "../../wit/secrets",
            "../../wit/metrics",
            "../../wit/sql",
            "wit",
        ],
        world: "paddock:host/sql",
        imports: { default: async | trappable },
        with: {
            "paddock:sql/sql.transaction": super::Transaction,
        },
    });
}

use bindings::paddock::sql::sql;

/// The SQL service.
pub(crate) struct Sql;

/// What the manifest asks of SQL for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SqlSpec {
    /// Datasources the instance may use, with the app's connection cap
    /// for each: the manifest's list, narrowed to the operator's grant.
    pub datasources: BTreeMap<String, u32>,
}

/// The node's datasources.
pub(crate) struct Shared {
    #[cfg(feature = "sql")]
    datasources: BTreeMap<String, Arc<pool::Datasource>>,
}

/// SQL state for one instance.
// Without the `sql` feature every call answers before reading it.
#[cfg_attr(not(feature = "sql"), allow(dead_code))]
pub(crate) struct Instance {
    shared: Arc<Shared>,
    app: String,
    datasources: BTreeMap<String, u32>,
    granted: bool,
}

/// A transaction handed to the guest: its connection until it ends.
pub struct Transaction {
    #[cfg(feature = "sql")]
    conn: Option<pool::Conn>,
    #[cfg(feature = "sql")]
    max_result_bytes: u64,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction").finish_non_exhaustive()
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        #[cfg(feature = "sql")]
        if let Some(conn) = self.conn.take() {
            conn.roll_back_later();
        }
    }
}

impl Service for Sql {
    const CAPABILITY: Capability = Capability::Sql;
    const INTERFACES: &'static [&'static str] = &["paddock:sql/sql"];
    type Spec = SqlSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> SqlSpec {
        SqlSpec {
            datasources: manifest
                .capabilities
                .sql
                .iter()
                .flat_map(|s| s.datasources.iter())
                .map(|(name, ds)| (name.clone(), ds.max_connections))
                .collect(),
        }
    }

    fn shared(config: &RuntimeConfig) -> Result<Shared, HostError> {
        #[cfg(not(feature = "sql"))]
        let _ = config;
        Ok(Shared {
            #[cfg(feature = "sql")]
            datasources: config
                .datasources
                .iter()
                .map(|(name, cfg)| {
                    (
                        name.clone(),
                        Arc::new(pool::Datasource::new(name, cfg.clone())),
                    )
                })
                .collect(),
        })
    }

    fn instance(shared: &Arc<Shared>, spec: &SqlSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            shared: shared.clone(),
            app: info.handle.app().to_string(),
            datasources: spec.datasources.clone(),
            granted: info.granted,
        }
    }

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        sql::add_to_linker::<T, Ctx<Self>>(linker, T::service)
    }
}

type Answer<T> = std::result::Result<T, sql::Error>;

#[cfg(not(feature = "sql"))]
fn not_built<T>() -> Answer<T> {
    Err(sql::Error::Unavailable(
        "this node was built without SQL (cargo feature `sql`)".into(),
    ))
}

#[cfg(feature = "sql")]
mod run {
    //! The calls, with the `sql` feature.

    use std::time::Instant;

    use super::pool::{self, Fault};
    use super::values::Value;
    use super::{Answer, Instance, ServiceCtx, Transaction, sql};

    impl From<Fault> for sql::Error {
        fn from(f: Fault) -> Self {
            match f {
                Fault::LimitReached => Self::LimitReached,
                Fault::Unavailable(why) => Self::Unavailable(why),
                Fault::Timeout => Self::Timeout,
                Fault::TooLarge => Self::TooLarge,
                Fault::Unsupported(why) => Self::Unsupported(why),
                Fault::Query { code, message } => Self::Query(sql::DbError { code, message }),
            }
        }
    }

    fn value_in(v: sql::Value) -> Value {
        match v {
            sql::Value::Null => Value::Null,
            sql::Value::Boolean(b) => Value::Bool(b),
            sql::Value::Int(i) => Value::Int(i),
            sql::Value::Float(f) => Value::Float(f),
            sql::Value::Text(s) => Value::Text(s),
            sql::Value::Bytes(b) => Value::Bytes(b),
        }
    }

    fn value_out(v: Value) -> sql::Value {
        match v {
            Value::Null => sql::Value::Null,
            Value::Bool(b) => sql::Value::Boolean(b),
            Value::Int(i) => sql::Value::Int(i),
            Value::Float(f) => sql::Value::Float(f),
            Value::Text(s) => sql::Value::Text(s),
            Value::Bytes(b) => sql::Value::Bytes(b),
        }
    }

    pub(super) fn rows_out(rows: pool::Rows) -> sql::Rows {
        sql::Rows {
            columns: rows.columns,
            rows: rows
                .rows
                .into_iter()
                .map(|r| r.into_iter().map(value_out).collect())
                .collect(),
        }
    }

    pub(super) fn values_in(params: Vec<sql::Value>) -> Vec<Value> {
        params.into_iter().map(value_in).collect()
    }

    impl ServiceCtx<'_, Instance> {
        /// The datasource `name` and the app's cap on it, if the instance
        /// may use it.
        fn datasource(&self, name: &str) -> Answer<(std::sync::Arc<pool::Datasource>, u32)> {
            let state = &*self.state;
            let Some(cap) = state
                .datasources
                .get(name)
                .copied()
                .filter(|_| state.granted)
            else {
                return Err(sql::Error::AccessDenied);
            };
            let ds = state
                .shared
                .datasources
                .get(name)
                .cloned()
                .ok_or(sql::Error::NotFound)?;
            Ok((ds, cap))
        }

        /// A connection on `name` for this instance's app, timing the
        /// wait as host time.
        pub(super) async fn connect(&mut self, name: &str) -> Answer<(pool::Conn, u64)> {
            let (ds, cap) = self.datasource(name)?;
            let started = Instant::now();
            let conn = ds.acquire(&self.state.app.clone(), cap).await;
            self.add_host_time(started.elapsed());
            Ok((conn?, ds.max_result_bytes()))
        }

        /// Run `f` as host time.
        pub(super) async fn timed<T>(&mut self, f: impl Future<Output = T>) -> T {
            let started = Instant::now();
            let out = f.await;
            self.add_host_time(started.elapsed());
            out
        }
    }

    impl Transaction {
        pub(super) fn conn(&self) -> Answer<&pool::Conn> {
            self.conn
                .as_ref()
                .ok_or_else(|| sql::Error::Unavailable("the transaction has ended".into()))
        }

        pub(super) fn max_result_bytes(&self) -> u64 {
            self.max_result_bytes
        }

        pub(super) fn new(conn: pool::Conn, max_result_bytes: u64) -> Self {
            Self {
                conn: Some(conn),
                max_result_bytes,
            }
        }

        pub(super) fn take(&mut self) -> Option<pool::Conn> {
            self.conn.take()
        }
    }
}

impl sql::Host for ServiceCtx<'_, Instance> {
    async fn query(
        &mut self,
        datasource: String,
        statement: String,
        params: Vec<sql::Value>,
    ) -> Result<Answer<sql::Rows>> {
        #[cfg(not(feature = "sql"))]
        {
            let _ = (datasource, statement, params);
            Ok(not_built())
        }
        #[cfg(feature = "sql")]
        {
            let (conn, max) = match self.connect(&datasource).await {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            let values = run::values_in(params);
            let rows = self
                .timed(pool::query(conn.client(), &statement, &values, max))
                .await;
            Ok(rows.map(run::rows_out).map_err(Into::into))
        }
    }

    async fn execute(
        &mut self,
        datasource: String,
        statement: String,
        params: Vec<sql::Value>,
    ) -> Result<Answer<u64>> {
        #[cfg(not(feature = "sql"))]
        {
            let _ = (datasource, statement, params);
            Ok(not_built())
        }
        #[cfg(feature = "sql")]
        {
            let (conn, _) = match self.connect(&datasource).await {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            let values = run::values_in(params);
            let changed = self
                .timed(pool::execute(conn.client(), &statement, &values))
                .await;
            Ok(changed.map_err(Into::into))
        }
    }

    async fn begin(&mut self, datasource: String) -> Result<Answer<Resource<Transaction>>> {
        #[cfg(not(feature = "sql"))]
        {
            let _ = datasource;
            Ok(not_built())
        }
        #[cfg(feature = "sql")]
        {
            let (conn, max) = match self.connect(&datasource).await {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            if let Err(e) = self.timed(conn.simple("BEGIN")).await {
                return Ok(Err(e.into()));
            }
            Ok(Ok(self.table.push(Transaction::new(conn, max))?))
        }
    }
}

impl sql::HostTransaction for ServiceCtx<'_, Instance> {
    async fn query(
        &mut self,
        tx: Resource<Transaction>,
        statement: String,
        params: Vec<sql::Value>,
    ) -> Result<Answer<sql::Rows>> {
        #[cfg(not(feature = "sql"))]
        {
            let _ = (tx, statement, params);
            Ok(not_built())
        }
        #[cfg(feature = "sql")]
        {
            // The connection stays in the transaction while the statement
            // runs: if the instance is killed meanwhile, dropping the
            // transaction rolls it back.
            let started = std::time::Instant::now();
            let t = self.table.get(&tx)?;
            let conn = match t.conn() {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            let values = run::values_in(params);
            let rows = pool::query(conn.client(), &statement, &values, t.max_result_bytes()).await;
            self.add_host_time(started.elapsed());
            Ok(rows.map(run::rows_out).map_err(Into::into))
        }
    }

    async fn execute(
        &mut self,
        tx: Resource<Transaction>,
        statement: String,
        params: Vec<sql::Value>,
    ) -> Result<Answer<u64>> {
        #[cfg(not(feature = "sql"))]
        {
            let _ = (tx, statement, params);
            Ok(not_built())
        }
        #[cfg(feature = "sql")]
        {
            let started = std::time::Instant::now();
            let t = self.table.get(&tx)?;
            let conn = match t.conn() {
                Ok(c) => c,
                Err(e) => return Ok(Err(e)),
            };
            let values = run::values_in(params);
            let changed = pool::execute(conn.client(), &statement, &values).await;
            self.add_host_time(started.elapsed());
            Ok(changed.map_err(Into::into))
        }
    }

    async fn commit(&mut self, tx: Resource<Transaction>) -> Result<Answer<()>> {
        self.end(tx, "COMMIT").await
    }

    async fn rollback(&mut self, tx: Resource<Transaction>) -> Result<Answer<()>> {
        self.end(tx, "ROLLBACK").await
    }

    async fn drop(&mut self, tx: Resource<Transaction>) -> Result<()> {
        // `Transaction`'s own drop rolls back a transaction still open.
        self.table.delete(tx)?;
        Ok(())
    }
}

impl ServiceCtx<'_, Instance> {
    /// End `tx` with `sql` (`COMMIT` or `ROLLBACK`) and give its connection
    /// back.
    async fn end(&mut self, tx: Resource<Transaction>, sql_text: &str) -> Result<Answer<()>> {
        #[cfg_attr(not(feature = "sql"), allow(unused_mut, unused_variables))]
        let mut t = self.table.delete(tx)?;
        #[cfg(not(feature = "sql"))]
        {
            let _ = sql_text;
            Ok(not_built())
        }
        #[cfg(feature = "sql")]
        {
            let Some(conn) = t.take() else {
                return Ok(Err(sql::Error::Unavailable(
                    "the transaction has ended".into(),
                )));
            };
            let ended = self.timed(conn.simple(sql_text)).await;
            Ok(ended.map_err(Into::into))
        }
    }
}
