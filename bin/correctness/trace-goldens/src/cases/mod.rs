//! Loading and materialization of golden v1.0 trace case specs.
//!
//! Each case lives in its own `<name>.json` spec file, with shared payload defaults in `defaults.json`. A spec
//! describes a tracer payload in logical form (plain strings, no string table) with relative span times. Loading
//! mirrors `loadGoldenCases` in the Go harness; [`CaseSpec::materialize`] resolves the times against a caller-supplied
//! clock, and [`MaterializedCase::request`] encodes the result as a `/v1.0/traces` request.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

mod encode;
pub use self::encode::V1Request;

mod time;
pub use self::time::{resolve_duration, resolve_start, TimeSpec};

/// File name of the shared defaults spec within a cases directory.
pub const DEFAULTS_FILE_NAME: &str = "defaults.json";

/// A span attribute value from a case spec.
///
/// JSON strings become string values and JSON numbers become double values (integers included, so `1` becomes
/// `1.0`), matching the Go harness. Any other JSON type is rejected.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(try_from = "serde_json::Value")]
pub enum AttributeSpec {
    /// A string attribute.
    String(String),

    /// A double attribute.
    Double(f64),
}

impl TryFrom<serde_json::Value> for AttributeSpec {
    type Error = String;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        match value {
            serde_json::Value::String(s) => Ok(Self::String(s)),
            serde_json::Value::Number(n) => n
                .as_f64()
                .map(Self::Double)
                .ok_or_else(|| format!("numeric attribute {} is not representable as a double", n)),
            other => Err(format!(
                "unsupported attribute value {}: only strings and numbers are allowed",
                other
            )),
        }
    }
}

/// A span kind from a case spec, carrying the numeric value of the `idx` `SpanKind` enum.
///
/// Accepted names are `unspecified` (0), `internal` (1), `server` (2), `client` (3), `producer` (4), and `consumer`
/// (5). An empty or absent kind leaves the span kind unset (0).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(try_from = "String")]
pub struct SpanKindSpec(pub u32);

impl TryFrom<String> for SpanKindSpec {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let kind = match value.as_str() {
            "" | "unspecified" => 0,
            "internal" => 1,
            "server" => 2,
            "client" => 3,
            "producer" => 4,
            "consumer" => 5,
            other => return Err(format!("unknown span kind '{}'", other)),
        };
        Ok(Self(kind))
    }
}

/// One span in a case spec.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SpanSpec {
    /// Span ID.
    #[serde(default)]
    pub span_id: u64,

    /// Parent span ID, or `0` for a root span.
    #[serde(default)]
    pub parent_id: u64,

    /// Service name.
    #[serde(default)]
    pub service: String,

    /// Operation name.
    #[serde(default)]
    pub name: String,

    /// Resource name.
    #[serde(default)]
    pub resource: String,

    /// Span type.
    #[serde(default, rename = "type")]
    pub span_type: String,

    /// Span kind.
    #[serde(default)]
    pub kind: SpanKindSpec,

    /// Whether the span is an error.
    #[serde(default)]
    pub error: bool,

    /// Span start: an offset from the test clock, or absolute nanoseconds.
    pub start: TimeSpec,

    /// Span duration: a Go duration string, or raw nanoseconds.
    pub duration: TimeSpec,

    /// Span attributes, in sorted key order.
    #[serde(default)]
    pub attributes: BTreeMap<String, AttributeSpec>,
}

/// One trace chunk in a case spec.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChunkSpec {
    /// Sampling priority. `0` leaves the priority unset on the wire.
    #[serde(default)]
    pub priority: i32,

    /// Trace origin (for example, `synthetics`).
    #[serde(default)]
    pub origin: String,

    /// Whether the tracer dropped the trace.
    #[serde(default)]
    pub dropped_trace: bool,

    /// Spans in the chunk.
    #[serde(default)]
    pub spans: Vec<SpanSpec>,
}

/// The tracer payload in a case spec.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PayloadSpec {
    /// Tracer language name.
    #[serde(default)]
    pub lang: String,

    /// Tracer language version.
    #[serde(default)]
    pub lang_version: String,

    /// Tracer version.
    #[serde(default)]
    pub tracer_version: String,

    /// Application version.
    #[serde(default)]
    pub app_version: String,

    /// Environment.
    #[serde(default)]
    pub env: String,

    /// Container ID.
    ///
    /// Parsed but not encoded, matching the Go harness's `build()`. Cases set the container ID through the
    /// `Datadog-Container-ID` header instead.
    #[serde(default)]
    pub container_id: String,

    /// Runtime ID.
    ///
    /// Parsed but not encoded, matching the Go harness's `build()`.
    #[serde(default)]
    pub runtime_id: String,

    /// Payload-level hostname. Empty leaves it unset.
    #[serde(default)]
    pub hostname: String,

    /// Trace chunks.
    #[serde(default)]
    pub chunks: Vec<ChunkSpec>,
}

/// Install signature agent configuration for a case.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InstallSignatureSpec {
    /// Install ID.
    #[serde(default)]
    pub install_id: String,

    /// Install type.
    #[serde(default)]
    pub install_type: String,

    /// Install time, in Unix seconds.
    #[serde(default)]
    pub install_time: i64,
}

/// Agent configuration tweaks for a case.
///
/// The loader exposes these unchanged; applying them to the system under test is up to the caller.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaseConfig {
    /// Install signature to configure on the agent, if any.
    #[serde(default)]
    pub install_signature: Option<InstallSignatureSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCaseSpec {
    #[serde(default)]
    name: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    config: Option<CaseConfig>,
    #[serde(default)]
    payload: Option<PayloadSpec>,
}

/// One golden scenario: a tracer payload plus optional request headers and agent configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct CaseSpec {
    /// Case name. The golden files are `v1_<name>.json` and `v1_<name>_wire.json`.
    pub name: String,

    /// Human description of the pipeline branches the case exercises.
    pub comment: String,

    /// Request headers that override the default request headers.
    pub headers: BTreeMap<String, String>,

    /// Agent configuration tweaks.
    pub config: CaseConfig,

    /// The tracer payload, with defaults merged.
    pub payload: PayloadSpec,
}

fn read_spec(path: &Path) -> Result<RawCaseSpec, GenericError> {
    let bytes = fs::read(path).with_error_context(|| format!("Failed to read case spec '{}'.", path.display()))?;
    serde_json::from_slice(&bytes).with_error_context(|| format!("Failed to parse case spec '{}'.", path.display()))
}

/// Loads every case spec in `dir`, merging the shared defaults from `defaults.json`.
///
/// Only `lang`, `lang_version`, `tracer_version`, `app_version`, and `env` fall back to the defaults, and only when
/// empty, exactly as in the Go harness. Cases are returned sorted by file name.
///
/// # Errors
///
/// Returns an error if the directory or any spec can't be read, if a spec is malformed (unknown keys, unknown span
/// kinds, unsupported attribute types, invalid times), if a case has no name or no payload, or if there are no cases.
pub fn load_cases(dir: impl AsRef<Path>) -> Result<Vec<CaseSpec>, GenericError> {
    let dir = dir.as_ref();
    let defaults = read_spec(&dir.join(DEFAULTS_FILE_NAME))?.payload.unwrap_or_default();

    let mut files = fs::read_dir(dir)
        .with_error_context(|| format!("Failed to read cases directory '{}'.", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<PathBuf>, _>>()
        .with_error_context(|| format!("Failed to list cases directory '{}'.", dir.display()))?;
    files.retain(|p| {
        p.extension().is_some_and(|ext| ext == "json") && p.file_name().is_some_and(|n| n != DEFAULTS_FILE_NAME)
    });
    files.sort();

    let mut cases = Vec::with_capacity(files.len());
    for file in files {
        let raw = read_spec(&file)?;
        if raw.name.is_empty() {
            return Err(generic_error!("Case spec '{}' has no name.", file.display()));
        }
        let mut payload = raw
            .payload
            .ok_or_else(|| generic_error!("Case spec '{}' has no payload.", file.display()))?;
        merge_default(&mut payload.lang, &defaults.lang);
        merge_default(&mut payload.lang_version, &defaults.lang_version);
        merge_default(&mut payload.tracer_version, &defaults.tracer_version);
        merge_default(&mut payload.app_version, &defaults.app_version);
        merge_default(&mut payload.env, &defaults.env);
        cases.push(CaseSpec {
            name: raw.name,
            comment: raw.comment,
            headers: raw.headers,
            config: raw.config.unwrap_or_default(),
            payload,
        });
    }

    if cases.is_empty() {
        return Err(generic_error!("No case specs found in '{}'.", dir.display()));
    }
    Ok(cases)
}

fn merge_default(field: &mut String, default: &str) {
    if field.is_empty() {
        *field = default.to_string();
    }
}

/// Loads the case specs bundled with this crate.
///
/// # Errors
///
/// Returns an error under the same conditions as [`load_cases`].
pub fn load_bundled_cases() -> Result<Vec<CaseSpec>, GenericError> {
    load_cases(crate::bundled_fixtures_dir().join("cases"))
}

/// A span with every time resolved to absolute nanoseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializedSpan {
    /// Span ID.
    pub span_id: u64,
    /// Parent span ID.
    pub parent_id: u64,
    /// Service name.
    pub service: String,
    /// Operation name.
    pub name: String,
    /// Resource name.
    pub resource: String,
    /// Span type.
    pub span_type: String,
    /// Numeric `idx` `SpanKind` value.
    pub kind: u32,
    /// Whether the span is an error.
    pub error: bool,
    /// Absolute start, in Unix nanoseconds.
    pub start: u64,
    /// Duration, in nanoseconds.
    pub duration: u64,
    /// Attributes, in sorted key order.
    pub attributes: BTreeMap<String, AttributeSpec>,
}

/// A trace chunk with resolved span times.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializedChunk {
    /// Sampling priority. `0` is omitted on the wire.
    pub priority: i32,
    /// Trace origin.
    pub origin: String,
    /// Whether the tracer dropped the trace.
    pub dropped_trace: bool,
    /// Spans in the chunk.
    pub spans: Vec<MaterializedSpan>,
}

/// A tracer payload with resolved span times.
///
/// Holds only the fields the Go harness encodes: container and runtime IDs are not part of the payload.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializedPayload {
    /// Tracer language name.
    pub lang: String,
    /// Tracer language version.
    pub lang_version: String,
    /// Tracer version.
    pub tracer_version: String,
    /// Application version.
    pub app_version: String,
    /// Environment.
    pub env: String,
    /// Payload-level hostname.
    pub hostname: String,
    /// Trace chunks.
    pub chunks: Vec<MaterializedChunk>,
}

/// A case materialized against a specific clock, ready to be encoded and sent.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializedCase {
    /// Case name.
    pub name: String,
    /// Request headers that override the defaults.
    pub headers: BTreeMap<String, String>,
    /// The clock the payload was built against, in Unix nanoseconds.
    ///
    /// Trace comparison needs this value to normalize span starts.
    pub epoch_unix_nanos: u64,
    /// The materialized payload.
    pub payload: MaterializedPayload,
}

impl CaseSpec {
    /// Materializes the case against `now_unix_nanos`, resolving every span time to absolute nanoseconds.
    ///
    /// The caller supplies the clock; nothing reads the system time internally.
    ///
    /// # Errors
    ///
    /// Returns an error if any span time can't be resolved (see [`resolve_start`] and [`resolve_duration`]).
    pub fn materialize(&self, now_unix_nanos: u64) -> Result<MaterializedCase, GenericError> {
        let p = &self.payload;
        let mut chunks = Vec::with_capacity(p.chunks.len());
        for chunk in &p.chunks {
            let mut spans = Vec::with_capacity(chunk.spans.len());
            for s in &chunk.spans {
                let context = || format!("Failed to materialize span {} of case '{}'.", s.span_id, self.name);
                spans.push(MaterializedSpan {
                    span_id: s.span_id,
                    parent_id: s.parent_id,
                    service: s.service.clone(),
                    name: s.name.clone(),
                    resource: s.resource.clone(),
                    span_type: s.span_type.clone(),
                    kind: s.kind.0,
                    error: s.error,
                    start: resolve_start(&s.start, now_unix_nanos).with_error_context(context)?,
                    duration: resolve_duration(&s.duration).with_error_context(context)?,
                    attributes: s.attributes.clone(),
                });
            }
            chunks.push(MaterializedChunk {
                priority: chunk.priority,
                origin: chunk.origin.clone(),
                dropped_trace: chunk.dropped_trace,
                spans,
            });
        }
        Ok(MaterializedCase {
            name: self.name.clone(),
            headers: self.headers.clone(),
            epoch_unix_nanos: now_unix_nanos,
            payload: MaterializedPayload {
                lang: p.lang.clone(),
                lang_version: p.lang_version.clone(),
                tracer_version: p.tracer_version.clone(),
                app_version: p.app_version.clone(),
                env: p.env.clone(),
                hostname: p.hostname.clone(),
                chunks,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cases(files: &[(&str, &str)]) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = format!(
            "trace-goldens-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(unique);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(DEFAULTS_FILE_NAME),
            r#"{"payload": {"lang": "python", "env": "prod"}}"#,
        )
        .unwrap();
        for (name, body) in files {
            fs::write(dir.join(name), body).unwrap();
        }
        dir
    }

    const SPAN: &str = r#"{"span_id": 1, "start": "-2s", "duration": "1ms"}"#;

    fn load_one(span: &str) -> Result<Vec<CaseSpec>, GenericError> {
        let body = format!(r#"{{"name": "x", "payload": {{"chunks": [{{"spans": [{}]}}]}}}}"#, span);
        load_cases(write_cases(&[("x.json", &body)]))
    }

    #[test]
    fn bundled_cases_load_sorted_with_defaults() {
        let cases = load_bundled_cases().unwrap();
        assert_eq!(cases.len(), 26);
        let names: Vec<_> = cases.iter().map(|c| c.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);

        let two_chunks = cases.iter().find(|c| c.name == "two_chunks").unwrap();
        assert_eq!(two_chunks.payload.lang, "python");
        assert_eq!(two_chunks.payload.env, "prod");
        assert_eq!(two_chunks.payload.app_version, "1.2.3");
        assert_eq!(two_chunks.payload.chunks.len(), 2);

        let malformed = cases.iter().find(|c| c.name == "malformed_times").unwrap();
        let spans = &malformed.payload.chunks[0].spans;
        assert_eq!(spans[0].start, TimeSpec::Nanos(1000));
        assert_eq!(spans[1].duration, TimeSpec::Nanos(9_500_000_000_000_000_000));
    }

    #[test]
    fn valid_minimal_spec_loads() {
        let cases = load_one(SPAN).unwrap();
        assert_eq!(cases[0].payload.lang, "python");
        assert_eq!(cases[0].payload.chunks[0].spans[0].kind, SpanKindSpec(0));
    }

    #[test]
    fn attribute_types_map_to_string_or_double() {
        let cases =
            load_one(r#"{"span_id": 1, "start": "-2s", "duration": "1ms", "attributes": {"a": "x", "b": 1}}"#).unwrap();
        let attrs = &cases[0].payload.chunks[0].spans[0].attributes;
        assert_eq!(attrs["a"], AttributeSpec::String("x".into()));
        assert_eq!(attrs["b"], AttributeSpec::Double(1.0));
    }

    #[test]
    fn malformed_specs_are_rejected() {
        // Unknown key.
        assert!(load_one(r#"{"span_id": 1, "start": "-2s", "duration": "1ms", "servce": "x"}"#).is_err());
        // Unknown kind.
        assert!(load_one(r#"{"span_id": 1, "start": "-2s", "duration": "1ms", "kind": "srv"}"#).is_err());
        // Boolean attribute.
        assert!(load_one(r#"{"span_id": 1, "start": "-2s", "duration": "1ms", "attributes": {"a": true}}"#).is_err());
        // Missing name.
        assert!(load_cases(write_cases(&[("x.json", r#"{"payload": {}}"#)])).is_err());
        // Missing payload.
        assert!(load_cases(write_cases(&[("x.json", r#"{"name": "x"}"#)])).is_err());
    }

    #[test]
    fn materialize_resolves_times() {
        let now = 1_700_000_000_000_000_000;
        let cases = load_bundled_cases().unwrap();
        let default = cases.iter().find(|c| c.name == "default").unwrap();
        let m = default.materialize(now).unwrap();
        assert_eq!(m.epoch_unix_nanos, now);
        let spans = &m.payload.chunks[0].spans;
        assert_eq!(spans[0].start, now - 2_000_000_000);
        assert_eq!(spans[0].duration, 100_000_000);
        assert_eq!(spans[1].start, now - 1_990_000_000);

        let malformed = cases.iter().find(|c| c.name == "malformed_times").unwrap();
        let m = malformed.materialize(now).unwrap();
        assert_eq!(m.payload.chunks[0].spans[0].start, 1000);
        assert_eq!(m.payload.chunks[0].spans[1].duration, 9_500_000_000_000_000_000);

        // A clock too early for a "-2s" offset is an error rather than a wrap.
        assert!(default.materialize(1_000).is_err());
    }
}
