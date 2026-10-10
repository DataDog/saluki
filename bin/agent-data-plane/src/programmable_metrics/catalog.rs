//! The host catalog: what ADP declares that plans may refer to.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    ir::{BindingName, InputName, OutputId, ScopeName, ValueType},
    model::PortType,
    validate::{Catalog, CatalogBinding, CatalogOutput, MemoryCatalog},
};

use super::{
    bindings::{Snapshots, BINDINGS},
    outputs::NativeOutputs,
};

/// The name of the scope artifacts replace: the policy region after metrics enrichment.
pub const POST_ENRICH_SCOPE: &str = "post_enrich";

/// The input of the `post_enrich` scope: the enriched metrics.
pub const POST_ENRICH_INPUT: &str = "metrics";

/// ADP's catalog: the native outputs, every configuration binding of the registry with its current value, and the
/// `post_enrich` scope.
///
/// Bindings are resolved when the catalog is built, `Startup` bindings from the startup configuration and `Live` ones
/// from the current configuration, so the catalog carries the same values lowering writes into plans. A binding the
/// configuration has no valid value for is declared without one. Keyed bindings are declared for every key the startup
/// configuration has.
#[derive(Clone, Debug, PartialEq)]
pub struct AdpCatalog {
    outputs: BTreeMap<OutputId, CatalogOutput>,
    bindings: BTreeMap<BindingName, CatalogBinding>,
    scope_input: PortType,
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

        Self {
            outputs,
            bindings,
            scope_input: PortType::ANY,
        }
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
        catalog.with_scope_input(
            ScopeName::new(POST_ENRICH_SCOPE).expect("scope names are identifiers"),
            InputName::new(POST_ENRICH_INPUT).expect("input names are identifiers"),
            self.scope_input,
        )
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
        (scope.as_str() == POST_ENRICH_SCOPE && input.as_str() == POST_ENRICH_INPUT).then_some(self.scope_input)
    }
}
