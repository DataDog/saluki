//! The agent environment of the trace pipeline.

use saluki_common::strings::StringBuilder;
use stringtheory::MetaString;
use tracing::debug;

use crate::common::otlp::traces::normalize::normalize_tag_value_into_unchecked;

/// Environment used when no source supplies a nonempty one.
const FALLBACK_ENV: &str = "none";

/// Prefix of a global tag that supplies the environment. Case-sensitive.
const ENV_TAG_PREFIX: &str = "env:";

/// The resolved, normalized environment of the trace pipeline.
///
/// Sampling and stats aggregation use it for traces that carry no environment of their own, and the trace and stats
/// encoders send it as the agent environment of each payload. The trace pipeline resolves it once, when it is built.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEnv(MetaString);

impl AgentEnv {
    /// Resolves the agent environment from its configured sources.
    ///
    /// `apm_env` and `env` are `None` when not configured. The first configured source is selected, even when its value
    /// is empty:
    ///
    /// 1. `apm_env` (`apm_config.env`)
    /// 2. `env`
    /// 3. the value of the first `env:` entry in `tags`, then in `extra_tags`
    ///
    /// When nothing is selected, or the selected value is empty, the environment is `none`. The result is then
    /// normalized as a tag value, with no further fallback: a value such as `!!!` resolves to an empty environment.
    pub fn resolve(apm_env: Option<&str>, env: Option<&str>, tags: &[String], extra_tags: &[String]) -> Self {
        let (source, configured) = apm_env
            .map(|value| ("apm_config.env", value))
            .or_else(|| env.map(|value| ("env", value)))
            .or_else(|| {
                let tags = tags.iter().map(|tag| ("tags", tag));
                let extra_tags = extra_tags.iter().map(|tag| ("extra_tags", tag));
                tags.chain(extra_tags)
                    .find_map(|(source, tag)| tag.strip_prefix(ENV_TAG_PREFIX).map(|value| (source, value)))
            })
            .unwrap_or(("default", ""));

        let selected = if configured.is_empty() {
            FALLBACK_ENV
        } else {
            configured
        };
        let mut normalized = StringBuilder::new();
        normalize_tag_value_into_unchecked(selected, &mut normalized);
        debug!(
            source,
            configured,
            env = normalized.as_str(),
            "Resolved the trace agent environment."
        );

        Self(MetaString::from(normalized.as_str()))
    }

    /// Returns the environment as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<AgentEnv> for MetaString {
    fn from(env: AgentEnv) -> Self {
        env.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn assert_resolves(apm_env: Option<&str>, env: Option<&str>, tags: &[&str], extra_tags: &[&str], expected: &str) {
        let tags: Vec<String> = tags.iter().map(|tag| tag.to_string()).collect();
        let extra_tags: Vec<String> = extra_tags.iter().map(|tag| tag.to_string()).collect();
        assert_eq!(AgentEnv::resolve(apm_env, env, &tags, &extra_tags).as_str(), expected);
    }

    // Expected values follow the Datadog Agent's `GetTraceAgentDefaultEnv` and the normalization that
    // follows it:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/config/utils/trace.go#L16-L35
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/comp/trace/config/impl/setup.go#L291-L299
    #[test]
    fn resolve_follows_agent_precedence() {
        assert_resolves(None, None, &[], &[], "none");

        // Precedence: `apm_config.env`, then `env`, then the first `env:` tag in `tags`, then in `extra_tags`.
        assert_resolves(Some("Staging"), Some("prod"), &["env:t"], &[], "staging");
        assert_resolves(None, Some("Prod"), &["env:t"], &[], "prod");
        assert_resolves(None, None, &["a:b", "env:first", "env:second"], &["env:x"], "first");
        assert_resolves(None, None, &["a:b"], &["env:x"], "x");
        assert_resolves(None, None, &["ENV:up", "env:low"], &[], "low");

        // A configured empty value is still selected, then replaced by `none`.
        assert_resolves(None, None, &["env:", "env:x"], &[], "none");
        assert_resolves(Some(""), Some("prod"), &[], &[], "none");
        assert_resolves(None, Some(""), &["env:t"], &[], "none");

        // `none` is not applied again after normalization.
        assert_resolves(Some("!!!"), None, &[], &[], "");

        // Empty `DD_APM_ENV` and `DD_ENV` load as not configured; `DD_TAGS="a:b env:tagged"` loads as two tags.
        assert_resolves(None, None, &["a:b", "env:tagged"], &[], "tagged");

        // A streamed default `apm_config.env` loads as not configured.
        assert_resolves(None, Some("prod"), &[], &[], "prod");
    }
}
