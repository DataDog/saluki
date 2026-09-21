use super::RemapperRule;

/// Returns remapper rules for the trace sampler metrics.
///
/// These metrics match the reference sampler metrics that customer dashboards are built on, so the
/// rules strip the internal `adp.` prefix and carry the sampler dimensions through unchanged.
pub fn get_trace_sampler_remappings() -> Vec<RemapperRule> {
    vec![
        RemapperRule::by_name(
            "adp.datadog.trace_agent.sampler.seen",
            "datadog.trace_agent.sampler.seen",
        )
        .with_original_tags(["sampler", "sampling_priority", "target_service", "target_env"])
        .with_help_text("Number of traces evaluated by each sampler per ten-second window"),
        RemapperRule::by_name(
            "adp.datadog.trace_agent.sampler.kept",
            "datadog.trace_agent.sampler.kept",
        )
        .with_original_tags(["sampler", "sampling_priority", "target_service", "target_env"])
        .with_help_text("Number of traces kept by each sampler per ten-second window"),
        RemapperRule::by_name(
            "adp.datadog.trace_agent.sampler.size",
            "datadog.trace_agent.sampler.size",
        )
        .with_original_tags(["sampler"])
        .with_help_text("Number of service signatures tracked by each adaptive sampler"),
        RemapperRule::by_name(
            "adp.datadog.trace_agent.sampler.rare.hits",
            "datadog.trace_agent.sampler.rare.hits",
        )
        .with_help_text("Number of traces kept by the rare sampler per ten-second window"),
        RemapperRule::by_name(
            "adp.datadog.trace_agent.sampler.rare.misses",
            "datadog.trace_agent.sampler.rare.misses",
        )
        .with_help_text("Number of traces not kept by the rare sampler per ten-second window"),
        RemapperRule::by_name(
            "adp.datadog.trace_agent.sampler.rare.shrinks",
            "datadog.trace_agent.sampler.rare.shrinks",
        )
        .with_help_text("Cumulative number of rare sampler signature table shrinks"),
    ]
}
