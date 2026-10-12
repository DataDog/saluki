//! The resources outputs deliver through: the outputs, their bindings, the recipients, and the delivery hosts'
//! settings, as plan parameters, resolved from the current configuration and the accepted resources artifact.
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
//! DogStatsD pipeline it belongs to. A resources artifact adds the outputs it declares, `resources_<name>`, on
//! `dd_out`.
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
//! A declared recipient's is `<host>/declared/<credential name>/<canonical endpoint>`.
//! An endpoint that is not plain printable ASCII, contains `@` (which may introduce credentials), or is too long is
//! written as `sha256:` and the first 16 hexadecimal digits of its digest instead.
//!
//! The plan declares every recipient of every host: those its outputs name, and the native set, every endpoint the
//! native forwarder of the host sends to, which the host routes payloads that do not name their recipients over (logs,
//! events, service checks, traces), by payload class. Each recipient names its credential by reference, never by value:
//! `native:api_key` (the primary endpoint and the alternate metrics intake), `native:additional/<endpoint digest>/<key
//! index>`, `native:multi_region_failover.api_key`, `native:cluster_agent.auth_token`, or, for a declared recipient,
//! `credentials/<name>`.
//!
//! # Fingerprints
//!
//! An output's fingerprint, version 2, covers what its declaration says it delivers to: its host, the records it
//! accepts, and its selector, a role for the main and failover outputs, a routing group's endpoint set, or a declared
//! output's recipients ([`resources::fingerprint`][declared_resources::fingerprint]). Recipients added to or removed
//! from a role change nothing. Each native output also keeps its version 1 digest, the stage 5 formula, which
//! artifacts written before the upgrade recorded: the digest of `{"encoder": .., "forwarder": .., "recipients": [..]}`,
//! the encoder variant and its endpoint routing, the forwarder, and the recipient identities, sorted.

pub mod params;
pub mod specs;

use std::collections::{BTreeMap, HashSet};

use agent_data_plane_config::{domains::multi_region_failover, shared::SharedConfiguration, Live, SalukiConfiguration};
use http::HeaderValue;
use saluki_components::{
    config::{AutoscalingFailoverConfiguration, MrfConfiguration},
    encoders::{DatadogMetricsConfiguration, MetricsEndpointRouting},
    forwarders::{
        delivery::{EncoderSettings, PayloadClasses as NativeClasses, RecipientProtocol, SeriesVariant, V3Acceptance},
        queue_endpoint, ClusterAgentForwarderConfiguration, DatadogForwarderConfiguration, MetricsRecipient,
        MetricsRecipientRoute,
    },
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{
    canonical::PlanHash,
    compile::FrameHosts,
    extension::DeliveryPolicy,
    ir::{
        ArtifactRevision, CredentialRef, HostBinding, HostId, NativeComponentId, OutputBinding, OutputDecl, OutputId,
        PayloadClasses, RecipientDecl, RecipientId, ResourceProvenance, Resources, RoleName, Selector,
    },
    model::{Kind, KindSet, PortType, TimestampForm},
    resources::{self as declared_resources, declared_recipient_id, ResourcesArtifact, CREDENTIALS_NAMESPACE},
};
use serde_json::{json, Value};

use self::params::{encode_encoder, encode_host_settings, encode_protocol};
use super::{bindings::routing_groups, host_facts::HostFacts, telemetry_profiles::lease_telemetry};
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

/// The delivery host of the main and endpoint routing outputs, and of declared outputs.
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

    /// Whether the host accepts recipients a resources artifact declares.
    pub declared_recipients: bool,
}

impl FixedHost {
    /// Returns the delivery host's identifier.
    pub fn id(&self) -> HostId {
        HostId::new(self.delivery).expect("fixed delivery host IDs are identifiers")
    }
}

/// `dd_out`, with `dd_metrics_encode`: the only host recipients a resources artifact declares may run in.
pub const DD_HOST: FixedHost = FixedHost {
    delivery: DD_OUT,
    encoder: DD_METRICS_ENCODE,
    declared_recipients: true,
};

/// `mrf_dd_out`, with `mrf_metrics_encode`.
pub const MRF_HOST: FixedHost = FixedHost {
    delivery: MRF_DD_OUT,
    encoder: MRF_METRICS_ENCODE,
    declared_recipients: false,
};

/// `cluster_agent_out`, with `af_metrics_encode`.
pub const CLUSTER_AGENT_HOST: FixedHost = FixedHost {
    delivery: CLUSTER_AGENT_OUT,
    encoder: AF_METRICS_ENCODE,
    declared_recipients: false,
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

    /// An output a resources artifact declares.
    Declared {
        /// The output's name in the artifact: its ID is `resources_<name>`.
        name: String,

        /// The output's display label, if the artifact gives one.
        label: Option<String>,
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

    /// The output's recipients, as its native forwarder resolves them, in the order the forwarder sends to them; empty
    /// for a declared output.
    pub recipients: Vec<MetricsRecipient>,

    /// The output's fingerprint as stage 5 computed it, for a native output: artifacts recorded it before fingerprints
    /// version 2, and keep composing while it still matches.
    pub legacy_fingerprint: Option<PlanHash>,
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

/// The resources a resources artifact declares, as lowering takes them: the accepted document, and the revision and
/// hash it was delivered with, which the resources it declares record as their provenance.
#[derive(Clone, Copy, Debug)]
pub struct Declared<'a> {
    /// The accepted resources artifact, validated: its endpoints are in canonical form.
    pub artifact: &'a ResourcesArtifact,

    /// The artifact's revision and hash.
    pub provenance: &'a ArtifactRevision,
}

/// The outputs, recipients, and host settings of one configuration: what the default program delivers through.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeResources {
    /// Every output, native and declared.
    pub outputs: NativeOutputs,

    /// Every delivery host's settings, and every recipient the outputs name or a host's native set holds, by
    /// identity.
    pub resources: Resources,
}

impl NativeResources {
    /// Resolves the outputs, recipients, and host settings of the configuration `current`, for a process that started
    /// with `startup`, the host facts `facts`, and the delivery hosts `hosts`, with the resources `declared` declares.
    ///
    /// There are none when the metrics pipeline is not required. Otherwise the main output always exists; the client
    /// telemetry tap when DogStatsD is enabled at startup; one output per endpoint routing group; the multi-region
    /// failover output when its endpoint override resolves; the autoscaling failover output when the branch is
    /// requested, forwarding to the Cluster Agent is enabled with a token, and its endpoint resolved at boot; and every
    /// output the resources artifact declares.
    ///
    /// Which native recipients exist is decided as native construction decides it at startup: the primary endpoint,
    /// and each additional endpoint's key whose value is not blank, can be sent in a header, and is not a key an
    /// earlier position of its endpoint holds. A key's value never decides more than that: see
    /// [`resolve_after`][Self::resolve_after].
    ///
    /// # Errors
    ///
    /// Returns the native error when `run.rs` would fail to build an output: an endpoint routing policy naming an
    /// unknown endpoint, or an endpoint that a forwarder cannot resolve. Returns an error also for a declared resource
    /// that cannot be lowered, which validating the artifact rules out.
    pub fn resolve(
        startup: &SalukiConfiguration, current: &SalukiConfiguration, facts: &HostFacts, hosts: &FixedHosts,
        declared: Option<Declared<'_>>,
    ) -> Result<Self, GenericError> {
        Self::resolve_after(startup, current, facts, hosts, declared, &Resources::default())
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
        declared: Option<Declared<'_>>, previous: &Resources,
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
                legacy_fingerprint: Some(digest(&json!({"encoder": {"destination": DSD_CLIENT_TELEMETRY_OUT},
                    "forwarder": null, "recipients": []}))),
            });
        }

        // The main output and the endpoint routing groups all deliver through `dd_out`, whose native set is every
        // endpoint its forwarder sends to.
        let routing = routing_groups(current)?;
        let dd_out = datadog_forwarder(&shared);
        for recipient in dd_out_native_set(&shared)? {
            insert(&mut recipients, native_recipient(&DD_HOST, &recipient));
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
                legacy_encoder: datadog_metrics_encoder(main_routing.as_ref()),
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
                    legacy_encoder: datadog_metrics_encoder(Some(&group_routing)),
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
                    legacy_encoder: json!({"type": "datadog_metrics", "metrics_endpoint_override": mrf_dd_url}),
                    encoder: &encoder,
                },
                mrf_recipients,
            )?);
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
                    legacy_encoder: json!({"type": "datadog_metrics", "v2_series_only": true}),
                    encoder: &encoder,
                },
                ca_recipients,
            )?);
        }

        if let Some(declared) = declared {
            let base = DatadogMetricsConfiguration::from_configuration(&shared)
                .encoder_settings()
                .error_context("Failed to resolve the encoder settings of declared outputs.")?;
            let allow_arbitrary_tags = shared.endpoints.allow_arbitrary_tags;
            declare(declared, &base, allow_arbitrary_tags, &mut outputs, &mut recipients)?;
        }

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

/// Returns the encoder part of a version 1 fingerprint, for a Datadog metrics encoder marking its payloads with
/// `routing`.
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
    /// The encoder part of the output's version 1 fingerprint.
    legacy_encoder: Value,
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
        legacy_encoder,
        encoder,
    } = output;
    let mut ids = recipients
        .iter()
        .map(|recipient| recipient_id(host.delivery, recipient))
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    // Version 1 names the recipients as stage 5 did, by their endpoints as configured.
    let mut legacy_ids = recipients
        .iter()
        .map(|recipient| legacy_recipient_id(host.delivery, recipient))
        .collect::<Vec<_>>();
    legacy_ids.sort();
    legacy_ids.dedup();
    let legacy = digest(&json!({
        "encoder": legacy_encoder,
        "forwarder": host.delivery,
        "recipients": legacy_ids,
    }));
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
        provenance: ResourceProvenance::Native,
    };
    Ok(NativeOutput {
        decl: OutputDecl {
            fingerprint: declared_resources::fingerprint(&host_id, accepts, &selector),
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
        legacy_fingerprint: Some(legacy),
    })
}

/// Adds the outputs and recipients a resources artifact declares.
///
/// A declared recipient receives metrics only, from the outputs that name it, with its delivery policy, and its
/// requests carry `Allow-Arbitrary-Tag-Value` as native ones do when `allow_arbitrary_tags` is set; a declared output's
/// encoder settings are the main output's, with the protocol variants its recipients accept.
fn declare(
    declared: Declared<'_>, base: &EncoderSettings, allow_arbitrary_tags: bool, outputs: &mut Vec<NativeOutput>,
    recipients: &mut BTreeMap<RecipientId, RecipientDecl>,
) -> Result<(), GenericError> {
    let Declared { artifact, provenance } = declared;
    let provenance = ResourceProvenance::Declared(provenance.clone());
    // V3 metrics intake is incompatible with zlib, and V3 series need the V2 series protocol, as natively.
    let v3_allowed = !base.compressor_kind.trim().eq_ignore_ascii_case("zlib");
    let mut by_name = BTreeMap::new();
    for recipient in &artifact.recipients {
        let invalid = |what: &str| {
            generic_error!(
                "Declared recipient `{}` cannot be lowered: {}; this is a validation bug.",
                recipient.name,
                what
            )
        };
        let credential_name = recipient
            .credential
            .as_str()
            .strip_prefix(CREDENTIALS_NAMESPACE)
            .ok_or_else(|| invalid("its credential is not in the `credentials/` namespace"))?;
        let id = declared_recipient_id(&recipient.host, credential_name, &recipient.endpoint);
        let v3 = V3Acceptance {
            series: v3_allowed && base.series == SeriesVariant::V2 && recipient.protocol.v3_series,
            sketches: v3_allowed && recipient.protocol.v3_sketches,
        };
        let decl = RecipientDecl {
            id: id.clone(),
            host: recipient.host.clone(),
            endpoint: recipient.endpoint.clone(),
            credential: recipient.credential.clone(),
            classes: PayloadClasses::MetricsOnly,
            protocol: encode_protocol(&RecipientProtocol::DatadogIntake {
                authorities: Default::default(),
                allow_arbitrary_tags,
                v3,
            }),
            delivery: match recipient.delivery {
                Some(bounded) => DeliveryPolicy::Bounded(bounded),
                None => DeliveryPolicy::Native,
            },
            provenance: provenance.clone(),
        };
        by_name.insert(recipient.name.as_str(), (decl.clone(), v3));
        insert(recipients, decl);
    }

    for output in &artifact.outputs {
        let invalid = |what: String| {
            generic_error!(
                "Declared output `{}` cannot be lowered: {}; this is a validation bug.",
                output.name,
                what
            )
        };
        let id = output
            .id()
            .map_err(|error| invalid(format!("its name is not a valid output name: {}", error)))?;
        let named = output
            .recipients
            .iter()
            .map(|name| {
                by_name.get(name.as_str()).ok_or_else(|| {
                    invalid(format!(
                        "it names recipient `{}`, which the artifact does not declare",
                        name
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let host = named
            .first()
            .map(|(decl, _)| decl.host.clone())
            .ok_or_else(|| invalid("it names no recipient".to_string()))?;
        let mut ids = named.iter().map(|(decl, _)| decl.id.clone()).collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        let selector = Selector::Declared(ids.clone());
        let accepts = output.accepts.port_type();
        let mut settings = base.clone();
        // Acceptance of a variant is exclusive: a recipient that takes V3 sketches takes no V2 sketches (plan F21), so
        // the output encodes a variant only for the recipients that take it.
        settings.variants.v2_series = named.iter().any(|(_, v3)| !v3.series);
        settings.variants.v2_sketches = named.iter().any(|(_, v3)| !v3.sketches);
        settings.variants.v3_series = named.iter().any(|(_, v3)| v3.series);
        settings.variants.v3_sketches = named.iter().any(|(_, v3)| v3.sketches);
        let binding = OutputBinding {
            host: host.clone(),
            selector: selector.clone(),
            encoder: encode_encoder(&settings)
                .error_context(format!("Failed to resolve the encoder settings of output `{}`.", id))?,
            telemetry: lease_telemetry(&id)?,
            provenance: provenance.clone(),
        };
        outputs.push(NativeOutput {
            decl: OutputDecl {
                fingerprint: declared_resources::fingerprint(&host, accepts, &selector),
                id: id.clone(),
                accepts,
                recipients: ids,
                delivery: DeliveryPolicy::Native,
                binding: Some(binding),
            },
            kind: OutputKind::Declared {
                name: output.name.clone(),
                label: output.label.clone(),
            },
            component: id.to_string(),
            forwarder: Some(DD_OUT),
            recipients: Vec::new(),
            legacy_fingerprint: None,
        });
    }
    Ok(())
}

/// Adds a recipient, keeping the first declaration of an identity: a native recipient that a resources artifact
/// declares again under the same identity, which declared identities rule out, stays native.
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
        provenance: ResourceProvenance::Native,
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

/// Returns the identity stage 5 gave a recipient, by its endpoint as configured, which version 1 fingerprints name.
fn legacy_recipient_id(host: &str, recipient: &MetricsRecipient) -> String {
    spelled_id(
        host,
        recipient.route,
        recipient.api_key_index,
        &recipient.configured_endpoint,
    )
    .to_string()
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
/// 16 hexadecimal digits of its digest, as the plan crate spells a declared recipient's
/// ([`declared_recipient_id`]).
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
