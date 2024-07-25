use saluki_context::BorrowedTag;
use saluki_core::constants::datadog::*;
use saluki_event::metric::{MetricMetadata, MetricOrigin};
use saluki_io::deser::codec::dogstatsd::{InterceptAction, TagMetadataInterceptor};

/// An interceptor that handles tags in a Datadog Agent-like way.
///
/// This interceptor specifically focuses on tags related to origin detection, such as `dd.internal.entity_id` and
/// `dd.internal.card`, as well as some tags that deal with metric origins (confusing, I know), such as
/// `dd.internal.jmx_check_name`.
#[derive(Clone, Debug)]
pub struct AgentLikeTagMetadataInterceptor;

impl TagMetadataInterceptor for AgentLikeTagMetadataInterceptor {
    fn evaluate(&self, tag: &str) -> InterceptAction {
        match tag {
            ENTITY_ID_TAG_KEY => InterceptAction::Intercept,
            JMX_CHECK_NAME_TAG_KEY => InterceptAction::Intercept,
            CARDINALITY_TAG_KEY => InterceptAction::Intercept,
            _ => InterceptAction::Pass,
        }
    }

    fn intercept(&self, tag: &str, metadata: &mut MetricMetadata) {
        let tag = BorrowedTag::from(tag);
        match tag.name_and_value() {
            (Some(ENTITY_ID_TAG_KEY), Some(entity_id)) if entity_id != ENTITY_ID_IGNORE_VALUE => {
                metadata.origin_entity_mut().set_pod_uid(entity_id)
            }
            (Some(JMX_CHECK_NAME_TAG_KEY), Some(jmx_check_name)) => {
                metadata.set_origin(MetricOrigin::jmx_check(jmx_check_name));
            }
            (Some(CARDINALITY_TAG_KEY), Some(value)) => {
                metadata.origin_entity_mut().set_cardinality(value);
            }
            _ => {}
        }
    }
}
