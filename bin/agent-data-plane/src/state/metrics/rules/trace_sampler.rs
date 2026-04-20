use super::RemapperRule;

pub fn get_trace_sampler_remappings() -> Vec<RemapperRule> {
    vec![
        RemapperRule::by_name("adp.trace_sampler_seen", "datadog.trace_agent.sampler.seen").with_original_tags([
            "sampler",
            "sampling_priority",
            "target_service",
            "target_env",
        ]),
        RemapperRule::by_name("adp.trace_sampler_kept", "datadog.trace_agent.sampler.kept").with_original_tags([
            "sampler",
            "sampling_priority",
            "target_service",
            "target_env",
        ]),
        RemapperRule::by_name("adp.trace_sampler_rare_hits", "datadog.trace_agent.sampler.rare.hits"),
        RemapperRule::by_name(
            "adp.trace_sampler_rare_misses",
            "datadog.trace_agent.sampler.rare.misses",
        ),
        RemapperRule::by_name(
            "adp.trace_sampler_rare_shrinks",
            "datadog.trace_agent.sampler.rare.shrinks",
        ),
    ]
}
