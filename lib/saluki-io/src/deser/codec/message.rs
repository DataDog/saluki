pub enum MessageType {
    MetricSampleType,
    EventType,
    ServiceCheckType,
}

const EVENT_PREFIX: &[u8] = b"_e{";
const SERVICE_CHECK_PREFIX: &[u8] = b"_sc";

pub fn parse_metric_type(data: &[u8]) -> MessageType {
    if data.starts_with(EVENT_PREFIX) {
        return MessageType::EventType;
    } else if data.starts_with(SERVICE_CHECK_PREFIX) {
        return MessageType::ServiceCheckType;
    }
    MessageType::MetricSampleType
}
