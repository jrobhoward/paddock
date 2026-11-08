//! SQL datasources the node binds, through `bulkhead:sql` (cargo feature
//! `sql`): Postgres.
//!
//! Name each datasource in the manifest, with how many connections the
//! app may hold on it:
//!
//! ```toml
//! [appserver.capabilities."bulkhead:sql".datasources.main]
//! max_connections = 4
//! ```
//!
//! The operator grants the name and binds it to a database; the app never
//! sees the address or the credentials. Parameters are bound (`$1`, `$2`),
//! never interpolated, and sent as text for the server to convert, so an
//! `i64` binds to any integer column.
//!
//! ```ignore
//! use bulkhead_sdk::sql::{self, Value};
//!
//! sql::execute("main", "INSERT INTO notes (body) VALUES ($1)", &["hi".into()])?;
//! let rows = sql::query("main", "SELECT id, body FROM notes WHERE id > $1", &[10.into()])?;
//! for row in &rows.rows {
//!     let id = row[0].as_int();
//!     let body = row[1].as_text();
//! }
//!
//! let tx = sql::begin("main")?;
//! tx.execute("UPDATE accounts SET balance = balance - $1 WHERE id = $2", &[5.into(), 1.into()])?;
//! tx.execute("UPDATE accounts SET balance = balance + $1 WHERE id = $2", &[5.into(), 2.into()])?;
//! tx.commit()?; // dropping `tx` instead rolls back
//! ```

use std::fmt;

use bulkhead_native::app::Ctx;
use bulkhead_native::protocol as p;

/// A parameter, or a cell of a row. Text carries what the interface has no
/// type for (dates, `numeric`, `json`, `uuid`); cast other columns in the
/// statement (`col::text`).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A boolean.
    Boolean(bool),
    /// An integer, of any width.
    Int(i64),
    /// A float.
    Float(f64),
    /// Text.
    Text(String),
    /// Bytes (`bytea`).
    Bytes(Vec<u8>),
}

/// A statement's result: the column names, then each row's values in
/// column order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rows {
    /// The columns' names.
    pub columns: Vec<String>,
    /// The rows, each a value per column.
    pub rows: Vec<Vec<Value>>,
}

/// `params` as the protocol's values.
fn params_of(params: &[Value]) -> Vec<p::SqlValue> {
    params
        .iter()
        .map(|v| match v {
            Value::Null => p::SqlValue::Null,
            Value::Boolean(b) => p::SqlValue::Bool(*b),
            Value::Int(i) => p::SqlValue::Int(*i),
            Value::Float(f) => p::SqlValue::Float(*f),
            Value::Text(t) => p::SqlValue::Text(t.clone()),
            Value::Bytes(b) => p::SqlValue::Bytes(p::Bytes(b.clone())),
        })
        .collect()
}

fn unexpected(answer: &p::SqlAnswer) -> SqlError {
    SqlError::Unavailable(format!("unexpected answer from the node: {answer:?}"))
}

/// A statement's answer as this module's rows.
fn rows_of(answer: p::SqlAnswer) -> Result<Rows, SqlError> {
    let p::SqlAnswer::Rows(rows) = answer else {
        return Err(unexpected(&answer));
    };
    let value = |v: p::SqlValue| match v {
        p::SqlValue::Null => Value::Null,
        p::SqlValue::Bool(b) => Value::Boolean(b),
        p::SqlValue::Int(i) => Value::Int(i),
        p::SqlValue::Float(f) => Value::Float(f),
        p::SqlValue::Text(t) => Value::Text(t),
        p::SqlValue::Bytes(b) => Value::Bytes(b.0),
    };
    Ok(Rows {
        columns: rows.columns,
        rows: rows
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(value).collect())
            .collect(),
    })
}

fn changed(answer: p::SqlAnswer) -> Result<u64, SqlError> {
    match answer {
        p::SqlAnswer::Changed(n) => Ok(n),
        other => Err(unexpected(&other)),
    }
}

fn ask(request: p::SqlRequest) -> Result<p::SqlAnswer, SqlError> {
    Ok(Ctx::current().sql(request)?)
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SqlError {
    /// Not granted, or not listed in the manifest.
    AccessDenied,
    /// The node binds no datasource of that name.
    NotFound,
    /// No connection free in time: the app's cap or the node's pool.
    LimitReached,
    /// The database could not be reached.
    Unavailable(String),
    /// The statement ran past the datasource's statement timeout.
    Timeout,
    /// The result is larger than the node allows.
    TooLarge,
    /// A column of a type the interface does not carry; cast it in the
    /// statement (`col::text`).
    Unsupported(String),
    /// The database refused the statement: its SQLSTATE code (`23505` for
    /// a unique violation) and message.
    Query {
        /// The SQLSTATE code.
        code: String,
        /// The server's message.
        message: String,
    },
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::NotFound => f.write_str("no such datasource"),
            Self::LimitReached => f.write_str("no connection free"),
            Self::Unavailable(why) => write!(f, "unavailable: {why}"),
            Self::Timeout => f.write_str("statement timeout"),
            Self::TooLarge => f.write_str("result too large"),
            Self::Unsupported(why) => write!(f, "unsupported: {why}"),
            Self::Query { code, message } => write!(f, "{message} (SQLSTATE {code})"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<p::SqlError> for SqlError {
    fn from(e: p::SqlError) -> Self {
        match e {
            p::SqlError::AccessDenied => Self::AccessDenied,
            p::SqlError::NotFound => Self::NotFound,
            p::SqlError::LimitReached => Self::LimitReached,
            p::SqlError::Unavailable(why) => Self::Unavailable(why),
            p::SqlError::Timeout => Self::Timeout,
            p::SqlError::TooLarge => Self::TooLarge,
            p::SqlError::Unsupported(why) => Self::Unsupported(why),
            p::SqlError::Query { code, message } => Self::Query { code, message },
        }
    }
}

impl Value {
    /// Whether it is SQL `NULL`.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The integer, if it is one.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The number, if it is a float or an integer.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // As SQL widens an integer to a float.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f),
            Self::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// The boolean, if it is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// The text, if it is text (dates, `numeric`, `json` and `uuid` arrive
    /// as text).
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The bytes, if it is `bytea`.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Boolean(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Text(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Null, Into::into)
    }
}

/// Run a statement that returns rows on `datasource`.
///
/// # Errors
///
/// As [`SqlError`] lists.
pub fn query(datasource: &str, statement: &str, params: &[Value]) -> Result<Rows, SqlError> {
    rows_of(ask(p::SqlRequest::Query {
        datasource: datasource.into(),
        statement: statement.into(),
        params: params_of(params),
    })?)
}

/// Run a statement for its effect on `datasource`; returns the rows it
/// changed.
///
/// # Errors
///
/// As [`SqlError`] lists.
pub fn execute(datasource: &str, statement: &str, params: &[Value]) -> Result<u64, SqlError> {
    changed(ask(p::SqlRequest::Execute {
        datasource: datasource.into(),
        statement: statement.into(),
        params: params_of(params),
    })?)
}

/// A transaction on one connection, which the node keeps open under `id`.
/// Dropping it without [`Transaction::commit`] rolls it back.
#[derive(Debug)]
pub struct Transaction {
    id: u32,
    ended: bool,
}

/// Begin a transaction on `datasource`. It holds one of the app's
/// connections until it ends.
///
/// # Errors
///
/// As [`SqlError`] lists.
pub fn begin(datasource: &str) -> Result<Transaction, SqlError> {
    match ask(p::SqlRequest::Begin(datasource.into()))? {
        p::SqlAnswer::Begun(id) => Ok(Transaction { id, ended: false }),
        other => Err(unexpected(&other)),
    }
}

impl Transaction {
    /// As [`query`], inside the transaction.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists.
    pub fn query(&self, statement: &str, params: &[Value]) -> Result<Rows, SqlError> {
        rows_of(ask(p::SqlRequest::TxQuery {
            tx: self.id,
            statement: statement.into(),
            params: params_of(params),
        })?)
    }

    /// As [`execute`], inside the transaction.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists.
    pub fn execute(&self, statement: &str, params: &[Value]) -> Result<u64, SqlError> {
        changed(ask(p::SqlRequest::TxExecute {
            tx: self.id,
            statement: statement.into(),
            params: params_of(params),
        })?)
    }

    /// Commit.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists; the transaction has ended either way.
    pub fn commit(self) -> Result<(), SqlError> {
        self.end(true)
    }

    /// Roll back.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists; the transaction has ended either way.
    pub fn rollback(self) -> Result<(), SqlError> {
        self.end(false)
    }

    fn end(mut self, commit: bool) -> Result<(), SqlError> {
        self.ended = true;
        match ask(p::SqlRequest::TxEnd {
            tx: self.id,
            commit,
        })? {
            p::SqlAnswer::Ended => Ok(()),
            other => Err(unexpected(&other)),
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.ended {
            bulkhead_native::app::end_transaction_quietly(self.id);
        }
    }
}
