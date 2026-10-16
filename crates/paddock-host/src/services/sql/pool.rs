//! Postgres for `paddock:sql`: a pool per datasource, built on first use,
//! the per-app connection caps, and running statements.
//!
//! Every connection a pool hands out again is first reset with `ROLLBACK;
//! DISCARD ALL`, so a transaction begun with `execute("BEGIN")`, or a
//! session setting, never carries over to the next caller, which may be
//! another app.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::BytesMut;
use deadpool_postgres::{Manager, ManagerConfig, Pool, PoolError, RecyclingMethod, Runtime};
use futures_util::StreamExt;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, NoTls};

use super::values::{DecodeError, Value, decode, param_text};
use crate::config::{SecretSource, SqlDatasource, SqlTls};

/// What a call failed with, before it becomes the interface's error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Fault {
    LimitReached,
    Unavailable(String),
    Timeout,
    TooLarge,
    Unsupported(String),
    Query { code: String, message: String },
}

/// One datasource: its configuration, its pool once built, and each app's
/// connection cap.
pub(super) struct Datasource {
    name: String,
    cfg: SqlDatasource,
    pool: OnceCell<Pool>,
    caps: Mutex<HashMap<String, (u32, Arc<Semaphore>)>>,
}

/// A connection held for a call or a transaction, and the app's permit for
/// it.
pub(super) struct Conn {
    client: deadpool_postgres::Object,
    _permit: OwnedSemaphorePermit,
}

impl Datasource {
    pub(super) fn new(name: &str, cfg: SqlDatasource) -> Self {
        Self {
            name: name.to_string(),
            cfg,
            pool: OnceCell::new(),
            caps: Mutex::default(),
        }
    }

    pub(super) fn max_result_bytes(&self) -> u64 {
        self.cfg.max_result_bytes
    }

    /// `app`'s cap of `max` connections, shared by its instances. A new cap
    /// (an upgrade changed it) replaces the old for new connections.
    fn cap(&self, app: &str, max: u32) -> Arc<Semaphore> {
        let mut caps = self.caps.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = caps.entry(app.to_string()).or_insert_with(|| {
            (
                max,
                Arc::new(Semaphore::new(usize::try_from(max).unwrap_or(1))),
            )
        });
        if entry.0 != max {
            *entry = (
                max,
                Arc::new(Semaphore::new(usize::try_from(max).unwrap_or(1))),
            );
        }
        entry.1.clone()
    }

    async fn pool(&self) -> Result<&Pool, Fault> {
        self.pool
            .get_or_try_init(|| async { build(&self.name, &self.cfg).await })
            .await
            .map_err(Fault::Unavailable)
    }

    /// A connection for `app`, within the acquire timeout: first its cap,
    /// then the pool.
    pub(super) async fn acquire(&self, app: &str, max: u32) -> Result<Conn, Fault> {
        let pool = self.pool().await?;
        let cap = self.cap(app, max);
        let got = tokio::time::timeout(self.cfg.acquire_timeout, async {
            let permit = cap.acquire_owned().await.map_err(|_| Fault::LimitReached)?;
            let client = pool.get().await.map_err(|e| match e {
                PoolError::Timeout(_) => Fault::LimitReached,
                PoolError::Backend(e) => pg_fault(&e),
                other => Fault::Unavailable(other.to_string()),
            })?;
            Ok(Conn {
                client,
                _permit: permit,
            })
        })
        .await;
        got.unwrap_or(Err(Fault::LimitReached))
    }
}

/// The pool for `cfg`: TLS as configured, statements cancelled after the
/// statement timeout, connections reset when they come back.
async fn build(name: &str, cfg: &SqlDatasource) -> Result<Pool, String> {
    let mut pg = tokio_postgres::Config::new();
    pg.host(&cfg.host)
        .port(cfg.port)
        .dbname(&cfg.dbname)
        .user(&cfg.user)
        .connect_timeout(cfg.connect_timeout)
        .application_name("paddock")
        .options(format!(
            "-c statement_timeout={}",
            cfg.statement_timeout.as_millis()
        ));
    if let Some(source) = &cfg.password {
        pg.password(read_secret(name, source).await?);
    }
    let manager_config = ManagerConfig {
        recycling_method: RecyclingMethod::Custom("ROLLBACK; DISCARD ALL".into()),
    };
    let manager = match &cfg.tls {
        SqlTls::Disable => {
            pg.ssl_mode(tokio_postgres::config::SslMode::Disable);
            Manager::from_config(pg, NoTls, manager_config)
        }
        SqlTls::Require { ca_file } => {
            pg.ssl_mode(tokio_postgres::config::SslMode::Require);
            let tls = tls_config(name, ca_file)?;
            Manager::from_config(
                pg,
                tokio_postgres_rustls::MakeRustlsConnect::new(tls),
                manager_config,
            )
        }
    };
    Pool::builder(manager)
        .max_size(cfg.pool_size.max(1))
        .runtime(Runtime::Tokio1)
        .create_timeout(Some(cfg.connect_timeout))
        .recycle_timeout(Some(cfg.connect_timeout))
        .build()
        .map_err(|e| format!("datasource {name}: {e}"))
}

async fn read_secret(name: &str, source: &SecretSource) -> Result<Vec<u8>, String> {
    let bytes = match source {
        SecretSource::File(path) => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || std::fs::read(path))
                .await
                .map_err(|_| format!("datasource {name}: password: read failed"))?
                .map_err(|e| format!("datasource {name}: password file: {}", e.kind()))?
        }
        SecretSource::Env(var) => std::env::var_os(var)
            .map(|v| v.into_encoded_bytes())
            .ok_or_else(|| format!("datasource {name}: password variable {var} is not set"))?,
    };
    // A trailing newline in a password file is not part of the password.
    let end = bytes
        .iter()
        .rposition(|b| *b != b'\n' && *b != b'\r')
        .map_or(0, |i| i + 1);
    Ok(bytes[..end].to_vec())
}

fn tls_config(name: &str, ca_file: &std::path::Path) -> Result<rustls::ClientConfig, String> {
    let fail = |e: &dyn std::fmt::Display| {
        format!("datasource {name}: CA file {}: {e}", ca_file.display())
    };
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca_file).map_err(|e| fail(&e))? {
        roots
            .add(cert.map_err(|e| fail(&e))?)
            .map_err(|e| fail(&e))?;
    }
    if roots.is_empty() {
        return Err(fail(&"no certificates"));
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| fail(&e))?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

/// The interface's fault for a driver error: the server's own error with
/// its SQLSTATE, a cancelled statement as a timeout, or anything else (the
/// connection) as unavailable.
pub(super) fn pg_fault(e: &tokio_postgres::Error) -> Fault {
    match e.as_db_error() {
        Some(db) if *db.code() == SqlState::QUERY_CANCELED => Fault::Timeout,
        Some(db) => Fault::Query {
            code: db.code().code().to_string(),
            message: db.message().to_string(),
        },
        None => Fault::Unavailable(e.to_string()),
    }
}

/// A parameter, sent as text for the server to convert.
#[derive(Debug)]
struct Param(Option<Vec<u8>>);

impl ToSql for Param {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            None => Ok(IsNull::Yes),
            Some(text) => {
                out.extend_from_slice(text);
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> tokio_postgres::types::Format {
        tokio_postgres::types::Format::Text
    }

    to_sql_checked!();
}

/// A column's raw binary value, decoded afterwards.
struct Raw<'a>(Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(Some(raw)))
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(None))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

fn params(values: &[Value]) -> Vec<Param> {
    values.iter().map(|v| Param(param_text(v))).collect()
}

/// A statement's result.
pub(super) struct Rows {
    pub(super) columns: Vec<String>,
    pub(super) rows: Vec<Vec<Value>>,
}

/// Run a statement that returns rows, up to `max_bytes` of them.
pub(super) async fn query(
    client: &Client,
    statement: &str,
    values: &[Value],
    max_bytes: u64,
) -> Result<Rows, Fault> {
    let stmt = client.prepare(statement).await.map_err(|e| pg_fault(&e))?;
    let params = params(values);
    let stream = client
        .query_raw(&stmt, params.iter().map(|p| p as &(dyn ToSql + Sync)))
        .await
        .map_err(|e| pg_fault(&e))?;
    let columns: Vec<String> = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let types: Vec<Type> = stmt.columns().iter().map(|c| c.type_().clone()).collect();
    tokio::pin!(stream);
    let mut rows = Vec::new();
    let mut size: u64 = 0;
    while let Some(row) = stream.next().await {
        let row = row.map_err(|e| pg_fault(&e))?;
        let mut values = Vec::with_capacity(types.len());
        for (i, ty) in types.iter().enumerate() {
            let Raw(raw) = row.try_get(i).map_err(|e| pg_fault(&e))?;
            let value = decode(ty, raw).map_err(|e| match e {
                DecodeError::Unsupported(t) => Fault::Unsupported(format!(
                    "column {} is of type {t}; cast it in the statement ({}::text)",
                    columns[i], columns[i]
                )),
                DecodeError::Malformed(m) => {
                    Fault::Unavailable(format!("bad value from server: {m}"))
                }
            })?;
            size = size.saturating_add(value.size());
            if size > max_bytes {
                return Err(Fault::TooLarge);
            }
            values.push(value);
        }
        rows.push(values);
    }
    Ok(Rows { columns, rows })
}

/// Run a statement for its effect: rows changed.
pub(super) async fn execute(
    client: &Client,
    statement: &str,
    values: &[Value],
) -> Result<u64, Fault> {
    let stmt = client.prepare(statement).await.map_err(|e| pg_fault(&e))?;
    let params = params(values);
    client
        .execute_raw(&stmt, params.iter().map(|p| p as &(dyn ToSql + Sync)))
        .await
        .map_err(|e| pg_fault(&e))
}

impl Conn {
    pub(super) fn client(&self) -> &Client {
        &self.client
    }

    /// Run `sql` (no parameters, no rows): `BEGIN`, `COMMIT`, `ROLLBACK`.
    pub(super) async fn simple(&self, sql: &str) -> Result<(), Fault> {
        self.client
            .batch_execute(sql)
            .await
            .map_err(|e| pg_fault(&e))
    }

    /// Roll back in the background and give the connection back: for a
    /// transaction dropped without `commit`, or an instance that exited
    /// holding one. Outside a runtime, the pool's reset rolls it back when
    /// the connection is next handed out.
    pub(super) fn roll_back_later(self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = self.client.batch_execute("ROLLBACK").await {
                    tracing::debug!(error = %e, "rollback of a dropped transaction failed");
                }
            });
        }
    }
}
