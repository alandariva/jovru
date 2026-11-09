//! Update failures as classified [`DriverError`]s. Messages carry an HTTP
//! status at most: addresses are never included, since a download's
//! redirect carries a signed token in its query.
use crate::drivers::{DriverError, ErrorCode};
use crate::tr;

/// GitHub's answer, or the transport, failed while reading the release list
/// or downloading an archive.
pub fn from_update_error(error: &self_update::Error) -> DriverError {
    use self_update::Error;
    match error {
        Error::RateLimited { .. } => DriverError::new(
            ErrorCode::Connection,
            tr!(
                "GitHub is limiting requests from this network; Jykre Studio will try again later."
            ),
        ),
        Error::NotFound { .. } => DriverError::new(
            ErrorCode::NotFound,
            tr!("GitHub did not find Jykre Studio's releases."),
        ),
        Error::Unauthorized { status, .. } | Error::HttpStatus { status, .. } => {
            from_status(*status)
        }
        Error::Transport(inner) => match inner.downcast_ref::<reqwest_genai::Error>() {
            Some(error) => from_reqwest(error),
            None => connection(),
        },
        Error::InvalidResponse { .. }
        | Error::Json(_)
        | Error::SemVer(_)
        | Error::MissingAssetField { .. } => DriverError::new(
            ErrorCode::Protocol,
            tr!("GitHub sent a release list that could not be read."),
        ),
        Error::Signature(_) | Error::NoSignatures(_) | Error::SignatureNonUTF8 => unsigned(),
        Error::Zip(_) => unreadable_archive(),
        Error::Io(error) => from_io(error),
        _ => internal(),
    }
}

/// An HTTP status other than success.
pub fn from_status(status: u16) -> DriverError {
    match status {
        300..=399 => DriverError::new(
            ErrorCode::Protocol,
            tr!("The download was redirected to an unexpected address; nothing was installed."),
        ),
        404 => DriverError::new(
            ErrorCode::NotFound,
            tr!("GitHub no longer has this file (HTTP {0}).", status),
        ),
        408 | 429 | 500..=599 => DriverError::new(
            ErrorCode::Connection,
            tr!("GitHub failed to answer (HTTP {0}).", status),
        ),
        _ => DriverError::new(
            ErrorCode::Protocol,
            tr!("GitHub refused the request (HTTP {0}).", status),
        ),
    }
}

/// A transport failure of the HTTP client. Its `Display` is never used: it
/// includes the address.
pub fn from_reqwest(error: &reqwest_genai::Error) -> DriverError {
    if let Some(status) = error.status() {
        from_status(status.as_u16())
    } else if error.is_timeout() {
        timeout()
    } else if error.is_redirect() {
        from_status(302)
    } else if error.is_decode() || error.is_body() {
        DriverError::new(
            ErrorCode::Connection,
            tr!("The connection to GitHub was interrupted."),
        )
    } else {
        connection()
    }
}

/// Reading or writing the files of the update.
pub fn from_io(error: &std::io::Error) -> DriverError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => DriverError::new(
            ErrorCode::PermissionDenied,
            tr!(
                "Jykre Studio cannot write to its own folder; download the release and replace it by hand."
            ),
        ),
        std::io::ErrorKind::StorageFull => DriverError::new(
            ErrorCode::Internal,
            tr!("There is not enough disk space for the update."),
        ),
        _ => DriverError::new(
            ErrorCode::Internal,
            tr!("The update's files could not be written."),
        ),
    }
}

pub fn connection() -> DriverError {
    DriverError::new(ErrorCode::Connection, tr!("Could not reach GitHub."))
}

pub fn timeout() -> DriverError {
    DriverError::new(ErrorCode::Timeout, tr!("GitHub did not answer in time."))
}

pub fn unsigned() -> DriverError {
    DriverError::new(
        ErrorCode::Protocol,
        tr!("The archive is not signed with Jykre Studio's release key; nothing was installed."),
    )
}

pub fn unreadable_archive() -> DriverError {
    DriverError::new(
        ErrorCode::Protocol,
        tr!("The release archive could not be read; nothing was installed."),
    )
}

pub fn too_large() -> DriverError {
    DriverError::new(
        ErrorCode::Protocol,
        tr!("The release archive is larger than expected; nothing was installed."),
    )
}

pub fn unexpected_layout() -> DriverError {
    DriverError::new(
        ErrorCode::Protocol,
        tr!("The release archive does not hold Jykre Studio as expected; nothing was installed."),
    )
}

pub fn not_restored() -> DriverError {
    DriverError::new(
        ErrorCode::Internal,
        tr!(
            "The update failed and Jykre Studio's folder could not be put back as it was; download the release and replace the folder by hand."
        ),
    )
}

pub fn internal() -> DriverError {
    DriverError::new(ErrorCode::Internal, tr!("The update failed."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_classified_by_code() {
        assert_eq!(from_status(302).code, ErrorCode::Protocol);
        assert_eq!(from_status(404).code, ErrorCode::NotFound);
        assert_eq!(from_status(503).code, ErrorCode::Connection);
        assert_eq!(from_status(429).code, ErrorCode::Connection);
        assert_eq!(from_status(401).code, ErrorCode::Protocol);
        assert!(from_status(503).message.contains("HTTP 503"));
    }

    #[test]
    fn signature_failures_install_nothing() {
        let error = from_update_error(&self_update::Error::SignatureNonUTF8);
        assert_eq!(error.code, ErrorCode::Protocol);
        assert!(error.message.contains("nothing was installed"));
    }

    #[test]
    fn permissions_keep_their_code() {
        let error = from_io(&std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert_eq!(error.code, ErrorCode::PermissionDenied);
    }
}
