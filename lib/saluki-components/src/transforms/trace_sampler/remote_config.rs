//! Remote Configuration settings for trace sampling.
//!
//! The `APM_SAMPLING` product can change the priority and error samplers' target traces per second
//! and turn rare sampling on or off. A configuration may set values for every environment or
//! override individual fields for a specific environment. Unset fields use the local trace sampler
//! settings. See the Datadog Agent's `pkg/trace/remoteconfighandler/remote_config_handler.go` for
//! the behavior this module follows.

use std::fmt;

use datadog_agent_remote_config::{
    decode_json, ApplyError, ConfigId, JsonError, ProductDecoder, RemoteConfigurationClient, Subscription,
};
use serde::{Deserialize, Deserializer};

/// Optional sampling settings for all environments or one named environment.
///
/// Missing and `null` fields leave that setting unset; `0` and `false` are explicit values.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct SamplerEnvConfig {
    /// Target traces per second for the priority sampler.
    #[serde(
        default,
        rename = "priority_sampler_target_TPS",
        alias = "priority_sampler_target_tps"
    )]
    pub(crate) priority_sampler_target_tps: Option<f64>,

    /// Target traces per second for the errors sampler.
    #[serde(default, rename = "errors_sampler_target_TPS", alias = "errors_sampler_target_tps")]
    pub(crate) errors_sampler_target_tps: Option<f64>,

    /// Whether the rare sampler is enabled.
    #[serde(default)]
    pub(crate) rare_sampler_enabled: Option<bool>,
}

/// An environment name and its sampling overrides.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct EnvAndConfig {
    /// Environment to match against the normalized local default; the payload name is used as given.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) env: String,

    /// The settings for `env`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) config: SamplerEnvConfig,
}

/// Settings supplied by one `APM_SAMPLING` configuration.
///
/// The Datadog Agent sends the `*_TPS` field names. Lowercase `*_tps` names also work;
/// unknown fields are ignored.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct SamplerConfig {
    /// Settings for every environment.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) all_envs: SamplerEnvConfig,

    /// Settings for individual environments.
    #[serde(default, deserialize_with = "null_entries_as_default")]
    pub(crate) by_env: Vec<EnvAndConfig>,
}

/// Treats `null` like a missing field, as the Datadog Agent's JSON decoder does.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Treats a `null` list as empty and a `null` entry as an empty value.
fn null_entries_as_default<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    let entries = Option::<Vec<Option<T>>>::deserialize(deserializer)?.unwrap_or_default();
    Ok(entries.into_iter().map(Option::unwrap_or_default).collect())
}

/// Effective sampling settings, either from local configuration or resolved remote overrides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SamplerSettings {
    /// Target traces per second for the priority sampler.
    pub(crate) target_traces_per_second: f64,

    /// Target traces per second for the errors sampler.
    pub(crate) errors_per_second: f64,

    /// Whether the rare sampler is enabled.
    pub(crate) rare_sampler_enabled: bool,
}

impl SamplerConfig {
    /// Chooses settings for the local default environment.
    ///
    /// For each field, use the last matching `by_env` entry, then `all_envs`, then the local settings.
    /// The caller normalizes `default_env` as a tag value; names in the remote payload are
    /// compared as given. A payload name such as `Prod` will not match a normalized `prod`.
    pub(crate) fn resolve(&self, default_env: &str, static_settings: &SamplerSettings) -> SamplerSettings {
        let for_env = self
            .by_env
            .iter()
            .rfind(|entry| entry.env == default_env)
            .map(|entry| &entry.config);

        let pick = |field: fn(&SamplerEnvConfig) -> Option<f64>, fallback: f64| {
            for_env
                .and_then(field)
                .or_else(|| field(&self.all_envs))
                .unwrap_or(fallback)
        };
        SamplerSettings {
            target_traces_per_second: pick(
                |c| c.priority_sampler_target_tps,
                static_settings.target_traces_per_second,
            ),
            errors_per_second: pick(|c| c.errors_sampler_target_tps, static_settings.errors_per_second),
            rare_sampler_enabled: for_env
                .and_then(|c| c.rare_sampler_enabled)
                .or(self.all_envs.rare_sampler_enabled)
                .unwrap_or(static_settings.rare_sampler_enabled),
        }
    }
}

/// The result of an accepted `APM_SAMPLING` assignment.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RemoteSampling {
    /// No configuration is assigned; keep the last applied remote settings, if any.
    Unassigned,

    /// The one assigned configuration and its ID.
    Remote {
        /// The configuration's ID.
        id: ConfigId,

        /// The configuration.
        config: SamplerConfig,
    },
}

/// Why an `APM_SAMPLING` assignment was rejected.
///
/// The client reports this error to the remote configuration service.
#[derive(Debug)]
pub(crate) enum SamplingError {
    /// A configuration's payload is not a valid sampling configuration.
    InvalidPayload(JsonError),

    /// More than one configuration was assigned.
    TooManyConfigurations {
        /// How many configurations were assigned, including ones that failed to decode.
        assigned: usize,
    },

    /// The one assigned configuration failed to decode.
    NoValidConfiguration,
}

impl fmt::Display for SamplingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPayload(error) => error.fmt(f),
            Self::TooManyConfigurations { assigned } => write!(
                f,
                "APM_SAMPLING carries {assigned} configurations; expected at most one."
            ),
            Self::NoValidConfiguration => f.write_str("The assigned APM_SAMPLING configuration is not valid."),
        }
    }
}

impl std::error::Error for SamplingError {}

impl ApplyError for SamplingError {
    fn apply_error(&self) -> String {
        self.to_string()
    }
}

/// Receives `APM_SAMPLING` updates for a trace sampler.
///
/// Create this from the remote configuration client and pass it to
/// [`TraceSamplerConfiguration::with_remote_sampling`]. The sampler applies accepted updates
/// before processing each buffer of traces. It keeps its current settings if an update is rejected
/// or no configuration is assigned. Dropping the last subscription (including copies held by
/// samplers) unsubscribes from the product.
///
/// [`TraceSamplerConfiguration::with_remote_sampling`]: super::TraceSamplerConfiguration::with_remote_sampling
#[derive(Debug)]
pub struct TraceSamplingSubscription {
    pub(super) subscription: Subscription<RemoteSampling, SamplingError>,
}

impl TraceSamplingSubscription {
    /// Subscribes to `APM_SAMPLING` on `client`.
    ///
    /// # Errors
    ///
    /// Returns an error if `client` already has a live subscription to `APM_SAMPLING`.
    pub fn new(client: &RemoteConfigurationClient) -> Result<Self, datadog_agent_remote_config::Error> {
        Ok(Self {
            subscription: client.subscribe::<SamplingDecoder>()?,
        })
    }
}

/// Accepts at most one `APM_SAMPLING` configuration per update.
///
/// No assignment produces [`RemoteSampling::Unassigned`]. An invalid or multiple-configuration
/// assignment is rejected, leaving subscribers with the last accepted update.
#[derive(Default)]
pub(crate) struct SamplingDecoder {
    assigned: usize,
    chosen: Option<(ConfigId, SamplerConfig)>,
}

impl ProductDecoder for SamplingDecoder {
    const PRODUCT: &'static str = "APM_SAMPLING";

    type Snapshot = RemoteSampling;
    type Error = SamplingError;

    fn decode(&mut self, id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
        // Count before parsing so invalid configurations still count toward the one-config limit.
        self.assigned += 1;
        // The Datadog Agent treats a `null` payload as a configuration with no fields set.
        let config = decode_json::<Option<SamplerConfig>>(payload)
            .map_err(SamplingError::InvalidPayload)?
            .unwrap_or_default();
        self.chosen = Some((id.clone(), config));
        Ok(())
    }

    fn build(self) -> Result<Self::Snapshot, Self::Error> {
        if self.assigned > 1 {
            // The client reports this rejection.
            return Err(SamplingError::TooManyConfigurations {
                assigned: self.assigned,
            });
        }
        match self.chosen {
            Some((id, config)) => Ok(RemoteSampling::Remote { id, config }),
            None if self.assigned == 0 => Ok(RemoteSampling::Unassigned),
            None => Err(SamplingError::NoValidConfiguration),
        }
    }
}

#[cfg(test)]
mod tests {
    use datadog_agent_remote_config::TestPublisher;

    use super::*;

    const STATIC: SamplerSettings = SamplerSettings {
        target_traces_per_second: 41.0,
        errors_per_second: 41.0,
        rare_sampler_enabled: true,
    };

    fn decode_all(assignment: &[(&str, &str)]) -> (Vec<Result<(), String>>, Result<RemoteSampling, String>) {
        let mut decoder = SamplingDecoder::default();
        let verdicts = assignment
            .iter()
            .map(|(id, payload)| {
                decoder
                    .decode(&ConfigId::new(*id), payload.as_bytes())
                    .map_err(|e| e.apply_error())
            })
            .collect();
        (verdicts, decoder.build().map_err(|e| e.apply_error()))
    }

    fn parse(json: &str) -> SamplerConfig {
        decode_json(json.as_bytes()).expect("payload should decode")
    }

    fn env_config(priority: Option<f64>, errors: Option<f64>, rare: Option<bool>) -> SamplerEnvConfig {
        SamplerEnvConfig {
            priority_sampler_target_tps: priority,
            errors_sampler_target_tps: errors,
            rare_sampler_enabled: rare,
        }
    }

    #[test]
    fn payload_uses_the_agent_field_names() {
        let config = parse(
            r#"{"all_envs":{"priority_sampler_target_TPS":1.5,"errors_sampler_target_TPS":2.5,
                "rare_sampler_enabled":true},
                "by_env":[{"env":"prod","config":{"priority_sampler_target_TPS":3}}]}"#,
        );
        assert_eq!(config.all_envs, env_config(Some(1.5), Some(2.5), Some(true)));
        assert_eq!(
            config.by_env,
            [EnvAndConfig {
                env: "prod".to_owned(),
                config: env_config(Some(3.0), None, None)
            }]
        );
    }

    #[test]
    fn payload_accepts_lowercase_tps_field_names() {
        let config = parse(r#"{"all_envs":{"priority_sampler_target_tps":1,"errors_sampler_target_tps":2}}"#);
        assert_eq!(config.all_envs, env_config(Some(1.0), Some(2.0), None));
    }

    #[test]
    fn payload_treats_null_and_absent_fields_as_unset() {
        assert_eq!(parse(r#"{"all_envs":null,"by_env":null}"#), SamplerConfig::default());
        assert_eq!(
            parse(r#"{"by_env":[{"env":"prod","config":null}]}"#).by_env,
            [EnvAndConfig {
                env: "prod".to_owned(),
                config: SamplerEnvConfig::default()
            }]
        );
        assert_eq!(
            parse(
                r#"{"all_envs":{"priority_sampler_target_TPS":null,"errors_sampler_target_TPS":null,
                    "rare_sampler_enabled":null}}"#
            ),
            SamplerConfig::default()
        );
        assert_eq!(parse("{}"), SamplerConfig::default());
    }

    #[test]
    fn payload_honors_explicit_zero_and_false() {
        let config = parse(
            r#"{"all_envs":{"priority_sampler_target_TPS":0,"errors_sampler_target_TPS":0,
                "rare_sampler_enabled":false}}"#,
        );
        assert_eq!(config.all_envs, env_config(Some(0.0), Some(0.0), Some(false)));
        assert_eq!(
            config.resolve("", &STATIC),
            SamplerSettings {
                target_traces_per_second: 0.0,
                errors_per_second: 0.0,
                rare_sampler_enabled: false,
            }
        );
    }

    #[test]
    fn payload_ignores_unknown_fields() {
        let config = parse(r#"{"future":1,"all_envs":{"rare_sampler_enabled":true,"other":"x"}}"#);
        assert_eq!(config.all_envs, env_config(None, None, Some(true)));
    }

    // Based on the Datadog Agent's `TestEnvPrecedence`, with additional fallback cases:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/remoteconfighandler/remote_config_handler_test.go#L289-L322
    #[test]
    fn resolve_prefers_env_then_all_envs_then_static() {
        let all_envs = env_config(Some(42.0), Some(42.0), Some(true));
        let for_env = |env: &str, config: SamplerEnvConfig| EnvAndConfig {
            env: env.to_owned(),
            config,
        };
        let settings = |priority, errors, rare| SamplerSettings {
            target_traces_per_second: priority,
            errors_per_second: errors,
            rare_sampler_enabled: rare,
        };

        let cases = [
            (
                "env entry beats all_envs",
                all_envs.clone(),
                vec![for_env("agent-env", env_config(Some(43.0), Some(43.0), Some(false)))],
                settings(43.0, 43.0, false),
            ),
            (
                "all_envs applies without an env entry",
                all_envs.clone(),
                vec![for_env("other-env", env_config(Some(43.0), Some(43.0), Some(false)))],
                settings(42.0, 42.0, true),
            ),
            (
                "static applies when neither sets a field",
                SamplerEnvConfig::default(),
                vec![],
                STATIC,
            ),
            (
                "a field the env entry leaves unset falls back to all_envs",
                all_envs.clone(),
                vec![for_env("agent-env", env_config(Some(43.0), None, None))],
                settings(43.0, 42.0, true),
            ),
            (
                "a field neither sets falls back to static per field",
                env_config(None, Some(42.0), None),
                vec![for_env("agent-env", env_config(Some(43.0), None, Some(false)))],
                settings(43.0, 42.0, false),
            ),
            (
                "the last matching env entry wins",
                SamplerEnvConfig::default(),
                vec![
                    for_env("agent-env", env_config(Some(43.0), Some(43.0), Some(false))),
                    for_env("agent-env", env_config(Some(44.0), None, None)),
                ],
                settings(44.0, 41.0, true),
            ),
        ];

        for (name, all_envs, by_env, expected) in cases {
            let config = SamplerConfig { all_envs, by_env };
            assert_eq!(config.resolve("agent-env", &STATIC), expected, "{name}");
        }
    }

    #[test]
    fn resolve_compares_the_payload_env_as_given() {
        let config = SamplerConfig {
            all_envs: SamplerEnvConfig::default(),
            by_env: vec![EnvAndConfig {
                env: "Agent-Env".to_owned(),
                config: env_config(Some(43.0), None, None),
            }],
        };
        assert_eq!(config.resolve("agent-env", &STATIC).target_traces_per_second, 41.0);
        assert_eq!(config.resolve("Agent-Env", &STATIC).target_traces_per_second, 43.0);
    }

    #[test]
    fn empty_assignment_is_unassigned() {
        assert_eq!(decode_all(&[]).1, Ok(RemoteSampling::Unassigned));
    }

    #[test]
    fn single_valid_configuration_is_chosen() {
        let (verdicts, snapshot) = decode_all(&[("a", r#"{"all_envs":{"rare_sampler_enabled":false}}"#)]);
        assert_eq!(verdicts, [Ok(())]);
        assert_eq!(
            snapshot,
            Ok(RemoteSampling::Remote {
                id: ConfigId::new("a"),
                config: SamplerConfig {
                    all_envs: env_config(None, None, Some(false)),
                    by_env: vec![],
                },
            })
        );
    }

    #[test]
    fn null_payload_is_a_configuration_with_nothing_set() {
        let (verdicts, snapshot) = decode_all(&[("a", "null")]);
        assert_eq!(verdicts, [Ok(())]);
        assert_eq!(
            snapshot,
            Ok(RemoteSampling::Remote {
                id: ConfigId::new("a"),
                config: SamplerConfig::default(),
            })
        );
    }

    #[test]
    fn null_env_is_the_empty_env() {
        let (_, snapshot) = decode_all(&[(
            "a",
            r#"{"by_env":[{"env":null,"config":{"priority_sampler_target_TPS":3}}]}"#,
        )]);
        let Ok(RemoteSampling::Remote { config, .. }) = snapshot else {
            panic!("the configuration should be chosen: {snapshot:?}");
        };
        assert_eq!(
            config.by_env,
            [EnvAndConfig {
                env: String::new(),
                config: env_config(Some(3.0), None, None)
            }]
        );
    }

    #[test]
    fn null_by_env_element_is_an_empty_entry() {
        let (_, snapshot) = decode_all(&[(
            "a",
            r#"{"by_env":[null,{"env":"prod","config":{"priority_sampler_target_TPS":3}}]}"#,
        )]);
        let Ok(RemoteSampling::Remote { config, .. }) = snapshot else {
            panic!("the configuration should be chosen: {snapshot:?}");
        };
        assert_eq!(
            config.by_env,
            [
                EnvAndConfig::default(),
                EnvAndConfig {
                    env: "prod".to_owned(),
                    config: env_config(Some(3.0), None, None)
                }
            ]
        );
    }

    #[test]
    fn invalid_payload_error_gives_only_its_position() {
        let (verdicts, snapshot) = decode_all(&[("a", r#"{"all_envs":{"rare_sampler_enabled":"secret"}}"#)]);
        assert_eq!(
            verdicts,
            [Err("Payload is malformed JSON at line 1, column 44.".to_owned())]
        );
        assert_eq!(
            snapshot,
            Err("The assigned APM_SAMPLING configuration is not valid.".to_owned())
        );
    }

    #[test]
    fn more_than_one_configuration_is_rejected_with_the_count() {
        let (verdicts, snapshot) = decode_all(&[("a", "{}"), ("b", "not json"), ("c", "{}")]);
        assert_eq!(
            verdicts,
            [
                Ok(()),
                Err("Payload is malformed JSON at line 1, column 2.".to_owned()),
                Ok(())
            ]
        );
        assert_eq!(
            snapshot,
            Err("APM_SAMPLING carries 3 configurations; expected at most one.".to_owned())
        );
    }

    #[test]
    fn configurations_that_fail_to_decode_count_toward_the_limit() {
        let (_, snapshot) = decode_all(&[("a", "not json"), ("b", "{}")]);
        assert_eq!(
            snapshot,
            Err("APM_SAMPLING carries 2 configurations; expected at most one.".to_owned())
        );
    }

    #[tokio::test]
    async fn assign_publishes_unassigned_after_a_remote_configuration() {
        let (publisher, mut subscription) = TestPublisher::<RemoteSampling, SamplingError>::new();
        publisher.assign::<SamplingDecoder>([("a", "{}")]);
        assert!(matches!(
            *subscription
                .changed()
                .await
                .expect("one configuration should be accepted"),
            RemoteSampling::Remote { .. }
        ));

        publisher.assign::<SamplingDecoder>(std::iter::empty::<(&str, &str)>());
        assert_eq!(
            *subscription
                .changed()
                .await
                .expect("an empty assignment should be accepted"),
            RemoteSampling::Unassigned
        );
    }
}
