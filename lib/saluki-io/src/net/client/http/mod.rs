//! Basic HTTP client.

mod client;

pub use saluki_tls::TlsMinimumVersion;

pub use self::client::{into_client_body, ClientBody, HttpClient, HttpClientBuilder};

mod conn;
pub use self::conn::{HttpProtocol, HttpsCapableConnector, HttpsCapableConnectorBuilder};

mod telemetry;
pub use self::telemetry::{EndpointTelemetry, EndpointTelemetryLayer};
