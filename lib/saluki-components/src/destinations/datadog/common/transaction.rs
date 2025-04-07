use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use http::Request;
use http_body::{Body, Frame};
use pin_project::pin_project;
use saluki_io::{
    buf::ReadIoBuffer,
    net::util::retry::{EventContainer, Retryable},
};
use serde::{ser::SerializeSeq as _, Deserialize, Serialize, Serializer};

/// Data type for the body of `TransactionBody<B>`.
pub enum TransactionBodyData<B>
where
    B: Body,
{
    /// Original body data.
    Original(B::Data),

    /// Rehydrated body data.
    Rehydrated(Bytes),
}

impl<B> Buf for TransactionBodyData<B>
where
    B: Body,
{
    fn remaining(&self) -> usize {
        match self {
            Self::Original(data) => data.remaining(),
            Self::Rehydrated(data) => data.remaining(),
        }
    }

    fn chunk(&self) -> &[u8] {
        match self {
            Self::Original(data) => data.chunk(),
            Self::Rehydrated(data) => data.chunk(),
        }
    }

    fn advance(&mut self, cnt: usize) {
        match self {
            Self::Original(data) => data.advance(cnt),
            Self::Rehydrated(data) => data.advance(cnt),
        }
    }
}

/// Custom body type that can abstract over an "original" body type `B` and a rehydrated body type based on `Bytes`.
///
/// In order for `Transaction<B>` to support being serialized and deserialized, we need to be able to serialize and
/// deserialize the body of the request. However, the body type `B` may not be `Serialize` or `Deserialize` itself. To
/// compensate for this, `TransactionBody<B>` provides the necessary serialization and deserialization logic by bounding
/// `B` in a way that ensures we can clone it and serialize it to disk, and then rehydrate it to a compatible body type
/// after deserialization.
#[derive(Clone, Deserialize)]
#[serde(bound = "", from = "String")]
#[pin_project(project = TransactionBodyProj)]
pub enum TransactionBody<B> {
    /// Original body.
    Original(#[pin] B),

    /// Rehydrated body.
    Rehydrated(Option<Bytes>),
}

impl<B> Buf for TransactionBody<B>
where
    B: Buf,
{
    fn remaining(&self) -> usize {
        match self {
            Self::Original(body) => body.remaining(),
            Self::Rehydrated(body) => body.as_ref().map_or(0, |body| body.remaining()),
        }
    }

    fn chunk(&self) -> &[u8] {
        match self {
            Self::Original(body) => body.chunk(),
            Self::Rehydrated(body) => body.as_ref().map_or(&[], |body| body.chunk()),
        }
    }

    fn advance(&mut self, cnt: usize) {
        match self {
            Self::Original(body) => body.advance(cnt),
            Self::Rehydrated(body) => match body {
                Some(body) => body.advance(cnt),
                None => panic!("attempted to advance a rehydrated body that was consumed"),
            },
        }
    }
}

impl<B> Body for TransactionBody<B>
where
    B: Body,
{
    type Data = TransactionBodyData<B>;
    type Error = B::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        match this {
            TransactionBodyProj::Original(body) => body.poll_frame(cx).map(|maybe_frame| {
                maybe_frame.map(|res| res.map(|frame| frame.map_data(|data| TransactionBodyData::Original(data))))
            }),
            TransactionBodyProj::Rehydrated(body) => Poll::Ready(
                body.take()
                    .map(|body| Ok(Frame::data(TransactionBodyData::Rehydrated(body)))),
            ),
        }
    }
}

impl<B> Serialize for TransactionBody<B>
where
    B: ReadIoBuffer + Clone,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Original(body) => {
                // We have to clone the body and then run through all of the chunks to serialize them.
                //
                // TODO: It's not clear to me if this is actually as optimized as we can make it, in terms of
                // serialization.
                let mut new_body = body.clone();
                let mut seq = serializer.serialize_seq(Some(new_body.remaining()))?;
                while new_body.remaining() > 0 {
                    let chunk = new_body.chunk();
                    for b in chunk {
                        seq.serialize_element(b)?;
                    }

                    new_body.advance(chunk.len());
                }

                seq.end()
            }
            Self::Rehydrated(body) => match body.as_ref() {
                Some(body) => body.serialize(serializer),
                None => Err(serde::ser::Error::custom(
                    "attempted to serialize a rehydrated body that was consumed",
                )),
            },
        }
    }
}

impl<B> From<String> for TransactionBody<B> {
    fn from(s: String) -> Self {
        Self::Rehydrated(Some(Bytes::from(s)))
    }
}

/// Transaction metadata.
#[derive(Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// Number of events represented by this transaction.
    pub event_count: usize,
}

impl Metadata {
    /// Create a new `Metadata` instance with the given event count.
    pub const fn from_event_count(event_count: usize) -> Self {
        Self { event_count }
    }
}

/// A generic HTTP transaction that can be serialized and deserialized.
///
/// In order to support using the retry queue, which may need to serialize and deserialize the transaction to disk, we
/// need a generic container for HTTP requests that can carry the necessary metadata (event count, etc), and the request
/// itself (headers, path, body, etc).
///
/// `Transaction<B>` supports this by allowing for wrapping an in-memory body type `B` (e.g. `ReadIoBuffer`) or wrapping
/// a body that has been rehydrated from a string (e.g. `Bytes`). This means that `B` can be a complex type that cannot
/// actually be rehydrated from a single string input (such as `FrozenChunkedBytesBuffer`) and we maintain optimal
/// memory usage, and performance, regardless of which body type was used to construct `Transaction<B>`.
#[derive(Clone, Deserialize, Serialize)]
#[serde(bound = "")]
pub struct Transaction<B>
where
    B: ReadIoBuffer + Clone,
{
    metadata: Metadata,

    #[serde(with = "http_serde_ext::request")]
    request: Request<TransactionBody<B>>,
}

impl<B> Transaction<B>
where
    B: ReadIoBuffer + Clone,
{
    /// Create a new `Transaction` from an original request.
    pub fn from_original(metadata: Metadata, request: Request<B>) -> Self {
        Self {
            metadata,
            request: request.map(TransactionBody::Original),
        }
    }

    /// Reassembles a `Transaction` from a decomposed `Transaction<B>`.
    pub fn reassemble(metadata: Metadata, request: Request<TransactionBody<B>>) -> Self {
        Self { metadata, request }
    }

    /// Consumes the `Transaction` and returns the transaction metadata and original request.
    pub fn into_parts(self) -> (Metadata, http::Request<TransactionBody<B>>) {
        (self.metadata, self.request)
    }
}

impl<B> EventContainer for Transaction<B>
where
    B: ReadIoBuffer + Clone,
{
    fn event_count(&self) -> u64 {
        self.metadata.event_count as u64
    }
}

impl<B> Retryable for Transaction<B>
where
    B: ReadIoBuffer + Clone,
{
    fn size_bytes(&self) -> u64 {
        self.request.body().remaining() as u64
    }
}
