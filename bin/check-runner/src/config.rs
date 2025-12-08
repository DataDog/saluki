//! Check configuration parsing from stdin JSON.

use std::collections::HashMap;
use std::hash::Hasher;
use std::io::{self, Read};

use fnv::FnvHasher;
use saluki_env::autodiscovery::{CheckConfig, Data, Instance, RawData};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use stringtheory::MetaString;
use twox_hash::XxHash64;

/// Execution mode for the check runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    /// Run the check once and exit.
    Once,
    /// Run the check continuously on its configured interval.
    Continuous,
}

/// Input configuration read from stdin.
#[derive(Debug, Deserialize)]
pub struct CheckRunnerInput {
    /// Name of the check to run.
    pub name: String,
    /// Initialization configuration for the check.
    pub init_config: serde_json::Value,
    /// Instance configurations for the check.
    pub instances: Vec<serde_json::Value>,
    /// Source identifier for the check.
    pub source: String,
    /// Execution mode for the check runner.
    pub execution_mode: ExecutionMode,
}

impl CheckRunnerInput {
    /// Reads check configuration from stdin.
    pub fn from_stdin() -> Result<Self, GenericError> {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| generic_error!("Failed to read from stdin: {}", e))?;

        serde_json::from_str(&input).map_err(|e| generic_error!("Failed to parse check configuration JSON: {}", e))
    }

    /// Converts the input into a `CheckConfig`.
    pub fn into_check_config(self) -> Result<CheckConfig, GenericError> {
        let init_config = json_value_to_data(self.init_config)?;

        // Convert instances
        let instance_data_list: Vec<Data> = self
            .instances
            .into_iter()
            .map(json_value_to_data)
            .collect::<Result<Vec<_>, _>>()?;

        // Calculate digest for instance IDs (simplified version since we don't have full Config)
        let digest = calculate_digest(&self.name, &init_config);

        // Build instances with IDs
        let instances: Vec<Instance> = instance_data_list
            .into_iter()
            .map(|data| {
                let id = instance_id(&self.name, &data, digest, &init_config);
                Instance::new(id, data)
            })
            .collect();

        Ok(CheckConfig {
            name: MetaString::from(self.name),
            init_config,
            instances,
            source: MetaString::from(self.source),
        })
    }
}

/// Convert a JSON value to the Data type used by CheckConfig.
fn json_value_to_data(value: serde_json::Value) -> Result<Data, GenericError> {
    // Convert JSON to YAML via string serialization
    let json_str =
        serde_json::to_string(&value).map_err(|e| generic_error!("Failed to serialize JSON value: {}", e))?;

    let yaml_value: serde_yaml::Value =
        serde_yaml::from_str(&json_str).map_err(|e| generic_error!("Failed to convert JSON to YAML: {}", e))?;

    // Convert to HashMap
    let map = match yaml_value {
        serde_yaml::Value::Mapping(mapping) => {
            let mut result = HashMap::new();
            for (k, v) in mapping {
                if let serde_yaml::Value::String(key) = k {
                    result.insert(MetaString::from(key), v);
                }
            }
            result
        }
        serde_yaml::Value::Null => HashMap::new(),
        _ => return Err(generic_error!("Expected object for config data")),
    };

    Ok(Data::from_map(map))
}

/// Calculate a digest for the check configuration.
fn calculate_digest(name: &str, init_config: &Data) -> u64 {
    let mut h = XxHash64::with_seed(0);
    h.write(name.as_bytes());
    h.write(&init_config.to_bytes().unwrap_or_default());
    h.finish()
}

/// Generate an instance ID following the same algorithm as saluki-env.
fn instance_id(name: &str, instance: &Data, digest: u64, init_config: &Data) -> String {
    let mut h2 = FnvHasher::default();
    h2.write_u64(digest);
    h2.write(&instance.to_bytes().unwrap_or_default());
    h2.write(&init_config.to_bytes().unwrap_or_default());

    let inst_name = instance_name(instance);
    let hash2 = h2.finish();

    if !inst_name.is_empty() {
        format!("{}:{}:{:X}", name, inst_name, hash2)
    } else {
        format!("{}:{:X}", name, hash2)
    }
}

/// Get the instance name from instance data.
fn instance_name(instance: &Data) -> String {
    if let Some(name) = instance.get("name") {
        if let Some(value) = name.as_str() {
            return value.to_string();
        }
    }
    if let Some(namespace) = instance.get("namespace") {
        if let Some(value) = namespace.as_str() {
            return value.to_string();
        }
    }
    String::new()
}
