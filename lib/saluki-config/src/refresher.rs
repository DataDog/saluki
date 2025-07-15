use std::sync::Arc;

use arc_swap::ArcSwap;
use saluki_error::GenericError;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

use crate::{ConfigurationError, GenericConfiguration};

/// Configuration for setting up `RefreshableConfiguration`.
#[derive(Default, Deserialize)]
pub struct RefresherConfiguration {}

/// A configuration whose values are refreshed from a remote source at runtime.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct RefreshableConfiguration {
    values: Arc<ArcSwap<Value>>,
}

impl RefresherConfiguration {
    /// Creates a new `RefresherConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    /// Builds a `RefreshableConfiguration`.
    pub async fn build(&self, values: Arc<ArcSwap<Value>>) -> Result<RefreshableConfiguration, GenericError> {
        let refreshable_configuration = RefreshableConfiguration { values };

        Ok(refreshable_configuration)
    }
}
impl RefreshableConfiguration {
    /// Gets a configuration value by key.
    ///
    ///
    /// ## Errors
    ///
    /// If the key does not exist in the configuration, or if the value could not be deserialized into `T`, an error
    /// variant will be returned.
    pub fn get_typed<T>(&self, key: &str) -> Result<T, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        let values = self.values.load();
        match values.get(key) {
            Some(value) => {
                // Attempt to deserialize the value to type T
                serde_json::from_value(value.clone()).map_err(|_| ConfigurationError::InvalidFieldType {
                    field: key.to_string(),
                    expected_ty: std::any::type_name::<T>().to_string(),
                    actual_ty: serde_json_value_type_name(value).to_string(),
                })
            }
            None => Err(ConfigurationError::MissingField {
                help_text: "Try validating remote source provides this field.".to_string(),
                field: key.to_string().into(),
            }),
        }
    }

    /// Gets a configuration value by key, if it exists.
    ///
    /// If the key exists in the configuration, and can be deserialized, `Ok(Some(value))` is returned. Otherwise,
    /// `Ok(None)` will be returned.
    ///
    /// ## Errors
    ///
    /// If the value could not be deserialized into `T`, an error will be returned.
    pub fn try_get_typed<T>(&self, key: &str) -> Result<Option<T>, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        let values = self.values.load();
        match values.get(key) {
            Some(value) => {
                serde_json::from_value(value.clone())
                    .map(Some)
                    .map_err(|_| ConfigurationError::InvalidFieldType {
                        field: key.to_string(),
                        expected_ty: std::any::type_name::<T>().to_string(),
                        actual_ty: serde_json_value_type_name(value).to_string(),
                    })
            }
            None => Ok(None),
        }
    }
}

fn serde_json_value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
