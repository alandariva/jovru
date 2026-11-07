//! What the assistant may be told about a tab (ADR-0028). These types are
//! the allow-list: a request is built from them and from the metadata
//! cache only. None of them holds a result's values, a bound parameter's
//! value or a credential, and none can be built from a session.
use crate::error_report::ErrorReport;
use crate::result_tabs::{ResultState, ResultTab};
use jykre_core::{ConnectionConfig, ConnectionId, SessionContext, SqlDialect};
use std::path::PathBuf;

/// A failed statement's error, as the server reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFacts {
    pub title: String,
    /// The server's own message, unchanged.
    pub message: String,
    pub sqlstate: Option<String>,
    pub vendor_code: Option<String>,
    /// One-based line and column in the statement, when the server gave a
    /// position.
    pub line_and_column: Option<(usize, usize)>,
}

impl ErrorFacts {
    pub fn new(report: &ErrorReport, statement: &str) -> Self {
        Self {
            title: report.title.clone(),
            message: report.message.clone(),
            sqlstate: report.sqlstate.clone(),
            vendor_code: report.vendor_code.clone(),
            line_and_column: report.line_and_column(statement),
        }
    }
}

/// How many rows a statement returned, never which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunFacts {
    pub rows: usize,
    /// More rows existed than were read.
    pub more: bool,
    pub rows_affected: Option<u64>,
}

impl RunFacts {
    /// The counts of a finished tab; `None` while it runs, or when it
    /// failed or was cancelled.
    pub fn of(tab: &ResultTab) -> Option<Self> {
        matches!(tab.state, ResultState::Done).then(|| Self {
            rows: tab.rows_read(),
            more: tab.more_rows(),
            rows_affected: tab.rows_affected,
        })
    }
}

/// Where the tools read: one connection's local metadata cache. It holds
/// the configuration only as the cache's key; there is no credential and
/// nothing that can open a session.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaScope {
    pub dir: PathBuf,
    pub connection: ConnectionId,
    pub config: ConnectionConfig,
    /// The database product, as the user sees it ("PostgreSQL").
    pub engine: String,
    pub dialect: Option<SqlDialect>,
    /// Where unqualified names resolve in the tab.
    pub context: SessionContext,
}
