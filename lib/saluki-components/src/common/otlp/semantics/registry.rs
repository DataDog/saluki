//! Semantic attribute registry — port of upstream `pkg/trace/semantics/registry.go`.
//!
//! Loads the embedded `mappings.json` once at startup and exposes the fallback
//! precedence list for each [`Concept`].

use std::sync::LazyLock;

use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;

use super::Concept;

/// Provenance of an attribute convention.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Datadog,
    Otel,
}

/// The expected type of an attribute's value at lookup time.
///
/// The lookup is type-strict: an attribute registered as `Int64` is only read
/// via the accessor's `get_int64` method. String-typed registrations allow the
/// lookup functions to parse the string into the requested numeric type.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ValueType {
    String,
    Int64,
    Float64,
}

/// One entry in a concept's fallback precedence list.
#[derive(Debug, Clone, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub provider: Provider,
    #[serde(default)]
    pub version: String,
    #[serde(rename = "type")]
    pub value_type: ValueType,
}

#[derive(Debug, Deserialize)]
struct ConceptMapping {
    #[serde(default)]
    #[allow(dead_code)]
    canonical: String,
    fallbacks: Vec<TagInfo>,
}

#[derive(Debug, Deserialize)]
struct RegistryData {
    version: String,
    concepts: FastHashMap<String, ConceptMapping>,
}

/// Semantic attribute registry.
pub struct Registry {
    version: String,
    mappings: FastHashMap<Concept, Vec<TagInfo>>,
}

impl Registry {
    /// Parse a registry from JSON matching the upstream `mappings.json` schema.
    ///
    /// Any concept key that does not correspond to a known [`Concept`] variant
    /// is treated as an error — keeping the enum and the embedded JSON in sync.
    pub fn from_json(json: &str) -> Result<Self, GenericError> {
        let data: RegistryData =
            serde_json::from_str(json).map_err(|e| generic_error!("failed to parse semantic mappings JSON: {}", e))?;

        let mut mappings = FastHashMap::default();
        for (key, mapping) in data.concepts {
            let concept = Concept::from_str(&key)
                .ok_or_else(|| generic_error!("unknown concept in semantic mappings: {}", key))?;
            mappings.insert(concept, mapping.fallbacks);
        }

        Ok(Self {
            version: data.version,
            mappings,
        })
    }

    /// Returns the ordered list of attribute fallbacks for a concept, or
    /// `None` if the concept has no registered mappings.
    pub fn get_attribute_precedence(&self, concept: Concept) -> Option<&[TagInfo]> {
        self.mappings.get(&concept).map(Vec::as_slice)
    }

    /// Version string from the embedded `mappings.json`.
    pub fn version(&self) -> &str {
        &self.version
    }
}

const MAPPINGS_JSON: &str = include_str!("mappings.json");

/// The default registry, loaded from the embedded `mappings.json`.
///
/// This mirrors upstream's `DefaultRegistry()` singleton.
pub static REGISTRY: LazyLock<Registry> =
    LazyLock::new(|| Registry::from_json(MAPPINGS_JSON).expect("embedded semantic mappings.json failed to load"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_mappings_load() {
        // Dereferencing the LazyLock forces parsing; any schema drift or unknown
        // concept would panic here.
        assert!(!REGISTRY.version().is_empty());
    }

    #[test]
    fn every_concept_variant_is_registered() {
        for concept in Concept::ALL {
            assert!(
                REGISTRY.get_attribute_precedence(*concept).is_some(),
                "concept {:?} (\"{}\") has no entry in mappings.json",
                concept,
                concept.as_str(),
            );
        }
    }

    #[test]
    fn http_status_code_has_int_and_string_fallbacks() {
        // Guards against regressions of the exact bug this module was written for.
        let tags = REGISTRY
            .get_attribute_precedence(Concept::HttpStatusCode)
            .expect("http.status_code concept missing");

        let has_int_status = tags
            .iter()
            .any(|t| t.name == "http.status_code" && t.value_type == ValueType::Int64);
        let has_str_status = tags
            .iter()
            .any(|t| t.name == "http.status_code" && t.value_type == ValueType::String);
        let has_int_response = tags
            .iter()
            .any(|t| t.name == "http.response.status_code" && t.value_type == ValueType::Int64);
        let has_str_response = tags
            .iter()
            .any(|t| t.name == "http.response.status_code" && t.value_type == ValueType::String);

        assert!(has_int_status && has_str_status && has_int_response && has_str_response);
    }

    #[test]
    fn from_json_rejects_unknown_concepts() {
        let bad = r#"{
            "version": "x",
            "concepts": {
                "this.is.not.a.real.concept": {
                    "canonical": "x",
                    "fallbacks": []
                }
            }
        }"#;
        assert!(Registry::from_json(bad).is_err());
    }

    #[test]
    fn from_json_rejects_malformed_input() {
        assert!(Registry::from_json("not json").is_err());
    }
}
