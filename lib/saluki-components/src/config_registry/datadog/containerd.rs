//! Annotations for ContainerdConfiguration keys.
use crate::config_registry::{generated::schema, structs, PipelineAffinity, SalukiAnnotation, SupportLevel};

crate::declare_annotations! {
    /// `cri_connection_timeout` - CRI runtime connection timeout, in seconds.
    CRI_CONNECTION_TIMEOUT = SalukiAnnotation {
        schema: &schema::CRI_CONNECTION_TIMEOUT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::CONTAINERD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
        // Seems like these could affect any system that needs to talk to CRI.
        pipeline_affinity: PipelineAffinity::CrossCutting,
    };

    /// `cri_query_timeout` - CRI runtime query timeout, in seconds.
    CRI_QUERY_TIMEOUT = SalukiAnnotation {
        schema: &schema::CRI_QUERY_TIMEOUT,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::CONTAINERD_CONFIGURATION],
        value_type_override: None,
        test_json: None,
        // Seems like these could affect any system that needs to talk to CRI.
        pipeline_affinity: PipelineAffinity::CrossCutting,
    };
}
