use http::{
    uri::{Authority, Scheme},
    HeaderName, HeaderValue, Request, Uri,
};

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
        let path_and_query = request.uri().path_and_query().expect("request path must exist").clone();

        let authority = if path_and_query.as_str().starts_with("/api/v2/logs") {
            endpoint
                .logs_authority()
                .cloned()
                .unwrap_or_else(|| new_uri_authority.clone())
        } else if path_and_query.as_str().starts_with("/api/v0.2/traces") {
            endpoint
                .traces_authority()
                .cloned()
                .unwrap_or_else(|| new_uri_authority.clone())
        } else {
            new_uri_authority.clone()
        };
        // Build an updated URI by taking the endpoint URL and slapping the request's URI path on the end of it.
        let new_uri = Uri::builder()
            .scheme(new_uri_scheme.clone())
            .authority(authority)
            .path_and_query(path_and_query)
            .build()
            .expect("should not fail to construct new URI");
        let api_key = endpoint.api_key();
        let api_key_value = HeaderValue::from_str(api_key).expect("should not fail to construct API key header value");
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
