//! [`SourceTree`]: configuration values kept together with the reason each one is present.
//!
//! A configuration layer is a tree of sections ending in leaves, where a leaf is one schema setting's
//! entire value. Each leaf records the [`Provenance`] of its value, so a layer can say not just what
//! a setting is but whether anything set it. That is what lets the Agent's own defaults be layered
//! over local configuration without shadowing it, and what lets translation resolve a setting whose
//! meaning depends on being set explicitly (`dd_url` over `site`, for example).
//!
//! Provenance crosses the crate boundary here: [`saluki_config::dynamic::Provenance`] describes a
//! setting in transit on a configuration stream, and [`Provenance`] describes a value in the ADP
//! model. [`SourceTree::set`] is the one place the two meet.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::OnceLock;

use agent_data_plane_config::Provenance;
use saluki_config::dynamic::{ConfigSetting, Provenance as StreamProvenance};
use serde_json::Value;

use crate::saluki_env_overlay;

/// One configuration layer: every value it supplies, with the provenance of each.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SourceTree {
    root: Node,
}

/// A node in a [`SourceTree`].
///
/// The section/leaf split follows the source models' own leaf paths (see [`section_paths`]), so a
/// map- or array-valued setting such as `additional_endpoints` is one leaf holding its whole value
/// rather than a section of entries. A key the models do not know is a leaf for the same reason: with
/// no schema to say otherwise, its value is indivisible.
#[derive(Clone, Debug, PartialEq)]
enum Node {
    /// An intermediate path segment that a modeled leaf lives under.
    Section(BTreeMap<String, Node>),

    /// One setting's entire value, and whether an input set it explicitly.
    Leaf { value: Value, provenance: Provenance },
}

impl SourceTree {
    /// Creates a layer that supplies nothing.
    pub(crate) fn empty() -> Self {
        Self {
            root: Node::Section(BTreeMap::new()),
        }
    }

    /// Creates a layer in which every value present was set explicitly.
    ///
    /// This is the local file and environment. A value is in `value` only because the file set it or
    /// an environment variable supplied it: absent keys are absent and null-valued keys were dropped
    /// while the base was built. So presence here *is* the record that an input set the value, and no
    /// provenance is being inferred after the fact.
    pub(crate) fn all_explicit(value: Value) -> Self {
        Self {
            root: Node::from_value(value, &mut Vec::new(), Provenance::Explicit),
        }
    }

    /// Creates a layer from a configuration producer's complete set of settings.
    pub(crate) fn from_settings(settings: &[ConfigSetting]) -> Self {
        let mut tree = Self::empty();
        for setting in settings {
            tree.set(setting);
        }

        tree
    }

    /// Applies one setting to this layer, replacing whatever it held for that key.
    ///
    /// The key is in the producer's own flat, possibly dotted form and expands into sections, while
    /// the value is inserted whole: an object-valued setting keeps entry keys that themselves contain
    /// dots (intake URLs, for example) intact.
    pub(crate) fn set(&mut self, setting: &ConfigSetting) {
        let provenance = match setting.provenance {
            StreamProvenance::Explicit => Provenance::Explicit,
            StreamProvenance::Default => Provenance::Default,
        };

        let mut node = &mut self.root;
        let mut segments = setting.key.split('.').peekable();
        while let Some(segment) = segments.next() {
            // A leaf standing where a section is needed is replaced, so a later, more specific key
            // wins over an earlier, shallower one.
            if matches!(node, Node::Leaf { .. }) {
                *node = Node::Section(BTreeMap::new());
            }
            let Node::Section(children) = node else {
                unreachable!("a leaf was just replaced with a section")
            };

            if segments.peek().is_none() {
                children.insert(
                    segment.to_string(),
                    Node::Leaf {
                        value: setting.value.clone(),
                        provenance,
                    },
                );
                return;
            }

            node = children
                .entry(segment.to_string())
                .or_insert_with(|| Node::Section(BTreeMap::new()));
        }
    }

    /// Layers `overlay` over this layer and returns the result.
    ///
    /// A value `overlay` set explicitly wins. A value `overlay` merely defaulted does not: it carries
    /// no intent, so it must not shadow a value this layer set explicitly. When neither layer set a
    /// value explicitly, `overlay` still wins, because a later default is a better answer than an
    /// earlier one.
    ///
    /// This is what makes the Agent's configuration stream safe to treat as authoritative. The Agent
    /// publishes every setting it knows about, including the ones nobody configured, so an
    /// unqualified overlay would let its schema defaults erase the local file.
    pub(crate) fn overlay(&self, overlay: &SourceTree) -> SourceTree {
        SourceTree {
            root: Node::merge(&self.root, &overlay.root),
        }
    }

    /// Returns the values in this layer, discarding provenance.
    ///
    /// This is the shape the source models deserialize from.
    pub(crate) fn to_value(&self) -> Value {
        self.root.to_value()
    }

    /// Returns whether an input set `key`'s value explicitly.
    ///
    /// A key this layer does not supply reports [`Provenance::Default`]: a setting nobody supplied
    /// takes the schema default, which is the same thing nothing set.
    pub(crate) fn provenance(&self, key: &str) -> Provenance {
        match self.root.get(key.split('.')) {
            Some(Node::Leaf { provenance, .. }) => *provenance,
            // A section is not a setting, so nothing set a value here.
            Some(Node::Section(_)) | None => Provenance::Default,
        }
    }
}

impl Node {
    /// Splits `value` into sections and leaves according to the source models' leaf paths.
    ///
    /// Every value that survives becomes a leaf with `provenance`.
    fn from_value(value: Value, path: &mut Vec<String>, provenance: Provenance) -> Self {
        // Recurse only through a section that is actually shaped like one; anywhere else the value is
        // one setting, whole.
        let Value::Object(fields) = value else {
            return Node::Leaf { value, provenance };
        };
        if !path.is_empty() && !section_paths().contains(path.as_slice()) {
            return Node::Leaf {
                value: Value::Object(fields),
                provenance,
            };
        }

        let mut children = BTreeMap::new();
        for (key, field) in fields {
            path.push(key.clone());
            children.insert(key, Node::from_value(field, path, provenance));
            path.pop();
        }

        Node::Section(children)
    }

    /// Layers `overlay` over `base`. See [`SourceTree::overlay`].
    fn merge(base: &Node, overlay: &Node) -> Node {
        match (base, overlay) {
            (Node::Section(base_children), Node::Section(overlay_children)) => {
                let mut children = base_children.clone();
                for (key, overlay_child) in overlay_children {
                    let merged = match children.remove(key) {
                        Some(base_child) => Node::merge(&base_child, overlay_child),
                        None => overlay_child.clone(),
                    };
                    children.insert(key.clone(), merged);
                }

                Node::Section(children)
            }
            (
                _,
                Node::Leaf {
                    provenance: Provenance::Default,
                    ..
                },
            ) if base.has_explicit() => base.clone(),
            // An explicit leaf, or a default with nothing explicit beneath it, replaces the base.
            // A shape mismatch between the layers lands here too, and the overlay wins: the models
            // agree on which paths are sections, so a mismatch means one layer holds a value the
            // models do not describe, and there is no meaningful way to merge it.
            _ => overlay.clone(),
        }
    }

    /// Returns whether any input explicitly set a value at or beneath this node.
    fn has_explicit(&self) -> bool {
        match self {
            Node::Leaf { provenance, .. } => *provenance == Provenance::Explicit,
            Node::Section(children) => children.values().any(Node::has_explicit),
        }
    }

    /// Returns the node at `path`, if this node has one.
    fn get<'a>(&self, mut path: impl Iterator<Item = &'a str>) -> Option<&Node> {
        match path.next() {
            None => Some(self),
            Some(segment) => match self {
                Node::Section(children) => children.get(segment)?.get(path),
                Node::Leaf { .. } => None,
            },
        }
    }

    /// Returns the values at and beneath this node, discarding provenance.
    fn to_value(&self) -> Value {
        match self {
            Node::Leaf { value, .. } => value.clone(),
            Node::Section(children) => Value::Object(
                children
                    .iter()
                    .map(|(key, child)| (key.clone(), child.to_value()))
                    .collect(),
            ),
        }
    }
}

/// Every proper prefix of a modeled leaf path, across both source models.
///
/// A path in this set is a section that a tree descends through; a path that is a leaf, or that no model
/// describes, is absent. Built once from the schema's own leaf tables, so it cannot drift from the
/// models.
fn section_paths() -> &'static HashSet<Vec<String>> {
    static SECTIONS: OnceLock<HashSet<Vec<String>>> = OnceLock::new();
    SECTIONS.get_or_init(|| {
        let mut sections = HashSet::new();
        let mut add_prefixes = |segments: Vec<String>| {
            for end in 1..segments.len() {
                sections.insert(segments[..end].to_vec());
            }
        };
        for path in datadog_agent_config::datadog_leaf_paths() {
            add_prefixes(path.iter().map(|segment| (*segment).to_string()).collect());
        }
        for path in saluki_env_overlay::leaf_paths() {
            add_prefixes(path.to_vec());
        }
        sections
    })
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::Provenance;
    use saluki_config::dynamic::{ConfigSetting, Provenance as StreamProvenance};
    use serde_json::json;

    use super::SourceTree;

    /// Builds an Agent layer from `(key, value, provenance)` triples.
    fn agent_layer(settings: &[(&str, serde_json::Value, StreamProvenance)]) -> SourceTree {
        let settings: Vec<_> = settings
            .iter()
            .map(|(key, value, provenance)| ConfigSetting::new(*key, value.clone(), *provenance))
            .collect();

        SourceTree::from_settings(&settings)
    }

    #[test]
    fn local_values_are_explicit() {
        let tree = SourceTree::all_explicit(json!({ "site": "datadoghq.eu", "apm_config": { "enabled": true } }));

        assert_eq!(tree.provenance("site"), Provenance::Explicit);
        assert_eq!(tree.provenance("apm_config.enabled"), Provenance::Explicit);
    }

    #[test]
    fn an_unsupplied_key_is_defaulted() {
        let tree = SourceTree::all_explicit(json!({ "site": "datadoghq.eu" }));

        assert_eq!(tree.provenance("dd_url"), Provenance::Default);
        // A section is not a setting, so querying one reports the same as querying a key nothing
        // supplied.
        assert_eq!(tree.provenance("apm_config"), Provenance::Default);
    }

    #[test]
    fn settings_expand_dotted_keys_and_keep_values_whole() {
        let tree = agent_layer(&[
            ("dogstatsd_port", json!(8125), StreamProvenance::Explicit),
            ("otlp_config.traces.enabled", json!(true), StreamProvenance::Explicit),
            // An object-valued setting is one leaf: these entry keys contain dots and must survive.
            (
                "additional_endpoints",
                json!({ "https://app.datadoghq.eu": ["deadbeef"] }),
                StreamProvenance::Explicit,
            ),
        ]);

        assert_eq!(
            tree.to_value(),
            json!({
                "dogstatsd_port": 8125,
                "otlp_config": { "traces": { "enabled": true } },
                "additional_endpoints": { "https://app.datadoghq.eu": ["deadbeef"] },
            })
        );
        assert_eq!(tree.provenance("otlp_config.traces.enabled"), Provenance::Explicit);
        // The dotted entry key inside the object value is not a path.
        assert_eq!(
            tree.provenance("additional_endpoints.https://app.datadoghq.eu"),
            Provenance::Default
        );
    }

    #[test]
    fn a_setting_carries_its_own_provenance() {
        let tree = agent_layer(&[
            ("site", json!("datadoghq.eu"), StreamProvenance::Explicit),
            ("dd_url", json!("https://app.datadoghq.com"), StreamProvenance::Default),
        ]);

        assert_eq!(tree.provenance("site"), Provenance::Explicit);
        assert_eq!(tree.provenance("dd_url"), Provenance::Default);
    }

    #[test]
    fn a_later_setting_replaces_an_earlier_one() {
        let tree = agent_layer(&[
            ("dd_url", json!("https://app.datadoghq.com"), StreamProvenance::Default),
            ("dd_url", json!("https://app.datadoghq.eu"), StreamProvenance::Explicit),
        ]);

        assert_eq!(tree.to_value(), json!({ "dd_url": "https://app.datadoghq.eu" }));
        assert_eq!(tree.provenance("dd_url"), Provenance::Explicit);
    }

    // This is issue #1965: the Core Agent streams `dd_url` at its schema default even when the
    // operator configured only `site`, so an unqualified overlay would shadow the local file.
    #[test]
    fn a_defaulted_overlay_value_does_not_shadow_a_local_one() {
        let local = SourceTree::all_explicit(json!({ "dd_url": "https://vector.example.com" }));
        let agent = agent_layer(&[("dd_url", json!("https://app.datadoghq.com"), StreamProvenance::Default)]);

        let merged = local.overlay(&agent);

        assert_eq!(merged.to_value(), json!({ "dd_url": "https://vector.example.com" }));
        assert_eq!(merged.provenance("dd_url"), Provenance::Explicit);
    }

    #[test]
    fn an_explicit_overlay_value_wins() {
        let local = SourceTree::all_explicit(json!({ "dd_url": "https://vector.example.com" }));
        let agent = agent_layer(&[("dd_url", json!("https://app.datadoghq.eu"), StreamProvenance::Explicit)]);

        let merged = local.overlay(&agent);

        assert_eq!(merged.to_value(), json!({ "dd_url": "https://app.datadoghq.eu" }));
        assert_eq!(merged.provenance("dd_url"), Provenance::Explicit);
    }

    #[test]
    fn a_defaulted_overlay_value_supplies_a_key_nobody_configured() {
        let agent = agent_layer(&[("dd_url", json!("https://app.datadoghq.com"), StreamProvenance::Default)]);

        let merged = SourceTree::empty().overlay(&agent);

        // The effective value is still the default URL; what changes is that translation can now see
        // that nothing set it.
        assert_eq!(merged.to_value(), json!({ "dd_url": "https://app.datadoghq.com" }));
        assert_eq!(merged.provenance("dd_url"), Provenance::Default);
    }

    #[test]
    fn a_section_merges_per_leaf() {
        let local = SourceTree::all_explicit(json!({
            "apm_config": { "compute_stats_by_span_kind": true, "enable_rare_sampler": true }
        }));
        let agent = agent_layer(&[
            (
                "apm_config.enable_rare_sampler",
                json!(false),
                StreamProvenance::Explicit,
            ),
            // A default must not erase the local value in the same section.
            (
                "apm_config.compute_stats_by_span_kind",
                json!(false),
                StreamProvenance::Default,
            ),
        ]);

        let merged = local.overlay(&agent);

        assert_eq!(
            merged.to_value(),
            json!({ "apm_config": { "compute_stats_by_span_kind": true, "enable_rare_sampler": false } })
        );
    }

    #[test]
    fn a_map_valued_leaf_is_replaced_wholesale_not_key_unioned() {
        let local = SourceTree::all_explicit(json!({ "additional_endpoints": { "https://a.example.com": ["k1"] } }));
        let agent = agent_layer(&[(
            "additional_endpoints",
            json!({ "https://b.example.com": ["k2"] }),
            StreamProvenance::Explicit,
        )]);

        let merged = local.overlay(&agent);

        assert_eq!(
            merged.to_value(),
            json!({ "additional_endpoints": { "https://b.example.com": ["k2"] } })
        );
    }

    #[test]
    fn an_array_leaf_is_replaced_rather_than_adjoined() {
        let local = SourceTree::all_explicit(json!({ "dogstatsd_mapper_profiles": ["a"] }));
        let agent = agent_layer(&[("dogstatsd_mapper_profiles", json!(["b"]), StreamProvenance::Explicit)]);

        assert_eq!(
            local.overlay(&agent).to_value(),
            json!({ "dogstatsd_mapper_profiles": ["b"] })
        );
    }

    #[test]
    fn an_unmodeled_object_key_is_one_leaf() {
        // No model describes `not_a_datadog_key`, so there is no schema to split its value by and it
        // is replaced whole rather than merged entry by entry.
        let local = SourceTree::all_explicit(json!({ "not_a_datadog_key": { "a": 1, "b": 2 } }));
        let agent = agent_layer(&[("not_a_datadog_key", json!({ "a": 9 }), StreamProvenance::Explicit)]);

        assert_eq!(
            local.overlay(&agent).to_value(),
            json!({ "not_a_datadog_key": { "a": 9 } })
        );
    }

    #[test]
    fn keys_from_both_layers_coexist() {
        let local = SourceTree::all_explicit(json!({ "site": "datadoghq.eu" }));
        let agent = agent_layer(&[("dogstatsd_port", json!(9125), StreamProvenance::Explicit)]);

        assert_eq!(
            local.overlay(&agent).to_value(),
            json!({ "site": "datadoghq.eu", "dogstatsd_port": 9125 })
        );
    }
}
