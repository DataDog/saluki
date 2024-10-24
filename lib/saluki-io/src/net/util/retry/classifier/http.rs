use http::StatusCode;

use super::RetryClassifier;

/// A standard HTTP response classifier.
///
/// Generally treats all client (4xx) and server (5xx) errors as retryable, with the exception of a few specific client
/// errors that should not be retried:
///
/// - 400 Bad Request (likely a client-side bug)
/// - 401 Unauthorized (likely a client-side misconfiguration)
/// - 403 Forbidden (likely a client-side misconfiguration)
/// - 413 Payload Too Large (likely a client-side bug)
#[derive(Clone)]
pub struct StandardHttpClassifier;

impl<B, Error> RetryClassifier<http::Response<B>, Error> for StandardHttpClassifier {
    fn should_retry(&self, response: &Result<http::Response<B>, Error>) -> bool {
        match response {
            Ok(resp) => match resp.status() {
                // There's some status codes that likely indicate a fundamental misconfiguration or bug on the client
                // side which won't be resolved by retrying the request.
                StatusCode::BAD_REQUEST
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::PAYLOAD_TOO_LARGE => false,

                // For all other status codes, we'll only retry if they're in the client/server error range.
                status => status.is_client_error() || status.is_server_error(),
            },
            Err(_) => true,
        }
    }
}
