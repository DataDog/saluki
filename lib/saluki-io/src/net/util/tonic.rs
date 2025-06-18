use tonic::{Code, Status};

/// A human-friendly wrapper around `tonic::Status`.
///
/// This wrapper type provides a more human-friendly error message over `tonic::Status` to avoid adding noisy/superfluous
/// information when the error is displayed.
pub struct StatusError(Status);

impl From<Status> for StatusError {
    fn from(status: Status) -> Self {
        Self(status)
    }
}

impl std::fmt::Debug for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Forward directly to `Status`.
        self.0.fmt(f)
    }
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status_code = code_to_identifier(self.0.code());
        let status_message = self.0.message();

        let message = if status_message.is_empty() {
            // When we don't have a message, just use the human-friendly description of the code itself.
            self.0.code().description()
        } else {
            status_message
        };

        write!(f, "{}({})", status_code, message)
    }
}

impl std::error::Error for StatusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

fn code_to_identifier(code: Code) -> &'static str {
    match code {
        Code::Ok => "Ok",
        Code::Cancelled => "Cancelled",
        Code::Unknown => "Unknown",
        Code::InvalidArgument => "InvalidArgument",
        Code::DeadlineExceeded => "DeadlineExceeded",
        Code::NotFound => "NotFound",
        Code::AlreadyExists => "AlreadyExists",
        Code::PermissionDenied => "PermissionDenied",
        Code::ResourceExhausted => "ResourceExhausted",
        Code::FailedPrecondition => "FailedPrecondition",
        Code::Aborted => "Aborted",
        Code::OutOfRange => "OutOfRange",
        Code::Unimplemented => "Unimplemented",
        Code::Internal => "Internal",
        Code::Unavailable => "Unavailable",
        Code::DataLoss => "DataLoss",
        Code::Unauthenticated => "Unauthenticated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_empty_message() {
        let status = Status::new(Code::Ok, "");
        let error = StatusError::from(status);
        assert_eq!(error.to_string(), "Ok(The operation completed successfully)");
    }

    #[test]
    fn status_non_empty_message() {
        let status = Status::new(Code::Unavailable, "tcp connect error");
        let error = StatusError::from(status);
        assert_eq!(error.to_string(), "Unavailable(tcp connect error)");
    }
}
