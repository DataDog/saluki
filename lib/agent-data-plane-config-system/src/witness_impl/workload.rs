use serde_json::Value;

use super::*;

pub(super) fn consume_key(translator: &mut Translator, key: &str, value: Value) -> Option<Value> {
    match key {
        "cri_connection_timeout" => {
            translator.native.components.workload.source.cri_connection_timeout_secs = u64_value(value, 0);

            None
        }
        "cri_query_timeout" => {
            translator.native.components.workload.source.cri_query_timeout_secs = u64_value(value, 0);

            None
        }
        _ => Some(value),
    }
}
