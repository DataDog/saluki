use http::{Method, Request, Uri};
use snafu::{ResultExt, Snafu};
use std::io;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
#[allow(unused)]
pub enum RequestBuilderError {
    #[snafu(display("failed to write payload: {}", source))]
    Io { source: io::Error },
    #[snafu(display("error when building API endpoint/request: {}", source))]
    Http { source: http::Error },
}

pub struct RequestBuilder {
    api_key: String,
    api_uri: Uri,
}

impl RequestBuilder {
    /// Creates a new `RequestBuilder` for the given endpoint, using the specified API key and base URI.
    pub async fn new(
        api_key: String, api_base_uri: Uri, endpoint: EventsServiceChecksEndpoint,
    ) -> Result<Self, RequestBuilderError> {
        let api_uri = build_uri_for_endpoint(api_base_uri, endpoint)?;

        Ok(Self { api_key, api_uri })
    }

    /// Creates the request to be sent by the Http Client.
    pub fn create_request(&self, data: String) -> Result<Request<String>, RequestBuilderError> {
        // let write_buffer = self.buffer_pool.acquire().await;
        Request::builder()
            .method(Method::POST)
            .uri(self.api_uri.clone())
            .header("Content-Type", "application/json")
            .header("DD-API-KEY", self.api_key.clone())
            // TODO: We can't access the version number of the package being built that _includes_ this library, so
            // using CARGO_PKG_VERSION or something like that would always be the version of `saluki-components`, which
            // isn't what we want... maybe we can figure out some way to shove it in a global somewhere or something?
            .header("DD-Agent-Version", "0.1.0")
            .header("User-Agent", "agent-data-plane/0.1.0")
            .body(data)
            .context(Http)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventsServiceChecksEndpoint {
    /// Intake for events.
    Events,

    /// Intake for service checks.
    ServiceChecks,
}

impl EventsServiceChecksEndpoint {
    /// Gets the path for this endpoint.
    pub const fn endpoint_path(&self) -> &'static str {
        match self {
            Self::Events => "/api/v1/events",
            Self::ServiceChecks => "/api/v1/check_run",
        }
    }
}

fn build_uri_for_endpoint(
    api_base_uri: Uri, endpoint: EventsServiceChecksEndpoint,
) -> Result<Uri, RequestBuilderError> {
    let mut builder = Uri::builder().path_and_query(endpoint.endpoint_path());

    if let Some(scheme) = api_base_uri.scheme() {
        builder = builder.scheme(scheme.clone());
    }

    if let Some(authority) = api_base_uri.authority() {
        builder = builder.authority(authority.clone());
    }

    builder.build().context(Http)
}
