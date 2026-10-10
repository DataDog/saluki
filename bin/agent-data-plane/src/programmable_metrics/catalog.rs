//! The host catalog: what ADP declares that plans may refer to.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    ir::{BindingName, ExitName, InputName, OutputId, ScopeName, ValueType},
    model::PortType,
    validate::{Catalog, CatalogBinding, CatalogOutput, MemoryCatalog},
};

use super::{
    bindings::{Snapshots, BINDINGS},
    outputs::NativeOutputs,
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

/// ADP's catalog: the native outputs, every configuration binding of the registry with its current value, the
/// `dogstatsd_pre_aggregation` and `post_enrich` scopes, and the budget of state every state owner shares.
///
/// Bindings are resolved when the catalog is built, `Startup` bindings from the startup configuration and `Live` ones
/// from the current configuration, so the catalog carries the same values lowering writes into plans. A binding the
/// configuration has no valid value for is declared without one. Keyed bindings are declared for every key the startup
/// configuration has.
///
/// It also declares the exits of the `dogstatsd_pre_aggregation` scope that lowering wrote, every one carrying any
/// port type (informational: a composed plan is validated whole), and the budget of state every state owner draws on.
#[derive(Clone, Debug, PartialEq)]
pub struct AdpCatalog {
    outputs: BTreeMap<OutputId, CatalogOutput>,
    bindings: BTreeMap<BindingName, CatalogBinding>,
    scope_input: PortType,
    state_budget: Option<u64>,
    pre_aggregation_exits: Vec<ExitName>,
}

impl AdpCatalog {
    /// Builds the catalog of the given native outputs and configuration snapshots.
    pub fn new(outputs: &NativeOutputs, snapshots: Snapshots<'_>) -> Self {
        let outputs = outputs
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
            let keys = match spec.key {
                None => vec![String::new()],
                Some(key) => key.keys(snapshots.startup),
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
}
