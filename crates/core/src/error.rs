use serde::{Deserialize, Serialize};

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Validation,
    NotImplemented,
    DriverNotRegistered,
    DuplicateDriver,
    Cancelled,
    Connection,
    Database,
    NotFound,
    Protocol,
    Authentication,
    PermissionDenied,
    Timeout,
    Internal,
}

/// Messages must not contain credentials, SQL, raw configuration or CLI output.
/// A database's own error message may be passed through, with what the server
/// said about it in [`DriverError::server`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DriverError {
    pub code: ErrorCode,
    pub message: String,
    /// What the database reported, when the error came from the server.
    pub server: Option<ServerDiagnostic>,
    /// The change of a [`crate::write::WriteRequest`] the error happened
    /// at, plus one ([`DriverError::change`]). Kept in four bytes, so the
    /// error stays as small as before it had one.
    change: Option<std::num::NonZeroU32>,
}

/// The parts of a database error the user needs to fix the SQL: the server's
/// own code and where in the statement it stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerDiagnostic {
    /// The standard SQLSTATE, when the server sends one.
    pub sqlstate: Option<String>,
    /// The server's own code: a MySQL error number or a Databricks error
    /// class.
    pub vendor_code: Option<String>,
    /// One-based position, in characters, in the statement as sent.
    pub position: Option<usize>,
}

impl DriverError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            server: None,
            change: None,
        }
    }

    /// An error the database reported, with its own message.
    pub fn from_server(
        code: ErrorCode,
        message: impl Into<String>,
        server: ServerDiagnostic,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            server: Some(server),
            change: None,
        }
    }

    /// The same error, at change `index` (zero-based) of a write: what lets
    /// an import name the file's line (ADR-0040). A write holds far fewer
    /// changes than `u32::MAX`; an index past it is not kept.
    pub fn at_change(mut self, index: usize) -> Self {
        self.change = u32::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .and_then(std::num::NonZeroU32::new);
        self
    }

    /// The change of a write the error happened at, zero-based, when the
    /// driver says which.
    pub fn change(&self) -> Option<usize> {
        self.change.map(|number| number.get() as usize - 1)
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Validation, message)
    }

    pub fn not_implemented(driver: &str, operation: &str) -> Self {
        Self::new(
            ErrorCode::NotImplemented,
            format!("{driver}: {operation} is not implemented"),
        )
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorCode::Cancelled, "Operation cancelled")
    }
}

pub type DriverResult<T> = Result<T, DriverError>;
