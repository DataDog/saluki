#[derive(PartialEq, Eq)]
pub enum MessageType {
    MetricSample,
    Event,
    ServiceCheck,
}

pub const EVENT_PREFIX: &[u8] = b"_e{";
pub const SERVICE_CHECK_PREFIX: &[u8] = b"_sc|";

pub const TIMESTAMP_PREFIX: &[u8] = b"d:";
pub const HOSTNAME_PREFIX: &[u8] = b"h:";
pub const AGGREGATION_KEY_PREFIX: &[u8] = b"k:";
pub const PRIORITY_PREFIX: &[u8] = b"p:";
pub const SOURCE_TYPE_PREFIX: &[u8] = b"s:";
pub const ALERT_TYPE_PREFIX: &[u8] = b"t:";
pub const TAGS_PREFIX: &[u8] = b"#";
pub const SERVICE_CHECK_MESSAGE_PREFIX: &[u8] = b"m:";
pub const CONTAINER_ID_PREFIX: &[u8] = b"c:";
pub const EXTERNAL_DATA_PREFIX: &[u8] = b"e:";

pub fn clean_data(s: &str) -> String {
    s.replace("\\n", "\n")
}

pub fn parse_message_type(data: &[u8]) -> MessageType {
    if data.starts_with(EVENT_PREFIX) {
        return MessageType::Event;
    } else if data.starts_with(SERVICE_CHECK_PREFIX) {
        return MessageType::ServiceCheck;
    }
    MessageType::MetricSample
}
