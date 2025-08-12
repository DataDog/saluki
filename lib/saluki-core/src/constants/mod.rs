//! Constants values used through various Saluki crates.

/// Datadog-specific constants.
pub mod datadog {
    /// Tag key used to specify the hostname attached to the given metric.
    pub const HOST_TAG_KEY: &str = "host";

    /// Tag key used to specify the entity ID attached to the given metric.
    pub const ENTITY_ID_TAG_KEY: &str = "dd.internal.entity_id";

    /// Tag key used to specify the tag cardinality to use when enriching the given metric.
    pub const CARDINALITY_TAG_KEY: &str = "dd.internal.card";

    /// Tag key used to specify the JMX check name attached to the given metric.
    pub const JMX_CHECK_NAME_TAG_KEY: &str = "dd.internal.jmx_check_name";

    /// Sentinel value for the entity ID tag key, indicating that the entity ID should be ignored.
    pub const ENTITY_ID_IGNORE_VALUE: &str = "none";
}
