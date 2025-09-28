use std::str::FromStr as _;

use http::{
    uri::{Authority, Scheme},
    HeaderName, HeaderValue, Request, Uri,
};
use tracing::info;

use super::endpoints::ResolvedEndpoint;

static DD_AGENT_VERSION_HEADER: HeaderName = HeaderName::from_static("dd-agent-version");
static DD_API_KEY_HEADER: HeaderName = HeaderName::from_static("dd-api-key");

/// Builds a middleware function that will update the request's URI to use the resolved endpoint's URI, and add the API
/// key as a header.
///
/// The request's URI must already container a relative path component. Any existing scheme, host, and port components
/// will be overridden by the resolved endpoint's URI.
pub fn for_resolved_endpoint<B>(mut endpoint: ResolvedEndpoint) -> impl FnMut(Request<B>) -> Request<B> + Clone {
    let new_uri_authority = Authority::try_from(endpoint.endpoint().authority())
        .expect("should not fail to construct new endpoint authority");
    let new_uri_scheme =
        Scheme::try_from(endpoint.endpoint().scheme()).expect("should not fail to construct new endpoint scheme");
    move |mut request| {
        // Build an updated URI by taking the endpoint URL and slapping the request's URI path on the end of it.
        let path_and_query = request.uri().path_and_query().expect("request path must exist").clone();
        info!("TEST1 path_and_query {:?}", path_and_query);
        // For logs, override to logs intake domain.
        let authority = if path_and_query.as_str().starts_with("/api/v2/logs") {
            // Attempt to derive the site from the endpoint host in the form "<version>.agent.<site>".
            // If the pattern isn't present, fall back to the default authority.
            let base_host = endpoint.endpoint().host_str().unwrap_or("");
            if let Some(idx) = base_host.find(".agent.") {
                let site = &base_host[idx + ".agent.".len()..];
                let logs_host = format!("agent-http-intake.logs.{}", site);
                Authority::from_str(&logs_host).unwrap_or_else(|_| new_uri_authority.clone())
            } else {
                new_uri_authority.clone()
            }
        } else {
            new_uri_authority.clone()
        };

        let new_uri = Uri::builder()
            .scheme(new_uri_scheme.clone())
            .authority(authority)
            .path_and_query(path_and_query)
            .build()
            .expect("should not fail to construct new URI");
        info!("TEST2 new_uri {:?}", new_uri);
        let api_key = endpoint.api_key();
        let api_key_value = HeaderValue::from_str(api_key).expect("should not fail to construct API key header value");
        info!("TEST3 api_key_value {:?}", api_key_value);
        *request.uri_mut() = new_uri;

        // Add the API key as a header.
        request
            .headers_mut()
            .insert(DD_API_KEY_HEADER.clone(), api_key_value.clone());

        request
    }
}

/// Builds a middleware function that adds request headers indicating the version of the data plane making the request.
///
/// Adds the `dd-agent-version` header with the version of the data plane (`x.y.z`), and the `User-Agent` header with
/// the name of data plane and the version, such as `agent-data-plane/x.y.z`.`
pub fn with_version_info<B>() -> impl Fn(Request<B>) -> Request<B> + Clone {
    let app_details = saluki_metadata::get_app_details();
    let formatted_full_name = app_details
        .full_name()
        .replace(" ", "-")
        .replace("_", "-")
        .to_lowercase();

    let agent_version_header_value = HeaderValue::from_static(app_details.version().raw());
    let raw_user_agent_header_value = format!("{}/{}", formatted_full_name, app_details.version().raw());
    let user_agent_header_value = HeaderValue::from_maybe_shared(raw_user_agent_header_value)
        .expect("should not fail to construct User-Agent header value");

    move |mut request| {
        request
            .headers_mut()
            .insert(DD_AGENT_VERSION_HEADER.clone(), agent_version_header_value.clone());
        request
            .headers_mut()
            .insert(http::header::USER_AGENT, user_agent_header_value.clone());
        request
    }
}
