//! Connects the trace pipeline to the core Agent's Remote Configuration service.
//!
//! The Agent supplies trace sampling settings (`APM_SAMPLING`) and mappings from trace concepts such
//! as HTTP status to attribute names (`APM_SEMANTIC_CORE_DD`). With a local trace pipeline, ADP
//! subscribes to enabled products through the remote agent runtime's Remote Configuration client,
//! which is only available in connected mode with Remote Configuration enabled. Without a
//! subscription, components use configured sampling settings and the mappings embedded in ADP.

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_apm::{sampling::TraceSamplingSubscription, semantics::SemanticRegistryProvider};
use datadog_agent_runtime::facade::{Initialized, RemoteAgent};
use saluki_error::GenericError;

use crate::{config::DataPlaneConfiguration, internal::AgentDataPlane};

/// The trace products ADP requests from the Agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RcProducts {
    /// Whether to request trace sampling settings.
    pub sampling: bool,

    /// Whether to request OTLP trace attribute mappings.
    pub semantics: bool,
}

impl RcProducts {
    /// Disables all products.
    pub(crate) const NONE: Self = Self {
        sampling: false,
        semantics: false,
    };

    /// Returns `true` if any product is enabled.
    pub(crate) const fn any(self) -> bool {
        self.sampling || self.semantics
    }
}

/// Chooses which trace products to subscribe to from `config`.
///
/// Subscriptions require a local trace pipeline and the corresponding product switch. Connected mode and the global
/// Remote Configuration switch are checked by the remote agent runtime.
pub(crate) fn enabled_products(config: &SalukiConfiguration) -> RcProducts {
    let dp = DataPlaneConfiguration::from_configuration(config);
    if !dp.traces_pipeline_required() {
        return RcProducts::NONE;
    }

    let rc = &config.domains.remote_configuration;
    RcProducts {
        sampling: rc.apm_sampling_enabled,
        semantics: rc.apm_semantics_enabled,
    }
}

/// Handles passed to trace components for Remote Configuration updates.
///
/// By default, the sampler uses its configured settings, and OTLP translation and APM stats use the attribute mappings
/// embedded in ADP. When subscribed, these components read updates from the same polling worker.
#[derive(Default)]
pub(crate) struct RemoteConfigSubscriptions {
    /// Sampling settings for the trace sampler, if `APM_SAMPLING` is enabled.
    pub trace_sampling: Option<TraceSamplingSubscription>,

    /// Attribute mappings shared by the OTLP source, OTLP decoder, and APM stats.
    pub semantic_registry: SemanticRegistryProvider,
}

impl RemoteConfigSubscriptions {
    /// Subscribes to the selected `products` through the remote agent's Remote Configuration client.
    ///
    /// With no products, returns default handles without asking for the client, so no connection is made. Default
    /// handles are also returned when the client isn't available, in standalone mode or with Remote Configuration
    /// turned off.
    ///
    /// # Errors
    ///
    /// Returns an error if the client can't connect to the Agent, or a subscription fails.
    pub(crate) async fn subscribe(
        products: RcProducts, agent: &mut RemoteAgent<AgentDataPlane, Initialized>,
    ) -> Result<Self, GenericError> {
        let mut subscriptions = Self::default();
        if !products.any() {
            return Ok(subscriptions);
        }
        let Some(client) = agent.remote_config_client().await? else {
            return Ok(subscriptions);
        };

        if products.sampling {
            subscriptions.trace_sampling = Some(TraceSamplingSubscription::new(&client)?);
        }
        if products.semantics {
            subscriptions.semantic_registry = SemanticRegistryProvider::subscribe(&client)?;
        }

        Ok(subscriptions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        sampling: bool,
        semantics: bool,
        otlp: bool,
        otlp_proxy: bool,
        otlp_proxy_traces: bool,
        expected: RcProducts,
    }

    const SAMPLING: RcProducts = RcProducts {
        sampling: true,
        semantics: false,
    };
    const SEMANTICS: RcProducts = RcProducts {
        sampling: false,
        semantics: true,
    };
    const BOTH: RcProducts = RcProducts {
        sampling: true,
        semantics: true,
    };

    // Baseline with both products and a local trace pipeline enabled.
    const ON: Case = Case {
        name: "",
        sampling: true,
        semantics: true,
        otlp: true,
        otlp_proxy: false,
        otlp_proxy_traces: false,
        expected: BOTH,
    };

    #[test]
    fn enabled_products_requires_a_product_key_and_the_traces_pipeline() {
        let cases = [
            Case { name: "all on", ..ON },
            Case {
                name: "sampling only",
                semantics: false,
                expected: SAMPLING,
                ..ON
            },
            Case {
                name: "semantics only",
                sampling: false,
                expected: SEMANTICS,
                ..ON
            },
            Case {
                name: "both product keys off",
                sampling: false,
                semantics: false,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "otlp off: no traces pipeline",
                otlp: false,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "otlp traces proxied: no traces pipeline",
                otlp_proxy: true,
                otlp_proxy_traces: true,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "otlp proxy with traces kept: traces pipeline",
                otlp_proxy: true,
                otlp_proxy_traces: false,
                ..ON
            },
        ];

        for case in cases {
            let mut config = SalukiConfiguration::default();
            config.domains.remote_configuration.apm_sampling_enabled = case.sampling;
            config.domains.remote_configuration.apm_semantics_enabled = case.semantics;
            config.control.otlp = case.otlp;
            config.domains.otlp.proxy.enabled = case.otlp_proxy;
            config.domains.otlp.proxy.traces_enabled = case.otlp_proxy_traces;

            let products = enabled_products(&config);
            assert_eq!(products, case.expected, "{}", case.name);
            assert_eq!(
                products.any(),
                case.expected != RcProducts::NONE,
                "{}: any()",
                case.name
            );
        }
    }
}
