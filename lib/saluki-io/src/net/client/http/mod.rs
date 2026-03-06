//! Basic HTTP client.

mod client;

pub use self::client::{into_client_body, ClientBody, HttpClient};

mod conn;
pub use self::conn::{HttpsCapableConnector, HttpsCapableConnectorBuilder};

mod telemetry;
pub use self::telemetry::{EndpointTelemetry, EndpointTelemetryLayer};
