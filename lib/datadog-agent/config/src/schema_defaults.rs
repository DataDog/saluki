use std::sync::LazyLock;

use crate::DatadogConfiguration;

static SCHEMA_DEFAULTS: LazyLock<DatadogConfiguration> = LazyLock::new(DatadogConfiguration::default);

impl DatadogConfiguration {
    /// Returns a shared configuration populated with the vendored schema's defaults.
    ///
    /// The configuration is initialized on first use.
    pub fn schema_defaults() -> &'static Self {
        &SCHEMA_DEFAULTS
    }
}

#[cfg(test)]
mod tests {
    use super::DatadogConfiguration;

    #[test]
    fn the_defaults_match_an_empty_configuration() {
        let deserialized: DatadogConfiguration =
            serde_json::from_value(serde_json::json!({})).expect("an empty object deserializes");

        assert_eq!(
            serde_json::to_value(&deserialized).expect("the deserialized configuration serializes"),
            serde_json::to_value(DatadogConfiguration::schema_defaults()).expect("the shared defaults serialize")
        );
    }

    #[test]
    fn the_defaults_are_built_once() {
        assert!(std::ptr::eq(
            DatadogConfiguration::schema_defaults(),
            DatadogConfiguration::schema_defaults()
        ));
    }
}
