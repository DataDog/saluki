use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use datadog_protos::agent::{config_event, ConfigSnapshot};
use futures::StreamExt;
use prost_types::value::Kind;
use saluki_config::dynamic::DynamicConfigurationHandler;
use saluki_config::{dynamic::ConfigChangeEvent, GenericConfiguration};
use saluki_error::GenericError;
use serde_json::{Map, Value};
use tracing::error;

use crate::helpers::remote_agent::RemoteAgentClient;

/// Creates a new `ConfigStreamer` that receives a stream of config events from the remote agent.
pub async fn create_config_stream(
    config: &GenericConfiguration, dynamic_handler: DynamicConfigurationHandler, snapshot_received: Arc<AtomicBool>,
) -> Result<(), GenericError> {
    let shared_config = dynamic_handler.values.clone();
    let notifier = dynamic_handler.notifier.clone();
    let mut client = match RemoteAgentClient::from_configuration(config).await {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create remote agent client: {}.", e);
            return Err(e);
        }
    };
    tokio::spawn(async move {
        let mut rac = client.stream_config_events();
        while let Some(result) = rac.next().await {
            match result {
                Ok(event) => {
                    let change_event = match event.event {
                        Some(config_event::Event::Snapshot(snapshot)) => {
                            let map = snapshot_to_map(&snapshot);
                            shared_config.store(map.into());
                            // Signal that a snapshot has been received.
                            snapshot_received.store(true, Ordering::SeqCst);
                            Some(ConfigChangeEvent::Snapshot)
                        }
                        Some(config_event::Event::Update(update)) => {
                            if let Some(setting) = update.setting {
                                let v = proto_value_to_serde_value(&setting.value);
                                let mut config = (**shared_config.load()).clone();
                                config.as_object_mut().unwrap().insert(setting.key.clone(), v.clone());
                                shared_config.store(Arc::new(config));
                                Some(ConfigChangeEvent::Modified {
                                    old_value: Value::Null,
                                    new_value: v,
                                    key: setting.key,
                                })
                            } else {
                                None
                            }
                        }
                        None => {
                            error!("Received a configuration update event with no data.");
                            None
                        }
                    };
                    if change_event.is_some() {
                        notifier.notify_waiters();
                    }
                }
                Err(e) => error!("Error while reading config event stream: {}.", e),
            }
        }
    });
    Ok(())
}

/// Converts a `ConfigSnapshot` into a single flat `serde_json::Value::Object` (a map).
fn snapshot_to_map(snapshot: &ConfigSnapshot) -> Value {
    let mut map = Map::new();

    for setting in &snapshot.settings {
        let value = proto_value_to_serde_value(&setting.value);
        map.insert(setting.key.clone(), value);
    }

    Value::Object(map)
}

/// Recursively converts a `google::protobuf::Value` into a `serde_json::Value`.
fn proto_value_to_serde_value(proto_val: &Option<prost_types::Value>) -> Value {
    let Some(kind) = proto_val.as_ref().and_then(|v| v.kind.as_ref()) else {
        return Value::Null;
    };

    match kind {
        Kind::NullValue(_) => Value::Null,
        Kind::NumberValue(n) => {
            if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Value::from(*n as i64)
            } else {
                Value::from(*n)
            }
        }
        Kind::StringValue(s) => Value::String(s.clone()),
        Kind::BoolValue(b) => Value::Bool(*b),
        Kind::StructValue(s) => {
            let json_map: Map<String, Value> = s
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), proto_value_to_serde_value(&Some(v.clone()))))
                .collect();
            Value::Object(json_map)
        }

        // If the value is a list, convert it to an array.
        Kind::ListValue(l) => {
            let json_list: Vec<Value> = l
                .values
                .iter()
                .map(|v| proto_value_to_serde_value(&Some(v.clone())))
                .collect();
            Value::Array(json_list)
        }
    }
}
