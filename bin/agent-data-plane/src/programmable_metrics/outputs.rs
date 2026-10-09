//! The native metrics outputs: what each one is, where it delivers, and the declaration plans refer to it by.
//!
//! An output is one encoder stream, or the internal DogStatsD client telemetry tap. Its declaration carries the record
//! types it accepts, its physical recipients, and a fingerprint of its binding. Outputs exist exactly when `run.rs`
//! wires their branches, decided by the same helpers (`MetricsEndpointRoutingConfiguration`, `MrfConfiguration`,
//! `AutoscalingFailoverConfiguration`, and the Cluster Agent configuration), and always from the startup
//! configuration: until stage 7, an output is added or removed only at restart.
//!
//! # Recipients
//!
//! A recipient is one endpoint I/O loop of a forwarder, with its own retry queue, as the forwarder resolves it from
//! configuration (see `MetricsRecipient`). Its identity is native: the forwarder, how the forwarder came to send to it
//! (`primary`, `metrics_override`, or `additional`), the position of its API key in its endpoint's key list, and the
//! endpoint as configured, written `<forwarder>/<route>/<key index>/<endpoint>`. Two raw spellings of one URL are two
//! recipients, and so is an additional endpoint that repeats the primary endpoint, as they are natively. An endpoint
//! that is not plain printable ASCII, contains `@` (which may introduce credentials), or is too long is written as
//! `sha256:` and the first 16 hexadecimal digits of its digest instead.
//!
//! # Fingerprints
//!
//! An output's fingerprint is the digest of the canonical JSON document `{"encoder": .., "forwarder": ..,
//! "recipients": [..]}`: which encoder variant serves the output and the endpoint routing it marks payloads with, the
//! forwarder it feeds, and its recipient identities, sorted. Encoder tuning (compression, payload limits) is not part
//! of it, since it never changes where an output delivers.

use agent_data_plane_config::{Live, SalukiConfiguration};
use saluki_components::{
    config::{AutoscalingFailoverConfiguration, MrfConfiguration},
    encoders::MetricsEndpointRouting,
    forwarders::{ClusterAgentForwarderConfiguration, DatadogForwarderConfiguration, MetricsRecipient},
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metric_plan::{
    canonical::PlanHash,
    extension::DeliveryPolicy,
    ir::{OutputDecl, OutputId, RecipientId},
    model::{Kind, KindSet, PortType, TimestampForm},
};
use serde_json::{json, Value};

use super::{bindings::routing_groups, host_facts::HostFacts};
use crate::config::DataPlaneConfiguration;

/// The internal output that taps the post-aggregation filter's output for DogStatsD client telemetry.
pub const DSD_CLIENT_TELEMETRY: &str = "dsd_client_telemetry";

/// The main metrics output.
pub const DD_METRICS_ENCODE: &str = "dd_metrics_encode";

/// The multi-region failover output.
pub const MRF_METRICS_ENCODE: &str = "mrf_metrics_encode";

/// The autoscaling failover output.
pub const AF_METRICS_ENCODE: &str = "af_metrics_encode";

/// The forwarder of the main and endpoint routing outputs.
pub const DD_OUT: &str = "dd_out";

/// The forwarder of the multi-region failover output.
pub const MRF_DD_OUT: &str = "mrf_dd_out";

/// The forwarder of the autoscaling failover output.
pub const CLUSTER_AGENT_OUT: &str = "cluster_agent_out";

/// The destination behind the DogStatsD client telemetry output.
pub const DSD_CLIENT_TELEMETRY_OUT: &str = "dsd_client_telemetry_out";

/// The longest endpoint a recipient identity spells out; longer ones are written as a digest.
const MAX_RECIPIENT_ENDPOINT_LEN: usize = 400;

/// Returns the ID of the output of the endpoint routing group at `index`, which is also its encoder's component ID.
pub fn routing_encode_id(index: usize) -> String {
    format!("metrics_routing_encode_{}", index)
}

/// Returns the key of the endpoint routing group that sends to `endpoints`: the first 16 hexadecimal digits of the
/// digest of the canonical JSON list of its endpoints, sorted.
///
/// A group's key depends only on which endpoints it sends to, not on its position among the groups or on its lists, so
/// it keeps its key when other groups are added or its lists change.
pub fn routing_group_key(endpoints: &[String]) -> String {
    let mut endpoints = endpoints.to_vec();
    endpoints.sort();
    endpoints.dedup();
    let digest = PlanHash::of_value(&json!(endpoints)).expect("a list of strings has a canonical form");
    digest.digest()[..8]
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

/// An endpoint routing group, as its branch lowers it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingGroup {
    /// The group's position among the groups, which numbers its filter and encoder.
    pub index: usize,

    /// The group's key: see [`routing_group_key`].
    pub key: String,

    /// The configured endpoints the group sends to, sorted.
    pub endpoints: Vec<String>,
}

/// What a native output is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputKind {
    /// The DogStatsD client telemetry tap, which has no recipients.
    ClientTelemetry,

    /// The main metrics output, which sends to every endpoint without an endpoint routing policy.
    Main,

    /// The output of an endpoint routing group.
    RoutingGroup(RoutingGroup),

    /// The multi-region failover output.
    MultiRegionFailover,

    /// The autoscaling failover output.
    AutoscalingFailover,
}

/// A native metrics output.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeOutput {
    /// The output's declaration, as plans carry it.
    pub decl: OutputDecl,

    /// What the output is.
    pub kind: OutputKind,

    /// The component that consumes the output: its encoder, or for the client telemetry tap, its destination.
    pub component: String,

    /// The forwarder the encoder feeds, or `None` for an output without recipients.
    pub forwarder: Option<&'static str>,

    /// The output's recipients, as its forwarder resolves them, in the order the forwarder sends to them.
    pub recipients: Vec<MetricsRecipient>,
}

/// The native metrics outputs of one configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeOutputs {
    outputs: Vec<NativeOutput>,
}

impl NativeOutputs {
    /// Determines the native metrics outputs of the given startup configuration.
    ///
    /// There are none when the metrics pipeline is not required. Otherwise the main output always exists, the client
    /// telemetry tap when DogStatsD is enabled, one output per endpoint routing group, the multi-region failover output
    /// when its endpoint override resolves, and the autoscaling failover output when the branch is requested and the
    /// Cluster Agent endpoint resolved at boot.
    ///
    /// # Errors
    ///
    /// Returns the native error when `run.rs` would fail to build an output: an endpoint routing policy naming an
    /// unknown endpoint, a Cluster Agent token that cannot be sent, or an endpoint that a forwarder cannot resolve.
    pub fn from_configuration(startup: &SalukiConfiguration, facts: &HostFacts) -> Result<Self, GenericError> {
        let dp = DataPlaneConfiguration::from_configuration(startup);
        if !dp.metrics_pipeline_required() {
            return Ok(Self::default());
        }
        let shared = &startup.shared;
        let mut outputs = Vec::new();

        if dp.dogstatsd_enabled() {
            outputs.push(output(
                DSD_CLIENT_TELEMETRY,
                OutputKind::ClientTelemetry,
                DSD_CLIENT_TELEMETRY_OUT,
                None,
                json!({"destination": DSD_CLIENT_TELEMETRY_OUT}),
                Vec::new(),
            ));
        }

        // The main output and the endpoint routing groups all feed `dd_out`.
        let routing = routing_groups(startup)?;
        let dd_out = DatadogForwarderConfiguration::from_configuration(
            shared,
            Live::new_fixed(shared.endpoints.api_key.clone()),
            Live::new_fixed(shared.endpoints.additional_endpoints.clone()),
            Live::new_fixed(shared.secrets.clone()),
        );
        let main_routing = (!routing.selected_endpoints().is_empty())
            .then(|| MetricsEndpointRouting::AllExcept(routing.selected_endpoints().iter().cloned().collect()));
        outputs.push(output(
            DD_METRICS_ENCODE,
            OutputKind::Main,
            DD_METRICS_ENCODE,
            Some(DD_OUT),
            datadog_metrics_encoder(main_routing.as_ref()),
            recipients(&dd_out, main_routing.as_ref())?,
        ));
        for (index, group) in routing.policy_groups().iter().enumerate() {
            let group_routing = MetricsEndpointRouting::Only(group.endpoints.iter().cloned().collect());
            let id = routing_encode_id(index);
            outputs.push(output(
                &id,
                OutputKind::RoutingGroup(RoutingGroup {
                    index,
                    key: routing_group_key(&group.endpoints),
                    endpoints: group.endpoints.clone(),
                }),
                &id,
                Some(DD_OUT),
                datadog_metrics_encoder(Some(&group_routing)),
                recipients(&dd_out, Some(&group_routing))?,
            ));
        }

        let mrf = MrfConfiguration::from_configuration(&startup.domains.multi_region_failover);
        if let Some((mrf_dd_url, mrf_api_key)) = mrf.metrics_endpoint_override() {
            let mrf_dd_out = DatadogForwarderConfiguration::for_endpoint_override(
                shared,
                mrf_dd_url.clone(),
                mrf_api_key,
                Live::new_fixed(startup.domains.multi_region_failover.api_key.clone()),
                Live::new_fixed(shared.secrets.clone()),
            );
            outputs.push(output(
                MRF_METRICS_ENCODE,
                OutputKind::MultiRegionFailover,
                MRF_METRICS_ENCODE,
                Some(MRF_DD_OUT),
                json!({"type": "datadog_metrics", "metrics_endpoint_override": mrf_dd_url}),
                recipients(&mrf_dd_out, None)?,
            ));
        }

        if let Some(cluster_agent_out) = cluster_agent_forwarder(startup, facts)? {
            let mut output = output(
                AF_METRICS_ENCODE,
                OutputKind::AutoscalingFailover,
                AF_METRICS_ENCODE,
                Some(CLUSTER_AGENT_OUT),
                json!({"type": "datadog_metrics", "v2_series_only": true}),
                cluster_agent_out.metrics_recipients()?,
            );
            // The Cluster Agent forwarder posts every payload as series, so only scalar series may reach it.
            output.decl.accepts = PortType {
                kinds: KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]),
                timestamps: TimestampForm::Mixed,
            };
            outputs.push(output);
        }

        Ok(Self { outputs })
    }

    /// Returns every output, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &NativeOutput> {
        self.outputs.iter()
    }

    /// Returns the output with the given ID, if there is one.
    pub fn get(&self, id: &str) -> Option<&NativeOutput> {
        self.outputs.iter().find(|output| output.decl.id.as_str() == id)
    }

    /// Returns the endpoint routing groups, in order.
    #[cfg(test)]
    pub fn routing_groups(&self) -> impl Iterator<Item = &RoutingGroup> {
        self.outputs.iter().filter_map(|output| match &output.kind {
            OutputKind::RoutingGroup(group) => Some(group),
            _ => None,
        })
    }

    /// Returns the declarations of every output, in declaration order, as a plan carries them.
    pub fn decls(&self) -> Vec<OutputDecl> {
        self.outputs.iter().map(|output| output.decl.clone()).collect()
    }
}

/// Returns the encoder part of a fingerprint for a Datadog metrics encoder marking its payloads with `routing`.
/// Returns the Cluster Agent forwarder of the autoscaling failover branch, if `run.rs` wires the branch: when
/// autoscaling failover requests it, and the Cluster Agent endpoint resolved and a token is configured.
///
/// # Errors
///
/// Returns the native error if the forwarder cannot be configured, such as for a token that cannot be sent.
pub(super) fn cluster_agent_forwarder(
    startup: &SalukiConfiguration, facts: &HostFacts,
) -> Result<Option<ClusterAgentForwarderConfiguration>, GenericError> {
    let shared = &startup.shared;
    let af = AutoscalingFailoverConfiguration::new(
        shared.autoscaling_failover.enabled,
        shared.autoscaling_failover.metrics.clone(),
    );
    let (Some(endpoint), Some(token), true) = (
        facts.cluster_agent_endpoint.as_ref(),
        shared.cluster_agent.auth_token.as_ref(),
        af.is_branch_requested(),
    ) else {
        return Ok(None);
    };
    ClusterAgentForwarderConfiguration::from_configuration(shared, endpoint.clone(), token.clone())
        .error_context("Failed to configure Cluster Agent forwarder.")
        .map(Some)
}

fn datadog_metrics_encoder(routing: Option<&MetricsEndpointRouting>) -> Value {
    match routing {
        None => json!({"type": "datadog_metrics"}),
        Some(MetricsEndpointRouting::AllExcept(endpoints)) => {
            json!({"type": "datadog_metrics", "endpoint_routing": {"all_except": endpoints}})
        }
        Some(MetricsEndpointRouting::Only(endpoints)) => {
            json!({"type": "datadog_metrics", "endpoint_routing": {"only": endpoints}})
        }
    }
}

fn recipients(
    forwarder: &DatadogForwarderConfiguration, routing: Option<&MetricsEndpointRouting>,
) -> Result<Vec<MetricsRecipient>, GenericError> {
    forwarder
        .metrics_recipients(routing)
        .error_context("Failed to resolve the endpoints of a metrics forwarder.")
}

fn output(
    id: &str, kind: OutputKind, component: &str, forwarder: Option<&'static str>, encoder: Value,
    recipients: Vec<MetricsRecipient>,
) -> NativeOutput {
    let mut ids = recipients
        .iter()
        .map(|recipient| recipient_id(forwarder.unwrap_or_default(), recipient))
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    let document = json!({
        "encoder": encoder,
        "forwarder": forwarder,
        "recipients": ids.iter().map(RecipientId::as_str).collect::<Vec<_>>(),
    });
    NativeOutput {
        decl: OutputDecl {
            id: OutputId::new(id).expect("native output IDs are identifiers"),
            accepts: PortType::ANY,
            fingerprint: PlanHash::of_value(&document).expect("fingerprint documents have a canonical form"),
            recipients: ids,
            delivery: DeliveryPolicy::Native,
        },
        kind,
        component: component.to_string(),
        forwarder,
        recipients,
    }
}

/// Returns the identity of a recipient of the given forwarder: `<forwarder>/<route>/<key index>/<endpoint>`.
pub fn recipient_id(forwarder: &str, recipient: &MetricsRecipient) -> RecipientId {
    let endpoint = &recipient.configured_endpoint;
    let spelled_out = endpoint.len() <= MAX_RECIPIENT_ENDPOINT_LEN
        && !endpoint.is_empty()
        && endpoint.bytes().all(|b| b.is_ascii_graphic() && b != b'@')
        && !endpoint.starts_with("sha256:");
    let endpoint = if spelled_out {
        endpoint.clone()
    } else {
        let digest = PlanHash::of_bytes(endpoint.as_bytes());
        let hex = digest.digest()[..8]
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect::<String>();
        format!("sha256:{}", hex)
    };
    let id = format!(
        "{}/{}/{}/{}",
        forwarder,
        recipient.route.as_str(),
        recipient.api_key_index.unwrap_or_default(),
        endpoint
    );
    RecipientId::new(id).expect("recipient identities are printable ASCII of bounded length")
}

#[cfg(test)]
mod tests;
