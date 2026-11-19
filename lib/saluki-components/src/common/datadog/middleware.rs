use http::{
    uri::{Authority, PathAndQuery, Scheme},
    HeaderName, HeaderValue, Request, Uri,
};

use super::endpoints::ResolvedEndpoint;

static ALLOW_ARBITRARY_TAG_VALUE_HEADER: HeaderName = HeaderName::from_static("allow-arbitrary-tag-value");
static DD_AGENT_VERSION_HEADER: HeaderName = HeaderName::from_static("dd-agent-version");
static DD_API_KEY_HEADER: HeaderName = HeaderName::from_static("dd-api-key");

/// Builds a middleware function that will update the request's URI to use the resolved endpoint's URI, and add the API
/// key as a header.
///
/// The request's URI must already contain a relative path component. The resolved endpoint's URI replaces any
/// existing scheme, host, and port components. If the endpoint has a base path, such as `https://proxy.example.com/prefix`,
/// the request's path is appended to it, which gives `/prefix/api/v2/series`. A request that goes to a derived logs or
/// traces intake host doesn't get the base path.
pub fn for_resolved_endpoint<B>(endpoint: ResolvedEndpoint) -> impl Fn(Request<B>) -> Request<B> + Clone {
    let new_uri_authority = Authority::try_from(endpoint.endpoint().authority())
        .expect("should not fail to construct new endpoint authority");
    let new_uri_scheme =
        Scheme::try_from(endpoint.endpoint().scheme()).expect("should not fail to construct new endpoint scheme");
    // Remove trailing slashes so that joining with the request path, which starts with `/`, gives a single `/`. An
    // endpoint with no path, or only `/`, gives an empty base path.
    let base_path = endpoint.endpoint().path().trim_end_matches('/').to_string();
    move |mut request| {
        let path_and_query = request.uri().path_and_query().expect("request path must exist").clone();

        // Select the authority from the request path alone. A derived intake authority is used only for official
        // Datadog endpoints.
        let intake_authority = if path_and_query.as_str().starts_with("/api/v2/logs") {
            endpoint.logs_authority()
        } else if path_and_query.as_str().starts_with("/api/v0.2/traces")
            || path_and_query.as_str().starts_with("/api/v0.2/stats")
        {
            endpoint.traces_authority()
        } else {
            None
        };

        // Prepend the base path only when the request goes to the endpoint's own authority. The base path belongs to
        // the configured endpoint, not to a derived intake host.
        let (authority, path_and_query) = match intake_authority {
            Some(intake_authority) => (intake_authority.clone(), path_and_query),
            None if base_path.is_empty() => (new_uri_authority.clone(), path_and_query),
            None => (
                new_uri_authority.clone(),
                PathAndQuery::try_from(format!("{}{}", base_path, path_and_query.as_str()))
                    .expect("should not fail to construct new path and query"),
            ),
        };

        // Build an updated URI from the endpoint's scheme and the selected authority, plus the path and query.
        let new_uri = Uri::builder()
            .scheme(new_uri_scheme.clone())
            .authority(authority)
            .path_and_query(path_and_query)
            .build()
            .expect("should not fail to construct new URI");
        let api_key = endpoint.api_key();
        let api_key_value = HeaderValue::from_str(&api_key).expect("should not fail to construct API key header value");
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

/// Returns the full name of the application, formatted for a request header: `Example App` gives `example-app`.
pub(crate) fn formatted_app_name(app_details: &saluki_metadata::AppDetails) -> String {
    app_details.full_name().replace([' ', '_'], "-").to_lowercase()
}

/// Builds a middleware function that adds request headers indicating the version of the data plane making the request.
///
/// Adds the `dd-agent-version` header with the version of the data plane (`x.y.z`), and the `User-Agent` header with
/// the name of data plane and the version, such as `agent-data-plane/x.y.z`.`
pub fn with_version_info<B>() -> impl Fn(Request<B>) -> Request<B> + Clone {
    let app_details = saluki_metadata::get_app_details();
    let formatted_full_name = formatted_app_name(app_details);

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
        let middleware = for_resolved_endpoint(endpoint);

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
        let middleware = for_resolved_endpoint(endpoint);

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
        let middleware = for_resolved_endpoint(endpoint);

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
    fn for_resolved_endpoint_prepends_endpoint_base_path() {
        // An endpoint with a base path keeps that path, and the request path is appended to it. A trailing slash on the
        // base path gives a single `/`.
        for raw_endpoint in ["https://proxy.example.com/prefix", "https://proxy.example.com/prefix/"] {
            let endpoint =
                ResolvedEndpoint::from_raw_endpoint(raw_endpoint, "fake-api-key").expect("endpoint should resolve");
            let middleware = for_resolved_endpoint(endpoint);

            for path in ["/api/v2/series", "/api/v2/logs", "/api/v0.2/traces"] {
                let request = Request::builder().uri(path).body(()).expect("request should build");
                let request = middleware(request);

                assert_eq!(
                    request.uri().to_string(),
                    format!("https://proxy.example.com/prefix{path}"),
                    "{raw_endpoint} {path}"
                );
            }
        }
    }

    #[test]
    fn for_resolved_endpoint_without_base_path_keeps_request_path() {
        // An endpoint with no path, or only `/`, uses the request path unchanged, with no `//`.
        for raw_endpoint in ["https://proxy.example.com", "https://proxy.example.com/"] {
            let endpoint =
                ResolvedEndpoint::from_raw_endpoint(raw_endpoint, "fake-api-key").expect("endpoint should resolve");
            let middleware = for_resolved_endpoint(endpoint);

            let request = Request::builder()
                .uri("/api/v2/series")
                .body(())
                .expect("request should build");
            let request = middleware(request);

            assert_eq!(
                request.uri().to_string(),
                "https://proxy.example.com/api/v2/series",
                "{raw_endpoint}"
            );
        }
    }

    #[test]
    fn for_resolved_endpoint_with_base_path_keeps_request_query() {
        let endpoint = ResolvedEndpoint::from_raw_endpoint("https://proxy.example.com/prefix", "fake-api-key")
            .expect("endpoint should resolve");
        let middleware = for_resolved_endpoint(endpoint);

        let request = Request::builder()
            .uri("/api/v1/check_run?foo=bar")
            .body(())
            .expect("request should build");
        let request = middleware(request);

        assert_eq!(request.uri().path(), "/prefix/api/v1/check_run");
        assert_eq!(request.uri().query(), Some("foo=bar"));
    }

    #[test]
    fn for_resolved_endpoint_with_base_path_routes_to_intake_authorities_without_base_path() {
        // The authority is selected from the request path, so a base path on an official Datadog endpoint doesn't
        // prevent routing to the logs and traces intake hosts. The base path belongs to the configured endpoint, so it
        // isn't added to the path on a derived intake host. Other requests keep the base path.
        let endpoint = ResolvedEndpoint::from_raw_endpoint("https://app.datadoghq.com/prefix", "fake-api-key")
            .expect("endpoint should resolve");
        let endpoint_authority = endpoint.endpoint().authority().to_string();
        let middleware = for_resolved_endpoint(endpoint);

        let default_authority = endpoint_authority.as_str();
        let cases = [
            ("/api/v2/logs", "agent-http-intake.logs.datadoghq.com", "/api/v2/logs"),
            ("/api/v0.2/traces", "trace.agent.datadoghq.com", "/api/v0.2/traces"),
            ("/api/v2/series", default_authority, "/prefix/api/v2/series"),
        ];

        for (path, expected_authority, expected_path) in cases {
            let request = Request::builder().uri(path).body(()).expect("request should build");
            let request = middleware(request);

            assert_eq!(
                request.uri().authority().map(|authority| authority.as_str()),
                Some(expected_authority),
                "{path}"
            );
            assert_eq!(request.uri().path(), expected_path, "{path}");
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
