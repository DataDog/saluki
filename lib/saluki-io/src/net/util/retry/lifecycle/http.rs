use std::{borrow::Cow, error::Error as _, fmt, time::Duration};

use http::StatusCode;
use tracing::{debug, warn};

use super::RetryLifecycle;

/// A standard HTTP retry lifecycle that emits contextual information about HTTP requests and responses..
///
/// This lifecycle emits user-friendly logs about retry attempts, including the request URI and response code. It
/// provides additional destructuring/introspection of errors to surface contextual information such as requests failing
/// due to DNS, connection errors, TLS, and so on.
#[derive(Clone)]
pub struct StandardHttpRetryLifecycle;

impl<B, B2, E> RetryLifecycle<http::Request<B>, http::Response<B2>, E> for StandardHttpRetryLifecycle
where
    E: DynError,
{
    fn before_retry(
        &self, req: &http::Request<B>, res: &Result<http::Response<B2>, E>, retry_backoff: Duration, error_count: u32,
    ) {
        let request_uri = SanitizedRequestUri(req.uri());
        let categorized_error = CategorizedError::try_categorize(res);

        warn!(error_count, %request_uri, "{}. Retrying after {:?}.", categorized_error, retry_backoff);
    }

    fn after_success(&self, req: &http::Request<B>, _: &Result<http::Response<B2>, E>) {
        let request_uri = SanitizedRequestUri(req.uri());
        debug!(%request_uri, "Request succeeded.");
    }
}

struct SanitizedRequestUri<'a>(&'a http::Uri);

impl<'a> fmt::Display for SanitizedRequestUri<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // We _should_ always have a scheme and a host, but we'll just make sure they exist first, to be safe. We'll
        // require needing both to be present to print either of them.
        let maybe_scheme = self.0.scheme_str();
        let maybe_host = self.0.host();

        if let (Some(scheme), Some(host)) = (maybe_scheme, maybe_host) {
            write!(f, "{}://{}", scheme, host)?;
        }

        // Now print the request path, which is always present.
        write!(f, "{}", self.0.path())
    }
}

enum CategorizedError {
    Client(String),
    Tls(String),
    Http(StatusCode),
    Other(String),
}

impl CategorizedError {
    fn try_categorize<B, E>(res: &Result<http::Response<B>, E>) -> Self
    where
        E: DynError,
    {
        match res {
            Ok(resp) => Self::Http(resp.status()),
            Err(e) => Self::extract_nested(e.as_dyn_error()),
        }
    }

    fn extract_nested(error: &(dyn std::error::Error + 'static)) -> Self {
        // See if we have a `hyper-util` error.
        if let Some(hyper_error) = error.downcast_ref::<hyper_util::client::legacy::Error>() {
            return match hyper_error.source() {
                Some(source) => Self::extract_nested(source),
                None => Self::Client(hyper_error.to_string()),
            };
        }

        // See if we have a `rustls` error.
        if let Some(rustls_error) = error.downcast_ref::<rustls::Error>() {
            return Self::from_rustls(rustls_error);
        }

        // See if we have a generic `std::io::Error`.
        //
        // It may be wrapping something else, or it may be standalone, so we'll try and suss that out.
        if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
            return match io_error.get_ref() {
                Some(source) => Self::extract_nested(source),
                None => Self::Other(io_error.to_string()),
            };
        }

        Self::Other(error.to_string())
    }

    fn from_rustls(error: &rustls::Error) -> Self {
        // We're really just specializing a few known types of errors to generate a better error message, but otherwise
        // we'll fallback on the description given by the error itself.
        let reason = match error {
            rustls::Error::InvalidCertificate(cert_error) => format!(
                "peer certificate is invalid: {}",
                rustls_cert_error_to_string(cert_error)
            ),
            _ => error.to_string(),
        };

        Self::Tls(reason)
    }
}

impl fmt::Display for CategorizedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CategorizedError::Client(reason) => write!(f, "Request failed due to a client error: {}", reason),
            CategorizedError::Tls(reason) => write!(f, "Request failed due to a TLS error: {}", reason),
            CategorizedError::Http(status_code) => write!(
                f,
                "Server responded with non-success status code {}.",
                status_code.as_str()
            ),
            CategorizedError::Other(reason) => write!(f, "Request failed: {}", reason),
        }
    }
}

fn rustls_cert_error_to_string(cert_error: &rustls::CertificateError) -> Cow<'static, str> {
    match cert_error {
        rustls::CertificateError::BadEncoding => "certificate incorrectly encoded".into(),
        rustls::CertificateError::Expired => "certificate expired (current time is after notAfter time)".into(),
        rustls::CertificateError::NotValidYet => {
            "certificate not valid yet (current time is before notBefore time)".into()
        }
        rustls::CertificateError::Revoked => "certificate has been revoked".into(),
        rustls::CertificateError::UnhandledCriticalExtension => {
            "certificate contains an extension marked critical, but it was not processed by the certificate validator"
                .into()
        }
        rustls::CertificateError::UnknownIssuer => "certificate chain is not issued by a known root certificate".into(),
        rustls::CertificateError::UnknownRevocationStatus => {
            "certificate's revocation status could not be determined".into()
        }
        rustls::CertificateError::ExpiredRevocationList => {
            "certificate's revocation status could not be determined due to an expired CRL".into()
        }
        rustls::CertificateError::BadSignature => {
            "certificate is not signed correctly by the key of its alleged issuer".into()
        }
        rustls::CertificateError::NotValidForName => "certificate is not valid for the given entity name".into(),
        rustls::CertificateError::InvalidPurpose => "certificate is not valid for the requested purpose".into(),
        rustls::CertificateError::ApplicationVerificationFailure => {
            "certificate is valid overall, but the handshake was rejected".into()
        }

        // This one could be a generic error that doesn't fit the above, returned by `rustls`, or it could be coming from
        // a custom certificate verifier which we don't know about, or can't reasonably know about to compensate for
        // here... so we'll just return it as-is.
        rustls::CertificateError::Other(other) => format!("generic error: {}", other).into(),
        other => format!("generic unhandled error: {:?}", other).into(),
    }
}

// Market trait for accepting generically-typed errors that can be downcasted to dynamically-dispatched trait references.
trait DynError {
    fn as_dyn_error(&self) -> &(dyn std::error::Error + 'static);
}

impl DynError for Box<dyn std::error::Error + Send + Sync> {
    fn as_dyn_error(&self) -> &(dyn std::error::Error + 'static) {
        &**self
    }
}
