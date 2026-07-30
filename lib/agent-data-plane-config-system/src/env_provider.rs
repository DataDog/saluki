//! A Figment provider that reads environment variables into their canonical configuration shape.
//!
//! The by-key configuration view is backed by Figment, whose environment provider can only split a
//! variable name on a fixed separator. The Datadog Agent does not name its variables that way: it
//! iterates a table of known keys and looks up each key's declared variable names, so
//! `DD_PROXY_HTTP` reaches `proxy.http` while `DD_DOGSTATSD_PORT` reaches the flat `dogstatsd_port`.
//! Nothing about the two names says where a nesting boundary falls.
//!
//! [`EnvironmentProvider`] resolves that by reusing the same schema-driven readers the typed
//! configuration path uses: [`apply_datadog_env`] for keys the vendored Datadog schema declares, and
//! the Saluki-only reader for keys it does not. Both write each value at its canonical nested path,
//! so a consumer deserializing the Agent's real configuration shape sees environment values at the
//! same place it sees file and Agent-stream values.
//!
//! Add this provider alongside the prefix-scanning environment provider rather than in place of it.
//! The scanning provider still supplies flat keys that no model declares, which by-key consumers
//! read directly.

use datadog_agent_config::apply_datadog_env;
use figment::providers::Serialized;
use figment::value::{Dict, Map};
use figment::{Error, Metadata, Profile, Provider};
use serde_json::Value;

use crate::saluki_env_overlay;

/// A Figment provider carrying every modeled configuration key set in the environment, at its
/// canonical path.
///
/// Values are snapshotted at construction time.
pub struct EnvironmentProvider {
    values: Value,
}

impl EnvironmentProvider {
    /// Reads the process environment for every modeled Datadog and Saluki-only key.
    ///
    /// Only keys whose environment variables are set to a non-empty value appear in the provider; it
    /// contributes no defaults, so it never masks a lower-precedence source.
    ///
    /// # Errors
    ///
    /// Returns a message naming the environment variable when its value is malformed for the shape
    /// its key declares. This is the same input the typed configuration path rejects at startup, so
    /// failing here keeps the two views from disagreeing about what loaded successfully.
    pub fn new() -> Result<Self, String> {
        let mut values = Value::Object(serde_json::Map::new());

        // The base starts empty, so nothing can be overwritten; `true` avoids a redundant
        // path-presence check per key.
        apply_datadog_env(&mut values, true)?;
        saluki_env_overlay::apply_env(&mut values, true)?;

        Ok(Self { values })
    }
}

impl Provider for EnvironmentProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named("Datadog schema environment variables")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, Error> {
        Serialized::defaults(&self.values).data()
    }
}

#[cfg(test)]
mod tests {
    use saluki_config::test_env_lock;

    use super::*;

    fn values_of(provider: &EnvironmentProvider) -> &Value {
        &provider.values
    }

    #[test]
    fn nested_datadog_key_lands_at_its_canonical_path() {
        let _guard = test_env_lock();
        std::env::set_var("DD_PROXY_HTTP", "http://proxy.example.com");

        let provider = EnvironmentProvider::new().expect("environment reads");

        std::env::remove_var("DD_PROXY_HTTP");
        assert_eq!(
            values_of(&provider).pointer("/proxy/http"),
            Some(&Value::String("http://proxy.example.com".to_string()))
        );
    }

    #[test]
    fn canonical_proxy_variable_is_honored() {
        // `HTTP_PROXY` carries no `DD_` prefix, so the prefix-scanning provider cannot see it at all.
        let _guard = test_env_lock();
        std::env::remove_var("DD_PROXY_HTTP");
        std::env::set_var("HTTP_PROXY", "http://canonical.example.com");

        let provider = EnvironmentProvider::new().expect("environment reads");

        std::env::remove_var("HTTP_PROXY");
        assert_eq!(
            values_of(&provider).pointer("/proxy/http"),
            Some(&Value::String("http://canonical.example.com".to_string()))
        );
    }

    #[test]
    fn saluki_only_key_lands_at_its_canonical_path() {
        let _guard = test_env_lock();
        std::env::set_var("DD_DATA_PLANE_STANDALONE_MODE", "true");

        let provider = EnvironmentProvider::new().expect("environment reads");

        std::env::remove_var("DD_DATA_PLANE_STANDALONE_MODE");
        assert_eq!(
            values_of(&provider).pointer("/data_plane/standalone_mode"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn unset_keys_contribute_nothing() {
        let _guard = test_env_lock();
        std::env::remove_var("DD_PROXY_HTTP");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("http_proxy");

        let provider = EnvironmentProvider::new().expect("environment reads");

        assert!(values_of(&provider).pointer("/proxy/http").is_none());
    }

    #[test]
    fn a_malformed_value_is_rejected() {
        let _guard = test_env_lock();
        std::env::set_var("DD_DOGSTATSD_PORT", "not-a-number");

        let result = EnvironmentProvider::new();

        std::env::remove_var("DD_DOGSTATSD_PORT");
        assert!(result.is_err());
    }
}
