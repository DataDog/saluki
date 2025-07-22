//! Basic HTTP client.

mod client;
use saluki_common::buf::FrozenChunkedBytesBuffer;

pub use self::client::HttpClient;

mod conn;
pub use self::conn::HttpsCapableConnector;

mod telemetry;
pub use self::telemetry::{EndpointTelemetry, EndpointTelemetryLayer};

pub type ChunkedHttpsClient = HttpClient<FrozenChunkedBytesBuffer>;
