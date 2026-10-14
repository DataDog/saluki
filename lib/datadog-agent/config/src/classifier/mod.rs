//! Configuration key classifier.
//!
//! Looks up the Datadog keys whose value can warrant a compatibility warning at runtime: `partial`
//! keys marked `warn`, `none` keys, and `unknown` keys with a `severity`. Each entry carries the
//! key's canonical YAML path, support level, affected pipelines and schema default, so a caller can
//! tell whether a configured value differs from the default. Fully supported and excluded keys have
//! no entry.
//!
//! The entries are generated at build time from the `inventory` section of
//! `lib/datadog-agent/config/schema/schema_overlay.yaml`. `SchemaOverlay::load()` fails the build
//! if a key in the vendored schema is missing from the overlay.

#[allow(clippy::module_inception)]
mod classifier;

pub use classifier::{Classification, ConfigClassifier};

/// The ADP pipeline a config key affects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pipeline {
    /// DogStatsD metrics pipeline.
    DogStatsD,
    /// Agent checks pipeline.
    Checks,
    /// OTLP ingestion frontend.
    Otlp,
    /// Internal trace processing. Active when OTLP is enabled and proxy/relay mode (which uses the
    /// core Agent for transport) is off.
    Traces,
}

/// Which pipelines a config key affects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineAffinity {
    /// The list of pipelines affected by the key.
    ///
    /// This list must be non-empty, enforced by test.
    Pipelines(&'static [Pipeline]),
    /// The key affects all pipelines or ADP behavior as a whole.
    CrossCutting,
}

/// The `Severity` level of a config key that Saluki doesn't support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Saluki's incompatibility with the key is considered minor.
    Low,

    /// Saluki's incompatibility with the key is considered potentially impactful.
    Medium,

    /// Saluki's incompatibility with the key is considered problematic.
    High,
}

/// The support level for a given configuration key.
///
/// Full support is omitted from the enum and those keys are not classified since there is nothing
/// to be done about them downstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportLevel {
    /// Partially supported.
    Partial,
    /// Explicitly incompatible.
    Incompatible(Severity),
    /// Intentionally ignored.
    #[allow(unused)]
    Ignored,
    /// Unrecognized.
    #[allow(unused)]
    Unrecognized,
}

/// The default value for a config key, as resolved at build time from the Agent schema.
///
/// Durations are normalized to nanoseconds during codegen (the build fails if a `format:
/// duration` default isn't a valid Go duration), so the runtime default check never has to parse a
/// schema default. Other keys keep their JSON-literal default and are compared structurally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultValue {
    /// The schema declares no default for this key.
    Missing,
    /// A JSON-encoded default value (for example, `"\"tlsv1.2\""` or `"1"`).
    Json(&'static str),
    /// A `format: duration` default, already parsed to nanoseconds. The Agent transmits durations
    /// as integer nanoseconds, so the incoming value is normalized the same way before comparing.
    DurationNanos(u64),
}

/// Slim per-key data generated at build time for the classifier.
///
/// Carries only what the classifier needs: enough to look up a key, determine its support level,
/// check whether a value is the default, and report which pipelines are affected.
pub struct ClassifierEntry {
    /// Canonical dot-separated YAML path.
    pub yaml_path: &'static str,
    /// Additional YAML paths (aliases) that resolve to this key.
    pub aliases: &'static [&'static str],
    /// How well saluki supports this key.
    pub support_level: SupportLevel,
    /// Which pipelines this key affects.
    pub pipeline_affinity: PipelineAffinity,
    /// Default value from the Agent schema (normalized at build time).
    pub default: DefaultValue,
}

use crate::generated::classifier_data::CLASSIFIER_ENTRIES;
