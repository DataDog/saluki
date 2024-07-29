#[derive(PartialEq, Eq)]
pub enum MessageType {
    MetricSample,
    Event,
    ServiceCheck,
}

pub const EVENT_PREFIX: &[u8] = b"_e{";
pub const SERVICE_CHECK_PREFIX: &[u8] = b"_sc";

pub fn parse_message_type(data: &[u8]) -> MessageType {
    if data.starts_with(EVENT_PREFIX) {
        return MessageType::Event;
    } else if data.starts_with(SERVICE_CHECK_PREFIX) {
        return MessageType::ServiceCheck;
    }
    MessageType::MetricSample
}
