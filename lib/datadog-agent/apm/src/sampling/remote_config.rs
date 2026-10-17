//! Loads trace sampling settings from the `APM_SAMPLING` Remote Configuration product.
//!
//! The product can change the priority and error samplers' target traces per second and turn rare sampling on or off.
//! A configuration may set values for every environment or override individual fields for a specific environment.
//! Unset fields use the consumer's local settings. This follows the Datadog Agent's
//! `pkg/trace/remoteconfighandler/remote_config_handler.go`.

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use datadog_agent_remote_config::{
    decode_json, ApplyError, ConfigId, JsonError, ProductDecoder, RemoteConfigurationClient, Subscription,
};
use serde::de::value::MapAccessDeserializer;
use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// Optional sampling settings for all environments or one named environment.
///
/// Missing and `null` fields leave that setting unset; `0` and `false` are explicit values.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
struct SamplerEnvConfig {
    /// Target traces per second for the priority sampler.
    #[serde(
        default,
        rename = "priority_sampler_target_TPS",
        alias = "priority_sampler_target_tps"
    )]
    priority_sampler_target_tps: Option<f64>,

    /// Target traces per second for the errors sampler.
    #[serde(default, rename = "errors_sampler_target_TPS", alias = "errors_sampler_target_tps")]
    errors_sampler_target_tps: Option<f64>,

    /// Whether the rare sampler is enabled.
    #[serde(default)]
    rare_sampler_enabled: Option<bool>,
}

/// An environment name and its sampling overrides.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
struct EnvAndConfig {
    /// Environment to match against the normalized local default; the payload name is used as given.
    #[serde(default, deserialize_with = "null_as_default")]
    env: String,

    /// The settings for `env`.
    #[serde(default, deserialize_with = "null_or_object")]
    config: SamplerEnvConfig,
}

/// Settings supplied by one `APM_SAMPLING` configuration.
///
/// The payload and each settings value in it must be a JSON object or `null`, as the Datadog Agent's JSON decoder
/// requires; serde's derived visitors would also accept an array of field values, such as `[]`.
///
/// Call [`resolve`](Self::resolve) to combine the configuration with local settings for one environment.
///
/// ```json
/// {
///   "all_envs": { "priority_sampler_target_TPS": 10, "errors_sampler_target_TPS": 5, "rare_sampler_enabled": true },
///   "by_env": [{ "env": "prod", "config": { "rare_sampler_enabled": false } }]
/// }
/// ```
///
/// Missing and `null` fields fall back through `all_envs` to local settings; `0` and `false` are explicit values. A
/// payload that sets nothing, such as `{}` or `null`, is still an assigned configuration
/// ([`RemoteSampling::Remote`]) and resolves entirely to local settings, not to previous remote values. This differs
/// from an empty assignment, which leaves the last accepted configuration in place.
///
/// The Datadog Agent sends the `*_TPS` field names. Lowercase `*_tps` names also work;
/// unknown fields are ignored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SamplerConfig {
    /// Settings for every environment.
    all_envs: SamplerEnvConfig,

    /// Settings for individual environments.
    by_env: Vec<EnvAndConfig>,
}

impl<'de> Deserialize<'de> for SamplerConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(SamplerConfigVisitor)
    }
}

/// Reads [`SamplerConfig`] from a JSON object only.
struct SamplerConfigVisitor;

impl<'de> Visitor<'de> for SamplerConfigVisitor {
    type Value = SamplerConfig;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a sampling configuration object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            AllEnvs,
            ByEnv,
            #[serde(other)]
            Unknown,
        }

        let mut all_envs = None;
        let mut by_env = None;
        while let Some(field) = map.next_key()? {
            match field {
                Field::AllEnvs if all_envs.is_some() => return Err(de::Error::duplicate_field("all_envs")),
                Field::AllEnvs => {
                    all_envs = Some(
                        map.next_value::<Option<Object<SamplerEnvConfig>>>()?
                            .unwrap_or_default()
                            .0,
                    )
                }
                Field::ByEnv if by_env.is_some() => return Err(de::Error::duplicate_field("by_env")),
                Field::ByEnv => {
                    // A `null` list is empty and a `null` entry is an empty entry, as in the Datadog Agent.
                    let entries = map.next_value::<Option<Vec<Option<Object<EnvAndConfig>>>>>()?;
                    by_env = Some(
                        entries
                            .unwrap_or_default()
                            .into_iter()
                            .map(|entry| entry.unwrap_or_default().0)
                            .collect(),
                    );
                }
                Field::Unknown => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(SamplerConfig {
            all_envs: all_envs.unwrap_or_default(),
            by_env: by_env.unwrap_or_default(),
        })
    }
}

/// Deserializes `T` from a JSON object only, rejecting the array form that serde's derived visitors also accept.
///
/// `T`'s own `Deserialize` implementation reads the object's fields, so field names, aliases, and defaults still come
/// from its derive.
#[derive(Default)]
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = T;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map))
            }
        }

        deserializer.deserialize_map(ObjectVisitor(PhantomData)).map(Self)
    }
}

/// Treats `null` like a missing field, as the Datadog Agent's JSON decoder does.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Treats `null` like a missing field and requires any other value to be an object.
fn null_or_object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(null_as_default::<D, Object<T>>(deserializer)?.0)
}

/// Effective sampling settings, either from local configuration or resolved remote overrides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SamplerSettings {
    /// Target traces per second for the priority sampler.
    pub target_traces_per_second: f64,

    /// Target traces per second for the errors sampler.
    pub errors_per_second: f64,

    /// Whether the rare sampler is enabled.
    pub rare_sampler_enabled: bool,
}

impl SamplerConfig {
    /// Chooses settings for the local default environment.
    ///
    /// For each field, uses the last matching `by_env` entry, then `all_envs`, then `static_settings`. Fields are
    /// chosen independently, so one field can come from `by_env` while another comes from `all_envs`; earlier
    /// matching `by_env` entries are ignored once a later one matches.
    ///
    /// The caller normalizes `default_env` as a tag value; names in the remote payload are compared as given. A
    /// payload name such as `Prod` does not match a normalized `prod`.
    pub fn resolve(&self, default_env: &str, static_settings: &SamplerSettings) -> SamplerSettings {
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

/// The latest accepted `APM_SAMPLING` state.
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteSampling {
    /// An empty assignment accepted before any concrete configuration; use local sampling settings.
    ///
    /// After accepting a concrete configuration, the subscription retains it on withdrawal rather than publishing
    /// this marker.
    Unassigned,

    /// The last accepted concrete sampling configuration and its ID.
    ///
    /// Retained after withdrawal, it may no longer be remotely assigned.
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
pub(super) enum SamplingError {
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

/// Watches the latest accepted remote trace sampling configuration.
///
/// Create with [`new`](Self::new) and read [`current`](Self::current) before processing a batch of traces. This is a
/// latest-state watch, not a queue: successive concrete configurations can coalesce before you read them.
///
/// Once accepted, a concrete configuration remains available across withdrawal, cache expiry, polling-worker restart,
/// and rejected updates. An initially accepted empty assignment publishes [`RemoteSampling::Unassigned`], for which
/// the consumer uses local settings. Later withdrawals never replace an accepted concrete configuration with that
/// marker.
///
/// The default subscription is inert: it receives no updates and contacts no service. Clones share a registration;
/// dropping the last clone unsubscribes.
#[derive(Clone, Debug)]
pub struct TraceSamplingSubscription {
    subscription: Subscription<RemoteSampling, SamplingError>,
}

impl TraceSamplingSubscription {
    /// Subscribes to `APM_SAMPLING` on `client`.
    ///
    /// # Errors
    ///
    /// Returns an error if `client` already has a live subscription to `APM_SAMPLING`.
    pub fn new(client: &RemoteConfigurationClient) -> Result<Self, datadog_agent_remote_config::Error> {
        Ok(Self::from_subscription(client.subscribe::<SamplingDecoder>()?))
    }

    /// Wraps a subscription to the sampling decoder.
    pub(super) fn from_subscription(subscription: Subscription<RemoteSampling, SamplingError>) -> Self {
        Self { subscription }
    }

    /// Returns the current accepted snapshot immediately.
    ///
    /// Returns `None` until the first accepted snapshot, not merely the first poll: polling may fail or reject an
    /// update. An initially accepted empty assignment publishes [`RemoteSampling::Unassigned`].
    ///
    /// Each newly accepted concrete configuration gets a new `Arc`, even if its values are unchanged. Later withdrawals
    /// and rejected updates preserve the current `Arc`; use [`Arc::ptr_eq`] to compare snapshot identity.
    pub fn current(&self) -> Option<Arc<RemoteSampling>> {
        self.subscription.current()
    }
}

impl Default for TraceSamplingSubscription {
    fn default() -> Self {
        Self::from_subscription(Subscription::inert())
    }
}

/// Accepts at most one `APM_SAMPLING` configuration per update.
///
/// No assignment produces [`RemoteSampling::Unassigned`], which the client publishes only if nothing was accepted
/// before; see [`keeps_previous`](ProductDecoder::keeps_previous). An invalid or multiple-configuration assignment is
/// rejected, leaving subscribers with the last accepted update.
#[derive(Default)]
pub(super) struct SamplingDecoder {
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

    fn keeps_previous(snapshot: &Self::Snapshot) -> bool {
        matches!(snapshot, RemoteSampling::Unassigned)
    }
}

#[cfg(test)]
mod tests {
    use datadog_agent_remote_config::TestPublisher;

    use super::*;
    use crate::sampling::test_util::TestSamplingPublisher;

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

    #[test]
    fn default_subscription_never_has_an_update() {
        assert_eq!(TraceSamplingSubscription::default().current(), None);
    }

    #[test]
    fn new_subscribes_to_apm_sampling_until_the_last_clone_drops() {
        let client = RemoteConfigurationClient::without_agent();

        let subscription = TraceSamplingSubscription::new(&client).expect("the product is not yet subscribed");
        assert_eq!(client.subscribed_products(), ["APM_SAMPLING"]);
        assert!(matches!(
            TraceSamplingSubscription::new(&client),
            Err(datadog_agent_remote_config::Error::AlreadySubscribed { product }) if product == "APM_SAMPLING"
        ));

        // A surviving clone keeps the registration alive.
        let clone = subscription.clone();
        drop(subscription);
        assert!(TraceSamplingSubscription::new(&client).is_err());

        drop(clone);
        assert!(client.subscribed_products().is_empty());
        let _resubscribed = TraceSamplingSubscription::new(&client).expect("the product is free again");
        assert_eq!(client.subscribed_products(), ["APM_SAMPLING"]);
    }

    #[test]
    fn subscription_has_no_update_before_one_is_accepted() {
        let (_publisher, subscription) = TestSamplingPublisher::new();
        assert_eq!(subscription.current(), None);
    }

    #[test]
    fn clones_observe_accepted_updates() {
        let (publisher, subscription) = TestSamplingPublisher::new();
        let clone = subscription.clone();

        publisher.assign([("a", r#"{"all_envs":{"rare_sampler_enabled":true}}"#)]);
        let first = subscription.current().expect("a configuration is accepted");
        assert!(matches!(&*first, RemoteSampling::Remote { id, .. } if id == &ConfigId::new("a")));
        assert!(Arc::ptr_eq(
            &first,
            &clone.current().expect("clones see the same update")
        ));
    }

    // The Datadog Agent decodes into structs with `json.Unmarshal`, which rejects an array where an object belongs:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/remoteconfig/state/products/apmsampling/sampler_config.go
    // Serde's derived visitors accept one, so `[]` would decode as a configuration with nothing set and
    // `{"all_envs":[0,0,false]}` would set both targets to zero and turn off the rare sampler.
    #[test]
    fn payload_rejects_non_objects_where_the_agent_requires_an_object() {
        let invalid = [
            "[]",
            "[null,null]",
            "5",
            r#""text""#,
            "true",
            r#"{"all_envs":[0,0,false]}"#,
            r#"{"all_envs":[]}"#,
            r#"{"all_envs":5}"#,
            r#"{"by_env":{}}"#,
            r#"{"by_env":[[]]}"#,
            r#"{"by_env":[["prod",{}]]}"#,
            r#"{"by_env":[5]}"#,
            r#"{"by_env":[{"env":"prod","config":[0,0,false]}]}"#,
            r#"{"by_env":[{"env":"prod","config":"x"}]}"#,
            r#"{"by_env":[{"env":5}]}"#,
        ];
        for payload in invalid {
            let (verdicts, snapshot) = decode_all(&[("a", payload)]);
            assert!(
                matches!(&verdicts[..], [Err(reason)] if reason.starts_with("Payload is malformed JSON at line 1")),
                "{payload}: {verdicts:?}"
            );
            assert!(snapshot.is_err(), "{payload}: {snapshot:?}");
        }

        let valid = [
            "null",
            "{}",
            r#"{"all_envs":null,"by_env":null}"#,
            r#"{"all_envs":{}}"#,
            r#"{"by_env":[]}"#,
            r#"{"by_env":[null,{},{"env":null,"config":null},{"env":"other","config":{}}]}"#,
        ];
        for payload in valid {
            let (verdicts, snapshot) = decode_all(&[("a", payload)]);
            assert_eq!(verdicts, [Ok(())], "{payload}");
            let Ok(RemoteSampling::Remote { config, .. }) = snapshot else {
                panic!("{payload} should be chosen: {snapshot:?}");
            };
            assert_eq!(
                config.resolve("prod", &STATIC),
                STATIC,
                "{payload} sets nothing for `prod`"
            );
        }
    }

    #[test]
    fn payload_rejects_a_duplicate_field() {
        let (verdicts, _) = decode_all(&[("a", r#"{"by_env":[],"by_env":[]}"#)]);
        assert!(matches!(&verdicts[..], [Err(_)]), "{verdicts:?}");
    }

    // A latest-state subscription coalesces publications, so an empty assignment published before the consumer reads
    // must not replace a configuration it has not applied. The Datadog Agent applies each update as it arrives and
    // ignores an empty one:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/remoteconfighandler/remote_config_handler.go#L234-L295
    #[test]
    fn withdrawal_and_rejection_keep_the_last_accepted_configuration() {
        let empty = std::iter::empty::<(&str, &str)>;
        let current_id = |subscription: &TraceSamplingSubscription| match &*subscription.current().expect("accepted") {
            RemoteSampling::Remote { id, .. } => Some(id.clone()),
            RemoteSampling::Unassigned => None,
        };
        let (publisher, subscription) = TestSamplingPublisher::new();

        publisher.assign(empty());
        assert_eq!(current_id(&subscription), None, "nothing has been assigned yet");

        publisher.assign([("a", "{}")]);
        let accepted = subscription.current().expect("a configuration is accepted");
        for (name, assignment) in [
            ("an empty assignment", vec![]),
            ("an array payload", vec![("b", "[]")]),
            ("an array settings value", vec![("b", r#"{"all_envs":[0,0,false]}"#)]),
            ("more than one configuration", vec![("b", "{}"), ("c", "{}")]),
        ] {
            publisher.assign(assignment);
            assert!(
                Arc::ptr_eq(&accepted, &subscription.current().expect("still accepted")),
                "{name} keeps the last accepted configuration"
            );
        }

        publisher.assign([("b", "{}")]);
        publisher.assign(empty());
        assert_eq!(
            current_id(&subscription),
            Some(ConfigId::new("b")),
            "the newest configuration is kept"
        );
    }

    #[test]
    fn rejected_assignment_keeps_the_last_update() {
        let (publisher, subscription) = TestSamplingPublisher::new();
        publisher.assign([("a", "{}")]);
        let accepted = subscription.current().expect("a configuration is accepted");

        publisher.assign([("a", "{}"), ("b", "{}")]);
        assert!(Arc::ptr_eq(
            &accepted,
            &subscription.current().expect("the earlier update is retained")
        ));
    }

    #[tokio::test]
    async fn an_empty_assignment_after_a_rejection_clears_it_and_notifies_with_the_last_configuration() {
        let (publisher, mut subscription) = TestPublisher::<RemoteSampling, SamplingError>::new();
        publisher.assign::<SamplingDecoder>([("a", "{}")]);
        let accepted = subscription
            .changed()
            .await
            .expect("one configuration should be accepted");

        publisher.assign::<SamplingDecoder>([("a", "[]")]);
        assert!(subscription.changed().await.is_err(), "the array payload is rejected");

        publisher.assign::<SamplingDecoder>(std::iter::empty::<(&str, &str)>());
        let kept = subscription.changed().await.expect("the empty assignment is accepted");
        assert!(Arc::ptr_eq(&accepted, &kept));
    }
}
