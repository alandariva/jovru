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
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DriverError {
    pub code: ErrorCode,
    pub message: String,
}

impl DriverError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
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
