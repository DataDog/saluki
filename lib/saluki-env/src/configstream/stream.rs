use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use datadog_protos::agent::{config_event, ConfigSnapshot};
use futures::StreamExt;
use prost_types::value::Kind;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use serde_json::{Map, Value};
use tracing::{debug, info, warn};

use crate::helpers::remote_agent::RemoteAgentClient;

/// A streamer that receives config events from the remote agent.
pub struct ConfigStreamer {}

impl ConfigStreamer {
    /// Creates a new `ConfigStreamer` that receives a stream of config events from the remote agent.
    pub async fn stream(
        config: &GenericConfiguration, shared_config: Option<Arc<ArcSwap<Value>>>,
        snapshot_received: Option<Arc<AtomicBool>>,
    ) -> Result<Self, GenericError> {
        let mut client = RemoteAgentClient::from_configuration(config).await?;
        let mut rac = client.stream_config_events();
        let snapshot_received = snapshot_received.clone();
        tokio::spawn(async move {
            while let Some(result) = rac.next().await {
                match result {
                    Ok(event) => match event.event {
                        Some(config_event::Event::Snapshot(snapshot)) => {
                            debug!("received config snapshot: {:#?}", snapshot);
                            let map = snapshot_to_map(&snapshot);
                            if let Some(c) = shared_config.as_ref() {
                                c.store(map.into());
                            }
                            // Signal that a snapshot has been received
                            if let Some(signal) = snapshot_received.as_ref() {
                                signal.store(true, Ordering::SeqCst);
                                info!("Configuration snapshot received and applied");
                            }
                        }
                        Some(config_event::Event::Update(update)) => {
                            debug!("received config update: {:#?}", update);
                            let v =
                                proto_value_to_serde_value(update.setting.as_ref().map(|s| &s.value).unwrap_or(&None));
                            let mut config = (**shared_config.as_ref().map(|c| c.load()).unwrap()).clone();
                            config
                                .as_object_mut()
                                .unwrap()
                                .insert(update.setting.as_ref().unwrap().key.clone(), v);
                            if let Some(c) = shared_config.as_ref() {
                                c.store(Arc::new(config));
                            }
                        }
                        None => {
                            warn!("Received a ConfigEvent with no event data");
                        }
                    },
                    Err(e) => warn!("Error while reading config event stream: {}", e),
                }
            }
        });
        Ok(Self {})
    }
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
        Kind::NumberValue(n) => Value::from(*n),
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
