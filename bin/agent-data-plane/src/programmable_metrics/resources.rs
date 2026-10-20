//! The resources outputs deliver through: the outputs, their bindings, the recipients, and the delivery hosts'
//! settings, as plan parameters, resolved from the current configuration.
//!
//! - [`params`]: the names and types of the plan parameters that carry encoder settings, host settings, and recipient
//!   protocols, which the catalog's host schemas are made of, and the codec between them and the typed specifications
//!   of `saluki-components`;
//! - [`specs`]: the typed specifications the hosts run, read from the leases, recipients, and publications the
//!   installer core decides.
//!
//! # Hosts
//!
//! The delivery hosts are fixed at boot ([`FixedHosts`]), each with its encode host: `dd_out` with
//! `dd_metrics_encode`, and `mrf_dd_out` with `mrf_metrics_encode`, whenever the metrics pipeline is;
//! `cluster_agent_out` with `af_metrics_encode` whenever a Cluster Agent endpoint resolves at boot (a host fact). A
//! host no output is bound to is idle: it runs no lease and no recipient until configuration enables its branch.
//!
//! # Outputs
//!
//! An output is one encoder stream, or the internal DogStatsD client telemetry tap. Its declaration carries the record
//! types it accepts, its recipients, its fingerprint, and, for an encoder stream, its binding: the delivery host it is
//! bound to, the [`Selector`] its fingerprint covers, its encoder parameters, and the identity its encoder telemetry
//! reports under. Native outputs exist exactly when `run.rs` wires their branches for the same configuration, decided
//! by the same helpers (`MetricsEndpointRoutingConfiguration`, `MrfConfiguration`, `AutoscalingFailoverConfiguration`,
//! and the Cluster Agent configuration), from the **current** configuration: an endpoint routing group or a failover
//! branch appears and disappears live. Only the client telemetry tap follows the startup configuration, with the
//! DogStatsD pipeline it belongs to. Programs add implied outputs on `dd_out` (see [`Demanded`]).
//!
//! The encoder keys the logs, events, service-check, and trace encoders read too (compression, the generic payload
//! limits, the flush timeout), the retry storage root, the stop timeout, and turning disk persistence on or off stay
//! as the process started with them: a live change would split behavior by payload type, or create or remove a queue's
//! directory. Everything else a binding or a host's settings carry follows the current configuration.
//!
//! # Recipients
//!
//! A recipient is one endpoint of one delivery host, with its own retry queue. A native recipient's identity is native:
//! the host, how the forwarder came to send to it (`primary`, `metrics_override`, or `additional`), the position of its
//! API key in its endpoint's key list, and the endpoint as its queue directory is named after
//! ([`MetricsRecipient::queue_endpoint`]), written `<host>/<route>/<key index>/<endpoint>`: an additional endpoint as
//! configured, any other as the forwarder resolves it, without its root path. So two raw spellings of one additional
//! URL are two recipients, and so is an additional endpoint that repeats the primary endpoint, as they are natively,
//! with two directories; two spellings of the primary endpoint (a trailing slash, a default port, a site and the URL it
//! implies) are one recipient, as they share one directory natively, and respelling it live changes only its settings.
//! An endpoint that is not plain printable ASCII, contains `@` (which may introduce credentials), or is too long is
//! written as `sha256:` and the first 16 hexadecimal digits of its digest instead.
//!
//! The plan declares every recipient of every host: those its outputs name, and the native set, every endpoint the
//! native forwarder of the host sends to, which the host routes payloads that do not name their recipients over (logs,
//! events, service checks, traces), by payload class. Each recipient names its credential by reference, never by value:
//! `native:api_key` (the primary endpoint and the alternate metrics intake), `native:additional/<endpoint digest>/<key
//! index>`, `native:multi_region_failover.api_key`, or `native:cluster_agent.auth_token`.
//!
//! # Fingerprints
//!
//! An output's fingerprint, version 2, covers what its declaration says it delivers to: its host, the records it
//! accepts, and its selector, a role for the main and failover outputs, a routing group's endpoint set, or an implied
//! output's recipients ([`resources::fingerprint`][fingerprints::fingerprint]). Recipients added to or removed
//! from a role change nothing.

pub mod params;
pub mod specs;

use std::collections::{BTreeMap, HashSet};

use agent_data_plane_config::{domains::multi_region_failover, shared::SharedConfiguration, Live, SalukiConfiguration};
use http::HeaderValue;
use saluki_components::{
    config::{AutoscalingFailoverConfiguration, MetricsEndpointRoutingConfiguration, MrfConfiguration},
    encoders::{DatadogMetricsConfiguration, MetricsEndpointRouting},
    forwarders::{
        delivery::PayloadClasses as NativeClasses, queue_endpoint, resolved_endpoint,
        ClusterAgentForwarderConfiguration, DatadogForwarderConfiguration, MetricsRecipient, MetricsRecipientRoute,
    },
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{
    canonical::PlanHash,
    compile::FrameHosts,
    compose::Demand,
    extension::DeliveryPolicy,
    ir::{
        CredentialRef, HostBinding, HostId, NativeComponentId, OutputBinding, OutputDecl, OutputId, PayloadClasses,
        RecipientDecl, RecipientId, RecipientName, Resources, RoleName, Selector,
    },
    model::{Kind, KindSet, PortType, TimestampForm},
    resources as fingerprints,
    validate::CatalogRecipient,
};
use serde_json::{json, Value};

use self::params::{encode_encoder, encode_host_settings, encode_protocol};
use super::{host_facts::HostFacts, telemetry_profiles::lease_telemetry};
use crate::{
    cli::run::{main_metrics_encoder, routing_group_encoder},
    config::DataPlaneConfiguration,
};

/// The internal output that taps the post-aggregation filter's output for DogStatsD client telemetry.
pub const DSD_CLIENT_TELEMETRY: &str = "dsd_client_telemetry";

/// The main metrics output.
pub const DD_METRICS_ENCODE: &str = "dd_metrics_encode";

/// The multi-region failover output.
pub const MRF_METRICS_ENCODE: &str = "mrf_metrics_encode";

/// The autoscaling failover output.
pub const AF_METRICS_ENCODE: &str = "af_metrics_encode";

/// The delivery host of the main, endpoint routing, and implied outputs.
pub const DD_OUT: &str = "dd_out";

/// The delivery host of the multi-region failover output.
pub const MRF_DD_OUT: &str = "mrf_dd_out";

/// The delivery host of the autoscaling failover output.
pub const CLUSTER_AGENT_OUT: &str = "cluster_agent_out";

/// The destination behind the DogStatsD client telemetry output.
pub const DSD_CLIENT_TELEMETRY_OUT: &str = "dsd_client_telemetry_out";

/// The role of the main output of `dd_out`.
pub const ROLE_MAIN: &str = "main";

/// The role of the multi-region failover output of `mrf_dd_out`.
pub const ROLE_MULTI_REGION_FAILOVER: &str = "multi_region_failover";

/// The role of the autoscaling failover output of `cluster_agent_out`.
pub const ROLE_AUTOSCALING_FAILOVER: &str = "autoscaling_failover";

/// The credential reference of the primary API key, which the primary endpoint and the alternate metrics intake send
/// with.
pub const PRIMARY_API_KEY: &str = "native:api_key";

/// The credential reference of the multi-region failover API key.
pub const MRF_API_KEY: &str = "native:multi_region_failover.api_key";

/// The credential reference of the Cluster Agent's bearer token.
pub const CLUSTER_AGENT_TOKEN: &str = "native:cluster_agent.auth_token";

/// The prefix of the credential references of the additional endpoints' API keys:
/// `native:additional/<endpoint digest>/<key index>`.
pub const ADDITIONAL_API_KEY_PREFIX: &str = "native:additional/";

/// The longest endpoint a recipient identity spells out; longer ones are written as a digest.
const MAX_RECIPIENT_ENDPOINT_LEN: usize = 400;

/// A fixed delivery host of the metrics path, with its encode host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedHost {
    /// The delivery host's component ID, which output bindings and recipients name it by.
    pub delivery: &'static str,

    /// The component ID of its encode host, which runs the leases of the outputs bound to it.
    pub encoder: &'static str,
}

impl FixedHost {
    /// Returns the delivery host's identifier.
    pub fn id(&self) -> HostId {
        HostId::new(self.delivery).expect("fixed delivery host IDs are identifiers")
    }
}

/// `dd_out`, with `dd_metrics_encode`.
pub const DD_HOST: FixedHost = FixedHost {
    delivery: DD_OUT,
    encoder: DD_METRICS_ENCODE,
};

/// `mrf_dd_out`, with `mrf_metrics_encode`.
pub const MRF_HOST: FixedHost = FixedHost {
    delivery: MRF_DD_OUT,
    encoder: MRF_METRICS_ENCODE,
};

/// `cluster_agent_out`, with `af_metrics_encode`.
pub const CLUSTER_AGENT_HOST: FixedHost = FixedHost {
    delivery: CLUSTER_AGENT_OUT,
    encoder: AF_METRICS_ENCODE,
};

/// The delivery hosts of a process's metrics path, decided at boot and fixed for its life.
///
/// `dd_out` and `mrf_dd_out` always exist with the metrics pipeline; `cluster_agent_out` only when a Cluster Agent
/// endpoint resolved at boot, whether or not forwarding to it is enabled, so that enabling autoscaling failover later
/// needs no restart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedHosts {
    cluster_agent: bool,
}

impl FixedHosts {
    /// Returns the delivery hosts of a process with the given host facts.
    pub fn at_boot(facts: &HostFacts) -> Self {
        Self {
            cluster_agent: facts.cluster_agent_endpoint.is_some(),
        }
    }

    /// Returns every fixed host there is, the Cluster Agent's included.
    #[cfg(test)]
    pub fn every() -> Self {
        Self { cluster_agent: true }
    }

    /// Returns every host, in identifier order.
    pub fn iter(&self) -> impl Iterator<Item = FixedHost> + '_ {
        [CLUSTER_AGENT_HOST, DD_HOST, MRF_HOST]
            .into_iter()
            .filter(|host| self.cluster_agent || host.delivery != CLUSTER_AGENT_OUT)
    }

    /// Returns the host with the given identifier, if the process has it.
    pub fn get(&self, host: &str) -> Option<FixedHost> {
        self.iter().find(|fixed| fixed.delivery == host)
    }

    /// Returns the hosts as the frame lays them out: each delivery host with its encode host, which the regions' exits
    /// toward bound outputs lead to.
    pub fn frame_hosts(&self) -> FrameHosts {
        self.iter().fold(FrameHosts::new(), |hosts, host| {
            let encoder = NativeComponentId::new(host.encoder).expect("fixed encode host IDs are component IDs");
            hosts.with_host(host.id(), encoder)
        })
    }
}

/// Returns the ID of the output of the endpoint routing group at `index`, which is also its encoder's component ID.
pub fn routing_encode_id(index: usize) -> String {
    format!("metrics_routing_encode_{}", index)
}

/// Returns the digest of the endpoint set of a routing group that sends to `endpoints`: the digest of the canonical
/// JSON list of its endpoints, sorted and without duplicates.
///
/// The digest is a routing group's selector, and [`routing_group_key`] its first 16 hexadecimal digits.
pub fn routing_group_digest(endpoints: &[String]) -> PlanHash {
    let mut endpoints = endpoints.to_vec();
    endpoints.sort();
    endpoints.dedup();
    PlanHash::of_value(&json!(endpoints)).expect("a list of strings has a canonical form")
}

/// Returns the key of the endpoint routing group that sends to `endpoints`: the first 16 hexadecimal digits of
/// [`routing_group_digest`].
///
/// A group's key depends only on which endpoints it sends to, not on its position among the groups or on its lists, so
/// it keeps its key when other groups are added or its lists change.
pub fn routing_group_key(endpoints: &[String]) -> String {
    short_digest(&routing_group_digest(endpoints))
}

/// Returns the first 16 hexadecimal digits of a digest.
/// Returns the endpoint routing groups of the given configuration, as `run.rs` resolves them.
pub(super) fn routing_groups(
    config: &SalukiConfiguration,
) -> Result<MetricsEndpointRoutingConfiguration, GenericError> {
    let routing = &config.domains.metrics_endpoint_routing;
    MetricsEndpointRoutingConfiguration::from_configuration(
        &routing.metric_allowlists,
        &routing.metric_prefix_allowlists,
        &config.shared.endpoints,
    )
}

fn short_digest(digest: &PlanHash) -> String {
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

/// What an output is.
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

    /// An output lowering adds for the recipients a program's `emit`s address when no other output delivers to exactly
    /// them: an **implied output** (see [`Demanded`]).
    Implied {
        /// The local ID of the `emit` its ID was derived from.
        emit: String,
    },
}

/// An output of the metrics path.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeOutput {
    /// The output's declaration, as plans carry it, with its binding.
    pub decl: OutputDecl,

    /// What the output is.
    pub kind: OutputKind,

    /// The component that consumes the output natively: its encoder, or for the client telemetry tap, its destination.
    /// Its encoder telemetry reports under this component ID.
    pub component: String,

    /// The delivery host the output is bound to, or `None` for an output without recipients.
    pub forwarder: Option<&'static str>,

    /// The output's recipients, as its native forwarder resolves them, in the order the forwarder sends to them.
    pub recipients: Vec<MetricsRecipient>,
}

/// The outputs of one configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeOutputs {
    outputs: Vec<NativeOutput>,
}

impl NativeOutputs {
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

    /// Returns each endpoint routing group's output that `self` declares for another group than `previous` did, by
    /// position: its ID, the key of the group it was, and the key of the group it is.
    ///
    /// Routing groups' outputs are numbered by position (`metrics_routing_encode_<n>`), so removing a group renumbers
    /// the ones after it, and an output's encoder series, under its ID, continue with another group's data (plan
    /// 4.15).
    pub fn renumbered_routing_groups<'a>(&'a self, previous: &'a NativeOutputs) -> Vec<(&'a str, &'a str, &'a str)> {
        self.outputs
            .iter()
            .filter_map(|output| match &output.kind {
                OutputKind::RoutingGroup(group) => {
                    let id = output.decl.id.as_str();
                    match &previous.get(id)?.kind {
                        OutputKind::RoutingGroup(was) if was.key != group.key => {
                            Some((id, was.key.as_str(), group.key.as_str()))
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }
}

/// What lowering adds outputs for beside native configuration: the **demand** of the
/// programs the installer holds, one set of recipients per distinct set their `emit`s address
/// ([`demanded_outputs`][saluki_metric_plan::compose::demanded_outputs]), and the outputs of the resolution before,
/// whose implied outputs keep their IDs.
///
/// For each set no output delivers to exactly, lowering adds an **implied output**: one encoder stream on the set's
/// delivery host, shared by every `emit` to the set, as a native routing group's is by the branches that reach it. Its
/// ID never contains a recipient name, since a name may be a URL: it is `implied_`, the local ID of the first `emit`,
/// in node ID order, of the first program that asks for the set, and eight hexadecimal digits of the set's digest (cut
/// to fit an identifier, and numbered after a collision; see `implied_output_id`), so that another set never takes it,
/// even from the same `emit`. It keeps the ID it had in the resolution before while any program asks for its set, so it
/// is stable across lowerings. Its encoder is the one native configuration builds for an endpoint routing group of its
/// endpoints, or for the main output without the others, and its selector its recipients ([`Selector::Recipients`]). It
/// retires, its lease draining, once no program asks for its set.
///
/// Implied outputs number at most
/// [`MAX_IMPLIED_OUTPUTS`](saluki_metric_plan::limits::values::MAX_IMPLIED_OUTPUTS), counted over the demand of every
/// program the installer holds, which is what exists together: the program running, any kept to roll back to, and any
/// installing or waiting. A replacement's outputs are built before it runs and the running program's retire after, so
/// a replacement that asks for new sets counts them beside the running program's. Lowering adds them on `dd_out`
/// only, for the main output's recipients (the primary endpoint and the additional endpoints): a set beyond the limit,
/// or on another host is refused, with the reason, which composition
/// reports ([`Catalog::implied_output_refused`][saluki_metric_plan::validate::Catalog::implied_output_refused]).
#[derive(Clone, Copy, Debug)]
pub struct Demanded<'a> {
    /// The sets of recipients the programs ask for.
    pub demand: &'a [Demand],

    /// The outputs of the resolution before.
    pub previous: &'a NativeOutputs,
}

/// No outputs of the resolution before.
static NO_OUTPUTS: NativeOutputs = NativeOutputs { outputs: Vec::new() };

impl Default for Demanded<'_> {
    fn default() -> Self {
        Self {
            demand: &[],
            previous: &NO_OUTPUTS,
        }
    }
}

/// The prefix of an implied output's ID.
pub const IMPLIED_OUTPUT_PREFIX: &str = "implied_";

/// The outputs, recipients, and host settings of one configuration: what the default program delivers through.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeResources {
    /// Every output, native and implied.
    pub outputs: NativeOutputs,

    /// Every delivery host's settings, and every recipient the outputs name or a host's native set holds, by
    /// identity.
    pub resources: Resources,

    /// The recipient directory: every recipient a program may address, by name (see [`Directory`]).
    pub directory: Directory,

    /// The sets of recipients a program asks for that lowering adds no implied output for, each with why
    /// ([`Demanded`]).
    pub refused_demand: BTreeMap<Vec<RecipientId>, String>,
}

/// The name of the main output's primary endpoint, or of the alternate metrics intake that replaces it for metrics.
pub const PRIMARY_RECIPIENT: &str = "primary";

/// The name of the Multi-Region Failover endpoint.
pub const MRF_RECIPIENT: &str = "mrf";

/// The name of the Cluster Agent.
pub const CLUSTER_AGENT_RECIPIENT: &str = "cluster_agent";

/// The recipient directory: what each name a program may address resolves to.
///
/// Lowering, the compatibility layer, names the native recipients: `primary` for the main output's primary endpoint
/// (or the alternate metrics intake that replaces it for metrics), `mrf` for the Multi-Region Failover endpoint,
/// `cluster_agent` for the Cluster Agent, and each additional endpoint by its URL as the forwarder resolves it
/// ([`additional_endpoint_name`]), which stands for every API key's recipient of that endpoint, under every spelling
/// of it. An entry never spans two delivery hosts: lowering fails, as a bug, rather than merge two.
/// Recipient identities stay positional and internal: names resolve to them when a program is composed.
///
/// The directory lists what is available, which is more than what the default program sends to: the Cluster Agent is
/// listed whenever forwarding to it is configured, whether or not autoscaling failover requests its branch. A listed
/// recipient that no output names runs nothing.
pub type Directory = BTreeMap<RecipientName, CatalogRecipient>;

/// Returns the name an additional endpoint's recipients have in the directory: its URL as the forwarder resolves and
/// sends to it, with its scheme (`https` for an endpoint configured without one), as status spells endpoints
/// ([`spelled_endpoint`]), so `b.example.com`, `https://b.example.com`, and `https://b.example.com/` are all
/// `https://b.example.com`. A URL that is not plain printable ASCII, contains `@`, or is too long is named by
/// `sha256:` and the first 16 hexadecimal digits of its digest, as status spells it, and so is an endpoint the
/// forwarder cannot resolve, which fails lowering anyway. A name is never an identifier.
pub fn additional_endpoint_name(configured_endpoint: &str) -> RecipientName {
    let resolved = resolved_endpoint(configured_endpoint);
    let spelled = if resolved.contains("://") {
        spelled_endpoint(&resolved)
    } else {
        format!(
            "sha256:{}",
            short_digest(&PlanHash::of_bytes(configured_endpoint.as_bytes()))
        )
    };
    RecipientName::new(spelled).expect("spelled endpoints are printable ASCII of bounded length")
}

/// Adds the recipient `id` of `host`, which spells its endpoint `endpoint`, to the directory under `name`.
///
/// # Errors
///
/// Returns an error if the name already stands for recipients of another host: lowering never gives two such
/// recipients one name, so this is a bug.
fn name_recipient(
    directory: &mut Directory, name: RecipientName, host: &FixedHost, id: RecipientId, endpoint: &str,
) -> Result<(), GenericError> {
    let entry = directory.entry(name.clone()).or_insert_with(|| CatalogRecipient {
        recipients: Vec::new(),
        host: host.id(),
        endpoint: spelled_endpoint(endpoint),
    });
    if entry.host != host.id() {
        return Err(generic_error!(
            "The recipient name `{}` would stand for recipients of `{}` and `{}`; this is a lowering bug.",
            name,
            entry.host,
            host.delivery
        ));
    }
    if !entry.recipients.contains(&id) {
        entry.recipients.push(id);
        entry.recipients.sort();
    }
    Ok(())
}

fn recipient_name(name: &str) -> RecipientName {
    RecipientName::new(name).expect("recipient names are printable ASCII")
}

impl NativeResources {
    /// Resolves the outputs, recipients, and host settings of the configuration `current`, for a process that started
    /// with `startup`, the host facts `facts`, and the delivery hosts `hosts`.
    ///
    /// There are none when the metrics pipeline is not required. Otherwise the main output always exists; the client
    /// telemetry tap when DogStatsD is enabled at startup; one output per endpoint routing group; the multi-region
    /// failover output when its endpoint override resolves; the autoscaling failover output when the branch is
    /// requested, forwarding to the Cluster Agent is enabled with a token, and its endpoint resolved at boot.
    ///
    /// Which native recipients exist is decided as native construction decides it at startup: the primary endpoint,
    /// and each additional endpoint's key whose value is not blank, can be sent in a header, and is not a key an
    /// earlier position of its endpoint holds. A key's value never decides more than that: see
    /// [`resolve_after`][Self::resolve_after].
    ///
    /// # Errors
    ///
    /// Returns the native error when `run.rs` would fail to build an output: an endpoint routing policy naming an
    /// unknown endpoint, or an endpoint that a forwarder cannot resolve.
    pub fn resolve(
        startup: &SalukiConfiguration, current: &SalukiConfiguration, facts: &HostFacts, hosts: &FixedHosts,
    ) -> Result<Self, GenericError> {
        Self::resolve_after(
            startup,
            current,
            facts,
            hosts,
            Demanded::default(),
            &Resources::default(),
        )
    }

    /// Resolves the outputs, recipients, and host settings of the configuration `current`, as
    /// [`resolve`][Self::resolve] does, following a resolution whose resources were `previous`.
    ///
    /// A key is a credential its recipient's cell follows while it runs, with the native rules (trimmed; a missing,
    /// blank, or invalid value keeps the last usable key), so a key's value adds or removes no recipient that
    /// `previous` declares: an additional endpoint's key position stays a recipient while its endpoint lists it, the
    /// failover recipient stays while failover is enabled with its endpoint, and the Cluster Agent recipient while its
    /// branch is requested, forwarding to it is enabled, and a token is set. Only a position, or a branch, that
    /// `previous` does not declare joins as at startup: when its key, or token, is one native construction would
    /// send with. So a rotation, even to a blank, duplicate, or invalid value, changes no recipient, no output, and no
    /// fingerprint, and values are never read here beyond that.
    ///
    /// # Errors
    ///
    /// As for [`resolve`][Self::resolve].
    pub fn resolve_after(
        startup: &SalukiConfiguration, current: &SalukiConfiguration, facts: &HostFacts, hosts: &FixedHosts,
        demanded: Demanded<'_>, previous: &Resources,
    ) -> Result<Self, GenericError> {
        let dp = DataPlaneConfiguration::from_configuration(startup);
        if !dp.metrics_pipeline_required() {
            return Ok(Self::default());
        }
        let shared = keyed_by_position(&effective_shared(&startup.shared, &current.shared), previous);
        let mut outputs = Vec::new();
        let mut recipients = BTreeMap::new();

        if dp.dogstatsd_enabled() {
            outputs.push(NativeOutput {
                decl: OutputDecl {
                    id: output_id(DSD_CLIENT_TELEMETRY),
                    accepts: PortType::ANY,
                    // A fixed destination has no binding, and its fingerprint never changes.
                    fingerprint: digest(
                        &json!({"encoder": {"destination": DSD_CLIENT_TELEMETRY_OUT}, "forwarder": null,
                        "recipients": []}),
                    ),
                    recipients: Vec::new(),
                    delivery: DeliveryPolicy::Native,
                    binding: None,
                },
                kind: OutputKind::ClientTelemetry,
                component: DSD_CLIENT_TELEMETRY_OUT.to_string(),
                forwarder: None,
                recipients: Vec::new(),
            });
        }

        // The main output and the endpoint routing groups all deliver through `dd_out`, whose native set is every
        // endpoint its forwarder sends to.
        let routing = routing_groups(current)?;
        let dd_out = datadog_forwarder(&shared);
        for recipient in dd_out_native_set(&shared)? {
            insert(&mut recipients, native_recipient(&DD_HOST, &recipient));
        }
        // The directory names what metrics go to: the alternate metrics intake in place of the primary endpoint, when
        // one is configured, and every additional endpoint.
        let mut directory = Directory::new();
        for recipient in metrics_recipients(&dd_out, None)? {
            let name = match recipient.route {
                MetricsRecipientRoute::Primary | MetricsRecipientRoute::MetricsOverride => {
                    recipient_name(PRIMARY_RECIPIENT)
                }
                MetricsRecipientRoute::Additional => additional_endpoint_name(&recipient.configured_endpoint),
            };
            let id = recipient_id(DD_OUT, &recipient);
            let endpoint = resolved_endpoint(&recipient.configured_endpoint);
            name_recipient(&mut directory, name, &DD_HOST, id, &endpoint)?;
        }
        let main_routing = (!routing.selected_endpoints().is_empty())
            .then(|| MetricsEndpointRouting::AllExcept(routing.selected_endpoints().iter().cloned().collect()));
        let main_encoder = main_metrics_encoder(&shared, &routing);
        outputs.push(bound_output(
            BoundOutput {
                id: DD_METRICS_ENCODE,
                kind: OutputKind::Main,
                host: &DD_HOST,
                selector: role(ROLE_MAIN),
                accepts: PortType::ANY,
                encoder: &main_encoder,
            },
            metrics_recipients(&dd_out, main_routing.as_ref())?,
        )?);
        for (index, group) in routing.policy_groups().iter().enumerate() {
            let group_routing = MetricsEndpointRouting::Only(group.endpoints.iter().cloned().collect());
            let id = routing_encode_id(index);
            let encoder = routing_group_encoder(&shared, group);
            outputs.push(bound_output(
                BoundOutput {
                    id: &id,
                    kind: OutputKind::RoutingGroup(RoutingGroup {
                        index,
                        key: routing_group_key(&group.endpoints),
                        endpoints: group.endpoints.clone(),
                    }),
                    host: &DD_HOST,
                    selector: Selector::Group(routing_group_digest(&group.endpoints)),
                    accepts: PortType::ANY,
                    encoder: &encoder,
                },
                metrics_recipients(&dd_out, Some(&group_routing))?,
            )?);
        }

        let mrf_domain = mrf_by_position(&current.domains.multi_region_failover, previous);
        let mrf = MrfConfiguration::from_configuration(&mrf_domain);
        if let Some((mrf_dd_url, mrf_api_key)) = mrf.metrics_endpoint_override() {
            let mrf_dd_out = DatadogForwarderConfiguration::for_endpoint_override(
                &shared,
                mrf_dd_url.clone(),
                mrf_api_key,
                Live::new_fixed(mrf_domain.api_key.clone()),
                Live::new_fixed(shared.secrets.clone()),
            );
            let mrf_recipients = metrics_recipients(&mrf_dd_out, None)?;
            for recipient in &mrf_recipients {
                insert(&mut recipients, native_recipient(&MRF_HOST, recipient));
                let id = recipient_id(MRF_DD_OUT, recipient);
                let name = recipient_name(MRF_RECIPIENT);
                name_recipient(&mut directory, name, &MRF_HOST, id, &recipient.configured_endpoint)?;
            }
            let encoder = DatadogMetricsConfiguration::from_configuration(&shared)
                .with_metrics_endpoint_override(mrf_dd_url.clone());
            outputs.push(bound_output(
                BoundOutput {
                    id: MRF_METRICS_ENCODE,
                    kind: OutputKind::MultiRegionFailover,
                    host: &MRF_HOST,
                    selector: role(ROLE_MULTI_REGION_FAILOVER),
                    accepts: PortType::ANY,
                    encoder: &encoder,
                },
                mrf_recipients,
            )?);
        }

        if let Some(endpoint) = cluster_agent_available(&shared, facts, previous) {
            let id = position_id(CLUSTER_AGENT_OUT, MetricsRecipientRoute::Primary, None, endpoint);
            let name = recipient_name(CLUSTER_AGENT_RECIPIENT);
            name_recipient(&mut directory, name, &CLUSTER_AGENT_HOST, id, endpoint)?;
        }
        if let Some(cluster_agent_out) = cluster_agent_by_position(&shared, facts, previous)? {
            let ca_recipients = cluster_agent_out.metrics_recipients()?;
            for recipient in &ca_recipients {
                insert(&mut recipients, native_recipient(&CLUSTER_AGENT_HOST, recipient));
            }
            let encoder = DatadogMetricsConfiguration::from_configuration(&shared).with_v2_series_only();
            outputs.push(bound_output(
                BoundOutput {
                    id: AF_METRICS_ENCODE,
                    kind: OutputKind::AutoscalingFailover,
                    host: &CLUSTER_AGENT_HOST,
                    selector: role(ROLE_AUTOSCALING_FAILOVER),
                    // The Cluster Agent forwarder posts every payload as series, so only scalar series may reach it.
                    accepts: PortType {
                        kinds: KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]),
                        timestamps: TimestampForm::Mixed,
                    },
                    encoder: &encoder,
                },
                ca_recipients,
            )?);
        }

        let refused_demand = imply(demanded, &shared, &dd_out, &recipients, &mut outputs)?;

        // Every fixed host has settings, idle or not, so that a branch enabled later finds them published.
        let settings = encode_host_settings(&dd_out.delivery_settings())
            .error_context("Failed to resolve the settings of the delivery hosts.")?;
        let hosts = hosts
            .iter()
            .map(|host| HostBinding {
                host: host.id(),
                settings: settings.clone(),
            })
            .collect();
        Ok(Self {
            outputs: NativeOutputs { outputs },
            resources: Resources {
                hosts,
                recipients: recipients.into_values().collect(),
            },
            directory,
            refused_demand,
        })
    }
}

/// Returns the shared configuration outputs and hosts resolve from: the current one, with the settings that apply only
/// at restart as the process started with them.
///
/// Those are the encoder settings the logs, events, service-check, and trace encoders read too (compression, the
/// generic payload limits, and the flush timeout), the retry storage root and run path, the stop timeout, and whether
/// disk persistence is on: a disk limit changes live only between nonzero values.
pub fn effective_shared(startup: &SharedConfiguration, current: &SharedConfiguration) -> SharedConfiguration {
    let mut effective = current.clone();
    effective.endpoints.compression = startup.endpoints.compression.clone();
    let encoding = &mut effective.metrics_encoding;
    encoding.max_payload_size = startup.metrics_encoding.max_payload_size;
    encoding.max_uncompressed_payload_size = startup.metrics_encoding.max_uncompressed_payload_size;
    encoding.flush_timeout = startup.metrics_encoding.flush_timeout;
    let forwarder = &mut effective.endpoints.forwarder;
    let at_startup = &startup.endpoints.forwarder;
    forwarder.storage_path = at_startup.storage_path.clone();
    forwarder.stop_timeout = at_startup.stop_timeout;
    if (forwarder.storage_max_size_in_bytes == 0) != (at_startup.storage_max_size_in_bytes == 0) {
        forwarder.storage_max_size_in_bytes = at_startup.storage_max_size_in_bytes;
    }
    effective.run_path = startup.run_path.clone();
    effective
}

/// The key native recipients are resolved with in place of the configured one: a key's value is a credential, which
/// decides which recipients exist only as [`NativeResources::resolve_after`] says.
const POSITION_KEY: &str = "position";

/// Returns `true` for a key native construction sends with: once trimmed, a valid HTTP header value.
fn can_be_sent(key: &str) -> bool {
    HeaderValue::try_from(key.trim()).is_ok()
}

/// Returns the shared configuration native recipients resolve from, following a resolution whose resources were
/// `previous`: `shared` with the primary key, and each additional endpoint's keys, replaced by placeholders.
///
/// An additional endpoint's key position keeps a placeholder if `previous` declares its recipient, and gets one if its
/// key is not blank, can be sent, and is not a key of a kept or earlier position of its endpoint, the rules native
/// construction applies at startup; every other position gets a blank key, which resolution skips. The primary
/// endpoint always exists.
fn keyed_by_position(shared: &SharedConfiguration, previous: &Resources) -> SharedConfiguration {
    let mut keyed = shared.clone();
    keyed.endpoints.api_key = POSITION_KEY.to_string();
    for (endpoint, keys) in keyed.endpoints.additional_endpoints.iter_mut() {
        let kept = |index: usize| {
            let id = position_id(DD_OUT, MetricsRecipientRoute::Additional, Some(index), endpoint);
            previous.recipient(&id).is_some()
        };
        let mut seen = keys
            .iter()
            .enumerate()
            .filter(|(index, _)| kept(*index))
            .map(|(_, key)| key.trim().to_string())
            .collect::<HashSet<_>>();
        let placeholders = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let key = key.trim();
                let joins = !key.is_empty() && can_be_sent(key) && seen.insert(key.to_string());
                if kept(index) || joins {
                    format!("{}-{}", POSITION_KEY, index)
                } else {
                    String::new()
                }
            })
            .collect();
        *keys = placeholders;
    }
    keyed
}

/// Returns the failover domain native recipients resolve from, following a resolution whose resources were
/// `previous`: `domain` with a placeholder key if `previous` declares its recipient, or its key is set and can be sent,
/// as native construction requires at startup, and without a key otherwise, which leaves its branch out.
fn mrf_by_position(domain: &multi_region_failover::Domain, previous: &Resources) -> multi_region_failover::Domain {
    let kept = domain.metrics_endpoint_url().is_some_and(|endpoint| {
        previous
            .recipient(&position_id(
                MRF_DD_OUT,
                MetricsRecipientRoute::Primary,
                None,
                &endpoint,
            ))
            .is_some()
    });
    let joins = domain.api_key.as_deref().is_some_and(can_be_sent);
    multi_region_failover::Domain {
        api_key: (kept || joins).then(|| POSITION_KEY.to_string()),
        ..domain.clone()
    }
}

/// Returns the Cluster Agent forwarder of the autoscaling failover branch for the shared configuration `shared`,
/// following a resolution whose resources were `previous`, with a placeholder token: `None` if `run.rs` does not wire
/// the branch for `shared`, or if `previous` does not declare its recipient and its token cannot be sent.
///
/// # Errors
///
/// Returns the native error if the forwarder cannot be configured.
fn cluster_agent_by_position(
    shared: &SharedConfiguration, facts: &HostFacts, previous: &Resources,
) -> Result<Option<ClusterAgentForwarderConfiguration>, GenericError> {
    if shared.cluster_agent.auth_token.is_none() {
        return Ok(None);
    }
    let mut keyed = shared.clone();
    keyed.cluster_agent.auth_token = Some(POSITION_KEY.to_string());
    let Some(forwarder) = cluster_agent_forwarder(&keyed, facts)? else {
        return Ok(None);
    };
    let kept = facts.cluster_agent_endpoint.as_deref().is_some_and(|endpoint| {
        previous
            .recipient(&position_id(
                CLUSTER_AGENT_OUT,
                MetricsRecipientRoute::Primary,
                None,
                endpoint,
            ))
            .is_some()
    });
    Ok((kept || cluster_agent_forwarder(shared, facts).is_ok()).then_some(forwarder))
}

/// Returns the Cluster Agent endpoint, if forwarding to the Cluster Agent is configured: its endpoint resolved at boot,
/// forwarding is enabled, and a token is set, which can be sent or, following a resolution whose resources were
/// `previous` and declared the recipient, was rotated (see [`NativeResources::resolve_after`]). Whether autoscaling
/// failover requests its branch does not matter: the Cluster Agent is then available to programs, as a recipient no
/// output names.
fn cluster_agent_available<'a>(
    shared: &SharedConfiguration, facts: &'a HostFacts, previous: &Resources,
) -> Option<&'a str> {
    let endpoint = facts.cluster_agent_endpoint.as_deref()?;
    let token = shared.cluster_agent.auth_token.as_deref()?;
    let kept = previous
        .recipient(&position_id(
            CLUSTER_AGENT_OUT,
            MetricsRecipientRoute::Primary,
            None,
            endpoint,
        ))
        .is_some();
    (shared.cluster_agent.enabled && (kept || can_be_sent(token))).then_some(endpoint)
}

/// Returns the Datadog forwarder of `dd_out` for the shared configuration `shared`, with the keys `shared` holds.
fn datadog_forwarder(shared: &SharedConfiguration) -> DatadogForwarderConfiguration {
    DatadogForwarderConfiguration::from_configuration(
        shared,
        Live::new_fixed(shared.endpoints.api_key.clone()),
        Live::new_fixed(shared.endpoints.additional_endpoints.clone()),
        Live::new_fixed(shared.secrets.clone()),
    )
}

/// Returns every endpoint `dd_out`'s native forwarder sends to: the primary endpoint, the alternate metrics intake
/// when one is configured, and the additional endpoints.
///
/// The forwarder resolves the endpoints of metrics payloads in place of the primary when an alternate metrics intake is
/// configured, so the primary is resolved as it would be without one: every other payload still reaches it.
fn dd_out_native_set(shared: &SharedConfiguration) -> Result<Vec<MetricsRecipient>, GenericError> {
    let mut set = metrics_recipients(&datadog_forwarder(shared), None)?;
    if !set
        .iter()
        .any(|recipient| recipient.route == MetricsRecipientRoute::Primary)
    {
        let mut without_override = shared.clone();
        without_override.endpoints.opw_intake.enabled = false;
        without_override.endpoints.vector_intake.enabled = false;
        let primary = metrics_recipients(&datadog_forwarder(&without_override), None)?
            .into_iter()
            .filter(|recipient| recipient.route == MetricsRecipientRoute::Primary);
        set.extend(primary);
    }
    Ok(set)
}

/// Returns the Cluster Agent forwarder of the autoscaling failover branch, if `run.rs` wires the branch for the shared
/// configuration `shared`: when autoscaling failover requests it, forwarding to the Cluster Agent is enabled with a
/// token, and the Cluster Agent endpoint resolved at boot.
///
/// # Errors
///
/// Returns the native error if the forwarder cannot be configured, such as for a token that cannot be sent.
pub(super) fn cluster_agent_forwarder(
    shared: &SharedConfiguration, facts: &HostFacts,
) -> Result<Option<ClusterAgentForwarderConfiguration>, GenericError> {
    let af = AutoscalingFailoverConfiguration::new(
        shared.autoscaling_failover.enabled,
        shared.autoscaling_failover.metrics.clone(),
    );
    let (Some(endpoint), Some(token), true, true) = (
        facts.cluster_agent_endpoint.as_ref(),
        shared.cluster_agent.auth_token.as_ref(),
        shared.cluster_agent.enabled,
        af.is_branch_requested(),
    ) else {
        return Ok(None);
    };
    ClusterAgentForwarderConfiguration::from_configuration(shared, endpoint.clone(), token.clone())
        .error_context("Failed to configure Cluster Agent forwarder.")
        .map(Some)
}

fn metrics_recipients(
    forwarder: &DatadogForwarderConfiguration, routing: Option<&MetricsEndpointRouting>,
) -> Result<Vec<MetricsRecipient>, GenericError> {
    forwarder
        .metrics_recipients(routing)
        .error_context("Failed to resolve the endpoints of a metrics forwarder.")
}

/// An output bound to a delivery host, as one of the native branches resolves it.
struct BoundOutput<'a> {
    id: &'a str,
    kind: OutputKind,
    host: &'a FixedHost,
    selector: Selector,
    accepts: PortType,
    /// The encoder configuration native wiring builds for the output.
    encoder: &'a DatadogMetricsConfiguration,
}

/// Returns a bound native output, with its recipients and their identities.
fn bound_output(output: BoundOutput<'_>, recipients: Vec<MetricsRecipient>) -> Result<NativeOutput, GenericError> {
    let BoundOutput {
        id,
        kind,
        host,
        selector,
        accepts,
        encoder,
    } = output;
    let mut ids = recipients
        .iter()
        .map(|recipient| recipient_id(host.delivery, recipient))
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    let settings = encoder
        .encoder_settings()
        .error_context(format!("Failed to resolve the encoder settings of output `{}`.", id))?;
    let id = output_id(id);
    let host_id = host.id();
    let binding = OutputBinding {
        host: host_id.clone(),
        encoder: encode_encoder(&settings)
            .error_context(format!("Failed to resolve the encoder settings of output `{}`.", id))?,
        telemetry: lease_telemetry(&id)?,
        selector: selector.clone(),
    };
    Ok(NativeOutput {
        decl: OutputDecl {
            fingerprint: fingerprints::fingerprint(&host_id, accepts, &selector),
            id: id.clone(),
            accepts,
            recipients: ids,
            delivery: DeliveryPolicy::Native,
            binding: Some(binding),
        },
        kind,
        component: id.to_string(),
        forwarder: Some(host.delivery),
        recipients,
    })
}

/// Adds an implied output for each set of recipients of `demanded` that no output of `outputs` delivers to exactly,
/// within the limit on implied outputs (see [`Demanded`]), and returns the sets it refused, each with why.
///
/// # Errors
///
/// Returns an error if the encoder settings of an implied output do not resolve, as for the native outputs.
fn imply(
    demanded: Demanded<'_>, shared: &SharedConfiguration, dd_out: &DatadogForwarderConfiguration,
    recipients: &BTreeMap<RecipientId, RecipientDecl>, outputs: &mut Vec<NativeOutput>,
) -> Result<BTreeMap<Vec<RecipientId>, String>, GenericError> {
    let mut refused = BTreeMap::new();
    let limit = usize::try_from(saluki_metric_plan::limits::values::MAX_IMPLIED_OUTPUTS).unwrap_or(usize::MAX);
    let mut added = 0usize;
    // The resolution before's implied outputs, by their recipients: their IDs stay theirs.
    let kept = demanded
        .previous
        .iter()
        .filter(|output| matches!(output.kind, OutputKind::Implied { .. }))
        .map(|output| (output.decl.recipients.clone(), output))
        .collect::<BTreeMap<_, _>>();
    // Sets the resolution before served first, then new ones, so that a new set never takes a kept set's ID.
    let mut demand = demanded.demand.iter().collect::<Vec<_>>();
    demand.sort_by_key(|demand| (!kept.contains_key(&demand.recipients), demand.recipients.clone()));
    for wanted in demand {
        if outputs.iter().any(|output| output.decl.recipients == wanted.recipients) {
            continue;
        }
        let refuse = |why: String| (wanted.recipients.clone(), why);
        if wanted.host.as_str() != DD_OUT {
            let (set, why) = refuse(format!(
                "outputs are added for the recipients of `{}` only, and these are recipients of `{}`",
                DD_OUT, wanted.host
            ));
            refused.insert(set, why);
            continue;
        }
        if added >= limit {
            let (set, why) = refuse(format!(
                "it would be one more than the limit of {} outputs programs imply, counted over every program the \
                 installer holds: the one running, any kept to roll back to, and any installing or waiting, whose \
                 outputs exist together until one replaces another",
                limit
            ));
            refused.insert(set, why);
            continue;
        }
        if !wanted.recipients.iter().all(|id| recipients.contains_key(id)) {
            let (set, why) = refuse("some are recipients the configuration does not declare".to_string());
            refused.insert(set, why);
            continue;
        }
        // The encoder native configuration builds for these endpoints: a routing group's, or the main output's
        // without the other endpoints.
        let all = metrics_recipients(dd_out, None)?;
        let included = |recipient: &MetricsRecipient| wanted.recipients.contains(&recipient_id(DD_OUT, recipient));
        let primary = all
            .iter()
            .any(|recipient| recipient.route != MetricsRecipientRoute::Additional && included(recipient));
        let additional = |selected: bool| {
            all.iter()
                .filter(|recipient| recipient.route == MetricsRecipientRoute::Additional)
                .filter(|recipient| included(recipient) == selected)
                .map(|recipient| recipient.configured_endpoint.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let routing = if primary {
            let others = additional(false);
            (!others.is_empty()).then(|| MetricsEndpointRouting::AllExcept(others.into_iter().collect()))
        } else {
            Some(MetricsEndpointRouting::Only(additional(true).into_iter().collect()))
        };
        let reached = metrics_recipients(dd_out, routing.as_ref())?;
        let mut ids = reached
            .iter()
            .map(|recipient| recipient_id(DD_OUT, recipient))
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        if ids != wanted.recipients {
            let (set, why) = refuse(
                "they are not every recipient of their endpoints, which one encoder sends to together".to_string(),
            );
            refused.insert(set, why);
            continue;
        }
        let mut encoder = DatadogMetricsConfiguration::from_configuration(shared);
        if let Some(routing) = routing {
            encoder = encoder.with_endpoint_routing(routing);
        }
        let id = match kept.get(&wanted.recipients) {
            Some(output) => output.decl.id.to_string(),
            None => implied_output_id(wanted.emit.as_str(), &wanted.recipients, outputs),
        };
        let mut output = bound_output(
            BoundOutput {
                id: &id,
                kind: OutputKind::Implied {
                    emit: wanted.emit.to_string(),
                },
                host: &DD_HOST,
                selector: Selector::Recipients(wanted.recipients.clone()),
                accepts: PortType::ANY,
                encoder: &encoder,
            },
            reached,
        )?;
        output.decl.recipients = wanted.recipients.clone();
        outputs.push(output);
        added += 1;
    }
    Ok(refused)
}

/// Returns the ID of an implied output for the recipients `recipients` whose first `emit` has the local ID `emit`:
/// `implied_<emit>_<set>`, where `<set>` is the first eight hexadecimal digits of the SHA-256 digest of the recipient
/// IDs, in order, each followed by a newline; the emit's part is cut to fit an identifier, and the whole numbered
/// (`_2`, `_3`, ...) if an output of `outputs` already has it.
///
/// The digest makes the ID the set's: an `emit` whose recipients change asks for another set, which gets another ID,
/// so telemetry under one ID always means one set of recipients. It never spells a recipient, since a name may be a
/// URL with credentials.
fn implied_output_id(emit: &str, recipients: &[RecipientId], outputs: &[NativeOutput]) -> String {
    let max = saluki_metric_plan::ir::MAX_IDENTIFIER_LEN;
    let listed = recipients.iter().map(|id| format!("{}\n", id)).collect::<String>();
    let digest = saluki_metric_plan::canonical::PlanHash::of_bytes(listed.as_bytes()).to_string();
    let set = &digest[digest.len() - 64..digest.len() - 56];
    let suffix = format!("_{}", set);
    let fit = |id: &str, room: usize| {
        let mut id = id[..id.len().min(room)].to_string();
        while id.ends_with('_') {
            id.pop();
        }
        id
    };
    let base = format!(
        "{}{}",
        fit(&format!("{}{}", IMPLIED_OUTPUT_PREFIX, emit), max - suffix.len()),
        suffix
    );
    let taken = |id: &str| outputs.iter().any(|output| output.decl.id.as_str() == id);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| {
            let suffix = format!("_{}", n);
            format!("{}{}", fit(&base, max - suffix.len()), suffix)
        })
        .find(|id| !taken(id))
        .expect("some number is free")
}

/// Adds a recipient, keeping the first declaration of an identity.
fn insert(recipients: &mut BTreeMap<RecipientId, RecipientDecl>, decl: RecipientDecl) {
    recipients.entry(decl.id.clone()).or_insert(decl);
}

/// Returns the declaration of a native recipient of `host`.
fn native_recipient(host: &FixedHost, recipient: &MetricsRecipient) -> RecipientDecl {
    RecipientDecl {
        id: recipient_id(host.delivery, recipient),
        host: host.id(),
        endpoint: recipient.configured_endpoint.clone(),
        credential: native_credential(host, recipient),
        classes: match recipient.classes {
            NativeClasses::MetricsOnly => PayloadClasses::MetricsOnly,
            NativeClasses::All => PayloadClasses::All,
        },
        protocol: encode_protocol(&recipient.protocol),
        delivery: DeliveryPolicy::Native,
    }
}

/// Returns the reference of the credential a native recipient of `host` sends with.
pub fn native_credential(host: &FixedHost, recipient: &MetricsRecipient) -> CredentialRef {
    let reference = match (host.delivery, recipient.route) {
        (MRF_DD_OUT, _) => MRF_API_KEY.to_string(),
        (CLUSTER_AGENT_OUT, _) => CLUSTER_AGENT_TOKEN.to_string(),
        (_, MetricsRecipientRoute::Additional) => additional_api_key(
            &recipient.configured_endpoint,
            recipient.api_key_index.unwrap_or_default(),
        ),
        (_, MetricsRecipientRoute::Primary | MetricsRecipientRoute::MetricsOverride) => PRIMARY_API_KEY.to_string(),
    };
    CredentialRef::new(reference).expect("native credential references are printable ASCII of bounded length")
}

/// Returns the reference of the API key at `index` of the additional endpoint configured as `endpoint`:
/// `native:additional/<the first 16 hexadecimal digits of the endpoint's SHA-256>/<index>`.
pub fn additional_api_key(endpoint: &str, index: usize) -> String {
    format!(
        "{}{}/{}",
        ADDITIONAL_API_KEY_PREFIX,
        short_digest(&PlanHash::of_bytes(endpoint.as_bytes())),
        index
    )
}

/// Returns the identity of a recipient of the given delivery host: `<host>/<route>/<key index>/<endpoint>`.
pub fn recipient_id(host: &str, recipient: &MetricsRecipient) -> RecipientId {
    position_id(
        host,
        recipient.route,
        recipient.api_key_index,
        &recipient.configured_endpoint,
    )
}

/// Returns the identity of the native recipient of the given delivery host at a configured position: the route, the
/// position of its key in its endpoint's key list, and its endpoint as its queue directory is named after
/// ([`queue_endpoint`]), so that two identities never share a directory.
fn position_id(host: &str, route: MetricsRecipientRoute, key_index: Option<usize>, endpoint: &str) -> RecipientId {
    spelled_id(host, route, key_index, &queue_endpoint(route, endpoint))
}

/// Returns `<host>/<route>/<key index>/<endpoint>`, with the endpoint spelled out when it can be
/// ([`spelled_endpoint`]).
fn spelled_id(host: &str, route: MetricsRecipientRoute, key_index: Option<usize>, endpoint: &str) -> RecipientId {
    let id = format!(
        "{}/{}/{}/{}",
        host,
        route.as_str(),
        key_index.unwrap_or_default(),
        spelled_endpoint(endpoint)
    );
    RecipientId::new(id).expect("recipient identities are printable ASCII of bounded length")
}

/// Returns an endpoint as a native recipient's identity spells it, which status shows: as written when it is plain
/// printable ASCII without `@`, of at most 400 bytes, and not spelled like a digest; otherwise `sha256:` and the first
/// 16 hexadecimal digits of its digest.
pub fn spelled_endpoint(endpoint: &str) -> String {
    let spelled_out = endpoint.len() <= MAX_RECIPIENT_ENDPOINT_LEN
        && !endpoint.is_empty()
        && endpoint.bytes().all(|b| b.is_ascii_graphic() && b != b'@')
        && !endpoint.starts_with("sha256:");
    if spelled_out {
        endpoint.to_string()
    } else {
        format!("sha256:{}", short_digest(&PlanHash::of_bytes(endpoint.as_bytes())))
    }
}

fn role(name: &str) -> Selector {
    Selector::Role(RoleName::new(name).expect("role names are identifiers"))
}

fn output_id(id: &str) -> OutputId {
    OutputId::new(id).expect("native output IDs are identifiers")
}

fn digest(document: &Value) -> PlanHash {
    PlanHash::of_value(document).expect("fingerprint documents have a canonical form")
}

#[cfg(test)]
mod tests;
