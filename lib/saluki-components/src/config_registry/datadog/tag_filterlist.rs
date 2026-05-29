//! Annotations for metric tag filterlist transform configuration keys.
use crate::config_registry::{generated::schema, structs, Pipeline, PipelineAffinity, SalukiAnnotation, SupportLevel};

crate::declare_annotations! {
    /// `aggregator_tag_filter_cache_capacity`—maximum entries in the per-context deduplication cache.
    AGGREGATOR_TAG_FILTER_CACHE_CAPACITY = SalukiAnnotation {
        schema: &schema::AGGREGATOR_TAG_FILTER_CACHE_CAPACITY,
        support_level: SupportLevel::Full,
        additional_yaml_paths: &[],
        env_var_override: None,
        used_by: &[structs::TAG_FILTERLIST_CONFIGURATION],
        value_type_override: None,
        test_json: None,
        pipeline_affinity: PipelineAffinity::Pipelines(&[Pipeline::DogStatsD]),
    };
}
