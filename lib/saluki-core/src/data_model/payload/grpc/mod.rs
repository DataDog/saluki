use saluki_common::buf::FrozenChunkedBytesBuffer;
use stringtheory::MetaString;

use super::PayloadMetadata;

/// A gRPC payload.
#[derive(Clone)]
pub struct GrpcPayload {
    metadata: PayloadMetadata,
    endpoint: MetaString,
    service_path: MetaString,
    body: FrozenChunkedBytesBuffer,
}

impl GrpcPayload {
    /// Creates a new `GrpcPayload`.
    pub fn new(
        metadata: PayloadMetadata, endpoint: MetaString, service_path: MetaString, body: FrozenChunkedBytesBuffer,
    ) -> Self {
        GrpcPayload {
            metadata,
            endpoint,
            service_path,
            body,
        }
    }

    /// Consumes the gRPC payload and returns the individual parts.
    pub fn into_parts(self) -> (PayloadMetadata, MetaString, MetaString, FrozenChunkedBytesBuffer) {
        (self.metadata, self.endpoint, self.service_path, self.body)
    }

    /// Gets a reference to the endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Gets a reference to the service path.
    pub fn service_path(&self) -> &str {
        &self.service_path
    }

    /// Gets a reference to the body.
    pub fn body(&self) -> &FrozenChunkedBytesBuffer {
        &self.body
    }

    /// Gets a reference to the metadata.
    pub fn metadata(&self) -> &PayloadMetadata {
        &self.metadata
    }
}
