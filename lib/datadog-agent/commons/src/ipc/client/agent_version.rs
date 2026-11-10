//! The version of the core Agent, from its command API.
//!
//! The command API serves REST endpoints on the same port as the gRPC IPC API, behind the same authentication: the
//! bearer token and the IPC mTLS identity. The gRPC API has no version RPC, so the version comes from
//! `GET /agent/version`, which returns the Agent's `version.Agent()`.

use std::time::Duration;

use bytes::Bytes;
use http::{header, Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Empty};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::HttpClient;
use serde::Deserialize;

use crate::ipc::{config::RemoteAgentClientConfiguration, tls::build_ipc_client_ipc_tls_config};

const AGENT_VERSION_PATH: &str = "/agent/version";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The version of the core Agent, as the running Agent reports it.
///
/// Unlike [`crate::agent_version`], which ADP was built against, this is the Agent ADP is running alongside.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunningAgentVersion {
    /// Version string, such as `7.81.0` or `7.81.0-devel+git.12.abcdef0`.
    ///
    /// This is the Agent's `version.RunningAgentVersion`, which the trace-agent, built alongside it, reports too.
    pub version: String,

    /// Short hash of the commit the Agent was built from.
    pub commit: String,
}

/// The Agent's `version.Version`, as `/agent/version` serializes it.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VersionResponse {
    major: i64,
    minor: i64,
    patch: i64,
    #[serde(default)]
    pre: String,
    #[serde(default)]
    meta: String,
    #[serde(default)]
    commit: String,
}

impl From<VersionResponse> for RunningAgentVersion {
    /// Rebuilds the version string the Agent parsed its version from: `MAJOR.MINOR.PATCH`, then `-PRE` and `+META`
    /// when present.
    fn from(response: VersionResponse) -> Self {
        let mut version = format!("{}.{}.{}", response.major, response.minor, response.patch);
        if !response.pre.is_empty() {
            version.push('-');
            version.push_str(&response.pre);
        }
        if !response.meta.is_empty() {
            version.push('+');
            version.push_str(&response.meta);
        }
        Self {
            version,
            commit: response.commit,
        }
    }
}

/// Fetches the version of the core Agent from its command API.
///
/// # Errors
///
/// If the authentication token or IPC certificate can't be read, the Agent can't be reached, or it answers with an
/// error status or an unexpected body, an error is returned. The request isn't retried.
pub async fn fetch_agent_version(config: &RemoteAgentClientConfiguration) -> Result<RunningAgentVersion, GenericError> {
    let token_path = config.auth.auth_token_file_path();
    let token = tokio::fs::read_to_string(token_path).await.with_error_context(|| {
        format!(
            "Failed to read the Agent authentication token from '{}'.",
            token_path.display()
        )
    })?;
    let tls_config = build_ipc_client_ipc_tls_config(config.auth.ipc_cert_file_path()).await?;

    let builder = HttpClient::builder()
        .with_client_tls_config(tls_config)
        .with_request_timeout(REQUEST_TIMEOUT);
    #[cfg(target_os = "linux")]
    let builder = match config.vsock_addr() {
        Some(addr) => builder.with_vsock_addr(addr),
        None => builder,
    };
    let mut client = builder.build()?;

    let request = version_request(&config.endpoint(), token.trim())?;
    let response = client
        .send(request)
        .await
        .error_context("Failed to query the Agent version.")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .error_context("Failed to read the Agent version response.")?
        .to_bytes();
    parse_version_response(status, &body)
}

/// Builds the `/agent/version` request for the command API at `endpoint`.
fn version_request(endpoint: &Uri, token: &str) -> Result<Request<Empty<Bytes>>, GenericError> {
    let uri = Uri::builder()
        .scheme(endpoint.scheme_str().unwrap_or("https"))
        .authority(
            endpoint
                .authority()
                .ok_or_else(|| generic_error!("The Agent command API endpoint has no authority."))?
                .clone(),
        )
        .path_and_query(AGENT_VERSION_PATH)
        .build()
        .error_context("Failed to build the Agent version URI.")?;

    // Unlike the gRPC API, the command API only accepts the `Bearer` scheme spelled with a capital letter.
    Request::get(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Empty::new())
        .error_context("Failed to build the Agent version request.")
}

fn parse_version_response(status: StatusCode, body: &[u8]) -> Result<RunningAgentVersion, GenericError> {
    if !status.is_success() {
        return Err(generic_error!(
            "The Agent answered the version request with status {status}."
        ));
    }
    let response: VersionResponse =
        serde_json::from_slice(body).error_context("The Agent version response is not a version.")?;
    Ok(response.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_string_is_rebuilt_from_its_parts() {
        let cases = [
            (
                r#"{"Major":7,"Minor":81,"Patch":0,"Pre":"","Meta":"","Commit":"abc1234"}"#,
                "7.81.0",
            ),
            (
                r#"{"Major":7,"Minor":81,"Patch":0,"Pre":"rc.2","Meta":"","Commit":"abc1234"}"#,
                "7.81.0-rc.2",
            ),
            (
                r#"{"Major":7,"Minor":81,"Patch":0,"Pre":"devel","Meta":"git.12.abc1234","Commit":"abc1234"}"#,
                "7.81.0-devel+git.12.abc1234",
            ),
        ];
        for (body, version) in cases {
            let parsed = parse_version_response(StatusCode::OK, body.as_bytes()).expect("version should parse");
            assert_eq!(
                parsed,
                RunningAgentVersion {
                    version: version.to_string(),
                    commit: "abc1234".to_string(),
                }
            );
        }
    }

    #[test]
    fn an_error_status_or_unexpected_body_is_an_error() {
        let body = br#"{"Major":7,"Minor":81,"Patch":0}"#;
        assert!(parse_version_response(StatusCode::UNAUTHORIZED, body).is_err());
        assert!(parse_version_response(StatusCode::OK, b"not json").is_err());
        assert!(parse_version_response(StatusCode::OK, b"{}").is_err());
    }

    #[test]
    fn the_request_targets_the_command_api_with_a_bearer_token() {
        let endpoint: Uri = "https://127.0.0.1:5001".parse().unwrap();
        let request = version_request(&endpoint, "token").expect("request should build");

        assert_eq!(request.uri(), "https://127.0.0.1:5001/agent/version");
        assert_eq!(request.headers()[header::AUTHORIZATION], "Bearer token");
    }
}
