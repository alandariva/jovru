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
        }
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
