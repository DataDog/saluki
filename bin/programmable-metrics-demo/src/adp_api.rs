//! Clients for ADP's two APIs: the unprivileged one (`/ready`, `/metrics`), and the privileged one, which requires
//! mutual TLS with the IPC certificate, where the programmable-metrics control surface lives.
//!
//! The control surface (`GET /programmable-metrics/{status,export,plan,default-program,explain}`) arrives with stage 5
//! of the implementation plan. An ADP that does not serve a route answers `404 Not Found`, which
//! [`Surface::NotExposed`] reports so that the UI can say so plainly instead of showing an error.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use datadog_agent_commons::ipc::tls::build_ipc_client_ipc_tls_config;
use http::{Request, StatusCode, Uri};
use http_body_util::{BodyExt as _, Empty};
use rustls::RootCertStore;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_io::net::client::http::HttpClient;
use serde::Serialize;

/// How long a request to ADP may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest response body read from ADP.
const MAX_RESPONSE_SIZE: usize = 32 * 1024 * 1024;

/// The control surface routes.
pub mod routes {
    /// ADP's programmable-metrics status: the poll status plus config states.
    pub const STATUS: &str = "/programmable-metrics/status";

    /// The export bundle of the default program's policy scope.
    pub const EXPORT: &str = "/programmable-metrics/export?scope=post_enrich";

    /// Returns the route of the export bundle of one scope of the default program.
    pub fn export_of(scope: &str) -> String {
        format!("/programmable-metrics/export?scope={}", scope)
    }

    /// The active composed logical plan.
    pub const PLAN: &str = "/programmable-metrics/plan";

    /// The default program ADP last lowered, which artifacts are composed into.
    pub const DEFAULT_PROGRAM: &str = "/programmable-metrics/default-program";

    /// The explain document of the active plan, as JSON.
    pub const EXPLAIN: &str = "/programmable-metrics/explain";

    /// The explain document of the active plan, as text.
    pub const EXPLAIN_TEXT: &str = "/programmable-metrics/explain?format=text";
}

/// The answer of one control surface route.
#[derive(Clone, Debug, PartialEq)]
pub enum Surface<T> {
    /// The route answered `200 OK` with this body.
    Available(T),

    /// The route answered `404 Not Found`: this ADP does not expose the control surface.
    NotExposed,

    /// The route could not be reached, or answered with another status or a body that could not be read.
    Failed(String),
}

impl<T> Surface<T> {
    /// Applies a function to an available body.
    pub fn map<U, F>(self, f: F) -> Surface<U>
    where
        F: FnOnce(T) -> Result<U, String>,
    {
        match self {
            Self::Available(value) => match f(value) {
                Ok(value) => Surface::Available(value),
                Err(error) => Surface::Failed(error),
            },
            Self::NotExposed => Surface::NotExposed,
            Self::Failed(error) => Surface::Failed(error),
        }
    }

    /// Returns the state's name, as the UI shows it: `available`, `not_exposed`, or `failed`.
    pub fn state(&self) -> SurfaceState {
        match self {
            Self::Available(_) => SurfaceState::Available,
            Self::NotExposed => SurfaceState::NotExposed,
            Self::Failed(_) => SurfaceState::Failed,
        }
    }
}

/// Whether a control surface route answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceState {
    /// Not asked yet.
    Unknown,

    /// The route answered.
    Available,

    /// The route does not exist in this ADP.
    NotExposed,

    /// The route could not be reached, or failed.
    Failed,
}

/// An HTTP client that can be shared between tasks: each request clones the client out of the lock.
#[derive(Clone)]
struct SharedClient(Arc<Mutex<HttpClient>>);

impl SharedClient {
    fn new(client: HttpClient) -> Self {
        Self(Arc::new(Mutex::new(client)))
    }

    fn get(&self) -> HttpClient {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

/// Clients for ADP's APIs.
///
/// Cheap to clone.
#[derive(Clone)]
pub struct AdpApi {
    plain: SharedClient,
    secure: Result<SharedClient, String>,
    api_authority: String,
    secure_authority: String,
}

impl AdpApi {
    /// Creates clients for ADP's APIs on the given loopback ports.
    ///
    /// The privileged client presents the IPC certificate at `ipc_cert_path` and accepts only a server presenting the
    /// same certificate, as ADP's own CLI does. If that client cannot be built, the unprivileged API still works, and
    /// every control surface request fails with the reason.
    ///
    /// # Errors
    ///
    /// Returns an error if the unprivileged client cannot be built.
    pub async fn new(api_port: u16, secure_api_port: u16, ipc_cert_path: &Path) -> Result<Self, GenericError> {
        // The unprivileged API is plain HTTP, so this client trusts no certificate authority at all, rather than
        // needing the platform's root certificates loaded.
        let plain = HttpClient::builder()
            .with_request_timeout(REQUEST_TIMEOUT)
            .with_tls_config(|tls| tls.with_root_cert_store(RootCertStore::empty()))
            .build()
            .error_context("Failed to build the HTTP client for ADP's unprivileged API.")?;
        let secure = match build_ipc_client_ipc_tls_config(ipc_cert_path).await {
            Ok(tls_config) => HttpClient::builder()
                .with_request_timeout(REQUEST_TIMEOUT)
                .with_client_tls_config(tls_config)
                .build()
                .map(SharedClient::new)
                .map_err(|error| format!("Failed to build the mTLS client for ADP's privileged API: {}", error)),
            Err(error) => Err(format!(
                "Failed to load the IPC certificate '{}' for ADP's privileged API: {}",
                ipc_cert_path.display(),
                error
            )),
        };
        Ok(Self {
            plain: SharedClient::new(plain),
            secure,
            api_authority: format!("127.0.0.1:{}", api_port),
            secure_authority: format!("127.0.0.1:{}", secure_api_port),
        })
    }

    /// Returns `true` if ADP's unprivileged API answers `GET /ready` with `200 OK`.
    pub async fn ready(&self) -> bool {
        match get(self.plain.get(), "http", &self.api_authority, "/ready").await {
            Ok((status, _)) => status == StatusCode::OK,
            Err(_) => false,
        }
    }

    /// Returns the text of ADP's `/metrics`.
    ///
    /// # Errors
    ///
    /// Returns an error message if the API cannot be reached or does not answer `200 OK`.
    pub async fn metrics_text(&self) -> Result<String, String> {
        let (status, body) = get(self.plain.get(), "http", &self.api_authority, "/metrics").await?;
        if status != StatusCode::OK {
            return Err(format!("GET /metrics answered {}.", status));
        }
        String::from_utf8(body.to_vec()).map_err(|_| "GET /metrics answered with text that is not UTF-8.".to_string())
    }

    /// Requests a control surface route (a path and query, such as [`routes::STATUS`]).
    pub async fn control(&self, route: &str) -> Surface<Bytes> {
        let client = match &self.secure {
            Ok(client) => client.get(),
            Err(error) => return Surface::Failed(error.clone()),
        };
        match get(client, "https", &self.secure_authority, route).await {
            Ok((StatusCode::OK, body)) => Surface::Available(body),
            Ok((StatusCode::NOT_FOUND, _)) => Surface::NotExposed,
            Ok((status, body)) => Surface::Failed(format!(
                "GET {} answered {}: {}",
                route,
                status,
                String::from_utf8_lossy(&body[..body.len().min(512)])
            )),
            Err(error) => Surface::Failed(error),
        }
    }

    /// Requests a control surface route and reads its body as JSON.
    pub async fn control_json<T>(&self, route: &str) -> Surface<T>
    where
        T: serde::de::DeserializeOwned,
    {
        self.control(route).await.map(|body| {
            serde_json::from_slice(&body)
                .map_err(|error| format!("GET {} answered with unreadable JSON: {}", route, error))
        })
    }
}

async fn get(
    mut client: HttpClient, scheme: &str, authority: &str, path_and_query: &str,
) -> Result<(StatusCode, Bytes), String> {
    let uri = Uri::builder()
        .scheme(scheme)
        .authority(authority)
        .path_and_query(path_and_query)
        .build()
        .map_err(|error| format!("Invalid ADP API URI: {}", error))?;
    let request = Request::get(uri.clone())
        .body(Empty::<Bytes>::new())
        .map_err(|error| format!("Invalid request to {}: {}", uri, error))?;
    let response = client
        .send(request)
        .await
        .map_err(|error| format!("GET {} failed: {:#}", uri, error))?;
    let status = response.status();
    let body = http_body_util::Limited::new(response.into_body(), MAX_RESPONSE_SIZE)
        .collect()
        .await
        .map_err(|error| format!("Failed to read the response of GET {}: {}", uri, error))?
        .to_bytes();
    Ok((status, body))
}
