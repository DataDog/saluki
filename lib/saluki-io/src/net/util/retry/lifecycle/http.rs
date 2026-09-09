use std::{borrow::Cow, fmt, time::Duration};

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

        // The HTTP/2 fields are only present when the failure was an HTTP/2 error: `None` values are not recorded.
        let http2_details = categorized_error.http2_details();

        warn!(
            error_count,
            %request_uri,
            http2_error_kind = http2_details.map(|details| details.kind.as_str()),
            http2_reason_code = http2_details.and_then(|details| details.reason).map(u32::from),
            http2_received_from_remote = http2_details.map(|details| details.received_from_remote),
            "{}. Retrying after {:?}.", categorized_error, retry_backoff
        );
    }

    fn after_success(&self, req: &http::Request<B>, _: &Result<http::Response<B2>, E>) {
        let request_uri = SanitizedRequestUri(req.uri());
        debug!(%request_uri, "Request succeeded.");
    }
}

struct SanitizedRequestUri<'a>(&'a http::Uri);

impl fmt::Display for SanitizedRequestUri<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // We _should_ always have a scheme and a host, but we'll just make sure they exist first, to be safe. We'll
        // require needing both to be present to print either of them.
        let maybe_scheme = self.0.scheme_str();
        let maybe_host = self.0.host();

        if let (Some(scheme), Some(host)) = (maybe_scheme, maybe_host) {
            write!(f, "{}://{}", scheme, host)?;
        }

        let maybe_port = self.0.port_u16();
        if let Some(port) = maybe_port {
            write!(f, ":{}", port)?;
        }

        // Now print the request path, which is always present.
        write!(f, "{}", self.0.path())
    }
}

/// Maximum number of errors visited when walking a source chain.
///
/// Chains are short in practice, so the limit only exists to bound the walk if an error ever reports itself, directly or
/// indirectly, as its own source.
const MAX_SOURCE_CHAIN_DEPTH: usize = 32;

enum CategorizedError {
    Client(String),
    Tls(String),
    Http2(Http2ErrorDetails),
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
        // Walk the source chain looking for an error we can say something specific about. Wrappers along the way, such
        // as `hyper-util`'s client error, get no handling of their own: we only care about what they wrap.
        let mut current = error;
        for _ in 0..MAX_SOURCE_CHAIN_DEPTH {
            if let Some(rustls_error) = current.downcast_ref::<rustls::Error>() {
                return Self::from_rustls(rustls_error);
            }

            if let Some(http2_error) = current.downcast_ref::<h2::Error>() {
                return Self::Http2(Http2ErrorDetails::from_http2(http2_error));
            }

            match next_source(current) {
                Some(source) => current = source,
                None => break,
            }
        }

        // Nothing in the chain was recognized, so we report the deepest error we reached, since that's the one closest
        // to the actual failure.
        if let Some(client_error) = current.downcast_ref::<hyper_util::client::legacy::Error>() {
            return Self::Client(client_error.to_string());
        }

        Self::Other(current.to_string())
    }

    fn http2_details(&self) -> Option<&Http2ErrorDetails> {
        match self {
            Self::Http2(details) => Some(details),
            _ => None,
        }
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
            CategorizedError::Http2(details) => write!(f, "Request failed due to {}", details),
            CategorizedError::Http(status_code) => write!(
                f,
                "Server responded with non-success status code {}.",
                status_code.as_str()
            ),
            CategorizedError::Other(reason) => write!(f, "Request failed: {}", reason),
        }
    }
}

/// Returns the next error in a source chain, if any.
fn next_source<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a (dyn std::error::Error + 'static)> {
    // `std::io::Error::source` skips the error that the I/O error itself wraps, and returns that error's source instead,
    // so we have to ask for the wrapped error directly.
    if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
        if let Some(inner) = io_error.get_ref() {
            return Some(inner);
        }
    }

    error.source()
}

/// The HTTP/2 frame, if any, that an error came from.
enum Http2ErrorKind {
    GoAway,
    Reset,
    Other,
}

impl Http2ErrorKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::GoAway => "go_away",
            Self::Reset => "reset",
            Self::Other => "other",
        }
    }
}

/// The details of an `h2::Error` that we report.
///
/// We snapshot a small set of fields instead of holding the error, so that we never log its `Debug` output or the debug
/// data carried by a GOAWAY frame.
struct Http2ErrorDetails {
    kind: Http2ErrorKind,
    reason: Option<h2::Reason>,
    received_from_remote: bool,

    /// The error's own text, used only when there is no reason code to describe the failure.
    fallback: Option<String>,
}

impl Http2ErrorDetails {
    fn from_http2(error: &h2::Error) -> Self {
        let kind = if error.is_go_away() {
            Http2ErrorKind::GoAway
        } else if error.is_reset() {
            Http2ErrorKind::Reset
        } else {
            Http2ErrorKind::Other
        };

        // The error's text includes GOAWAY debug data, so we only take it when we have nothing better.
        let reason = error.reason();
        let fallback = reason.is_none().then(|| error.to_string());

        Self {
            kind,
            reason,
            received_from_remote: error.is_remote(),
            fallback,
        }
    }
}

impl fmt::Display for Http2ErrorDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let description = match self.kind {
            Http2ErrorKind::GoAway => "an HTTP/2 GOAWAY",
            Http2ErrorKind::Reset => "an HTTP/2 stream reset",
            Http2ErrorKind::Other => "an HTTP/2 error",
        };

        write!(f, "{}", description)?;

        if self.received_from_remote {
            write!(f, " received from the remote peer")?;
        }

        match self.reason {
            Some(reason) => write!(f, " (reason: {}, code {})", reason.description(), u32::from(reason)),
            None => match &self.fallback {
                Some(fallback) => write!(f, ": {}", fallback),
                None => Ok(()),
            },
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

#[cfg(test)]
mod tests {
    use std::{future::Future, io};

    use bytes::Bytes;
    use http::{Response, StatusCode, Uri};

    use super::*;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    /// An error that reports another error as its source, for building nested chains.
    #[derive(Debug)]
    struct NestedError {
        message: &'static str,
        source: BoxError,
    }

    impl NestedError {
        fn new(message: &'static str, source: impl Into<BoxError>) -> Self {
            Self {
                message,
                source: source.into(),
            }
        }
    }

    impl fmt::Display for NestedError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl std::error::Error for NestedError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&*self.source)
        }
    }

    fn categorize(res: Result<Response<()>, BoxError>) -> String {
        CategorizedError::try_categorize(&res).to_string()
    }

    fn categorize_error(error: impl Into<BoxError>) -> CategorizedError {
        let res: Result<Response<()>, BoxError> = Err(error.into());
        CategorizedError::try_categorize(&res)
    }

    /// Runs a request over an in-memory HTTP/2 connection and returns the error the client sees.
    ///
    /// The server side completes the handshake, waits until the client has sent its request, and then runs `reject`,
    /// which is expected to fail either the stream or the connection. `reject` hands the connection back, since the
    /// server has to keep polling it to flush what `reject` queued, and has to hold it open so that closing the socket
    /// doesn't race the client's read.
    async fn failed_http2_request<F, Fut>(reject: F) -> h2::Error
    where
        F: FnOnce(h2::server::Connection<tokio::io::DuplexStream, Bytes>) -> Fut + Send + 'static,
        Fut: Future<Output = h2::server::Connection<tokio::io::DuplexStream, Bytes>> + Send,
    {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (request_sent_tx, request_sent_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let connection = h2::server::handshake(server_io).await.unwrap();
            request_sent_rx.await.unwrap();

            let mut connection = reject(connection).await;
            let _ = std::future::poll_fn(|cx| connection.poll_closed(cx)).await;
            std::future::pending::<()>().await;
        });

        let (send_request, connection) = h2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(connection);

        let mut send_request = send_request.ready().await.unwrap();
        let request = http::Request::get("https://localhost/api/v2/series").body(()).unwrap();
        let (response, _send_stream) = send_request.send_request(request, true).unwrap();
        request_sent_tx.send(()).unwrap();

        let error = response.await.expect_err("request should have failed");
        driver.abort();

        error
    }

    #[test]
    fn categorizes_http_status_response() {
        // A response (any `Ok`) is categorized by its status code, and non-success codes render with the code value.
        let res: Result<Response<()>, BoxError> = Ok(Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(())
            .unwrap());
        assert_eq!(categorize(res), "Server responded with non-success status code 500.");
    }

    #[test]
    fn categorizes_standalone_io_error_as_other() {
        // A bare transport error that isn't a hyper/rustls/nested error falls through to the generic "Other" bucket.
        let err: BoxError = Box::new(io::Error::from(io::ErrorKind::ConnectionRefused));
        assert_eq!(categorize(Err(err)), "Request failed: connection refused");
    }

    #[test]
    fn categorizes_rustls_certificate_error_as_tls() {
        // A rustls certificate error is specialized into a TLS category with a human-readable reason.
        let err: BoxError = Box::new(rustls::Error::InvalidCertificate(rustls::CertificateError::Expired));
        assert_eq!(
            categorize(Err(err)),
            "Request failed due to a TLS error: peer certificate is invalid: certificate expired (current time is after notAfter time)"
        );
    }

    #[test]
    fn categorizes_io_wrapped_rustls_error_by_unwrapping_source() {
        // An io::Error that wraps a rustls error is unwrapped via its source and categorized as TLS, not reported as
        // a generic io failure.
        let inner = rustls::Error::InvalidCertificate(rustls::CertificateError::Revoked);
        let err: BoxError = Box::new(io::Error::other(inner));
        assert_eq!(
            categorize(Err(err)),
            "Request failed due to a TLS error: peer certificate is invalid: certificate has been revoked"
        );
    }

    #[test]
    fn categorizes_deepest_source_when_nothing_is_recognized() {
        // With no recognized error in the chain, the deepest source is reported, since it sits closest to the failure.
        let inner = NestedError::new("connecting to endpoint", io::Error::from(io::ErrorKind::TimedOut));
        let err: BoxError = Box::new(NestedError::new("sending request", inner));
        assert_eq!(categorize(Err(err)), "Request failed: timed out");
    }

    #[test]
    fn categorizes_nested_http2_error_as_http2() {
        // An HTTP/2 error is found however deeply it is wrapped, and the wrappers add nothing to the message.
        let inner = NestedError::new(
            "connection closed",
            io::Error::other(h2::Error::from(h2::Reason::ENHANCE_YOUR_CALM)),
        );
        let err: BoxError = Box::new(NestedError::new("sending request", inner));

        let categorized = categorize_error(err);
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 error (reason: detected excessive load generating behavior, code 11)"
        );

        let details = categorized.http2_details().expect("should be an HTTP/2 error");
        assert_eq!(details.kind.as_str(), "other");
        assert_eq!(details.reason.map(u32::from), Some(11));
        assert!(!details.received_from_remote);
    }

    #[test]
    fn categorizes_http2_reason_only_error_as_http2() {
        // An error carrying only a reason code has no frame behind it, so nothing is said about a frame or the remote.
        let categorized = categorize_error(h2::Error::from(h2::Reason::INTERNAL_ERROR));
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 error (reason: unexpected internal error encountered, code 2)"
        );

        let details = categorized.http2_details().expect("should be an HTTP/2 error");
        assert_eq!(details.kind.as_str(), "other");
        assert_eq!(details.reason.map(u32::from), Some(2));
        assert!(!details.received_from_remote);
    }

    #[test]
    fn categorizes_http2_unknown_reason_code_as_http2() {
        // Reason codes that `h2` has no description for are still reported by their numeric value.
        let categorized = categorize_error(h2::Error::from(h2::Reason::from(9_001)));
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 error (reason: unknown reason, code 9001)"
        );

        let details = categorized.http2_details().expect("should be an HTTP/2 error");
        assert_eq!(details.reason.map(u32::from), Some(9_001));
    }

    #[tokio::test]
    async fn categorizes_remote_http2_goaway_as_http2() {
        // A GOAWAY from the peer is reported as such, along with the fact that it came from the remote.
        let error = failed_http2_request(|mut connection| async move {
            connection.abrupt_shutdown(h2::Reason::ENHANCE_YOUR_CALM);
            connection
        })
        .await;

        let categorized = categorize_error(io::Error::other(error));
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 GOAWAY received from the remote peer (reason: detected excessive load generating behavior, code 11)"
        );

        let details = categorized.http2_details().expect("should be an HTTP/2 error");
        assert_eq!(details.kind.as_str(), "go_away");
        assert_eq!(details.reason.map(u32::from), Some(11));
        assert!(details.received_from_remote);
    }

    #[tokio::test]
    async fn categorizes_remote_http2_reset_as_http2() {
        // A stream reset from the peer is reported as a stream-level failure that came from the remote.
        let error = failed_http2_request(|mut connection| async move {
            let (_request, mut respond) = connection.accept().await.unwrap().unwrap();
            respond.send_reset(h2::Reason::REFUSED_STREAM);
            connection
        })
        .await;

        let categorized = categorize_error(io::Error::other(error));
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 stream reset received from the remote peer (reason: refused stream before processing any application logic, code 7)"
        );

        let details = categorized.http2_details().expect("should be an HTTP/2 error");
        assert_eq!(details.kind.as_str(), "reset");
        assert_eq!(details.reason.map(u32::from), Some(7));
        assert!(details.received_from_remote);
    }

    #[test]
    fn sanitized_request_uri_display() {
        // Scheme, host, explicit port, and path are all rendered.
        let uri: Uri = "http://localhost:8125/foo/bar".parse().unwrap();
        assert_eq!(SanitizedRequestUri(&uri).to_string(), "http://localhost:8125/foo/bar");

        // A default (implicit) port is omitted.
        let uri: Uri = "https://api.datadoghq.com/api/v1/series".parse().unwrap();
        assert_eq!(
            SanitizedRequestUri(&uri).to_string(),
            "https://api.datadoghq.com/api/v1/series"
        );

        // With neither scheme nor host, only the path is rendered.
        let uri: Uri = "/health".parse().unwrap();
        assert_eq!(SanitizedRequestUri(&uri).to_string(), "/health");
    }
}
