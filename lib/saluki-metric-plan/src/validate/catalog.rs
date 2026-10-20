//! The catalog of what the host declares: outputs, configuration bindings, its state budget, whether it runs at most
//! one aggregation per program, and what it reads by name from the nodes of a plan.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::TelemetryContract;
use crate::{
    ir::{
        BindingName, HostId, OutputDecl, OutputId, ParamSpec, ParamType, RecipientDecl, RecipientId, RecipientName,
        ValueType,
    },
    model::PortType,
    predicate::BindingResolver,
};

/// An output the host declares.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogOutput {
    /// The records the output accepts.
    pub accepts: PortType,

    /// The output's physical recipients.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<RecipientId>,
}

/// An entry of the host's recipient directory: what a recipient name a program addresses resolves to.
///
/// The directory is how programs name destinations: by a name the host gives, never by a recipient's
/// identity, which stays internal to the host (its queues, slots, and leases keep it). It never holds a credential.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRecipient {
    /// The identities of the recipients the name stands for, sorted: one for most names, and every API key's recipient
    /// for a name that is an endpoint's URL.
    pub recipients: Vec<RecipientId>,

    /// The delivery host the recipients belong to.
    pub host: HostId,

    /// The endpoint, as the host's status spells it: never with credentials.
    pub endpoint: String,
}

/// Whether a configuration binding's value can change while the process runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// The value can change at any time; each change produces a new generation.
    Live,

    /// The value is read once, at startup, and changes only take effect at restart.
    Startup,
}

/// What a binding's value is to a program: something true of the host, or a state the program reacts to.
///
/// What to do with metrics is never a binding: a program writes its policy as literals, the default program included,
/// whose lowering writes native policy settings as their values.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingClass {
    /// Something true of the host or its resources that a program does not choose, such as the host's tags.
    Fact,

    /// A live operational state a program reacts to, such as whether a failover is in progress.
    Signal,
}

impl BindingClass {
    /// Returns the class's name, as catalog documents write it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Signal => "signal",
        }
    }
}

/// A configuration binding the host declares.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogBinding {
    /// The type of the binding's value. A parameter bound to it must [admit][ValueType::admits] this type.
    #[serde(rename = "type")]
    pub ty: ValueType,

    /// Whether the value can change while the process runs.
    pub liveness: Liveness,

    /// What the value is to a program: a fact or a signal.
    pub class: BindingClass,

    /// The binding's current value, written as a parameter of its type is, if the catalog carries it.
    ///
    /// A host's exported catalog carries the values, so that a plan whose bindings are unresolved, such as an exported
    /// program, can be previewed against them away from the host: the catalog is then a [`BindingResolver`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// What a delivery host accepts as plan parameters: the schema of each of its typed [`Params`][crate::ir::Params]
/// members.
///
/// The plan crate never reads what these parameters mean: the host provides their schema, generated from the same
/// table its codec decodes them with, so that validation, `explain`, and decoding cannot disagree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostSchema {
    /// The parameters of an output binding's encoder ([`OutputBinding::encoder`][crate::ir::OutputBinding::encoder]).
    pub encoder: &'static [ParamSpec],

    /// The host's shared settings ([`HostBinding::settings`][crate::ir::HostBinding::settings]).
    pub settings: &'static [ParamSpec],

    /// A recipient's protocol ([`RecipientDecl::protocol`][crate::ir::RecipientDecl::protocol]).
    pub protocol: &'static [ParamSpec],
}

/// What the host declares, which plans may refer to.
///
/// The validator checks every output a plan declares and every configuration binding its parameters use against the
/// catalog. The host implements it from its own registry of outputs and
/// bindings; off the host, an exported [`MemoryCatalog`] document implements the same.
///
/// Every catalog also resolves the bindings whose values it carries: `dyn Catalog` is a [`BindingResolver`].
pub trait Catalog {
    /// Returns the output with the given ID, if the host declares it.
    fn output(&self, id: &OutputId) -> Option<&CatalogOutput>;

    /// Returns the configuration binding with the given name, if the host declares it.
    fn binding(&self, name: &BindingName) -> Option<&CatalogBinding>;

    /// Returns the host's state budget: the most contexts its state owners hold together, every lineage of every
    /// state owner included, or `None` if the host does not bound them together.
    ///
    /// Validation warns about an artifact's state owner whose context limit exceeds it (`state_budget`). The default
    /// declares none.
    fn state_budget(&self) -> Option<u64> {
        None
    }

    /// Returns `true` if the host runs at most one state owner, one aggregation, per program.
    ///
    /// Validation then reports every state owner after the first (`single_aggregation`), wherever it is: placement
    /// alone accepts a second one wherever an isolation slot can run it, since an isolation slot runs several state
    /// owners by design. The default allows any number.
    fn single_aggregation(&self) -> bool {
        false
    }

    /// Returns what the host reads by name from the nodes of a plan, if it reads anything.
    ///
    /// Validation warns about each node of the contract a plan lacks or runs as another operator
    /// (`telemetry_contract`), and rewrites move a filter the contract names only whole (see
    /// [`contract`](super::contract)). The default reads nothing.
    fn telemetry_contract(&self) -> Option<&TelemetryContract> {
        None
    }

    /// Returns the recipient directory's entry for `name`, if the host's directory has it.
    ///
    /// The default has no directory.
    fn recipient(&self, name: &RecipientName) -> Option<&CatalogRecipient> {
        let _ = name;
        None
    }

    /// Returns the output whose recipients are exactly `recipients`, sorted and without repeats, if the host declares
    /// one: the output an `emit` addressed to the names of those recipients delivers to. Of several, the first in
    /// output ID order.
    ///
    /// The default looks for none.
    fn output_for(&self, recipients: &[RecipientId]) -> Option<OutputId> {
        let _ = recipients;
        None
    }

    /// Returns why the host does not add an output for exactly `recipients`, sorted and without repeats, for the `emit`s
    /// of a program that address them, if it refuses to: an output it would add beyond its limit on outputs, or
    /// recipients it cannot encode for together. A program that needs one is then refused rather than left waiting.
    ///
    /// The default refuses none.
    fn implied_output_refused(&self, recipients: &[RecipientId]) -> Option<String> {
        let _ = recipients;
        None
    }

    /// Returns the schema of the given delivery host's parameters, if the host declares it.
    ///
    /// Output bindings, host settings, and recipients name a delivery host; validation checks that their parameters
    /// match its schema. The default declares no host, as an exported catalog, which leaves host settings out, does.
    fn host(&self, host: &HostId) -> Option<HostSchema> {
        let _ = host;
        None
    }

    /// Returns `true` if the catalog declares every delivery host the host has, so that a host it does not declare is
    /// not one.
    ///
    /// Validation then rejects an output binding, host settings, or a recipient on a host the catalog does not
    /// declare. A catalog that declares hosts only in part, such as an exported one, which leaves host settings out,
    /// returns `false`, the default: validation then checks the parameters of the hosts it declares, and leaves the
    /// others to the host, whose frame has no encode host for a host it does not have.
    fn declares_hosts(&self) -> bool {
        false
    }

    /// Returns why `recipient` cannot receive what `output`, bound to its delivery host, produces, or `None` if it can.
    ///
    /// The plan crate does not read what protocols mean: the host decides from the output's encoder parameters and the
    /// recipient's protocol, as a recipient that takes only some protocol variants or record kinds does. The default
    /// refuses nothing.
    fn refuses(&self, output: &OutputDecl, recipient: &RecipientDecl) -> Option<String> {
        let _ = (output, recipient);
        None
    }
}

impl BindingResolver for dyn Catalog + '_ {
    fn resolve_bool(&self, binding: &BindingName) -> Option<bool> {
        self.binding(binding)?.value.as_ref()?.as_bool()
    }

    fn resolve_strings(&self, binding: &BindingName) -> Option<Vec<String>> {
        self.binding(binding)?
            .value
            .as_ref()?
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect()
    }
}

/// A catalog held in memory, which is also the catalog's document form.
///
/// Serialized as an object with these members, each of which may be left out when empty: `outputs`, from output ID to
/// the output's `accepts` and `recipients`; `bindings`, from binding name to the binding's `type` (a [`ValueType`]),
/// `liveness` (`live` or `startup`), `class` (`fact` or `signal`), and optional `value`; `recipients`, the host's
/// recipient directory, from recipient name to the entry's `recipients`, `host`, `endpoint`, and `source`;
/// `state_budget`, the host's state budget; `single_aggregation`, `true` when the host runs at most one aggregation
/// per program; and `telemetry_contract`, what the host reads by name from the nodes of a plan. A host exports its
/// catalog in this form, so that plans can be validated and previewed away from it. It declares no delivery host: an
/// export leaves host settings out.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCatalog {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    outputs: BTreeMap<OutputId, CatalogOutput>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    bindings: BTreeMap<BindingName, CatalogBinding>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    state_budget: Option<u64>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    single_aggregation: bool,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    recipients: BTreeMap<RecipientName, CatalogRecipient>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    telemetry_contract: Option<TelemetryContract>,
}

impl MemoryCatalog {
    /// Creates an empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares an output.
    pub fn with_output(mut self, id: OutputId, output: CatalogOutput) -> Self {
        self.outputs.insert(id, output);
        self
    }

    /// Declares a fact's configuration binding, without its value.
    pub fn with_binding(self, name: BindingName, ty: ParamType, liveness: Liveness) -> Self {
        self.with_classified_binding(name, ty, liveness, BindingClass::Fact)
    }

    /// Declares a configuration binding of the given class, without its value.
    pub fn with_classified_binding(
        self, name: BindingName, ty: ParamType, liveness: Liveness, class: BindingClass,
    ) -> Self {
        self.with_catalog_binding(
            name,
            CatalogBinding {
                ty: ValueType::from(&ty),
                liveness,
                class,
                value: None,
            },
        )
    }

    /// Declares a configuration binding.
    pub fn with_catalog_binding(mut self, name: BindingName, binding: CatalogBinding) -> Self {
        self.bindings.insert(name, binding);
        self
    }

    /// Declares the host's state budget: the most contexts its state owners hold together.
    pub fn with_state_budget(mut self, capacity: u64) -> Self {
        self.state_budget = Some(capacity);
        self
    }

    /// Declares that the host runs at most one aggregation per program.
    pub fn with_single_aggregation(mut self) -> Self {
        self.single_aggregation = true;
        self
    }

    /// Declares what the host reads by name from the nodes of a plan.
    pub fn with_telemetry_contract(mut self, contract: TelemetryContract) -> Self {
        self.telemetry_contract = Some(contract);
        self
    }

    /// Returns every configuration binding the catalog declares, by name, in name order.
    pub fn bindings(&self) -> impl Iterator<Item = (&BindingName, &CatalogBinding)> {
        self.bindings.iter()
    }

    /// Returns every entry of the recipient directory, by name, in name order.
    pub fn recipients(&self) -> impl Iterator<Item = (&RecipientName, &CatalogRecipient)> {
        self.recipients.iter()
    }

    /// Declares an entry of the recipient directory.
    pub fn with_recipient(mut self, name: RecipientName, recipient: CatalogRecipient) -> Self {
        self.recipients.insert(name, recipient);
        self
    }
}

impl Catalog for MemoryCatalog {
    fn output(&self, id: &OutputId) -> Option<&CatalogOutput> {
        self.outputs.get(id)
    }

    fn binding(&self, name: &BindingName) -> Option<&CatalogBinding> {
        self.bindings.get(name)
    }

    fn state_budget(&self) -> Option<u64> {
        self.state_budget
    }

    fn single_aggregation(&self) -> bool {
        self.single_aggregation
    }

    fn telemetry_contract(&self) -> Option<&TelemetryContract> {
        self.telemetry_contract.as_ref()
    }

    fn recipient(&self, name: &RecipientName) -> Option<&CatalogRecipient> {
        self.recipients.get(name)
    }

    fn output_for(&self, recipients: &[RecipientId]) -> Option<OutputId> {
        self.outputs
            .iter()
            .find(|(_, output)| !output.recipients.is_empty() && output.recipients == recipients)
            .map(|(id, _)| id.clone())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn document() -> Value {
        json!({
            "outputs": {"internal": {"accepts": {"kinds": "all", "timestamps": "mixed"}}},
            "bindings": {
                "test.names": {"type": {"type": "list", "element": {"type": "string"}}, "liveness": "live",
                               "class": "fact", "value": ["a.b", "c.d"]},
                "test.flag": {"type": {"type": "bool"}, "liveness": "startup", "class": "signal", "value": true},
                "test.unknown": {"type": {"type": "bool"}, "liveness": "live", "class": "fact"}
            },
            "recipients": {
                "primary": {"recipients": ["out/primary/0/https://a.example.com"], "host": "out",
                            "endpoint": "https://a.example.com"},
                "https://b.example.com": {"recipients": ["out/additional/0/https://b.example.com",
                                                         "out/additional/1/https://b.example.com"],
                                          "host": "out", "endpoint": "https://b.example.com"}
            }
        })
    }

    #[test]
    fn catalogs_round_trip_through_their_document_form() {
        let catalog: MemoryCatalog = serde_json::from_value(document()).unwrap();
        assert_eq!(serde_json::to_value(&catalog).unwrap(), document());

        let names = catalog.binding(&"test.names".parse().unwrap()).unwrap();
        assert_eq!(names.liveness, Liveness::Live);
        assert!(ValueType::from(ParamType::List(&ParamType::String)).admits(&names.ty));
        assert!(catalog
            .output(&"internal".parse().unwrap())
            .unwrap()
            .recipients
            .is_empty());
        assert_eq!(names.class, BindingClass::Fact);
        assert_eq!(
            catalog.binding(&"test.flag".parse().unwrap()).unwrap().class,
            BindingClass::Signal
        );
        // The directory names one recipient, and every key of an endpoint by its URL.
        let endpoint = catalog.recipient(&"https://b.example.com".parse().unwrap()).unwrap();
        assert_eq!(endpoint.recipients.len(), 2);
        assert!(catalog.recipient(&"mrf".parse().unwrap()).is_none());

        // A binding says what it is: a policy setting is never a binding.
        let mut policy = document();
        policy["bindings"]["test.flag"]["class"] = json!("policy");
        assert!(serde_json::from_value::<MemoryCatalog>(policy).is_err());

        // Every member is checked.
        let mut unknown = document();
        unknown["bindings"]["test.flag"]["default"] = json!(false);
        assert!(serde_json::from_value::<MemoryCatalog>(unknown).is_err());
        assert!(serde_json::from_value::<MemoryCatalog>(json!({"labels": {}})).is_err());

        // The state budget round-trips; a catalog has no scopes.
        let mut budgeted = document();
        budgeted["state_budget"] = json!(1000);
        let catalog: MemoryCatalog = serde_json::from_value(budgeted.clone()).unwrap();
        assert_eq!(catalog.state_budget(), Some(1000));
        assert_eq!(serde_json::to_value(&catalog).unwrap(), budgeted);
        assert_eq!(
            serde_json::from_value::<MemoryCatalog>(document())
                .unwrap()
                .with_state_budget(1000),
            catalog
        );
        // The telemetry contract round-trips.
        let mut contracted = document();
        contracted["telemetry_contract"] = json!({"entries": [{
            "series": "aggregator.flush", "node": "dsd_agg", "operator": "aggregate",
            "reports": "aggregate_flushed_total",
        }]});
        let catalog: MemoryCatalog = serde_json::from_value(contracted.clone()).unwrap();
        assert_eq!(catalog.telemetry_contract().unwrap().entries.len(), 1);
        assert_eq!(serde_json::to_value(&catalog).unwrap(), contracted);
        let mut scoped = document();
        scoped["scopes"] = json!({"post_enrich": {"metrics": {"kinds": "all", "timestamps": "mixed"}}});
        assert!(serde_json::from_value::<MemoryCatalog>(scoped).is_err());
    }

    #[test]
    fn catalogs_resolve_the_bindings_whose_values_they_carry() {
        let catalog: MemoryCatalog = serde_json::from_value(document()).unwrap();
        let resolver: &dyn Catalog = &catalog;
        let name = |name: &str| name.parse::<BindingName>().unwrap();

        assert_eq!(
            resolver.resolve_strings(&name("test.names")),
            Some(vec!["a.b".to_string(), "c.d".to_string()])
        );
        assert_eq!(resolver.resolve_bool(&name("test.flag")), Some(true));
        // No value, a value of another type, or no binding at all: not resolved.
        assert_eq!(resolver.resolve_bool(&name("test.unknown")), None);
        assert_eq!(resolver.resolve_bool(&name("test.names")), None);
        assert_eq!(resolver.resolve_strings(&name("test.flag")), None);
        assert_eq!(resolver.resolve_strings(&name("test.missing")), None);
    }
}
