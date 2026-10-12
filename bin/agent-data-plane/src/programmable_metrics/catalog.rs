//! The host catalog: what ADP declares that plans may refer to.

use std::collections::BTreeMap;

use saluki_components::forwarders::delivery::RecipientProtocol;
use saluki_metric_plan::{
    ir::{BindingName, ExitName, HostId, InputName, OutputDecl, OutputId, RecipientDecl, ScopeName, ValueType},
    model::{Kind, PortType},
    validate::{Catalog, CatalogBinding, CatalogOutput, HostSchema, Liveness, MemoryCatalog},
};

use super::{
    bindings::{Snapshots, BINDINGS},
    resources::{
        params::{decode_encoder, decode_protocol, host_schema},
        FixedHosts, NativeOutputs,
    },
};
use crate::config::DataPlaneConfiguration;

/// The name of the scope artifacts replace: the policy region after metrics enrichment.
pub const POST_ENRICH_SCOPE: &str = "post_enrich";

/// The input of the `post_enrich` scope: the enriched metrics.
pub const POST_ENRICH_INPUT: &str = "metrics";

/// The name of the scope of DogStatsD's processing up to and including aggregation: from the DogStatsD source to the
/// post-aggregation filter.
pub const DSD_PRE_AGGREGATION_SCOPE: &str = "dogstatsd_pre_aggregation";

/// The input of the `dogstatsd_pre_aggregation` scope: the metrics the DogStatsD source admits.
pub const DSD_PRE_AGGREGATION_INPUT: &str = "metrics";

/// The exit of the `dogstatsd_pre_aggregation` scope that carries aggregated metrics.
pub const EXIT_AGGREGATED: &str = "aggregated";

/// The exit of the `dogstatsd_pre_aggregation` scope that carries the timestamped metrics the no-aggregation pipeline
/// passes by aggregation.
pub const EXIT_PASSTHROUGH: &str = "passthrough";

/// ADP's catalog: the outputs, native and declared, every configuration binding of the registry with its current value,
/// the `dogstatsd_pre_aggregation` and `post_enrich` scopes, the budget of state every state owner shares, and the
/// delivery hosts with the schemas of their parameters.
///
/// Bindings are resolved when the catalog is built, `Startup` bindings from the startup configuration and `Live` ones
/// from the current configuration, so the catalog carries the same values lowering writes into plans. A binding the
/// configuration has no valid value for is declared without one. Keyed bindings are declared for every key the
/// configuration they resolve from has.
///
/// It also declares the exits of the `dogstatsd_pre_aggregation` scope that lowering wrote, every one carrying any
/// port type (informational: a composed plan is validated whole), and the budget of state every state owner draws on.
/// It declares a host only if the process has it: `cluster_agent_out` only when a Cluster Agent endpoint resolved at
/// boot.
#[derive(Clone, Debug, PartialEq)]
pub struct AdpCatalog {
    outputs: BTreeMap<OutputId, CatalogOutput>,
    bindings: BTreeMap<BindingName, CatalogBinding>,
    scope_input: PortType,
    state_budget: Option<u64>,
    pre_aggregation_exits: Vec<ExitName>,
    hosts: FixedHosts,
}

impl AdpCatalog {
    /// Builds the catalog of the given outputs, configuration snapshots, and delivery hosts.
    pub fn new(outputs: &NativeOutputs, snapshots: Snapshots<'_>, hosts: FixedHosts) -> Self {
        let outputs = outputs
            .iter()
            .map(|output| {
                (
                    output.decl.id.clone(),
                    CatalogOutput {
                        accepts: output.decl.accepts,
                        fingerprint: output.decl.fingerprint,
                        recipients: output.decl.recipients.clone(),
                        legacy_fingerprint: output.legacy_fingerprint,
                    },
                )
            })
            .collect();

        let mut bindings = BTreeMap::new();
        for spec in BINDINGS {
            let keys = match (spec.key, spec.liveness) {
                (None, _) => vec![String::new()],
                (Some(key), Liveness::Live) => key.keys(snapshots.current),
                (Some(key), Liveness::Startup) => key.keys(snapshots.startup),
            };
            for key in keys {
                let name = BindingName::new(spec.instance_name(&key)).expect("registry binding names are valid");
                bindings.insert(
                    name,
                    CatalogBinding {
                        ty: ValueType::from(&spec.ty),
                        liveness: spec.liveness,
                        value: spec.resolve(snapshots, &key),
                    },
                );
            }
        }

        // DogStatsD aggregation's context limit is a startup setting: every lineage of every state owner, in every
        // generation, shares the limit the process started with.
        let startup = snapshots.startup;
        let state_budget = DataPlaneConfiguration::from_configuration(startup)
            .dogstatsd_enabled()
            .then(|| u64::try_from(startup.domains.dogstatsd.aggregation.context_limit).unwrap_or(u64::MAX));

        Self {
            outputs,
            bindings,
            scope_input: PortType::ANY,
            state_budget,
            pre_aggregation_exits: Vec::new(),
            hosts,
        }
    }

    /// Declares the exits of the `dogstatsd_pre_aggregation` scope: those lowering wrote into the default program.
    pub fn with_pre_aggregation_exits(mut self, exits: Vec<ExitName>) -> Self {
        self.pre_aggregation_exits = exits;
        self
    }

    /// Returns how many contexts the state owners of every generation hold together at most: native DogStatsD
    /// aggregation's context limit, or `None` without DogStatsD, when no plan has a state owner.
    pub fn state_budget(&self) -> Option<u64> {
        self.state_budget
    }

    /// Returns the catalog's document form, as exports carry it.
    pub fn to_document(&self) -> MemoryCatalog {
        let mut catalog = MemoryCatalog::new();
        for (id, output) in &self.outputs {
            catalog = catalog.with_output(id.clone(), output.clone());
        }
        for (name, binding) in &self.bindings {
            catalog = catalog.with_catalog_binding(name.clone(), binding.clone());
        }
        let pre_aggregation = ScopeName::new(DSD_PRE_AGGREGATION_SCOPE).expect("scope names are identifiers");
        catalog = catalog
            .with_scope_input(
                pre_aggregation.clone(),
                InputName::new(DSD_PRE_AGGREGATION_INPUT).expect("input names are identifiers"),
                self.scope_input,
            )
            .with_scope_input(
                ScopeName::new(POST_ENRICH_SCOPE).expect("scope names are identifiers"),
                InputName::new(POST_ENRICH_INPUT).expect("input names are identifiers"),
                self.scope_input,
            );
        for exit in &self.pre_aggregation_exits {
            catalog = catalog.with_scope_exit(pre_aggregation.clone(), exit.clone(), self.scope_input);
        }
        match self.state_budget {
            Some(capacity) => catalog.with_state_budget(capacity),
            None => catalog,
        }
    }
}

impl Catalog for AdpCatalog {
    fn output(&self, id: &OutputId) -> Option<&CatalogOutput> {
        self.outputs.get(id)
    }

    fn binding(&self, name: &BindingName) -> Option<&CatalogBinding> {
        self.bindings.get(name)
    }

    fn scope_input(&self, scope: &ScopeName, input: &InputName) -> Option<PortType> {
        let declared = matches!(
            (scope.as_str(), input.as_str()),
            (POST_ENRICH_SCOPE, POST_ENRICH_INPUT) | (DSD_PRE_AGGREGATION_SCOPE, DSD_PRE_AGGREGATION_INPUT)
        );
        declared.then_some(self.scope_input)
    }

    fn scope_exit(&self, scope: &ScopeName, exit: &ExitName) -> Option<PortType> {
        let declared = scope.as_str() == DSD_PRE_AGGREGATION_SCOPE && self.pre_aggregation_exits.contains(exit);
        declared.then_some(self.scope_input)
    }

    fn state_budget(&self) -> Option<u64> {
        self.state_budget
    }

    fn host(&self, host: &HostId) -> Option<HostSchema> {
        self.hosts.get(host.as_str()).map(|host| host_schema(&host))
    }

    fn declares_hosts(&self) -> bool {
        true
    }

    fn refuses(&self, output: &OutputDecl, recipient: &RecipientDecl) -> Option<String> {
        // Parameters that do not decode are the schema rule's to report.
        let binding = output.binding.as_ref()?;
        let variants = decode_encoder(&binding.encoder).ok()?.variants;
        let protocol = decode_protocol(&recipient.protocol).ok()?;
        refusal(output.accepts, variants, &protocol)
    }
}

/// Returns why a recipient of `protocol` cannot receive what an output accepting `accepts`, encoded in `variants`,
/// produces, or `None` if it can: every payload names the recipients of its own protocol variant, so a recipient must
/// take one variant the output encodes for each kind of record the output carries.
fn refusal(
    accepts: PortType, variants: saluki_components::forwarders::delivery::ProtocolVariants,
    protocol: &RecipientProtocol,
) -> Option<String> {
    let series_kinds = [Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set, Kind::Histogram];
    let carries_series = series_kinds.into_iter().any(|kind| accepts.kinds.contains(kind));
    let carries_sketches = accepts.kinds.contains(Kind::Distribution);
    match protocol {
        // The Cluster Agent takes V2 series of scalar kinds only: a payload of another variant names no Cluster Agent
        // recipient, so what matters is what the output carries, and that it encodes V2 series for it.
        RecipientProtocol::ClusterAgent => {
            let unscalar = [Kind::Histogram, Kind::Distribution]
                .into_iter()
                .any(|kind| accepts.kinds.contains(kind));
            if unscalar {
                Some(
                    "the Cluster Agent takes only series of scalar kinds (counters, rates, gauges, and sets), and the \
                     output carries histograms or distributions"
                        .to_string(),
                )
            } else if carries_series && !variants.v2_series {
                Some("the Cluster Agent takes only V2 series, which the output does not encode".to_string())
            } else {
                None
            }
        }
        RecipientProtocol::DatadogIntake { v3, .. } => {
            let series = if v3.series {
                variants.v3_series
            } else {
                variants.v2_series
            };
            let sketches = if v3.sketches {
                variants.v3_sketches
            } else {
                variants.v2_sketches
            };
            if carries_series && !series {
                Some(format!(
                    "it takes series only in their {} form, which the output does not encode",
                    if v3.series { "V3" } else { "V1 or V2" }
                ))
            } else if carries_sketches && !sketches {
                Some(format!(
                    "it takes sketches only in their {} form, which the output does not encode",
                    if v3.sketches { "V3" } else { "V2" }
                ))
            } else {
                None
            }
        }
    }
}
