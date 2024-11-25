//! Basic HTTP client.

use super::replay::ReplayBody;
use crate::buf::ChunkedBytesBuffer;

mod client;
pub use self::client::HttpClient;

mod conn;
pub use self::conn::HttpsCapableConnector;

pub type ChunkedHttpsClient<O> = HttpClient<ReplayBody<ChunkedBytesBuffer<O>>>;
