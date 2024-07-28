pub const TIMESTAMP_PREFIX: &[u8] = b"d:";
pub const HOSTNAME_PREFIX: &[u8] = b"h:";
pub const AGGREGATION_KEY_PREFIX: &[u8] = b"k:";
pub const PRIORITY_PREFIX: &[u8] = b"p:";
pub const SOURCE_TYPE_PREFIX: &[u8] = b"s:";
pub const ALERT_TYPE_PREFIX: &[u8] = b"t:";
pub const TAGS_PREFIX: &[u8] = b"#:";

pub const PRIORITY_LOW: &str = "low";

pub const ALERT_TYPE_ERROR: &str = "error";
pub const ALERT_TYPE_WARNING: &str = "warning";
pub const ALERT_TYPE_SUCCESS: &str = "success";

pub fn clean_data(s: &str) -> String {
    s.replace("\\n", "\n")
}
