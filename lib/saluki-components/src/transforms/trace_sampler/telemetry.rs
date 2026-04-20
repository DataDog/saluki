use metrics::{counter, Label, Level};

pub(super) enum SamplerName {
    Priority,
    NoPriority,
    Error,
    Rare,
    Probabilistic,
    Unknown,
}

impl SamplerName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::NoPriority => "no_priority",
            Self::Error => "error",
            Self::Rare => "rare",
            Self::Probabilistic => "probabilistic",
            Self::Unknown => "unknown",
        }
    }

    fn should_add_env_tag(&self) -> bool {
        matches!(self, Self::Priority | Self::NoPriority | Self::Rare | Self::Error)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

fn priority_tag_value(priority: i32) -> &'static str {
    match priority {
        -1 => "manual_drop",
        0 => "auto_drop",
        1 => "auto_keep",
        2 => "manual_keep",
        _ => "none",
    }
}

pub(super) struct SamplerTelemetry;

impl SamplerTelemetry {
    pub fn record(&self, sampler: &SamplerName, kept: bool, priority: i32, service: &str, env: &str) {
        let mut labels: Vec<Label> = Vec::with_capacity(4);
        labels.push(Label::new("sampler", sampler.as_str()));
        if matches!(sampler, SamplerName::Priority) {
            labels.push(Label::new("sampling_priority", priority_tag_value(priority)));
        }
        if !service.is_empty() {
            labels.push(Label::new("target_service", service.to_owned()));
        }
        if !env.is_empty() && sampler.should_add_env_tag() {
            labels.push(Label::new("target_env", env.to_owned()));
        }
        counter!(level: Level::INFO, "trace_sampler_seen", labels.clone()).increment(1);
        if kept {
            counter!(level: Level::INFO, "trace_sampler_kept", labels).increment(1);
        }
    }
}
