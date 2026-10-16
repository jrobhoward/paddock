//! SQL datasources the node binds, through `paddock:sql` (cargo feature
//! `sql`): Postgres.
//!
//! Name each datasource in the manifest, with how many connections the
//! app may hold on it:
//!
//! ```toml
//! [appserver.capabilities."paddock:sql".datasources.main]
//! max_connections = 4
//! ```
//!
//! The operator grants the name and binds it to a database; the app never
//! sees the address or the credentials. Parameters are bound (`$1`, `$2`),
//! never interpolated, and sent as text for the server to convert, so an
//! `i64` binds to any integer column.
//!
//! ```ignore
//! use paddock_sdk::sql::{self, Value};
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

use crate::bindings::paddock::sql::sql;

pub use sql::{Rows, Value};

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

impl From<sql::Error> for SqlError {
    fn from(e: sql::Error) -> Self {
        match e {
            sql::Error::AccessDenied => Self::AccessDenied,
            sql::Error::NotFound => Self::NotFound,
            sql::Error::LimitReached => Self::LimitReached,
            sql::Error::Unavailable(why) => Self::Unavailable(why),
            sql::Error::Timeout => Self::Timeout,
            sql::Error::TooLarge => Self::TooLarge,
            sql::Error::Unsupported(why) => Self::Unsupported(why),
            sql::Error::Query(db) => Self::Query {
                code: db.code,
                message: db.message,
            },
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
    Ok(sql::query(datasource, statement, params)?)
}

/// Run a statement for its effect on `datasource`; returns the rows it
/// changed.
///
/// # Errors
///
/// As [`SqlError`] lists.
pub fn execute(datasource: &str, statement: &str, params: &[Value]) -> Result<u64, SqlError> {
    Ok(sql::execute(datasource, statement, params)?)
}

/// A transaction on one connection. Dropping it without
/// [`Transaction::commit`] rolls it back.
#[derive(Debug)]
pub struct Transaction(sql::Transaction);

/// Begin a transaction on `datasource`. It holds one of the app's
/// connections until it ends.
///
/// # Errors
///
/// As [`SqlError`] lists.
pub fn begin(datasource: &str) -> Result<Transaction, SqlError> {
    Ok(Transaction(sql::begin(datasource)?))
}

impl Transaction {
    /// As [`query`], inside the transaction.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists.
    pub fn query(&self, statement: &str, params: &[Value]) -> Result<Rows, SqlError> {
        Ok(self.0.query(statement, params)?)
    }

    /// As [`execute`], inside the transaction.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists.
    pub fn execute(&self, statement: &str, params: &[Value]) -> Result<u64, SqlError> {
        Ok(self.0.execute(statement, params)?)
    }

    /// Commit.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists; the transaction has ended either way.
    pub fn commit(self) -> Result<(), SqlError> {
        Ok(sql::Transaction::commit(self.0)?)
    }

    /// Roll back.
    ///
    /// # Errors
    ///
    /// As [`SqlError`] lists; the transaction has ended either way.
    pub fn rollback(self) -> Result<(), SqlError> {
        Ok(sql::Transaction::rollback(self.0)?)
    }
}
