//! `bulkhead:sql`: SQL datasources the node binds, Postgres.
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
//! A native app asks over the protocol (`ServiceCtx::native`, ROADMAP
//! Z8d), on the same checks, pools and caps; its transactions are kept by
//! number in its instance's state, and those still open when it exits are
//! rolled back.
//!
//! Without the `sql` feature the interface is still linked and every call
//! answers `unavailable`.

#[cfg(feature = "sql")]
mod pool;
#[cfg(feature = "sql")]
mod values;

use std::collections::BTreeMap;
use std::sync::Arc;

use bulkhead_bundle::manifest::{AppSpec, Capability};
use bulkhead_native::protocol as p;

use super::{InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;

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
    /// A native app's open transactions, by the number it was given.
    native_txs: BTreeMap<u32, Transaction>,
    next_tx: u32,
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
            native_txs: BTreeMap::new(),
            next_tx: 0,
        }
    }
}

/// A SQL operation's outcome, on the protocol's error.
type Answer<T> = std::result::Result<T, p::SqlError>;

#[cfg(not(feature = "sql"))]
fn not_built<T>() -> Answer<T> {
    Err(p::SqlError::Unavailable(
        "this node was built without SQL (cargo feature `sql`)".into(),
    ))
}

#[cfg(feature = "sql")]
mod run {
    //! The calls, with the `sql` feature.

    use super::pool::{self, Fault};
    use super::values::Value;
    use super::{Answer, Instance, ServiceCtx, Transaction, p};

    impl From<Fault> for p::SqlError {
        fn from(f: Fault) -> Self {
            match f {
                Fault::LimitReached => Self::LimitReached,
                Fault::Unavailable(why) => Self::Unavailable(why),
                Fault::Timeout => Self::Timeout,
                Fault::TooLarge => Self::TooLarge,
                Fault::Unsupported(why) => Self::Unsupported(why),
                Fault::Query { code, message } => Self::Query { code, message },
            }
        }
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
                return Err(p::SqlError::AccessDenied);
            };
            let ds = state
                .shared
                .datasources
                .get(name)
                .cloned()
                .ok_or(p::SqlError::NotFound)?;
            Ok((ds, cap))
        }

        /// A connection on `name` for this instance's app, timing the
        /// wait as host time.
        pub(super) async fn connect(&mut self, name: &str) -> Answer<(pool::Conn, u64)> {
            let (ds, cap) = self.datasource(name)?;
            let conn = ds.acquire(&self.state.app.clone(), cap).await;
            Ok((conn?, ds.max_result_bytes()))
        }
    }

    fn value_native(v: p::SqlValue) -> Value {
        match v {
            p::SqlValue::Null => Value::Null,
            p::SqlValue::Bool(b) => Value::Bool(b),
            p::SqlValue::Int(i) => Value::Int(i),
            p::SqlValue::Float(f) => Value::Float(f),
            p::SqlValue::Text(s) => Value::Text(s),
            p::SqlValue::Bytes(b) => Value::Bytes(b.0),
        }
    }

    fn rows_native(rows: pool::Rows) -> p::SqlRows {
        p::SqlRows {
            columns: rows.columns,
            rows: rows
                .rows
                .into_iter()
                .map(|r| {
                    r.into_iter()
                        .map(|v| match v {
                            Value::Null => p::SqlValue::Null,
                            Value::Bool(b) => p::SqlValue::Bool(b),
                            Value::Int(i) => p::SqlValue::Int(i),
                            Value::Float(f) => p::SqlValue::Float(f),
                            Value::Text(s) => p::SqlValue::Text(s),
                            Value::Bytes(b) => p::SqlValue::Bytes(p::Bytes(b)),
                        })
                        .collect()
                })
                .collect(),
        }
    }

    fn ended() -> p::SqlError {
        p::SqlError::Unavailable("the transaction has ended".into())
    }

    impl ServiceCtx<'_, Instance> {
        /// [`ServiceCtx::native`]'s work.
        pub(super) async fn native_run(&mut self, request: p::SqlRequest) -> Answer<p::SqlAnswer> {
            let values = |params: Vec<p::SqlValue>| -> Vec<Value> {
                params.into_iter().map(value_native).collect()
            };
            match request {
                p::SqlRequest::Query {
                    datasource,
                    statement,
                    params,
                } => {
                    let (conn, max) = self.connect(&datasource).await?;
                    let rows = pool::query(conn.client(), &statement, &values(params), max).await?;
                    Ok(p::SqlAnswer::Rows(rows_native(rows)))
                }
                p::SqlRequest::Execute {
                    datasource,
                    statement,
                    params,
                } => {
                    let (conn, _) = self.connect(&datasource).await?;
                    let changed = pool::execute(conn.client(), &statement, &values(params)).await?;
                    Ok(p::SqlAnswer::Changed(changed))
                }
                p::SqlRequest::Begin(datasource) => {
                    let (conn, max) = self.connect(&datasource).await?;
                    conn.simple("BEGIN").await?;
                    let state = &mut *self.state;
                    let mut id = state.next_tx;
                    while state.native_txs.contains_key(&id) {
                        id = id.wrapping_add(1);
                    }
                    state.next_tx = id.wrapping_add(1);
                    state.native_txs.insert(id, Transaction::new(conn, max));
                    Ok(p::SqlAnswer::Begun(id))
                }
                p::SqlRequest::TxQuery {
                    tx,
                    statement,
                    params,
                } => {
                    // The connection stays in the transaction while the
                    // statement runs: if the instance is killed meanwhile,
                    // dropping its state rolls the transaction back.
                    let t = self.state.native_txs.get(&tx).ok_or_else(ended)?;
                    let rows = pool::query(
                        t.conn()?.client(),
                        &statement,
                        &values(params),
                        t.max_result_bytes(),
                    )
                    .await?;
                    Ok(p::SqlAnswer::Rows(rows_native(rows)))
                }
                p::SqlRequest::TxExecute {
                    tx,
                    statement,
                    params,
                } => {
                    let t = self.state.native_txs.get(&tx).ok_or_else(ended)?;
                    let changed =
                        pool::execute(t.conn()?.client(), &statement, &values(params)).await?;
                    Ok(p::SqlAnswer::Changed(changed))
                }
                p::SqlRequest::TxEnd { tx, commit } => {
                    let mut t = self.state.native_txs.remove(&tx).ok_or_else(ended)?;
                    let conn = t.take().ok_or_else(ended)?;
                    conn.simple(if commit { "COMMIT" } else { "ROLLBACK" })
                        .await?;
                    Ok(p::SqlAnswer::Ended)
                }
            }
        }
    }

    impl Transaction {
        pub(super) fn conn(&self) -> Answer<&pool::Conn> {
            self.conn
                .as_ref()
                .ok_or_else(|| p::SqlError::Unavailable("the transaction has ended".into()))
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

impl ServiceCtx<'_, Instance> {
    /// A native app's SQL request (ROADMAP Z8d): the import's checks, pool
    /// and caps, with transactions kept by number.
    pub(crate) async fn native(
        &mut self,
        request: p::SqlRequest,
    ) -> Result<p::SqlAnswer, p::SqlError> {
        #[cfg(not(feature = "sql"))]
        {
            let _ = request;
            not_built()
        }
        #[cfg(feature = "sql")]
        {
            self.native_run(request).await
        }
    }
}
