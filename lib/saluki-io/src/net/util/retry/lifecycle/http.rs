use std::{borrow::Cow, fmt, io, time::Duration};

use http::StatusCode;
use tower::timeout::error::Elapsed;
use tracing::{debug, warn};

use super::{RetryCauseTelemetry, RetryLifecycle};

/// A standard HTTP retry lifecycle that emits contextual information about HTTP requests and responses..
///
/// This lifecycle emits user-friendly logs about retry attempts, including the request URI and response code. It
/// provides additional destructuring/introspection of errors to surface contextual information such as requests failing
/// due to DNS, connection errors, TLS, and so on.
///
/// The same categorization drives telemetry when [`RetryCauseTelemetry`] is attached with
/// [`with_telemetry`][Self::with_telemetry], so the logs and the metrics can't disagree about why a request was
/// retried.
#[derive(Clone, Default)]
pub struct StandardHttpRetryLifecycle {
    telemetry: Option<RetryCauseTelemetry>,
}

impl StandardHttpRetryLifecycle {
    /// Creates a new `StandardHttpRetryLifecycle` that only logs retries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts retries by cause, in addition to logging them.
    pub fn with_telemetry(mut self, telemetry: RetryCauseTelemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }
}

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
            http2.frame = http2_details.map(|details| details.frame.as_str()),
            http2.reason_code = http2_details.and_then(|details| details.reason).map(u32::from),
            http2.initiator = http2_details.map(|details| details.initiator.as_str()),
            "{}. Retrying after {:?}.", categorized_error, retry_backoff
        );

        if let Some(telemetry) = &self.telemetry {
            telemetry.increment(categorized_error.retry_cause());
        }
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
    Timeout(TimeoutStage, String),
    Connection(ConnectionFailure, String),
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

        // The outermost I/O error we can categorize by kind. It's only used if nothing more specific turns up deeper in
        // the chain, since an I/O error often wraps a more descriptive error that the walk continues into.
        let mut io_failure = None;

        for _ in 0..MAX_SOURCE_CHAIN_DEPTH {
            if let Some(rustls_error) = current.downcast_ref::<rustls::Error>() {
                return Self::from_rustls(rustls_error);
            }

            if let Some(http2_error) = current.downcast_ref::<h2::Error>() {
                if let Some(io_error) = http2_error.get_io() {
                    if let Some(categorized) = Self::from_io(io_error) {
                        return categorized;
                    }
                }

                return Self::Http2(Http2ErrorDetails::from_http2(http2_error));
            }

            if current.is::<Elapsed>() {
                return Self::Timeout(TimeoutStage::Request, current.to_string());
            }

            if io_failure.is_none() {
                if let Some(io_error) = current.downcast_ref::<io::Error>() {
                    io_failure = Self::from_io(io_error);
                }
            }

            match next_source(current) {
                Some(source) => current = source,
                None => break,
            }
        }

        if let Some(io_failure) = io_failure {
            return io_failure;
        }

        // Nothing in the chain was recognized, so we report the deepest error we reached, since that's the one closest
        // to the actual failure.
        if let Some(client_error) = current.downcast_ref::<hyper_util::client::legacy::Error>() {
            return Self::Client(client_error.to_string());
        }

        Self::Other(current.to_string())
    }

    /// Categorizes an I/O error by its kind.
    ///
    /// Returns `None` for kinds we have nothing specific to say about, which leaves the error to the generic fallback.
    fn from_io(error: &io::Error) -> Option<Self> {
        // Connect and TLS handshake deadlines are surfaced as timed-out I/O errors, so a timeout here is always about
        // establishing the connection rather than waiting for a response.
        if error.kind() == io::ErrorKind::TimedOut {
            return Some(Self::Timeout(TimeoutStage::Connect, error.to_string()));
        }

        ConnectionFailure::from_io_kind(error.kind()).map(|failure| Self::Connection(failure, error.to_string()))
    }

    fn http2_details(&self) -> Option<&Http2ErrorDetails> {
        match self {
            Self::Http2(details) => Some(details),
            _ => None,
        }
    }

    /// Distills this error into the bounded value that telemetry reports.
    fn retry_cause(&self) -> RetryCause {
        match self {
            Self::Client(_) => RetryCause::Client,
            Self::Tls(_) => RetryCause::Tls,
            Self::Http2(details) => RetryCause::Http2 {
                frame: details.frame,
                initiator: details.initiator,
                reason: details.reason.and_then(http2_reason_tag),
            },
            Self::Http(_) => RetryCause::HttpStatus,
            Self::Timeout(stage, _) => RetryCause::Timeout(*stage),
            Self::Connection(failure, _) => RetryCause::Connection(*failure),
            Self::Other(_) => RetryCause::Other,
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
            CategorizedError::Timeout(_, reason) => write!(f, "Request failed due to a timeout: {}", reason),
            CategorizedError::Connection(_, reason) => {
                write!(f, "Request failed due to a connection error: {}", reason)
            }
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
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum Http2Frame {
    GoAway,
    Reset,
    Other,
}

impl Http2Frame {
    fn as_str(&self) -> &'static str {
        match self {
            Self::GoAway => "go_away",
            Self::Reset => "reset",
            Self::Other => "other",
        }
    }
}

/// The side of an HTTP/2 connection that ended the request.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum Http2Initiator {
    /// This client: either `h2` enforcing the protocol, or the code driving it.
    Local,

    /// The remote endpoint, through a GOAWAY or RST_STREAM frame.
    Remote,

    /// Neither: the error carries no frame to attribute.
    Unknown,
}

impl Http2Initiator {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Unknown => "unknown",
        }
    }
}

/// The stage of a request that a timeout applies to.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum TimeoutStage {
    /// Waiting for a response after the request was sent.
    Request,

    /// Establishing the connection, including the TLS handshake.
    Connect,
}

impl TimeoutStage {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Connect => "connect",
        }
    }
}

/// A transport failure recognized from an I/O error's kind.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum ConnectionFailure {
    Refused,
    Reset,
    Aborted,
    BrokenPipe,
    NotConnected,
    UnexpectedEof,
    HostUnreachable,
    NetworkUnreachable,
    NetworkDown,
}

impl ConnectionFailure {
    fn from_io_kind(kind: io::ErrorKind) -> Option<Self> {
        match kind {
            io::ErrorKind::ConnectionRefused => Some(Self::Refused),
            io::ErrorKind::ConnectionReset => Some(Self::Reset),
            io::ErrorKind::ConnectionAborted => Some(Self::Aborted),
            io::ErrorKind::BrokenPipe => Some(Self::BrokenPipe),
            io::ErrorKind::NotConnected => Some(Self::NotConnected),
            io::ErrorKind::UnexpectedEof => Some(Self::UnexpectedEof),
            io::ErrorKind::HostUnreachable => Some(Self::HostUnreachable),
            io::ErrorKind::NetworkUnreachable => Some(Self::NetworkUnreachable),
            io::ErrorKind::NetworkDown => Some(Self::NetworkDown),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Reset => "reset",
            Self::Aborted => "aborted",
            Self::BrokenPipe => "broken_pipe",
            Self::NotConnected => "not_connected",
            Self::UnexpectedEof => "unexpected_eof",
            Self::HostUnreachable => "host_unreachable",
            Self::NetworkUnreachable => "network_unreachable",
            Self::NetworkDown => "network_down",
        }
    }
}

/// Returns the tag value for an HTTP/2 reason code, or `None` for a code that `h2` doesn't name.
///
/// An unnamed code is left out of the tags rather than turned into one, since the remote endpoint chooses the codes it
/// sends. The numeric code stays in the retry log.
fn http2_reason_tag(reason: h2::Reason) -> Option<&'static str> {
    match reason {
        h2::Reason::NO_ERROR => Some("no_error"),
        h2::Reason::PROTOCOL_ERROR => Some("protocol_error"),
        h2::Reason::INTERNAL_ERROR => Some("internal_error"),
        h2::Reason::FLOW_CONTROL_ERROR => Some("flow_control_error"),
        h2::Reason::SETTINGS_TIMEOUT => Some("settings_timeout"),
        h2::Reason::STREAM_CLOSED => Some("stream_closed"),
        h2::Reason::FRAME_SIZE_ERROR => Some("frame_size_error"),
        h2::Reason::REFUSED_STREAM => Some("refused_stream"),
        h2::Reason::CANCEL => Some("cancel"),
        h2::Reason::COMPRESSION_ERROR => Some("compression_error"),
        h2::Reason::CONNECT_ERROR => Some("connect_error"),
        h2::Reason::ENHANCE_YOUR_CALM => Some("enhance_your_calm"),
        h2::Reason::INADEQUATE_SECURITY => Some("inadequate_security"),
        h2::Reason::HTTP_1_1_REQUIRED => Some("http_1_1_required"),
        _ => None,
    }
}

const TAG_CAUSE: &str = "cause";
const TAG_FRAME: &str = "frame";
const TAG_INITIATOR: &str = "initiator";
const TAG_REASON: &str = "reason";

/// The bounded classification of a retry, as reported to telemetry.
///
/// Every part of this value is an enumerated variant or a fixed string, so the set of tag combinations it can produce is
/// known at compile time. That's what bounds the cardinality of the retry cause metric, and it's why error text, GOAWAY
/// debug data, and unrecognized reason codes stay in the retry log instead.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum RetryCause {
    HttpStatus,
    Http2 {
        frame: Http2Frame,
        initiator: Http2Initiator,
        reason: Option<&'static str>,
    },
    Tls,
    Timeout(TimeoutStage),
    Connection(ConnectionFailure),
    Client,
    Other,
}

impl RetryCause {
    /// Returns the tags for this cause's counter.
    pub(super) fn tags(&self) -> Vec<(&'static str, &'static str)> {
        match self {
            Self::HttpStatus => vec![(TAG_CAUSE, "http_status")],
            Self::Http2 {
                frame,
                initiator,
                reason,
            } => {
                let mut tags = vec![
                    (TAG_CAUSE, "http2"),
                    (TAG_FRAME, frame.as_str()),
                    (TAG_INITIATOR, initiator.as_str()),
                ];

                if let Some(reason) = reason {
                    tags.push((TAG_REASON, reason));
                }

                tags
            }
            Self::Tls => vec![(TAG_CAUSE, "tls")],
            Self::Timeout(stage) => vec![(TAG_CAUSE, "timeout"), (TAG_REASON, stage.as_str())],
            Self::Connection(failure) => vec![(TAG_CAUSE, "connection"), (TAG_REASON, failure.as_str())],
            Self::Client => vec![(TAG_CAUSE, "client")],
            Self::Other => vec![(TAG_CAUSE, "other")],
        }
    }
}

/// The details of an `h2::Error` that we report.
///
/// We snapshot a small set of fields instead of holding the error, so that we never log its `Debug` output or the debug
/// data carried by a GOAWAY frame.
struct Http2ErrorDetails {
    frame: Http2Frame,
    initiator: Http2Initiator,
    reason: Option<h2::Reason>,

    /// The error's own text, used only when there is no reason code to describe the failure.
    fallback: Option<String>,
}

impl Http2ErrorDetails {
    fn from_http2(error: &h2::Error) -> Self {
        let frame = if error.is_go_away() {
            Http2Frame::GoAway
        } else if error.is_reset() {
            Http2Frame::Reset
        } else {
            Http2Frame::Other
        };

        // Only GOAWAY and RST_STREAM errors have a side that sent them. `h2` distinguishes the errors it raises itself
        // from the ones the calling code causes, but both mean the same thing here: the request ended locally.
        let initiator = match frame {
            Http2Frame::GoAway | Http2Frame::Reset if error.is_remote() => Http2Initiator::Remote,
            Http2Frame::GoAway | Http2Frame::Reset => Http2Initiator::Local,
            Http2Frame::Other => Http2Initiator::Unknown,
        };

        // The error's text includes GOAWAY debug data, so we only take it when we have nothing better.
        let reason = error.reason();
        let fallback = reason.is_none().then(|| error.to_string());

        Self {
            frame,
            initiator,
            reason,
            fallback,
        }
    }
}

impl fmt::Display for Http2ErrorDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let description = match self.frame {
            Http2Frame::GoAway => "an HTTP/2 GOAWAY",
            Http2Frame::Reset => "an HTTP/2 stream reset",
            Http2Frame::Other => "an HTTP/2 error",
        };

        write!(f, "{}", description)?;

        match self.initiator {
            Http2Initiator::Local => write!(f, " sent by this client")?,
            Http2Initiator::Remote => write!(f, " received from the remote peer")?,
            Http2Initiator::Unknown => {}
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
    use std::{collections::HashMap, future::Future, io};

    use bytes::Bytes;
    use http::{Response, StatusCode, Uri};
    use metrics::{Key, Label, SharedString, Unit};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey, MetricKind,
    };
    use saluki_metrics::MetricsBuilder;

    use super::*;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;
    type MetricsSnapshot = HashMap<CompositeKey, (Option<Unit>, Option<SharedString>, DebugValue)>;

    const RETRY_CAUSES_TOTAL: &str = "network_http_requests_retry_causes_total";

    /// Enough empty DATA frames to exhaust `h2`'s budget for them, which trips its abuse protection.
    const EMPTY_DATA_FRAMES: usize = 256;

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

    /// Returns the telemetry tags for a failed request, which is what bounds the cause's cardinality.
    fn cause_tags(error: impl Into<BoxError>) -> Vec<(&'static str, &'static str)> {
        categorize_error(error).retry_cause().tags()
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

    /// Runs a request over an in-memory HTTP/2 connection whose server closes without responding.
    async fn failed_http2_io_request() -> h2::Error {
        let (client_io, server_io) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            let mut connection = h2::server::handshake(server_io).await.unwrap();
            let _ = connection.accept().await;
        });

        let (send_request, connection) = h2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(connection);

        let mut send_request = send_request.ready().await.unwrap();
        let request = http::Request::get("https://localhost/api/v2/series").body(()).unwrap();
        let (response, _send_stream) = send_request.send_request(request, true).unwrap();
        let error = response
            .await
            .expect_err("request should fail when the connection closes");
        driver.abort();

        error
    }

    /// Runs a request over an in-memory HTTP/2 connection whose server floods the response body with empty DATA frames,
    /// and returns the error our own `h2` raises when its abuse protection trips.
    ///
    /// This is the failure that ended requests during the retry storm, apart from which frames `h2` charged against its
    /// budget. As in [`failed_http2_request`], the server holds the connection open afterwards so that closing the
    /// socket doesn't race the client's read.
    async fn http2_abuse_protection_error() -> h2::Error {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        tokio::spawn(async move {
            let mut connection = h2::server::handshake(server_io).await.unwrap();
            let (_request, mut respond) = connection.accept().await.unwrap().unwrap();
            let mut send_stream = respond.send_response(Response::new(()), false).unwrap();
            for _ in 0..EMPTY_DATA_FRAMES {
                send_stream.send_data(Bytes::new(), false).unwrap();
            }

            let _ = std::future::poll_fn(|cx| connection.poll_closed(cx)).await;
            std::future::pending::<()>().await;
        });

        let (send_request, connection) = h2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(connection);

        let mut send_request = send_request.ready().await.unwrap();
        let request = http::Request::get("https://localhost/api/v2/series").body(()).unwrap();
        let (response, _send_stream) = send_request.send_request(request, true).unwrap();

        let mut body = response.await.expect("response head should arrive").into_body();
        let error = loop {
            match body.data().await {
                Some(Ok(_)) => continue,
                Some(Err(error)) => break error,
                None => panic!("response body ended without an error"),
            }
        };
        driver.abort();

        error
    }

    /// Drives one retry decision through the lifecycle, which is what logs and counts it.
    fn record_retry(lifecycle: &StandardHttpRetryLifecycle, res: Result<Response<()>, BoxError>) {
        let request = http::Request::get("https://example.com/api/v2/series")
            .body(())
            .unwrap();
        lifecycle.before_retry(&request, &res, Duration::from_millis(1), 1);
    }

    /// Returns the value of the retry cause counter carrying `tags`, or panics if it was never registered.
    ///
    /// A snapshot reports each counter's value once, so take one snapshot and read every counter from it.
    #[track_caller]
    fn retry_cause_count(snapshot: &MetricsSnapshot, tags: &[(&'static str, &'static str)]) -> u64 {
        let mut labels = vec![Label::new("domain", "https://example.com")];
        labels.extend(tags.iter().map(|(name, value)| Label::new(*name, *value)));

        let key = CompositeKey::new(MetricKind::Counter, Key::from_parts(RETRY_CAUSES_TOTAL, labels));
        match snapshot.get(&key) {
            Some((_, _, DebugValue::Counter(value))) => *value,
            _ => panic!("no retry cause counter for {:?}", tags),
        }
    }

    fn error_response(status: StatusCode) -> Result<Response<()>, BoxError> {
        Ok(Response::builder().status(status).body(()).unwrap())
    }

    #[test]
    fn categorizes_http_status_response() {
        // A response (any `Ok`) is categorized by its status code, and non-success codes render with the code value. The
        // code itself isn't tagged: `network_http_requests_errors_total` already counts responses by status code.
        let response = Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(())
            .unwrap();
        let res: Result<Response<()>, BoxError> = Ok(response);
        let categorized = CategorizedError::try_categorize(&res);

        assert_eq!(
            categorized.to_string(),
            "Server responded with non-success status code 500."
        );
        assert_eq!(categorized.retry_cause().tags(), vec![("cause", "http_status")]);
    }

    #[test]
    fn categorizes_io_error_kinds_as_connection_failures() {
        // Transport failures we can name from the I/O error kind get their own reason, so they don't share a bucket with
        // protocol errors.
        let cases = [
            (io::ErrorKind::ConnectionRefused, "refused", "connection refused"),
            (io::ErrorKind::ConnectionReset, "reset", "connection reset"),
            (io::ErrorKind::ConnectionAborted, "aborted", "connection aborted"),
            (io::ErrorKind::BrokenPipe, "broken_pipe", "broken pipe"),
            (io::ErrorKind::NotConnected, "not_connected", "not connected"),
            (io::ErrorKind::UnexpectedEof, "unexpected_eof", "unexpected end of file"),
            (io::ErrorKind::HostUnreachable, "host_unreachable", "host unreachable"),
            (
                io::ErrorKind::NetworkUnreachable,
                "network_unreachable",
                "network unreachable",
            ),
            (io::ErrorKind::NetworkDown, "network_down", "network down"),
        ];

        for (kind, expected_reason, expected_message) in cases {
            let err: BoxError = Box::new(io::Error::from(kind));
            assert_eq!(
                categorize(Err(err)),
                format!("Request failed due to a connection error: {}", expected_message)
            );

            let err: BoxError = Box::new(io::Error::from(kind));
            assert_eq!(
                cause_tags(err),
                vec![("cause", "connection"), ("reason", expected_reason)],
                "{:?} should report a connection failure",
                kind
            );
        }
    }

    #[test]
    fn categorizes_request_timeout_as_timeout() {
        // The client's per-request timeout fires above the transport, so nothing in the chain says what failed beyond
        // the elapsed deadline itself.
        let err: BoxError = Box::new(Elapsed::new());
        assert_eq!(
            categorize(Err(err)),
            "Request failed due to a timeout: request timed out"
        );

        let err: BoxError = Box::new(Elapsed::new());
        assert_eq!(cause_tags(err), vec![("cause", "timeout"), ("reason", "request")]);
    }

    #[test]
    fn categorizes_timed_out_io_error_as_connect_timeout() {
        // Connect and TLS handshake deadlines reach us as timed-out I/O errors, and their message says which one it was.
        let inner = NestedError::new(
            "connecting to endpoint",
            io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"),
        );
        let err: BoxError = Box::new(NestedError::new("sending request", inner));
        assert_eq!(
            categorize(Err(err)),
            "Request failed due to a timeout: TLS handshake timed out"
        );

        let err: BoxError = Box::new(io::Error::from(io::ErrorKind::TimedOut));
        assert_eq!(cause_tags(err), vec![("cause", "timeout"), ("reason", "connect")]);
    }

    #[test]
    fn categorizes_rustls_certificate_error_as_tls() {
        // A rustls certificate error is specialized into a TLS category with a human-readable reason. The reason stays in
        // the log: rustls has far too many error variants to tag.
        let err: BoxError = Box::new(rustls::Error::InvalidCertificate(rustls::CertificateError::Expired));
        assert_eq!(
            categorize(Err(err)),
            "Request failed due to a TLS error: peer certificate is invalid: certificate expired (current time is after notAfter time)"
        );

        let err: BoxError = Box::new(rustls::Error::InvalidCertificate(rustls::CertificateError::Expired));
        assert_eq!(cause_tags(err), vec![("cause", "tls")]);
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

        // A recognizable I/O error kind doesn't win over the error it wraps, since the TLS failure is the more useful of
        // the two.
        let inner = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let err: BoxError = Box::new(io::Error::new(io::ErrorKind::ConnectionReset, inner));
        assert_eq!(cause_tags(err), vec![("cause", "tls")]);
    }

    #[test]
    fn categorizes_deepest_source_when_nothing_is_recognized() {
        // With no recognized error in the chain, the deepest source is reported, since it sits closest to the failure.
        let inner = NestedError::new("connecting to endpoint", io::Error::other("something went sideways"));
        let err: BoxError = Box::new(NestedError::new("sending request", inner));
        assert_eq!(categorize(Err(err)), "Request failed: something went sideways");

        let inner = NestedError::new("connecting to endpoint", io::Error::other("something went sideways"));
        let err: BoxError = Box::new(NestedError::new("sending request", inner));
        assert_eq!(cause_tags(err), vec![("cause", "other")]);
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
        assert_eq!(details.reason.map(u32::from), Some(11));
        assert_eq!(
            categorized.retry_cause().tags(),
            vec![
                ("cause", "http2"),
                ("frame", "other"),
                ("initiator", "unknown"),
                ("reason", "enhance_your_calm")
            ]
        );
    }

    #[tokio::test]
    async fn categorizes_http2_io_error_by_its_io_kind() {
        // A transport failure wrapped by `h2` is still a connection failure; the HTTP/2 layer adds no useful protocol
        // details in this case.
        let error = failed_http2_io_request().await;
        assert!(error.get_io().is_some(), "the h2 error should wrap the I/O failure");
        assert_eq!(
            cause_tags(error),
            vec![("cause", "connection"), ("reason", "broken_pipe")]
        );
    }

    #[test]
    fn categorizes_http2_reason_only_error_as_http2() {
        // An error carrying only a reason code has no frame behind it, so neither side of the connection can be held
        // responsible for it.
        let categorized = categorize_error(h2::Error::from(h2::Reason::INTERNAL_ERROR));
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 error (reason: unexpected internal error encountered, code 2)"
        );
        assert_eq!(
            categorized.retry_cause().tags(),
            vec![
                ("cause", "http2"),
                ("frame", "other"),
                ("initiator", "unknown"),
                ("reason", "internal_error")
            ]
        );
    }

    #[test]
    fn categorizes_http2_unknown_reason_code_as_http2() {
        // Reason codes that `h2` has no description for are still reported by their numeric value, but they don't become
        // tag values: the peer, not us, decides what codes it sends.
        let categorized = categorize_error(h2::Error::from(h2::Reason::from(9_001)));
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 error (reason: unknown reason, code 9001)"
        );

        let details = categorized.http2_details().expect("should be an HTTP/2 error");
        assert_eq!(details.reason.map(u32::from), Some(9_001));
        assert_eq!(
            categorized.retry_cause().tags(),
            vec![("cause", "http2"), ("frame", "other"), ("initiator", "unknown")]
        );
    }

    #[test]
    fn http2_reason_codes_are_named_for_every_code_h2_defines() {
        // Each reason code `h2` names gets a tag value, so a retry can be attributed to a specific protocol failure.
        let codes = [
            h2::Reason::NO_ERROR,
            h2::Reason::PROTOCOL_ERROR,
            h2::Reason::INTERNAL_ERROR,
            h2::Reason::FLOW_CONTROL_ERROR,
            h2::Reason::SETTINGS_TIMEOUT,
            h2::Reason::STREAM_CLOSED,
            h2::Reason::FRAME_SIZE_ERROR,
            h2::Reason::REFUSED_STREAM,
            h2::Reason::CANCEL,
            h2::Reason::COMPRESSION_ERROR,
            h2::Reason::CONNECT_ERROR,
            h2::Reason::ENHANCE_YOUR_CALM,
            h2::Reason::INADEQUATE_SECURITY,
            h2::Reason::HTTP_1_1_REQUIRED,
        ];

        let mut named = Vec::new();
        for code in codes {
            let tag = http2_reason_tag(code).unwrap_or_else(|| panic!("code {} should be named", u32::from(code)));
            named.push(tag);
        }

        named.sort_unstable();
        named.dedup();
        assert_eq!(named.len(), codes.len(), "each code should have its own name");
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

        assert_eq!(
            categorized.retry_cause().tags(),
            vec![
                ("cause", "http2"),
                ("frame", "go_away"),
                ("initiator", "remote"),
                ("reason", "enhance_your_calm")
            ]
        );
    }

    #[tokio::test]
    async fn categorizes_local_http2_goaway_as_locally_initiated() {
        // Regression coverage for the retry storm that motivated this: `h2`'s abuse protection counted legitimate small
        // DATA frames, so our own client sent GOAWAY(ENHANCE_YOUR_CALM) and every in-flight request was retried. The
        // retry looked identical to one caused by the remote endpoint, which is what reporting the initiator fixes.
        let error = http2_abuse_protection_error().await;

        let categorized = categorize_error(io::Error::other(error));
        assert_eq!(
            categorized.to_string(),
            "Request failed due to an HTTP/2 GOAWAY sent by this client (reason: detected excessive load generating behavior, code 11)"
        );
        assert_eq!(
            categorized.retry_cause().tags(),
            vec![
                ("cause", "http2"),
                ("frame", "go_away"),
                ("initiator", "local"),
                ("reason", "enhance_your_calm")
            ]
        );
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

        assert_eq!(
            categorized.retry_cause().tags(),
            vec![
                ("cause", "http2"),
                ("frame", "reset"),
                ("initiator", "remote"),
                ("reason", "refused_stream")
            ]
        );
    }

    #[test]
    fn retries_are_counted_by_cause() {
        // Before the cause tags existed, these three retries were indistinguishable in telemetry: the status responses
        // and the timeout landed in the same broad transaction-error bucket.
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let telemetry = RetryCauseTelemetry::from_builder(&MetricsBuilder::default(), "https://example.com");
        let lifecycle = StandardHttpRetryLifecycle::new().with_telemetry(telemetry);

        metrics::with_local_recorder(&recorder, || {
            record_retry(&lifecycle, error_response(StatusCode::SERVICE_UNAVAILABLE));
            record_retry(&lifecycle, error_response(StatusCode::INTERNAL_SERVER_ERROR));
            record_retry(&lifecycle, Err(Box::new(Elapsed::new())));
        });

        let snapshot = snapshotter.snapshot().into_hashmap();

        // Both status codes share one series, so a retry storm's shape doesn't depend on how many codes it spans.
        assert_eq!(retry_cause_count(&snapshot, &[("cause", "http_status")]), 2);
        assert_eq!(
            retry_cause_count(&snapshot, &[("cause", "timeout"), ("reason", "request")]),
            1
        );

        let series = snapshot
            .keys()
            .filter(|key| key.key().name() == RETRY_CAUSES_TOTAL)
            .count();
        assert_eq!(series, 2);
    }

    #[tokio::test]
    async fn locally_initiated_http2_retries_are_counted_as_local() {
        // The signal the retry storm investigation lacked: whether this client or the remote endpoint ended the request.
        let error = http2_abuse_protection_error().await;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let telemetry = RetryCauseTelemetry::from_builder(&MetricsBuilder::default(), "https://example.com");
        let lifecycle = StandardHttpRetryLifecycle::new().with_telemetry(telemetry);

        metrics::with_local_recorder(&recorder, || {
            record_retry(&lifecycle, Err(Box::new(io::Error::other(error))));
        });

        assert_eq!(
            retry_cause_count(
                &snapshotter.snapshot().into_hashmap(),
                &[
                    ("cause", "http2"),
                    ("frame", "go_away"),
                    ("initiator", "local"),
                    ("reason", "enhance_your_calm")
                ]
            ),
            1
        );
    }

    #[test]
    fn retries_are_not_counted_without_telemetry() {
        // Retry logging works on its own, so a policy built without telemetry registers nothing.
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let lifecycle = StandardHttpRetryLifecycle::new();

        metrics::with_local_recorder(&recorder, || {
            record_retry(&lifecycle, Err(Box::new(Elapsed::new())));
        });

        assert!(snapshotter.snapshot().into_hashmap().is_empty());
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
