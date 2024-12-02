//! Basic HTTP client.

use crate::buf::FrozenChunkedBytesBuffer;

mod client;
pub use self::client::HttpClient;

mod conn;
pub use self::conn::HttpsCapableConnector;

pub type ChunkedHttpsClient = HttpClient<FrozenChunkedBytesBuffer>;
