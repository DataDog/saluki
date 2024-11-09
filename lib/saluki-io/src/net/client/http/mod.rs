//! Basic HTTP client.

use super::replay::ReplayBody;
use crate::buf::ChunkedBuffer;

mod client;
pub use self::client::HttpClient;

mod conn;
pub use self::conn::HttpsCapableConnector;

pub type ChunkedHttpsClient<O> = HttpClient<ReplayBody<ChunkedBuffer<O>>>;
