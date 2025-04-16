//! Basic HTTP client.

use crate::buf::FrozenChunkedBytesBuffer;

mod client;
pub use self::client::{HttpClient, ResetHttpClient};

mod conn;
pub use self::conn::HttpsCapableConnector;

mod telemetry;
pub use self::telemetry::{EndpointTelemetry, EndpointTelemetryLayer};

pub type ChunkedHttpsClient = HttpClient<FrozenChunkedBytesBuffer>;
