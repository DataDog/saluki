use http::{
    uri::{Authority, Scheme},
    HeaderName, HeaderValue, Request, Uri,
};

use super::endpoints::ResolvedEndpoint;

static ALLOW_ARBITRARY_TAG_VALUE_HEADER: HeaderName = HeaderName::from_static("allow-arbitrary-tag-value");
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
        } else if path_and_query.as_str().starts_with("/api/v0.2/traces")
            || path_and_query.as_str().starts_with("/api/v0.2/stats")
        {
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

/// Builds a middleware function that optionally signals backend support for arbitrary tag values.
///
/// When enabled, adds `Allow-Arbitrary-Tag-Value: true` to every outbound request. This mirrors the Datadog Agent's
/// forwarder behavior for `allow_arbitrary_tags`; it does not perform local tag validation.
pub fn with_allow_arbitrary_tags<B>(enabled: bool) -> impl Fn(Request<B>) -> Request<B> + Clone {
    move |mut request| {
        if enabled {
            request.headers_mut().insert(
                ALLOW_ARBITRARY_TAG_VALUE_HEADER.clone(),
                HeaderValue::from_static("true"),
            );
        }

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

#[cfg(test)]
mod tests {
    use http::Request;

    use super::{for_resolved_endpoint, with_allow_arbitrary_tags, with_version_info};
    use crate::common::datadog::endpoints::ResolvedEndpoint;

    #[test]
    fn for_resolved_endpoint_rewrites_scheme_authority_and_sets_api_key() {
        // A non-Datadog endpoint is used verbatim (no version prefix, no intake-authority markers), so
        // the request's scheme/host/port are replaced with the endpoint's and the API key is attached.
        let endpoint = ResolvedEndpoint::from_raw_endpoint("http://proxy.example.com:8080", "secret-api-key")
            .expect("endpoint should resolve");
        let mut middleware = for_resolved_endpoint(endpoint);

        let request = Request::builder()
            .uri("/api/v2/series")
            .body(())
            .expect("request should build");
        let request = middleware(request);

        assert_eq!(request.uri().to_string(), "http://proxy.example.com:8080/api/v2/series");
        assert_eq!(
            request.headers().get("dd-api-key"),
            Some(&http::HeaderValue::from_static("secret-api-key"))
        );
    }

    #[test]
    fn for_resolved_endpoint_routes_by_path_to_intake_authorities() {
        // A resolved Datadog endpoint carries pre-computed logs/traces intake authorities (its host gains
        // the `.agent.` marker via version-prefixing). Logs go to the logs intake host, traces and APM
        // stats go to the traces intake host, and everything else goes to the endpoint's own authority.
        let endpoint = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com", "fake-api-key")
            .expect("endpoint should resolve");
        let default_authority = endpoint.endpoint().authority().to_string();
        let mut middleware = for_resolved_endpoint(endpoint);

        let cases = [
            ("/api/v2/logs", "agent-http-intake.logs.datadoghq.com"),
            ("/api/v0.2/traces", "trace.agent.datadoghq.com"),
            ("/api/v0.2/stats", "trace.agent.datadoghq.com"),
            ("/api/v2/series", default_authority.as_str()),
        ];

        for (path, expected_authority) in cases {
            let request = Request::builder().uri(path).body(()).expect("request should build");
            let request = middleware(request);

            assert_eq!(request.uri().scheme_str(), Some("https"), "{path}");
            assert_eq!(
                request.uri().authority().map(|authority| authority.as_str()),
                Some(expected_authority),
                "{path}"
            );
            assert_eq!(request.uri().path(), path, "{path}");
        }
    }

    #[test]
    fn for_resolved_endpoint_falls_back_to_endpoint_authority_without_intake_markers() {
        // A custom endpoint has no logs/traces intake authorities, so logs and traces paths fall back to
        // the endpoint's own authority instead of a derived intake host.
        let endpoint = ResolvedEndpoint::from_raw_endpoint("https://custom.example.com", "fake-api-key")
            .expect("endpoint should resolve");
        let mut middleware = for_resolved_endpoint(endpoint);

        for path in ["/api/v2/logs", "/api/v0.2/traces"] {
            let request = Request::builder().uri(path).body(()).expect("request should build");
            let request = middleware(request);

            assert_eq!(
                request.uri().authority().map(|authority| authority.as_str()),
                Some("custom.example.com"),
                "{path}"
            );
        }
    }

    #[test]
    fn with_version_info_sets_agent_version_and_user_agent_headers() {
        let app_details = saluki_metadata::get_app_details();
        let expected_version = app_details.version().raw();
        let expected_user_agent = format!(
            "{}/{}",
            app_details.full_name().replace([' ', '_'], "-").to_lowercase(),
            expected_version,
        );

        let middleware = with_version_info();
        let request = Request::builder()
            .uri("/api/v2/series")
            .body(())
            .expect("request should build");
        let request = middleware(request);

        assert_eq!(
            request
                .headers()
                .get("dd-agent-version")
                .and_then(|value| value.to_str().ok()),
            Some(expected_version)
        );
        assert_eq!(
            request
                .headers()
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some(expected_user_agent.as_str())
        );
    }

    #[test]
    fn allow_arbitrary_tags_header_is_added_when_enabled() {
        let middleware = with_allow_arbitrary_tags(true);
        let request = Request::builder()
            .uri("/api/v1/series")
            .body(())
            .expect("request should build");

        let request = middleware(request);

        assert_eq!(
            request.headers().get("allow-arbitrary-tag-value"),
            Some(&http::HeaderValue::from_static("true"))
        );
    }

    #[test]
    fn allow_arbitrary_tags_header_is_not_added_when_disabled() {
        let middleware = with_allow_arbitrary_tags(false);
        let request = Request::builder()
            .uri("/api/v1/series")
            .body(())
            .expect("request should build");

        let request = middleware(request);

        assert!(request.headers().get("allow-arbitrary-tag-value").is_none());
    }
}
