//! Builders for the OTLP types shared by every signal.

use otlp_protos::opentelemetry::proto::common::v1::{any_value::Value, AnyValue, ArrayValue, KeyValue, KeyValueList};

pub fn string(value: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(value.to_string())),
    }
}

pub fn int(value: i64) -> AnyValue {
    AnyValue {
        value: Some(Value::IntValue(value)),
    }
}

pub fn double(value: f64) -> AnyValue {
    AnyValue {
        value: Some(Value::DoubleValue(value)),
    }
}

pub fn boolean(value: bool) -> AnyValue {
    AnyValue {
        value: Some(Value::BoolValue(value)),
    }
}

pub fn bytes(value: &[u8]) -> AnyValue {
    AnyValue {
        value: Some(Value::BytesValue(value.to_vec())),
    }
}

pub fn array(values: Vec<AnyValue>) -> AnyValue {
    AnyValue {
        value: Some(Value::ArrayValue(ArrayValue { values })),
    }
}

pub fn map(entries: Vec<KeyValue>) -> AnyValue {
    AnyValue {
        value: Some(Value::KvlistValue(KeyValueList { values: entries })),
    }
}

pub fn empty() -> AnyValue {
    AnyValue { value: None }
}

pub fn kv(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(value),
    }
}
