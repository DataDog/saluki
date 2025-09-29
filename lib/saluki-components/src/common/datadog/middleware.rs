use std::str::FromStr as _;

use http::{
    uri::{Authority, Scheme},
    HeaderName, HeaderValue, Request, Uri,
};

use super::endpoints::ResolvedEndpoint;

use tracing::info;

static DD_AGENT_VERSION_HEADER: HeaderName = HeaderName::from_static("dd-agent-version");
static DD_API_KEY_HEADER: HeaderName = HeaderName::from_static("dd-api-key");

/// Resolves the authority host to use for a given request path.
///
/// If the path targets the logs endpoint (`/api/v2/logs`), this derives the logs intake
/// host from the resolved endpoint's base host in the form `agent-http-intake.logs.{site}`
/// where `{site}` is extracted from `<version>.agent.{site}`. If the pattern isn't present
/// or parsing fails, the `default_authority` is returned. For non-logs paths, the
/// `default_authority` is always returned.
fn resolve_logs_authority(
    path_and_query: &str, endpoint: &ResolvedEndpoint, default_authority: &Authority,
) -> Authority {
    if path_and_query.starts_with("/api/v2/logs") {
        let base_host = endpoint.endpoint().host_str().unwrap_or("");
        // Handle different site/region suffixes (example .eu)
        if let Some(idx) = base_host.find(".agent.") {
            let site = &base_host[idx + ".agent.".len()..];
            let logs_host = format!("agent-http-intake.logs.{}", site);
            Authority::from_str(&logs_host).unwrap_or_else(|_| default_authority.clone())
        } else {
            default_authority.clone()
        }
    } else {
        default_authority.clone()
    }
}

/// Builds a middleware function that will update the request's URI to use the resolved endpoint's URI, and add the API
/// key as a header.
///
/// The request's URI must already container a relative path component. Any existing scheme, host, and port components
/// will be overridden by the resolved endpoint's URI.
pub fn for_resolved_endpoint<B>(mut endpoint: ResolvedEndpoint) -> impl FnMut(Request<B>) -> Request<B> + Clone {
    info!("WELLWELLLWELL1 {:?}", endpoint);
    let new_uri_authority = Authority::try_from(endpoint.endpoint().authority())
        .expect("should not fail to construct new endpoint authority");
    info!("WELLWELLLWELL2 {:?}", new_uri_authority);
    let new_uri_scheme =
        Scheme::try_from(endpoint.endpoint().scheme()).expect("should not fail to construct new endpoint scheme");
    info!("WELLWELLLWELL3 {:?}", new_uri_scheme);
    move |mut request| {
        let path_and_query = request.uri().path_and_query().expect("request path must exist").clone();
        info!("WELLWELLLWELL4 {:?}", path_and_query);
        let authority = resolve_logs_authority(path_and_query.as_str(), &endpoint, &new_uri_authority);
        info!("WELLWELLLWELL5 {:?}", authority);
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
