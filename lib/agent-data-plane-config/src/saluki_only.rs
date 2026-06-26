//! [`SalukiOnlyConfiguration`]: represents Saluki-schema-only source input.
//!
//! Saluki-schema-only keys are those the Datadog Agent schema does not know about. Their authority
//! is Saluki's own model. The purpose of this struct is to provide an input mechanism (whether
//! deserialization or otherwise) for configuration values that cannot arrive through Datadog
//! configuration.
//!
//! [`SalukiOnlyConfiguration::seed`] produces a base [`SalukiConfiguration`]: it starts from
//! `SalukiConfiguration::default()` and assigns each Saluki-only field into its component-native
//! destination. The Datadog `drive` later overlays its Datadog schema fields on top of this base; a
//! Saluki-only field and a Datadog-schema field should not target the same destination.
//!
//! Models the DogStatsD source knobs in the Saluki-only configuration schema.
// TODO: add remaining subsystem groups as their native destinations land.

use bytesize::ByteSize;

use crate::model::SalukiConfiguration;

/// The Saluki-schema-only configuration, grouped by subsystem.
///
/// Every field is `#[serde(default)]`: the Saluki source is frequently absent entirely (a deployment
/// may set no `SALUKI_*` keys), so each subsystem group must fall back to its defaults independently
/// when its keys are missing.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct SalukiOnlyConfiguration {
    /// DogStatsD source knobs.
    pub dogstatsd: DogStatsDSalukiOnly,
}

/// DogStatsD source Saluki-schema-only knobs.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct DogStatsDSalukiOnly {
    /// Whether to allow heap allocations for contexts (`dogstatsd_allow_context_heap_allocs`).
    pub allow_context_heap_allocs: Option<bool>,

    /// Whether to bind multiple UDP sockets via `SO_REUSEPORT` (`dogstatsd_autoscale_udp_listeners`).
    pub autoscale_udp_listeners: Option<bool>,

    /// Number of receive buffers (`dogstatsd_buffer_count`).
    pub buffer_count: Option<usize>,

    /// Max receive buffers (`dogstatsd_buffer_count_max`).
    pub buffer_count_max: Option<usize>,

    /// Max cached metric contexts (`dogstatsd_cached_contexts_limit`).
    pub cached_contexts_limit: Option<usize>,

    /// Max cached tagsets (`dogstatsd_cached_tagsets_limit`).
    pub cached_tagsets_limit: Option<usize>,

    /// Floor for metric sample rates (`dogstatsd_minimum_sample_rate`).
    pub minimum_sample_rate: Option<f64>,

    /// Whether to relax decoder strictness (`dogstatsd_permissive_decoding`).
    pub permissive_decoding: Option<bool>,

    /// Explicit byte budget for the context interner (`dogstatsd_string_interner_size_bytes`).
    pub string_interner_size_bytes: Option<u64>,

    /// TCP listen port for DogStatsD (`dogstatsd_tcp_port`).
    pub tcp_port: Option<u16>,
}

impl SalukiOnlyConfiguration {
    /// Produces a base [`SalukiConfiguration`] seeded with defaults and Saluki-schema-only values.
    ///
    /// Starts from `SalukiConfiguration::default()` and assigns each present Saluki-only value into
    /// its component-native destination. Absent values leave the default in place. The Datadog
    /// `drive` later overlays its disjoint schema fields on top of this base.
    pub fn seed(&self) -> SalukiConfiguration {
        let mut c = SalukiConfiguration::default();

        let dsd = &mut c.components.dogstatsd.source;
        if let Some(v) = self.dogstatsd.allow_context_heap_allocs {
            dsd.allow_context_heap_allocations = v;
        }
        if let Some(v) = self.dogstatsd.autoscale_udp_listeners {
            dsd.autoscale_udp_listeners = v;
        }
        if let Some(v) = self.dogstatsd.buffer_count {
            dsd.buffer_count = v;
        }
        if let Some(v) = self.dogstatsd.buffer_count_max {
            dsd.buffer_count_max = v;
        }
        if let Some(v) = self.dogstatsd.cached_contexts_limit {
            dsd.cached_contexts_limit = v;
        }
        if let Some(v) = self.dogstatsd.cached_tagsets_limit {
            dsd.cached_tagsets_limit = v;
        }
        if let Some(v) = self.dogstatsd.minimum_sample_rate {
            dsd.minimum_sample_rate = v;
        }
        if let Some(v) = self.dogstatsd.permissive_decoding {
            dsd.permissive_decoding = v;
        }
        if let Some(v) = self.dogstatsd.string_interner_size_bytes {
            dsd.context_string_interner_size_bytes = Some(ByteSize(v));
        }
        if let Some(v) = self.dogstatsd.tcp_port {
            dsd.tcp_port = v;
        }

        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_seed_equals_default() {
        let seeded = SalukiOnlyConfiguration::default().seed();
        assert_eq!(seeded, SalukiConfiguration::default());
    }

    #[test]
    fn seed_assigns_saluki_only_fields() {
        let mut only = SalukiOnlyConfiguration::default();
        only.dogstatsd.tcp_port = Some(9000);
        only.dogstatsd.cached_contexts_limit = Some(42);
        only.dogstatsd.string_interner_size_bytes = Some(1024);

        let seeded = only.seed();

        assert_eq!(seeded.components.dogstatsd.source.tcp_port, 9000);
        assert_eq!(seeded.components.dogstatsd.source.cached_contexts_limit, 42);
        assert_eq!(
            seeded.components.dogstatsd.source.context_string_interner_size_bytes,
            Some(ByteSize(1024))
        );
    }

    #[test]
    fn saluki_configuration_serializes() {
        // The `/config/internal` view relies on `SalukiConfiguration: Serialize`.
        let json = serde_json::to_string(&SalukiConfiguration::default()).expect("serialize");
        assert!(json.contains("control"));
        assert!(json.contains("components"));
    }
}
