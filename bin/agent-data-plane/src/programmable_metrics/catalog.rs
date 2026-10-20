//! The host catalog: what ADP declares that plans may refer to.

use std::collections::BTreeMap;

use saluki_components::forwarders::delivery::RecipientProtocol;
use saluki_metric_plan::{
    ir::{BindingName, HostId, OutputDecl, OutputId, RecipientDecl, RecipientId, RecipientName, ValueType},
    model::{Kind, PortType},
    validate::{
        Catalog, CatalogBinding, CatalogOutput, CatalogRecipient, HostSchema, MemoryCatalog, TelemetryContract,
    },
};

use super::{
    bindings::{Snapshots, BINDINGS},
    resources::{
        params::{decode_encoder, decode_protocol, host_schema},
        Directory, FixedHosts, NativeResources,
    },
};
use crate::config::DataPlaneConfiguration;

/// ADP's catalog: the outputs, native and declared, every configuration binding of the registry with its class and
/// current value, the recipient directory, the budget of state every state owner shares, that a program runs at most
/// one aggregation, the telemetry contract, and the delivery hosts with the schemas of their parameters.
///
/// Bindings are resolved when the catalog is built, `Startup` bindings from the startup configuration and `Live` ones
/// from the current configuration, so the catalog carries the same values lowering writes into plans. A binding the
/// configuration has no valid value for is declared without one. Keyed bindings are declared for every key the
/// configuration they resolve from has.
///
/// It declares no sources: those are the default program's `source` nodes, which a program's `input.<name>` edges
/// leave. It declares a host only if the process has it: `cluster_agent_out` only when a Cluster Agent endpoint
/// resolved at boot.
#[derive(Clone, Debug, PartialEq)]
pub struct AdpCatalog {
    outputs: BTreeMap<OutputId, CatalogOutput>,
    bindings: BTreeMap<BindingName, CatalogBinding>,
    directory: Directory,
    state_budget: Option<u64>,
    hosts: FixedHosts,
    /// The sets of recipients lowering added no implied output for, each with why.
    refused_demand: BTreeMap<Vec<RecipientId>, String>,
    /// The nodes the remapper reads native series from by name, as the default program has them.
    telemetry_contract: Option<TelemetryContract>,
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
                        recipients: output.decl.recipients.clone(),
                    },
                )
            })
            .collect();

        let bindings = BINDINGS
            .iter()
            .map(|spec| {
                (
                    BindingName::new(spec.name).expect("registry binding names are valid"),
                    CatalogBinding {
                        ty: ValueType::from(&spec.ty),
                        liveness: spec.liveness,
                        class: spec.class,
                        value: spec.resolve(snapshots),
                    },
                )
            })
            .collect();

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
            state_budget,
            hosts,
            refused_demand: resources.refused_demand.clone(),
            telemetry_contract: None,
        }
    }

    /// Declares the telemetry contract: what the remapper reads by name from the nodes of a plan, as the default program
    /// meets it ([`compatibility::contract`](super::compatibility::contract)).
    pub fn with_telemetry_contract(mut self, contract: TelemetryContract) -> Self {
        self.telemetry_contract = Some(contract);
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
        if let Some(contract) = &self.telemetry_contract {
            catalog = catalog.with_telemetry_contract(contract.clone());
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

    fn state_budget(&self) -> Option<u64> {
        self.state_budget
    }

    fn telemetry_contract(&self) -> Option<&TelemetryContract> {
        self.telemetry_contract.as_ref()
    }

    fn recipient(&self, name: &RecipientName) -> Option<&CatalogRecipient> {
        self.directory.get(name)
    }

    fn output_for(&self, recipients: &[RecipientId]) -> Option<OutputId> {
        self.outputs
            .iter()
            .find(|(_, output)| !output.recipients.is_empty() && output.recipients == recipients)
            .map(|(id, _)| id.clone())
    }

    fn implied_output_refused(&self, recipients: &[RecipientId]) -> Option<String> {
        self.refused_demand.get(recipients).cloned()
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
