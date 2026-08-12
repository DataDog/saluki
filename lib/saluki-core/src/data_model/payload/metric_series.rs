use std::sync::Arc;

use super::PayloadMetadata;

/// A versioned, compressed logical metric series batch.
#[derive(Clone)]
pub struct MetricSeriesPayload {
    version: u16,
    metadata: PayloadMetadata,
    compressed_data: Arc<[u8]>,
}

impl MetricSeriesPayload {
    /// Creates a logical series payload.
    pub fn new(version: u16, metadata: PayloadMetadata, compressed_data: Vec<u8>) -> Self {
        Self {
            version,
            metadata,
            compressed_data: compressed_data.into(),
        }
    }

    /// Returns the logical serialization version.
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the payload metadata.
    pub const fn metadata(&self) -> &PayloadMetadata {
        &self.metadata
    }

    /// Returns the compressed logical bytes.
    pub fn compressed_data(&self) -> &[u8] {
        &self.compressed_data
    }

    /// Consumes the payload and transfers ownership of its fields.
    pub fn into_parts(self) -> (u16, PayloadMetadata, Arc<[u8]>) {
        (self.version, self.metadata, self.compressed_data)
    }
}
