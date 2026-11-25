use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use saluki_core::data_model::tags::TagSet;

use super::{origin_id_from_attributes, tags_from_attributes, ResourceAttributeTagMode};

pub struct AttributeTranslator {}

impl AttributeTranslator {
    pub fn new() -> Self {
        Self {}
    }

    /// Returns mapped tags, optionally including every scalar attribute under its original key.
    pub fn tags_from_attributes(&self, attributes: &[otlp_common::KeyValue], mode: ResourceAttributeTagMode) -> TagSet {
        tags_from_attributes(attributes, mode)
    }

    pub fn origin_id_from_attributes(&self, attributes: &[otlp_common::KeyValue]) -> Option<String> {
        origin_id_from_attributes(attributes)
    }
}
