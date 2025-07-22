use crate::data_model::payload::PayloadMetadata;

/// An raw payload.
#[derive(Clone)]
pub struct RawPayload {
    metadata: PayloadMetadata,
    data: Vec<u8>,
}

impl RawPayload {
    /// Creates a new `RawPayload` from the given data.
    pub fn new(metadata: PayloadMetadata, data: Vec<u8>) -> Self {
        RawPayload { metadata, data }
    }

    /// Consumes the raw payload and returns the individual parts.
    pub fn into_inner(self) -> (PayloadMetadata, Vec<u8>) {
        (self.metadata, self.data)
    }
}
