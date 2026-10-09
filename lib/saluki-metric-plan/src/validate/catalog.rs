//! The catalog of what the host declares: outputs, configuration bindings, and scope inputs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    canonical::PlanHash,
    ir::{BindingName, InputName, OutputId, ParamType, RecipientId, ScopeName, ValueType},
    model::PortType,
    predicate::BindingResolver,
};

/// An output the host declares.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogOutput {
    /// The records the output accepts.
    pub accepts: PortType,

    /// The digest of the output's resolved binding.
    pub fingerprint: PlanHash,

    /// The output's physical recipients.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<RecipientId>,
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

/// A configuration binding the host declares.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogBinding {
    /// The type of the binding's value. A parameter bound to it must [admit][ValueType::admits] this type.
    #[serde(rename = "type")]
    pub ty: ValueType,

    /// Whether the value can change while the process runs.
    pub liveness: Liveness,

    /// The binding's current value, written as a parameter of its type is, if the catalog carries it.
    ///
    /// A host's exported catalog carries the values, so that a plan whose bindings are unresolved, such as an exported
    /// program, can be previewed against them away from the host: the catalog is then a [`BindingResolver`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// What the host declares, which plans may refer to.
///
/// The validator checks every output a plan declares, every configuration binding its parameters use, and every input
/// of every scope it declares against the catalog. The host implements it from its own registry of outputs and
/// bindings; off the host, an exported [`MemoryCatalog`] document implements the same.
///
/// Every catalog also resolves the bindings whose values it carries: `dyn Catalog` is a [`BindingResolver`].
pub trait Catalog {
    /// Returns the output with the given ID, if the host declares it.
    fn output(&self, id: &OutputId) -> Option<&CatalogOutput>;

    /// Returns the configuration binding with the given name, if the host declares it.
    fn binding(&self, name: &BindingName) -> Option<&CatalogBinding>;

    /// Returns the port type of the given input of the given scope, if the host declares it.
    fn scope_input(&self, scope: &ScopeName, input: &InputName) -> Option<PortType>;
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
/// Serialized as an object with three members, each a map that may be left out when empty: `outputs`, from output ID
/// to the output's `accepts`, `fingerprint`, and `recipients`; `bindings`, from binding name to the binding's `type`
/// (a [`ValueType`]), `liveness` (`live` or `startup`), and optional `value`; and `scopes`, from scope name to a map
/// from input name to port type. A host exports its catalog in this form, so that plans can be validated and previewed
/// away from it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCatalog {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    outputs: BTreeMap<OutputId, CatalogOutput>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    bindings: BTreeMap<BindingName, CatalogBinding>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    scopes: BTreeMap<ScopeName, BTreeMap<InputName, PortType>>,
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

    /// Declares a configuration binding, without its value.
    pub fn with_binding(self, name: BindingName, ty: ParamType, liveness: Liveness) -> Self {
        self.with_catalog_binding(
            name,
            CatalogBinding {
                ty: ValueType::from(&ty),
                liveness,
                value: None,
            },
        )
    }

    /// Declares a configuration binding.
    pub fn with_catalog_binding(mut self, name: BindingName, binding: CatalogBinding) -> Self {
        self.bindings.insert(name, binding);
        self
    }

    /// Declares one input of a scope.
    pub fn with_scope_input(mut self, scope: ScopeName, input: InputName, port_type: PortType) -> Self {
        self.scopes.entry(scope).or_default().insert(input, port_type);
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

    fn scope_input(&self, scope: &ScopeName, input: &InputName) -> Option<PortType> {
        self.scopes.get(scope)?.get(input).copied()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::{KindSet, TimestampForm};

    fn document() -> Value {
        json!({
            "outputs": {"internal": {"accepts": {"kinds": "all", "timestamps": "mixed"},
                                     "fingerprint": format!("sha256:{}", "0".repeat(64))}},
            "bindings": {
                "test.names": {"type": {"type": "list", "element": {"type": "string"}}, "liveness": "live",
                               "value": ["a.b", "c.d"]},
                "test.flag": {"type": {"type": "bool"}, "liveness": "startup", "value": true},
                "test.unknown": {"type": {"type": "bool"}, "liveness": "live"}
            },
            "scopes": {"post_enrich": {"metrics": {"kinds": ["gauge"], "timestamps": "untimestamped"}}}
        })
    }

    #[test]
    fn catalogs_round_trip_through_their_document_form() {
        let catalog: MemoryCatalog = serde_json::from_value(document()).unwrap();
        assert_eq!(serde_json::to_value(&catalog).unwrap(), document());

        let names = catalog.binding(&"test.names".parse().unwrap()).unwrap();
        assert_eq!(names.liveness, Liveness::Live);
        assert!(ValueType::from(ParamType::List(&ParamType::String)).admits(&names.ty));
        assert_eq!(
            catalog.scope_input(&"post_enrich".parse().unwrap(), &"metrics".parse().unwrap()),
            Some(PortType {
                kinds: KindSet::of(&[crate::model::Kind::Gauge]),
                timestamps: TimestampForm::Untimestamped,
            })
        );
        assert!(catalog
            .output(&"internal".parse().unwrap())
            .unwrap()
            .recipients
            .is_empty());

        // Every member is checked.
        let mut unknown = document();
        unknown["bindings"]["test.flag"]["default"] = json!(false);
        assert!(serde_json::from_value::<MemoryCatalog>(unknown).is_err());
        assert!(serde_json::from_value::<MemoryCatalog>(json!({"labels": {}})).is_err());
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
