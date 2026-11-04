use crate::{DriverError, DriverResult};
use std::time::Duration;

/// Where unqualified names resolve in a session: the catalog (a Databricks
/// catalog, the PostgreSQL database) and the schema (a PostgreSQL or
/// Databricks schema, the MySQL database). `None` leaves that level as the
/// session has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionContext {
    pub catalog: Option<String>,
    pub schema: Option<String>,
}

/// The transaction state of a session as the server reports it
/// ([`crate::Session::transaction`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// No transaction is open; each statement commits on its own.
    Idle,
    /// A transaction is open. `changes` tells whether it already wrote
    /// something a rollback would undo; `None` when the server does not say.
    Open { changes: Option<bool> },
    /// A statement failed inside the transaction: the server rejects every
    /// other statement until it is rolled back.
    Failed,
    /// Statements do not commit on their own (MySQL with `autocommit = 0`):
    /// each one waits for a `COMMIT`. Whether one ran since the last commit
    /// is not known.
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOptions {
    pub max_rows: u32,
    /// None disables the timeout; Some must be greater than zero.
    pub timeout: Option<Duration>,
    /// The context the tab chose. A session applies it only when it differs
    /// from the last context it applied or reported
    /// ([`crate::Session::context`]), so a `USE` or `SET` the user ran is not
    /// undone by the next statement.
    pub context: SessionContext,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            max_rows: 1_000,
            timeout: Some(Duration::from_secs(300)),
            context: SessionContext::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRequest {
    pub sql: String,
    pub options: ExecutionOptions,
}

impl QueryRequest {
    /// Local validation only; no SQL parsing or rewriting.
    pub fn validate(&self) -> DriverResult<()> {
        if self.sql.trim().is_empty() {
            return Err(DriverError::validation("SQL must not be empty"));
        }
        if self.options.max_rows == 0 {
            return Err(DriverError::validation(
                "Row limit must be greater than zero",
            ));
        }
        if self
            .options
            .timeout
            .is_some_and(|timeout| timeout.is_zero())
        {
            return Err(DriverError::validation("Timeout must be greater than zero"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: Option<bool>,
}

/// Exact decimals and temporal values keep their original textual representation.
/// Null remains distinct from an empty string or the literal text "NULL".
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Boolean(bool),
    Integer(i64),
    UnsignedInteger(u64),
    Float(f64),
    Decimal(String),
    Text(String),
    Binary(Vec<u8>),
    Temporal(String),
}

pub type Row = Vec<Cell>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkControl {
    Continue,
    Stop,
}

/// A bounded synchronous receiver. Call columns before rows, and never send
/// further data after Stop. Receivers must not perform blocking I/O.
pub trait RowSink: Send {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl;
    fn row(&mut self, row: Row) -> SinkControl;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverOutcome {
    pub rows_affected: Option<u64>,
    /// The row limit ended this page with rows left. They stay readable with
    /// `Session::fetch` until the session runs, pings or closes anything
    /// else, which discards them.
    pub has_more: bool,
    /// The driver itself stopped reading before the end of the result (for
    /// example, a server-side row cap), and the rows left cannot be read.
    pub truncated: bool,
}
