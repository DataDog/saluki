//! A Figment provider that reads the Datadog schema's environment variables into their canonical
//! shape.
//!
//! Figment's own environment provider can only split a variable name on a fixed separator, but the
//! Datadog Agent does not name its variables that way: it looks up each known key's declared
//! variable names, so `DD_PROXY_HTTP` reaches `proxy.http` while `DD_DOGSTATSD_PORT` reaches the
//! flat `dogstatsd_port`. Nothing about either name says where a nesting boundary falls.
//!
//! [`DatadogEnvProvider`] resolves that by reading the environment through
//! [`apply_datadog_env`](crate::apply_datadog_env), the same schema-driven reader the typed
//! configuration path uses, so every value lands at the path its key declares.

use figment::providers::Serialized;
use figment::value::{Dict, Map};
use figment::{Error, Metadata, Profile, Provider};
use serde_json::Value;

use crate::env_reader::{apply_datadog_env, apply_datadog_env_vars};

/// A Figment provider carrying every Datadog schema key set in the environment, at its canonical
/// path.
///
/// Values are snapshotted at construction time.
pub struct DatadogEnvProvider {
    values: Value,
}

impl DatadogEnvProvider {
    /// Reads the process environment for every modeled Datadog key.
    ///
    /// Only keys whose environment variables are set to a non-empty value appear in the provider; it
    /// contributes no defaults, so it never masks a lower-precedence source.
    ///
    /// # Errors
    ///
    /// Returns a message naming the environment variable when its value is malformed for the shape
    /// its key declares.
    pub fn new() -> Result<Self, String> {
        Self::build(|values| apply_datadog_env(values, true))
    }

    /// Reads explicitly provided environment variable name/value pairs instead of the process
    /// environment.
    ///
    /// # Errors
    ///
    /// Returns a message naming the environment variable when its value is malformed for the shape
    /// its key declares.
    pub fn from_env_vars(vars: Vec<(String, String)>) -> Result<Self, String> {
        Self::build(|values| apply_datadog_env_vars(values, vars, true))
    }

    fn build(read: impl FnOnce(&mut Value) -> Result<(), String>) -> Result<Self, String> {
        // The base starts empty, so nothing can be overwritten and the readers' `overwrite` flag is
        // immaterial.
        let mut values = Value::Object(serde_json::Map::new());
        read(&mut values)?;
        Ok(Self { values })
    }
}

impl Provider for DatadogEnvProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named("Datadog schema environment variables")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, Error> {
        Serialized::defaults(&self.values).data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_key_lands_at_its_canonical_path() {
        let provider = DatadogEnvProvider::from_env_vars(vec![(
            "DD_PROXY_HTTP".to_string(),
            "http://proxy.example.com".to_string(),
        )])
        .expect("environment reads");

        assert_eq!(
            provider.values.pointer("/proxy/http"),
            Some(&Value::String("http://proxy.example.com".to_string()))
        );
    }

    #[test]
    fn canonical_proxy_variable_is_honored() {
        // `HTTP_PROXY` carries no `DD_` prefix, so a prefix-scanning provider cannot see it at all.
        let provider = DatadogEnvProvider::from_env_vars(vec![(
            "HTTP_PROXY".to_string(),
            "http://canonical.example.com".to_string(),
        )])
        .expect("environment reads");

        assert_eq!(
            provider.values.pointer("/proxy/http"),
            Some(&Value::String("http://canonical.example.com".to_string()))
        );
    }

    #[test]
    fn a_value_is_decoded_into_the_shape_its_key_declares() {
        let provider = DatadogEnvProvider::from_env_vars(vec![("DD_DOGSTATSD_PORT".to_string(), "9125".to_string())])
            .expect("environment reads");

        assert_eq!(provider.values.pointer("/dogstatsd_port"), Some(&Value::from(9125)));
    }

    #[test]
    fn unset_keys_contribute_nothing() {
        let provider = DatadogEnvProvider::from_env_vars(Vec::new()).expect("environment reads");
        assert_eq!(provider.values, serde_json::json!({}));
    }

    #[test]
    fn a_malformed_value_is_rejected() {
        let result =
            DatadogEnvProvider::from_env_vars(vec![("DD_DOGSTATSD_PORT".to_string(), "not-a-number".to_string())]);
        assert!(result.is_err());
    }
}
