//! The host catalog: what ADP declares that plans may refer to.

use std::collections::BTreeMap;

use saluki_components::forwarders::delivery::RecipientProtocol;
use saluki_metric_plan::{
    ir::{BindingName, HostId, InputName, OutputDecl, OutputId, RecipientDecl, RecipientName, ScopeName, ValueType},
    model::{Kind, PortType},
    operators::SourceBinding,
    validate::{Catalog, CatalogBinding, CatalogOutput, CatalogRecipient, HostSchema, Liveness, MemoryCatalog},
};

use super::{
    bindings::{Snapshots, BINDINGS},
    resources::{
        params::{decode_encoder, decode_protocol, host_schema},
        Directory, FixedHosts, NativeResources, RESERVED_RECIPIENT_NAMES,
    },
};
use crate::config::DataPlaneConfiguration;

/// The name of the one scope of the default program, which artifacts replace: the whole metrics path after its
/// sources, from DogStatsD's processing through aggregation, enrichment, and the policy region to the outputs.
pub const METRICS_SCOPE: &str = "metrics";

/// Returns the name of the input of the [`METRICS_SCOPE`] that a source of `binding` feeds: the binding's own name,
/// such as `dogstatsd` for the DogStatsD source's metrics.
pub fn scope_input_name(binding: SourceBinding) -> InputName {
    InputName::new(binding.as_str()).expect("source binding names are identifiers")
}

/// ADP's catalog: the outputs, native and declared, every configuration binding of the registry with its class and
/// current value, the recipient directory, the inputs of the `metrics` scope, the budget of state every state owner
/// shares, that a program runs at most one aggregation, and the delivery hosts with the schemas of their parameters.
///
/// Bindings are resolved when the catalog is built, `Startup` bindings from the startup configuration and `Live` ones
/// from the current configuration, so the catalog carries the same values lowering writes into plans. A binding the
/// configuration has no valid value for is declared without one. Keyed bindings are declared for every key the
/// configuration they resolve from has.
///
/// It declares one input of the `metrics` scope for each source the lowered program has
/// ([`with_scope_inputs`](Self::with_scope_inputs)), carrying what the source produces: so an artifact is checked
/// against the kinds and timestamps each input can carry. It declares a host only if the process has it:
/// `cluster_agent_out` only when a Cluster Agent endpoint resolved at boot.
#[derive(Clone, Debug, PartialEq)]
pub struct AdpCatalog {
    outputs: BTreeMap<OutputId, CatalogOutput>,
    bindings: BTreeMap<BindingName, CatalogBinding>,
    directory: Directory,
    scope_inputs: BTreeMap<InputName, PortType>,
    state_budget: Option<u64>,
    hosts: FixedHosts,
}

impl AdpCatalog {
    /// Builds the catalog of the given resources (outputs and the recipient directory), configuration snapshots, and
    /// delivery hosts.
    pub fn new(resources: &NativeResources, snapshots: Snapshots<'_>, hosts: FixedHosts) -> Self {
        let outputs = resources
            .outputs
            .iter()
            .map(|output| {
                (
                    output.decl.id.clone(),
                    CatalogOutput {
                        accepts: output.decl.accepts,
                        fingerprint: output.decl.fingerprint,
                        recipients: output.decl.recipients.clone(),
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
                        class: spec.class,
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
            directory: resources.directory.clone(),
            scope_inputs: BTreeMap::new(),
            state_budget,
            hosts,
        }
    }

    /// Declares the inputs of the `metrics` scope, each with the port type of what it carries: those lowering wrote
    /// into the default program, one per source it has.
    pub fn with_scope_inputs(mut self, inputs: impl IntoIterator<Item = (InputName, PortType)>) -> Self {
        self.scope_inputs = inputs.into_iter().collect();
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
        for (name, recipient) in &self.directory {
            catalog = catalog.with_recipient(name.clone(), recipient.clone());
        }
        let scope = ScopeName::new(METRICS_SCOPE).expect("scope names are identifiers");
        for (input, port_type) in &self.scope_inputs {
            catalog = catalog.with_scope_input(scope.clone(), input.clone(), *port_type);
        }
        if Catalog::single_aggregation(self) {
            catalog = catalog.with_single_aggregation();
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
        if scope.as_str() != METRICS_SCOPE {
            return None;
        }
        self.scope_inputs.get(input).copied()
    }

    fn state_budget(&self) -> Option<u64> {
        self.state_budget
    }

    fn single_aggregation(&self) -> bool {
        // One aggregation per program, for now: the frame has one isolation slot, which placement lets run a state
        // owner of each branch of a split ahead of it, and the demo and the operator surfaces assume one.
        true
    }

    fn recipient(&self, name: &RecipientName) -> Option<&CatalogRecipient> {
        self.directory.get(name)
    }

    fn reserves_recipient_name(&self, name: &RecipientName) -> bool {
        RESERVED_RECIPIENT_NAMES.contains(&name.as_str())
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
