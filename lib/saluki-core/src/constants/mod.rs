pub mod datadog {
    pub const ENTITY_ID_TAG_KEY: &str = "dd.internal.entity_id";
    pub const CARDINALITY_TAG_KEY: &str = "dd.internal.card";
    pub const JMX_CHECK_NAME_TAG_KEY: &str = "dd.internal.jmx_check_name";
    pub const ENTITY_ID_IGNORE_VALUE: &str = "none";
}

pub mod internal {
    pub const CONTAINER_ID_TAG_KEY: &str = "saluki.internal.container_id";
    pub const ORIGIN_PID_TAG_KEY: &str = "saluki.internal.origin_pid";
}
