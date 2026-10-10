//! The catalog of what the host declares: outputs, configuration bindings, scope inputs and exits, and its state
//! budget.

use std::collections::BTreeMap;

use serde::{de::Error as _, Deserialize, Serialize};
use serde_json::Value;

use crate::{
    canonical::PlanHash,
    ir::{BindingName, ExitName, InputName, OutputId, ParamType, RecipientId, ScopeName, ValueType},
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

    /// Returns the port type of what leaves the given scope by the given exit in the host's default program, if the
    /// host declares it.
    ///
    /// An exit's type is informational, for tools that show an artifact's ends: validation checks the plan an artifact
    /// is composed into as a whole, so what reaches an exit is typed from the scope's inputs. The default declares no
    /// exit.
    fn scope_exit(&self, scope: &ScopeName, exit: &ExitName) -> Option<PortType> {
        let _ = (scope, exit);
        None
    }

    /// Returns the host's state budget: the most contexts its state owners hold together, every lineage of every
    /// state owner included, or `None` if the host does not bound them together.
    ///
    /// Validation warns about an artifact's state owner whose context limit exceeds it (`state_budget`). The default
    /// declares none.
    fn state_budget(&self) -> Option<u64> {
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
/// Serialized as an object with four members, each of which may be left out when empty: `outputs`, from output ID to
/// the output's `accepts`, `fingerprint`, and `recipients`; `bindings`, from binding name to the binding's `type` (a
/// [`ValueType`]), `liveness` (`live` or `startup`), and optional `value`; `scopes`, from scope name to the scope's
/// `inputs` and `exits`, each a map from name to port type; and `state_budget`, the host's state budget. A scope
/// without exits is written as the map of its inputs alone, as documents written before scopes had exits are, and
/// either form is read. A host exports its catalog in this form, so that plans can be validated and previewed away from
/// it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCatalog {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    outputs: BTreeMap<OutputId, CatalogOutput>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    bindings: BTreeMap<BindingName, CatalogBinding>,

    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "deserialize_scopes"
    )]
    scopes: BTreeMap<ScopeName, CatalogScope>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    state_budget: Option<u64>,
}

/// What a catalog declares about one scope: the port types of its inputs and of its exits.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CatalogScope {
    inputs: BTreeMap<InputName, PortType>,
    exits: BTreeMap<ExitName, PortType>,
}

/// The two forms a scope's document takes, for errors that say how to correct one.
const SCOPE_FORMS: &str = "a scope is written either as a map from input name to port type, or as an object with an \
                           `inputs` map and an optional `exits` map, each from name to port type";

/// A scope written with both its ends.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScopeEnds {
    inputs: BTreeMap<InputName, PortType>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    exits: BTreeMap<ExitName, PortType>,
}

impl Serialize for CatalogScope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.exits.is_empty() {
            self.inputs.serialize(serializer)
        } else {
            ScopeEnds {
                inputs: self.inputs.clone(),
                exits: self.exits.clone(),
            }
            .serialize(serializer)
        }
    }
}

impl CatalogScope {
    /// Reads a scope from its document: in the form with both ends if it has an `inputs` or `exits` member, and as the
    /// map of its inputs otherwise, or if that form does not read and the map of its inputs, so named, does.
    ///
    /// The form is chosen by its members, rather than by trying each, so that an error says what is wrong in the form
    /// the document is written in.
    fn from_document(document: &Value) -> Result<Self, serde_json::Error> {
        let inputs_only = |document| {
            BTreeMap::<InputName, PortType>::deserialize(document).map(|inputs| Self {
                inputs,
                exits: BTreeMap::new(),
            })
        };
        let has_ends = document
            .as_object()
            .is_some_and(|members| members.contains_key("inputs") || members.contains_key("exits"));
        if !has_ends {
            return inputs_only(document);
        }
        match ScopeEnds::deserialize(document) {
            Ok(ScopeEnds { inputs, exits }) => Ok(Self { inputs, exits }),
            Err(error) => inputs_only(document).map_err(|_| error),
        }
    }
}

/// Reads the `scopes` member of a catalog document: each scope in either of its forms, with an error that names the
/// scope and says what is wrong in it.
fn deserialize_scopes<'de, D>(deserializer: D) -> Result<BTreeMap<ScopeName, CatalogScope>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    BTreeMap::<ScopeName, Value>::deserialize(deserializer)?
        .into_iter()
        .map(|(name, document)| {
            let scope = CatalogScope::from_document(&document)
                .map_err(|error| D::Error::custom(format!("scope `{}`: {}; {}", name, error, SCOPE_FORMS)))?;
            Ok((name, scope))
        })
        .collect()
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
        self.scopes.entry(scope).or_default().inputs.insert(input, port_type);
        self
    }

    /// Declares one exit of a scope, with the port type of what leaves the scope by it in the default program.
    pub fn with_scope_exit(mut self, scope: ScopeName, exit: ExitName, port_type: PortType) -> Self {
        self.scopes.entry(scope).or_default().exits.insert(exit, port_type);
        self
    }

    /// Declares the host's state budget: the most contexts its state owners hold together.
    pub fn with_state_budget(mut self, capacity: u64) -> Self {
        self.state_budget = Some(capacity);
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
        self.scopes.get(scope)?.inputs.get(input).copied()
    }

    fn scope_exit(&self, scope: &ScopeName, exit: &ExitName) -> Option<PortType> {
        self.scopes.get(scope)?.exits.get(exit).copied()
    }

    fn state_budget(&self) -> Option<u64> {
        self.state_budget
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
    fn scopes_with_exits_and_the_state_budget_round_trip_and_older_documents_still_read() {
        let mut document = document();
        document["scopes"]["dogstatsd_pre_aggregation"] = json!({
            "inputs": {"metrics": {"kinds": "all", "timestamps": "mixed"}},
            "exits": {"aggregated": {"kinds": ["rate", "gauge"], "timestamps": "timestamped"}},
        });
        document["state_budget"] = json!(1000);
        let catalog: MemoryCatalog = serde_json::from_value(document.clone()).unwrap();
        // A scope without exits keeps the older form; one with exits writes both ends.
        assert_eq!(serde_json::to_value(&catalog).unwrap(), document);

        let scope = "dogstatsd_pre_aggregation".parse::<ScopeName>().unwrap();
        assert_eq!(
            catalog.scope_input(&scope, &"metrics".parse().unwrap()),
            Some(PortType::ANY)
        );
        assert_eq!(
            catalog.scope_exit(&scope, &"aggregated".parse().unwrap()),
            Some(PortType {
                kinds: KindSet::of(&[crate::model::Kind::Rate, crate::model::Kind::Gauge]),
                timestamps: TimestampForm::Timestamped,
            })
        );
        assert_eq!(catalog.scope_exit(&scope, &"passthrough".parse().unwrap()), None);
        assert_eq!(catalog.state_budget(), Some(1000));

        // Built in memory, the same catalog.
        let built = serde_json::from_value::<MemoryCatalog>(super::tests::document())
            .unwrap()
            .with_scope_input(scope.clone(), "metrics".parse().unwrap(), PortType::ANY)
            .with_scope_exit(
                scope.clone(),
                "aggregated".parse().unwrap(),
                catalog.scope_exit(&scope, &"aggregated".parse().unwrap()).unwrap(),
            )
            .with_state_budget(1000);
        assert_eq!(built, catalog);

        // A scope written with both ends, but no exits, reads too, and is written in the older form.
        let mut both = super::tests::document();
        both["scopes"]["post_enrich"] =
            json!({"inputs": {"metrics": {"kinds": ["gauge"], "timestamps": "untimestamped"}}});
        let read: MemoryCatalog = serde_json::from_value(both).unwrap();
        assert_eq!(serde_json::to_value(&read).unwrap(), super::tests::document());

        // Members of a scope's ends are checked.
        let mut unknown = super::tests::document();
        unknown["scopes"]["post_enrich"] = json!({"inputs": {}, "entries": {}});
        assert!(serde_json::from_value::<MemoryCatalog>(unknown).is_err());

        // A scope in the older form may have inputs named like the members of the newer one.
        let mut named = super::tests::document();
        named["scopes"]["post_enrich"] = json!({"inputs": {"kinds": "all", "timestamps": "mixed"}});
        let read: MemoryCatalog = serde_json::from_value(named.clone()).unwrap();
        assert_eq!(
            read.scope_input(&"post_enrich".parse().unwrap(), &"inputs".parse().unwrap()),
            Some(PortType::ANY)
        );
        assert_eq!(serde_json::to_value(&read).unwrap(), named);
    }

    #[test]
    fn a_scope_that_does_not_read_is_reported_with_its_name_and_what_is_wrong_in_it() {
        let misspelled = json!({"kinds": "all", "timestamps": "untimestampd"});
        let older = json!({"metrics": misspelled});
        let newer = json!({
            "inputs": {"metrics": {"kinds": "all", "timestamps": "mixed"}},
            "exits": {"aggregated": misspelled},
        });
        for scope in [older, newer] {
            let mut document = super::tests::document();
            document["scopes"]["post_enrich"] = scope;
            let error = serde_json::from_value::<MemoryCatalog>(document)
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("scope `post_enrich`: unknown variant `untimestampd`"),
                "{}",
                error
            );
            assert!(error.ends_with(SCOPE_FORMS), "{}", error);
        }
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
