use otlp_protos::opentelemetry::proto::common::v1 as otlp_common;
use saluki_context::tags::SharedTagSet;

use super::{origin_id_from_attributes, resource_to_source, tags_from_attributes};
use crate::sources::otlp::attributes::source::Source;

pub struct AttributeTranslator {}

impl AttributeTranslator {
    pub fn new() -> Self {
        Self {}
    }

    pub fn tags_from_attributes(&self, attributes: &[otlp_common::KeyValue]) -> SharedTagSet {
        tags_from_attributes(attributes)
    }

    #[allow(dead_code)]
    pub fn origin_id_from_attributes(&self, attributes: &[otlp_common::KeyValue]) -> Option<String> {
        origin_id_from_attributes(attributes)
    }

    pub fn resource_to_source(
        &self, resource: &otlp_protos::opentelemetry::proto::resource::v1::Resource,
    ) -> Option<Source> {
        resource_to_source(resource)
    }
}
