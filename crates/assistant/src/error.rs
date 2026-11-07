//! Provider failures as classified [`DriverError`]s. Messages carry the
//! HTTP status at most: a provider's error body may echo the request, and
//! the provider library's own `Display` includes bodies and payloads, so it
//! is never used.
use jykre_core::{DriverError, ErrorCode};

/// The error for an HTTP status the provider answered with.
pub fn classify_status(status: u16) -> DriverError {
    let (code, message) = match status {
        401 | 403 => (
            ErrorCode::Authentication,
            "The AI provider refused the API key",
        ),
        404 => (
            ErrorCode::NotFound,
            "The AI provider does not know this model or address",
        ),
        408 => (ErrorCode::Timeout, "The AI provider timed out"),
        413 => (
            ErrorCode::Validation,
            "The request is too large for this model",
        ),
        429 => (
            ErrorCode::Connection,
            "The AI provider is limiting requests or the quota is used up",
        ),
        400..=499 => (
            ErrorCode::Validation,
            "The AI provider rejected the request",
        ),
        _ => (ErrorCode::Connection, "The AI provider failed to answer"),
    };
    DriverError::new(code, format!("{message} (HTTP {status})"))
}

pub(crate) fn connection() -> DriverError {
    DriverError::new(ErrorCode::Connection, "Could not reach the AI provider")
}

pub(crate) fn protocol() -> DriverError {
    DriverError::new(
        ErrorCode::Protocol,
        "The AI provider sent an answer that could not be read",
    )
}

pub(crate) fn timeout() -> DriverError {
    DriverError::new(ErrorCode::Timeout, "The AI provider did not answer in time")
}

pub(crate) fn classify(error: &genai::Error) -> DriverError {
    if let Some(status) = error.status() {
        return classify_status(status.as_u16());
    }
    match error {
        genai::Error::WebStream { error, .. } => classify_boxed(error.as_ref()),
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::Reqwest(error) => classify_reqwest(error),
            _ => protocol(),
        },
        genai::Error::RequiresApiKey { .. } | genai::Error::NoAuthData { .. } => {
            DriverError::new(ErrorCode::Authentication, "Enter the API key")
        }
        _ => protocol(),
    }
}

fn classify_boxed(error: &(dyn std::error::Error + Send + Sync + 'static)) -> DriverError {
    if let Some(inner) = error.downcast_ref::<genai::Error>() {
        return classify(inner);
    }
    if error.downcast_ref::<genai::webc::StreamTimeout>().is_some() {
        return timeout();
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return classify_reqwest(error);
    }
    connection()
}

fn classify_reqwest(error: &reqwest::Error) -> DriverError {
    if let Some(status) = error.status() {
        classify_status(status.as_u16())
    } else if error.is_timeout() {
        timeout()
    } else if error.is_decode() || error.is_body() {
        protocol()
    } else {
        connection()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_codes_and_name_only_the_status() {
        for (status, code) in [
            (401, ErrorCode::Authentication),
            (403, ErrorCode::Authentication),
            (404, ErrorCode::NotFound),
            (400, ErrorCode::Validation),
            (413, ErrorCode::Validation),
            (408, ErrorCode::Timeout),
            (429, ErrorCode::Connection),
            (500, ErrorCode::Connection),
            (503, ErrorCode::Connection),
        ] {
            let error = classify_status(status);
            assert_eq!(error.code, code, "{status}");
            assert!(error.message.ends_with(&format!("(HTTP {status})")));
        }
    }
}
