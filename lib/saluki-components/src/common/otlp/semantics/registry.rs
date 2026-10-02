//! Parses mappings that find an attribute by meaning rather than by a fixed name.
//!
//! A [`Concept`] such as HTTP status code can have several attribute names or value types across instrumentation
//! versions. A [`Registry`] lists these alternatives, called fallbacks, in lookup order. Lookups use the first
//! fallback whose conditions match and whose value can be read as the requested type.
//!
//! The embedded `mappings.json` supplies the defaults; remote configurations can supply replacement registries.
//! [`SemanticRegistryProvider`](super::SemanticRegistryProvider) hides that choice from consumers. Parsing follows
//! upstream `pkg/trace/semantics/registry.go`.

use std::{
    borrow::Cow,
    fmt,
    sync::{Arc, LazyLock},
};

use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

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

/// A predicate that must match before a fallback tag can be used.
///
/// A condition reads an attribute by exact key—no fallback chaining—and
/// holds up to two predicate fields, all of which must hold (logical AND). A
/// condition with no predicates set always matches.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Condition {
    /// Exact attribute name to test, without looking up other names for the same concept.
    ///
    /// An absent or `null` JSON field becomes an empty name, matching upstream decoding.
    #[serde(default, deserialize_with = "null_as_default")]
    pub attribute: String,
    /// When set, requires the attribute's presence (or absence) to match.
    #[serde(default)]
    pub present: Option<bool>,
    /// When set, requires the attribute's string value to equal this.
    #[serde(default)]
    pub eq: Option<String>,
}

/// One entry in a concept's fallback precedence list.
#[derive(Debug, Clone, Deserialize)]
pub struct TagInfo {
    /// Attribute name to read. An absent or `null` JSON field becomes an empty name.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    pub provider: Provider,
    #[serde(default, deserialize_with = "null_as_default")]
    pub version: String,
    #[serde(rename = "type")]
    pub value_type: ValueType,
    /// Conditions that must all match before this attribute can supply the concept's value.
    ///
    /// An absent or `null` list is empty; a `null` element has no predicates and always matches.
    #[serde(default, deserialize_with = "null_elements_as_default")]
    pub when: Vec<Condition>,
}

/// Treats JSON `null` as the Rust type's default, matching upstream decoding of zero-valued fields.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Treats a JSON `null` list as empty and `null` elements as defaults, matching upstream decoding.
fn null_elements_as_default<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let elements = Option::<Vec<Option<T>>>::deserialize(deserializer)?;
    Ok(elements.into_iter().flatten().map(Option::unwrap_or_default).collect())
}

/// Maps semantic concepts to ordered attribute names, types, and conditions for lookups.
///
/// For example, the HTTP status-code concept can read either `http.response.status_code` or `http.status_code`.
/// A registry holds one complete set of mappings; remote registries replace rather than extend the embedded set.
#[derive(Clone)]
pub struct Registry {
    version: String,
    content_hash: String,
    fingerprint: u64,
    mappings: FastHashMap<Concept, Vec<TagInfo>>,
}

impl Registry {
    /// Parses a registry JSON string, rejecting malformed or unsupported entries.
    ///
    /// # Errors
    ///
    /// Uses the same validation as [`Self::from_slice`]. Errors may quote payload keys and must not be sent to the
    /// remote endpoint.
    pub fn from_json(json: &str) -> Result<Self, GenericError> {
        Self::from_slice(json.as_bytes())
    }

    /// Parses registry JSON, rejecting unsupported entries.
    ///
    /// Use this for embedded mappings, where unsupported entries indicate a build defect. Remote payloads use
    /// [`Registry::from_slice_permissive`] to keep supported entries.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid JSON, a missing or empty `concepts` object, or a missing or empty
    /// `metadata.content_hash` string. Also rejects unknown concepts, malformed mappings, and fallbacks with invalid
    /// fields, including unknown providers or value types. Errors may quote payload keys; do not send them to the
    /// remote endpoint.
    pub fn from_slice(json: &[u8]) -> Result<Self, GenericError> {
        let (registry, skipped) = Self::from_slice_permissive(json)?;
        if !skipped.is_empty() {
            return Err(generic_error!(
                "Registry JSON has parts the registry cannot represent: {}",
                skipped
            ));
        }
        Ok(registry)
    }

    /// Parses remote registry JSON while retaining the entries this binary understands.
    ///
    /// Remote mappings may contain concepts or conventions added after this binary was built. Unknown concepts,
    /// malformed mappings, and fallbacks with invalid fields (including unknown providers or value types) are skipped
    /// and recorded in [`SkipReport`]. The result is accepted even if every entry was skipped; missing mappings are
    /// not filled from the embedded registry. The report is for local logging only.
    ///
    /// A `null` mapping or absent or `null` `fallbacks` gives an empty fallback list. See [`TagInfo`] and [`Condition`]
    /// for field defaults, and [`Registry::version`] for version handling.
    ///
    /// The [fingerprint](Registry::fingerprint) includes skipped entries.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid JSON, a missing or empty `concepts` object, or a missing or empty
    /// `metadata.content_hash` string. These requirements apply before skipping entries. Errors never quote the
    /// payload, so they can be reported to the remote endpoint.
    pub(crate) fn from_slice_permissive(json: &[u8]) -> Result<(Self, SkipReport), GenericError> {
        let document: Value = serde_json::from_slice(json).map_err(malformed_error)?;
        let concepts = document.get("concepts").and_then(Value::as_object);
        let content_hash = document
            .pointer("/metadata/content_hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        check_required(concepts.is_some_and(|c| !c.is_empty()), content_hash)?;

        let mut skipped = SkipReport::default();
        let mut mappings = FastHashMap::default();
        for (key, mapping) in concepts.into_iter().flatten() {
            let Some(concept) = Concept::from_str(key) else {
                skipped.unknown_concepts.push(key.clone());
                continue;
            };
            let entries: &[Value] = match mapping {
                // Go decodes a `null` mapping to an empty fallback list.
                Value::Null => &[],
                Value::Object(map) => match map.get("fallbacks") {
                    None | Some(Value::Null) => &[],
                    Some(Value::Array(entries)) => entries.as_slice(),
                    _ => {
                        skipped.malformed_concepts.push(key.clone());
                        continue;
                    }
                },
                _ => {
                    skipped.malformed_concepts.push(key.clone());
                    continue;
                }
            };
            let mut fallbacks = Vec::with_capacity(entries.len());
            for (position, entry) in entries.iter().enumerate() {
                match TagInfo::deserialize(entry) {
                    Ok(tag) => fallbacks.push(tag),
                    Err(_) => skipped.fallbacks.push((key.clone(), position)),
                }
            }
            mappings.insert(concept, fallbacks);
        }

        let registry = Self {
            version: document
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            content_hash: content_hash.to_owned(),
            fingerprint: fingerprint_for(json),
            mappings,
        };
        Ok((registry, skipped))
    }

    /// Returns the registry's `metadata.content_hash` label for logs and status.
    ///
    /// This value is not computed or verified here; different mappings can carry the same label. Use
    /// [`Self::fingerprint`] to detect changes.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Returns a non-cryptographic hash of the original JSON bytes for change detection within this process.
    ///
    /// Compare fingerprints to decide whether to rebuild derived data, such as peer tag keys for APM stats.
    /// Formatting, metadata, and skipped entries also affect the hash. A collision can leave derived data stale;
    /// this is not an integrity check.
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// Returns the ordered list of attribute fallbacks for a concept, or
    /// `None` if the concept has no registered mappings.
    pub fn get_attribute_precedence(&self, concept: Concept) -> Option<&[TagInfo]> {
        self.mappings.get(&concept).map(Vec::as_slice)
    }

    /// Returns the document's `version` label, or `""` if absent, `null`, or not a string.
    ///
    /// This label is informational; it does not control parsing or update selection.
    pub fn version(&self) -> &str {
        &self.version
    }
}

/// Records the concept keys and fallback positions skipped by [`Registry::from_slice_permissive`].
///
/// Its [`Display`](fmt::Display) reports counts and a bounded list of escaped, shortened keys.
/// It quotes payload keys: log it locally, but never report it to the remote endpoint.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SkipReport {
    /// Concept keys that don't correspond to a known [`Concept`].
    unknown_concepts: Vec<String>,

    /// Keys of concepts with malformed mappings or `fallbacks`.
    malformed_concepts: Vec<String>,

    /// Unsupported fallbacks, as (concept key, zero-based position).
    fallbacks: Vec<(String, usize)>,
}

impl SkipReport {
    /// Returns `true` if nothing was skipped.
    pub(crate) fn is_empty(&self) -> bool {
        self.unknown_concepts.is_empty() && self.malformed_concepts.is_empty() && self.fallbacks.is_empty()
    }
}

impl fmt::Display for SkipReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("nothing skipped");
        }
        let fallbacks: Vec<String> = self
            .fallbacks
            .iter()
            .take(MAX_LISTED_KEYS)
            .map(|(key, position)| format!("{:?}[{position}]", shorten_key(key)))
            .collect();
        let quoted = |keys: &[String]| -> Vec<String> {
            keys.iter()
                .take(MAX_LISTED_KEYS)
                .map(|key| format!("{:?}", shorten_key(key)))
                .collect()
        };
        let sections: [(&str, usize, Vec<String>); 3] = [
            (
                "unknown concepts",
                self.unknown_concepts.len(),
                quoted(&self.unknown_concepts),
            ),
            (
                "malformed concepts",
                self.malformed_concepts.len(),
                quoted(&self.malformed_concepts),
            ),
            ("unrepresentable fallbacks", self.fallbacks.len(), fallbacks),
        ];
        let mut separator = "";
        for (label, count, listed) in sections.into_iter().filter(|(_, count, _)| *count > 0) {
            let more = if count > listed.len() { ", ..." } else { "" };
            write!(f, "{separator}{label}: {count} ({}{more})", listed.join(", "))?;
            separator = "; ";
        }
        Ok(())
    }
}

/// Caps the keys listed in each skip-report section.
const MAX_LISTED_KEYS: usize = 10;

/// Caps the characters shown from each key in a skip report.
const MAX_KEY_CHARS: usize = 64;

fn shorten_key(key: &str) -> Cow<'_, str> {
    if key.chars().count() <= MAX_KEY_CHARS {
        return Cow::Borrowed(key);
    }
    let mut shortened: String = key.chars().take(MAX_KEY_CHARS).collect();
    shortened.push('…');
    Cow::Owned(shortened)
}

/// Reports a JSON error by position without quoting the payload.
fn malformed_error(e: serde_json::Error) -> GenericError {
    generic_error!(
        "Registry JSON is malformed at line {}, column {}.",
        e.line(),
        e.column()
    )
}

/// Rejects documents without concepts or a content-hash label, matching upstream registry validation.
fn check_required(has_concepts: bool, content_hash: &str) -> Result<(), GenericError> {
    if !has_concepts {
        return Err(generic_error!("Registry JSON contains no concepts."));
    }
    if content_hash.is_empty() {
        return Err(generic_error!("Registry JSON is missing `metadata.content_hash`."));
    }
    Ok(())
}

const MAPPINGS_JSON: &str = include_str!("mappings.json");

/// Hashes a raw registry document for change detection.
fn fingerprint_for(json: &[u8]) -> u64 {
    use std::hash::Hasher as _;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(json);
    hasher.finish()
}

/// The default semantic mappings compiled into the binary from `mappings.json`.
///
/// Parsed once on first access. Components use [`SemanticRegistryProvider`](super::SemanticRegistryProvider) rather
/// than reading this static directly, so they can use remote mappings when available.
///
/// # Panics
///
/// Panics on first access if the embedded mappings are invalid or unsupported, which indicates a build defect.
pub static EMBEDDED_REGISTRY: LazyLock<Arc<Registry>> = LazyLock::new(|| {
    Arc::new(Registry::from_json(MAPPINGS_JSON).expect("embedded semantic mappings.json failed to load"))
});

/// Wraps `concepts` in a registry document with the required metadata.
#[cfg(test)]
pub(crate) fn registry_json(concepts: &str) -> String {
    format!(r#"{{"version":"test","metadata":{{"content_hash":"test"}},"concepts":{concepts}}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_json_accepts_when_conditions() {
        let with_when = registry_json(
            r#"{
                "http.status_code": {
                    "canonical": "http.status_code",
                    "fallbacks": [
                        {"name": "rpc.response.status_code", "provider": "otel", "type": "int64",
                         "when": [{"attribute": "rpc.system", "eq": "grpc"}]}
                    ]
                }
            }"#,
        );
        let registry = Registry::from_json(&with_when).expect("registry with a `when` clause should parse");
        let tags = registry
            .get_attribute_precedence(Concept::HttpStatusCode)
            .expect("concept missing");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].when.len(), 1);
        assert_eq!(tags[0].when[0].attribute, "rpc.system");
        assert_eq!(tags[0].when[0].eq.as_deref(), Some("grpc"));
        assert_eq!(tags[0].when[0].present, None);
    }

    #[test]
    fn embedded_mappings_use_when_for_grpc_fallback() {
        // The `rpc.response.status_code` fallbacks are gated on the span being a
        // gRPC span; the registry must preserve those conditions.
        let registry = &EMBEDDED_REGISTRY;
        let tags = registry
            .get_attribute_precedence(Concept::RpcGrpcStatusCode)
            .expect("rpc.grpc.status_code concept missing");
        let gated = tags.iter().filter(|t| !t.when.is_empty()).count();
        assert_eq!(gated, 4, "expected four when-gated fallbacks");
    }

    #[test]
    fn every_concept_variant_is_registered() {
        for concept in Concept::ALL {
            assert!(
                EMBEDDED_REGISTRY.get_attribute_precedence(*concept).is_some(),
                "concept {:?} (\"{}\") has no entry in mappings.json",
                concept,
                concept.as_str(),
            );
        }
    }

    #[test]
    fn http_status_code_has_int_and_string_fallbacks() {
        // Guards against regressions of the exact bug this module was written for.
        let registry = &EMBEDDED_REGISTRY;
        let tags = registry
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
        let bad = registry_json(r#"{"this.is.not.a.real.concept": {"canonical": "x", "fallbacks": []}}"#);
        let error = Registry::from_json(&bad)
            .err()
            .expect("unknown concept should be rejected");
        assert_eq!(
            error.to_string(),
            r#"Registry JSON has parts the registry cannot represent: unknown concepts: 1 "#.to_owned()
                + r#"("this.is.not.a.real.concept")"#
        );
    }

    fn parse_strict(json: &str) -> Result<Registry, GenericError> {
        Registry::from_json(json)
    }

    fn parse_permissive(json: &str) -> Result<Registry, GenericError> {
        Registry::from_slice_permissive(json.as_bytes()).map(|(registry, _)| registry)
    }

    type Parser = fn(&str) -> Result<Registry, GenericError>;

    const PARSERS: [(&str, Parser); 2] = [("strict", parse_strict), ("permissive", parse_permissive)];

    // Ported from upstream `TestNewRegistryFromJSON_MalformedJSON`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L27
    #[test]
    fn parsers_reject_malformed_input_by_position() {
        for (name, parse) in PARSERS {
            let error = parse("not json").err().expect("malformed input should be rejected");
            assert_eq!(
                error.to_string(),
                "Registry JSON is malformed at line 1, column 2.",
                "{name}"
            );
        }
    }

    #[test]
    fn from_json_rejects_what_the_permissive_parser_skips() {
        let json = registry_json(
            r#"{
                "http.status_code": {"fallbacks": [{"name": "http.status_code", "provider": "X", "type": "string"}]},
                "http.method": "not an object"
            }"#,
        );
        let error = Registry::from_json(&json)
            .err()
            .expect("unrepresentable parts should be rejected");
        assert_eq!(
            error.to_string(),
            r#"Registry JSON has parts the registry cannot represent: malformed concepts: 1 ("http.method"); "#
                .to_owned()
                + r#"unrepresentable fallbacks: 1 ("http.status_code"[0])"#
        );
    }

    #[test]
    fn from_json_rejects_absent_concepts_key_as_empty() {
        // Upstream treats an absent `concepts` key as an empty map.
        let json = r#"{"version":"0.1.0","metadata":{"content_hash":"hash-a"}}"#;
        let error = Registry::from_json(json)
            .err()
            .expect("absent concepts key should be rejected");
        assert_eq!(error.to_string(), "Registry JSON contains no concepts.");
    }

    // Ported from upstream `TestNewRegistryFromJSON_ValidJSON`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L17
    #[test]
    fn from_slice_parses_the_embedded_mappings() {
        let registry = Registry::from_slice(MAPPINGS_JSON.as_bytes()).expect("embedded mappings should parse");
        for concept in Concept::ALL {
            assert!(
                registry.get_attribute_precedence(*concept).is_some(),
                "concept {concept:?} should be present"
            );
        }
    }

    // Ported from upstream `TestNewRegistryFromJSON_EmptyConcepts`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L32
    #[test]
    fn parsers_reject_empty_concepts() {
        let json = r#"{"version":"0.1.0","metadata":{"content_hash":"hash-a"},"concepts":{}}"#;
        for (name, parse) in PARSERS {
            let error = parse(json).err().expect("empty concepts should be rejected");
            assert_eq!(error.to_string(), "Registry JSON contains no concepts.", "{name}");
        }
    }

    // Ported from upstream `TestNewRegistryFromJSON_MissingContentHash`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L37
    #[test]
    fn parsers_reject_missing_content_hash() {
        let json = r#"{"version":"0.1.0","concepts":{"db.statement":{"canonical":"db.statement","fallbacks":[
            {"name":"db.statement","provider":"datadog","type":"string"}]}}}"#;
        for (name, parse) in PARSERS {
            let error = parse(json).err().expect("missing content hash should be rejected");
            assert_eq!(
                error.to_string(),
                "Registry JSON is missing `metadata.content_hash`.",
                "{name}"
            );
        }
    }

    #[test]
    fn from_json_rejects_empty_content_hash() {
        let json = r#"{"version":"0.1.0","metadata":{"content_hash":""},"concepts":{"db.statement":{
            "canonical":"db.statement","fallbacks":[{"name":"db.statement","provider":"datadog","type":"string"}]}}}"#;
        let error = Registry::from_json(json)
            .err()
            .expect("empty content hash should be rejected");
        assert_eq!(error.to_string(), "Registry JSON is missing `metadata.content_hash`.");
    }

    #[test]
    fn content_hash_is_the_declared_label() {
        assert_eq!(
            EMBEDDED_REGISTRY.content_hash(),
            "sha256:d2d19e7f53f8d3cb4abe235d3a83b3dcb3399cadcc21cc2ebb5d5e724143b7f3"
        );
    }

    /// An upstream fixture; placeholder concepts `alpha` and `beta` are skipped as unknown.
    const FINGERPRINT_BASE_JSON: &str = r#"{
  "version":"1.0.0",
  "git_commit":"commit-a",
  "timestamp":"2026-01-01T00:00:00Z",
  "metadata":{"content_hash":"same-declared-hash"},
  "concepts":{
    "alpha":{"canonical":"ignored.alpha","fallbacks":[
      {"name":"alpha.one","provider":"datadog","version":"1.0","type":"string"},
      {"name":"alpha.two","provider":"otel","type":"float64",
       "when":[{"attribute":"span.kind","present":true,"eq":"client"}]}
    ]},
    "beta":{"canonical":"ignored.beta","fallbacks":[{"name":"beta.one","provider":"datadog","type":"string"}]}
  }
}"#;

    #[track_caller]
    fn fingerprint_of(json: &str) -> u64 {
        let (registry, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("registry should parse");
        assert_eq!(registry.content_hash(), "same-declared-hash");
        assert_eq!(skipped.unknown_concepts, ["alpha", "beta"]);
        registry.fingerprint()
    }

    // Ported from upstream `TestFingerprintChangesWithParsedMappings`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L64
    #[test]
    fn fingerprint_changes_with_parsed_mappings() {
        let base = fingerprint_of(FINGERPRINT_BASE_JSON);
        let cases = [
            (
                "fallback name",
                FINGERPRINT_BASE_JSON.replacen(r#""name":"alpha.one""#, r#""name":"alpha.changed""#, 1),
            ),
            (
                "condition",
                FINGERPRINT_BASE_JSON.replacen(r#""eq":"client""#, r#""eq":"server""#, 1),
            ),
        ];
        for (name, json) in cases {
            assert_ne!(
                json, FINGERPRINT_BASE_JSON,
                "{name}: the replacement must change the payload"
            );
            assert_ne!(fingerprint_of(&json), base, "{name}");
        }
    }

    // Ported from upstream `TestFingerprintChangesWithJSONPresentationAndMetadata`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L78
    #[test]
    fn fingerprint_changes_with_json_presentation_and_metadata() {
        let base = fingerprint_of(FINGERPRINT_BASE_JSON);

        let reformatted = FINGERPRINT_BASE_JSON.replace("  ", "    ");
        assert_ne!(fingerprint_of(&reformatted), base);

        let metadata_changed =
            FINGERPRINT_BASE_JSON.replacen(r#""git_commit":"commit-a""#, r#""git_commit":"commit-b""#, 1);
        assert_ne!(fingerprint_of(&metadata_changed), base);
    }

    // Ported from upstream `TestFingerprintDeterministicAcrossLoads`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L91
    #[test]
    fn fingerprint_is_deterministic_across_loads() {
        let want = fingerprint_of(FINGERPRINT_BASE_JSON);
        for _ in 0..100 {
            assert_eq!(fingerprint_of(FINGERPRINT_BASE_JSON), want);
        }
    }

    // Ported from upstream `TestRegistryEqual_IdenticalPayloadBytesRegardlessOfSource`:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L111
    #[test]
    fn fingerprint_matches_for_identical_payload_bytes() {
        let (remote, skipped) =
            Registry::from_slice_permissive(MAPPINGS_JSON.as_bytes()).expect("embedded mappings should parse");
        assert!(skipped.is_empty(), "{skipped}");
        assert_eq!(remote.fingerprint(), EMBEDDED_REGISTRY.fingerprint());
    }

    // Ported from upstream `TestRegistryEqual_DifferentPayloadBytesWhen*` tests:
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e83ccbb0e68aeb99461d1a1d902927/pkg/trace/semantics/registry_update_test.go#L120-L142
    #[test]
    fn fingerprint_differs_when_payload_bytes_change() {
        const DB_STATEMENT: &str = concat!(
            r#"{"db.statement":{"canonical":"db.statement","fallbacks":["#,
            r#"{"name":"db.statement","provider":"datadog","type":"string"}]}}"#
        );
        const HTTP_METHOD: &str = concat!(
            r#"{"http.method":{"canonical":"http.method","fallbacks":["#,
            r#"{"name":"http.method","provider":"otel","type":"string"}]}}"#
        );
        let payload = |version: &str, content_hash: &str, concepts: &str| {
            format!(r#"{{"version":"{version}","metadata":{{"content_hash":"{content_hash}"}},"concepts":{concepts}}}"#)
        };
        let fingerprint = |json: &str| Registry::from_json(json).expect("registry should parse").fingerprint();

        let base = payload("1.0.0", "hash-a", DB_STATEMENT);
        let cases = [
            ("version", payload("2.0.0", "hash-a", DB_STATEMENT)),
            (
                "mappings under the same declared hash",
                payload("1.0.0", "hash-a", HTTP_METHOD),
            ),
            (
                "declared hash over the same mappings",
                payload("1.0.0", "hash-b", DB_STATEMENT),
            ),
        ];
        for (name, changed) in cases {
            assert_ne!(fingerprint(&changed), fingerprint(&base), "{name}");
        }
    }

    #[test]
    fn permissive_skips_unknown_concepts_and_keeps_known_ones() {
        let json = registry_json(
            r#"{
                "alpha": {"canonical": "alpha", "fallbacks": [{"name": "a", "provider": "otel", "type": "string"}]},
                "db.statement": {"canonical": "db.statement", "fallbacks": [
                    {"name": "db.statement", "provider": "datadog", "type": "string"}]},
                "zeta": {"canonical": "zeta", "fallbacks": []}
            }"#,
        );
        let (registry, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("known concepts should load");
        let tags = registry
            .get_attribute_precedence(Concept::DbStatement)
            .expect("known concept should be kept");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "db.statement");
        assert_eq!(registry.mappings.len(), 1);
        assert_eq!(skipped.unknown_concepts, ["alpha", "zeta"]);
        assert!(skipped.malformed_concepts.is_empty() && skipped.fallbacks.is_empty());
    }

    #[test]
    fn permissive_accepts_a_payload_whose_concepts_are_all_unknown() {
        // Upstream rejects only an empty `concepts` map; unknown keys are not a rejection.
        let json = registry_json(r#"{"alpha": {"canonical": "alpha", "fallbacks": []}}"#);
        let (registry, skipped) =
            Registry::from_slice_permissive(json.as_bytes()).expect("unknown concepts are not a rejection");
        assert!(registry.mappings.is_empty());
        assert_eq!(skipped.unknown_concepts, ["alpha"]);
    }

    #[test]
    fn permissive_skips_unrepresentable_fallbacks_and_keeps_the_rest() {
        let json = registry_json(
            r#"{"http.status_code": {"canonical": "http.status_code", "fallbacks": [
                {"name": "unknown.provider", "provider": "vendor", "type": "string"},
                {"name": "kept.first", "provider": "otel", "type": "int64"},
                {"name": "unknown.type", "provider": "otel", "type": "bool"},
                {"name": "absent.type", "provider": "otel"},
                {"name": 5, "provider": "otel", "type": "string"},
                {"name": "bad.when", "provider": "otel", "type": "string", "when": [{"attribute": 5}]},
                "not an object",
                {"name": "kept.second", "provider": "datadog", "type": "string", "future_field": true}
            ]}}"#,
        );
        let (registry, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("payload should load");
        let names: Vec<&str> = registry
            .get_attribute_precedence(Concept::HttpStatusCode)
            .expect("concept should be kept")
            .iter()
            .map(|tag| tag.name.as_str())
            .collect();
        assert_eq!(names, ["kept.first", "kept.second"]);
        let positions: Vec<usize> = skipped.fallbacks.iter().map(|(_, position)| *position).collect();
        assert_eq!(positions, [0, 2, 3, 4, 5, 6]);
        assert!(skipped.fallbacks.iter().all(|(key, _)| key == "http.status_code"));
        assert!(skipped.unknown_concepts.is_empty() && skipped.malformed_concepts.is_empty());
    }

    #[test]
    fn parsers_treat_absent_fallbacks_as_empty() {
        let json = registry_json(r#"{"db.statement": {"canonical": "db.statement"}}"#);
        for (name, parse) in PARSERS {
            let registry = parse(&json).expect("absent fallbacks should parse");
            assert_eq!(
                registry.get_attribute_precedence(Concept::DbStatement).map(<[_]>::len),
                Some(0),
                "{name}"
            );
        }
    }

    #[test]
    fn parsers_treat_null_fallbacks_as_empty() {
        let json = registry_json(r#"{"db.statement": {"canonical": "db.statement", "fallbacks": null}}"#);
        for (name, parse) in PARSERS {
            let registry = parse(&json).expect("null fallbacks should parse");
            assert_eq!(
                registry.get_attribute_precedence(Concept::DbStatement).map(<[_]>::len),
                Some(0),
                "{name}"
            );
        }
    }

    #[test]
    fn permissive_skips_malformed_concepts() {
        let json = registry_json(
            r#"{
                "db.statement": "not an object",
                "http.method": {"canonical": "http.method", "fallbacks": {"name": "http.method"}},
                "http.route": {"canonical": "http.route", "fallbacks": [
                    {"name": "http.route", "provider": "otel", "type": "string"}]}
            }"#,
        );
        let (registry, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("payload should load");
        assert!(registry.get_attribute_precedence(Concept::DbStatement).is_none());
        assert!(registry.get_attribute_precedence(Concept::HttpMethod).is_none());
        assert_eq!(
            registry.get_attribute_precedence(Concept::HttpRoute).map(<[_]>::len),
            Some(1)
        );
        assert_eq!(skipped.malformed_concepts, ["db.statement", "http.method"]);
    }

    #[test]
    fn permissive_reads_version_and_content_hash() {
        let json = r#"{"version":"2.1.0","metadata":{"content_hash":"hash-a"},"concepts":{"alpha":{}}}"#;
        let (registry, _) = Registry::from_slice_permissive(json.as_bytes()).expect("payload should load");
        assert_eq!(registry.version(), "2.1.0");
        assert_eq!(registry.content_hash(), "hash-a");

        let non_string_version = r#"{"version":2,"metadata":{"content_hash":"hash-a"},"concepts":{"alpha":{}}}"#;
        let (registry, _) =
            Registry::from_slice_permissive(non_string_version.as_bytes()).expect("a non-string version is not fatal");
        assert_eq!(registry.version(), "");
    }

    #[test]
    fn permissive_errors_never_quote_skipped_parts() {
        let without_hash = r#"{"version":"0.1.0","concepts":{"secret.concept":{"fallbacks":[
            {"name":"secret.name","provider":"secret-provider","type":"string"}]}}}"#;
        let empty_hash = r#"{"metadata":{"content_hash":""},"concepts":{"secret.concept":{}}}"#;
        for json in [without_hash, empty_hash] {
            let error = Registry::from_slice_permissive(json.as_bytes())
                .err()
                .expect("missing content hash should be rejected");
            assert_eq!(error.to_string(), "Registry JSON is missing `metadata.content_hash`.");
        }

        let malformed = r#"{"concepts":{"secret.concept":{"fallbacks":[secret]}}}"#;
        let error = Registry::from_slice_permissive(malformed.as_bytes())
            .err()
            .expect("malformed JSON should be rejected");
        assert_eq!(error.to_string(), "Registry JSON is malformed at line 1, column 45.");
    }

    #[test]
    fn skip_report_display_names_skipped_parts() {
        let json = registry_json(
            r#"{
                "alpha": {},
                "beta": {},
                "db.statement": {"fallbacks": [{"name": "x", "provider": "vendor", "type": "string"}]},
                "http.method": "not an object",
                "http.route": {"fallbacks": [
                    {"name": "http.route", "provider": "otel", "type": "string"},
                    {"name": "http.route", "provider": "otel"}]}
            }"#,
        );
        let (_, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("payload should load");
        assert_eq!(
            skipped.to_string(),
            r#"unknown concepts: 2 ("alpha", "beta"); malformed concepts: 1 ("http.method"); "#.to_owned()
                + r#"unrepresentable fallbacks: 2 ("db.statement"[0], "http.route"[1])"#
        );
        assert_eq!(SkipReport::default().to_string(), "nothing skipped");
    }

    #[test]
    fn parsers_treat_null_mapping_as_empty() {
        let json = registry_json(r#"{"db.statement": null}"#);
        for (name, parse) in PARSERS {
            let registry = parse(&json).expect("null mapping should parse");
            assert_eq!(
                registry.get_attribute_precedence(Concept::DbStatement).map(<[_]>::len),
                Some(0),
                "{name}"
            );
        }
    }

    // Go's `encoding/json` leaves a `null` slice element at its zero value, a condition with no predicates.
    #[test]
    fn parsers_read_null_condition_as_empty() {
        let json = registry_json(
            r#"{"db.statement": {"fallbacks": [
                {"name": "db.statement", "provider": "datadog", "type": "string", "when": [null]}]}}"#,
        );
        for (name, parse) in PARSERS {
            let registry = parse(&json).expect("a null condition should parse");
            let tags = registry
                .get_attribute_precedence(Concept::DbStatement)
                .expect("concept should be kept");
            assert_eq!(tags.len(), 1, "{name}");
            let [condition] = tags[0].when.as_slice() else {
                panic!("{name}: expected one condition, got {:?}", tags[0].when);
            };
            assert_eq!(condition.attribute, "", "{name}");
            assert_eq!(condition.present, None, "{name}");
            assert_eq!(condition.eq, None, "{name}");
        }
    }

    #[test]
    fn parsers_read_null_version_and_when_as_defaults() {
        let json = registry_json(
            r#"{"db.statement": {"fallbacks": [
                {"name": "db.statement", "provider": "datadog", "type": "string", "version": null, "when": null}]}}"#,
        );
        for (name, parse) in PARSERS {
            let registry = parse(&json).expect("null version and when should parse");
            let tags = registry
                .get_attribute_precedence(Concept::DbStatement)
                .expect("concept should be kept");
            assert_eq!(tags.len(), 1, "{name}");
            assert_eq!(tags[0].version, "", "{name}");
            assert!(tags[0].when.is_empty(), "{name}");
        }
    }

    #[test]
    fn fallback_missing_or_null_name_defaults_to_empty_string() {
        // Go's `encoding/json` leaves an absent or `null` name at its zero value.
        let json = registry_json(
            r#"{"db.statement": {"fallbacks": [
                {"provider": "datadog", "type": "string"},
                {"name": null, "provider": "otel", "type": "string"}
            ]}}"#,
        );
        let (registry, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("payload should load");
        assert!(skipped.fallbacks.is_empty(), "{skipped}");
        let tags = registry
            .get_attribute_precedence(Concept::DbStatement)
            .expect("concept should be kept");
        assert_eq!(tags.len(), 2, "both fallbacks should be kept rather than skipped");
        assert!(tags.iter().all(|tag| tag.name.is_empty()));
    }

    #[test]
    fn skip_report_display_shortens_long_keys_on_a_char_boundary() {
        // A multi-byte character sits right at the cutoff; truncating by byte count would panic.
        let long_key: String = "é".repeat(70);
        let json = registry_json(&format!(r#"{{"{long_key}": {{}}}}"#));
        let (_, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("payload should load");
        let shortened: String = long_key.chars().take(64).chain(std::iter::once('…')).collect();
        assert_eq!(skipped.to_string(), format!("unknown concepts: 1 ({shortened:?})"));
    }

    #[test]
    fn parsers_read_absent_or_null_condition_attribute_as_empty() {
        let json = registry_json(
            r#"{"db.statement": {"fallbacks": [
                {"name": "db.statement", "provider": "datadog", "type": "string",
                 "when": [{"eq": "grpc"}, {"attribute": null, "present": true}]}]}}"#,
        );
        for (name, parse) in PARSERS {
            let registry = parse(&json).expect("conditions without an attribute should parse");
            let when = &registry
                .get_attribute_precedence(Concept::DbStatement)
                .expect("concept should be kept")[0]
                .when;
            assert_eq!(when.len(), 2, "{name}");
            assert_eq!(when[0].attribute, "", "{name}");
            assert_eq!(when[0].eq.as_deref(), Some("grpc"), "{name}");
            assert_eq!(when[1].attribute, "", "{name}");
            assert_eq!(when[1].present, Some(true), "{name}");
        }
    }

    #[test]
    fn skip_report_display_escapes_keys_and_caps_each_list() {
        let unknown: Vec<String> = (0..12).map(|i| format!(r#""k{i:02}": {{}}"#)).collect();
        let bad_entries = [r#"{"provider": "vendor"}"#; 11].join(", ");
        let json = registry_json(&format!(
            r#"{{"a\nbreak": {{}}, {}, "db.statement": {{"fallbacks": [{bad_entries}]}}}}"#,
            unknown.join(", ")
        ));
        let (_, skipped) = Registry::from_slice_permissive(json.as_bytes()).expect("payload should load");
        let listed_unknown: Vec<String> = std::iter::once(r#""a\nbreak""#.to_owned())
            .chain((0..9).map(|i| format!(r#""k{i:02}""#)))
            .collect();
        let listed_fallbacks: Vec<String> = (0..10).map(|i| format!(r#""db.statement"[{i}]"#)).collect();
        assert_eq!(
            skipped.to_string(),
            format!(
                "unknown concepts: 13 ({}, ...); unrepresentable fallbacks: 11 ({}, ...)",
                listed_unknown.join(", "),
                listed_fallbacks.join(", ")
            )
        );
    }
}
