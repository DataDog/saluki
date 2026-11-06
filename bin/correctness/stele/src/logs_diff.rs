//! Field-by-field differences between two logs, and the path patterns that select them.
//!
//! Each difference has a location-qualified path, such as `envelope.status`, `envelope_properties.attr`, or
//! `content.otel.trace_id`, so that a property found in a different location on each side shows up as a difference.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

use crate::Log;

/// Envelope fields held as raw values, in the order they're compared.
const RAW_ENVELOPE_FIELDS: &[&str] = &["message", "status", "hostname", "service", "ddsource", "timestamp"];

/// Where a property was found in a log.
///
/// Envelope fields are always at the top level, so they have no location of their own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FieldLocation {
    /// A top-level key other than the envelope fields.
    EnvelopeProperties,

    /// A property parsed from the envelope `message`.
    Content,
}

impl FieldLocation {
    /// Returns the path prefix used for fields in this location.
    const fn prefix(self) -> &'static str {
        match self {
            Self::EnvelopeProperties => "envelope_properties",
            Self::Content => "content",
        }
    }
}

/// The kind of a difference between two paired logs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// Both sides have the field, with different values.
    Value,

    /// Only the baseline has the field.
    Missing,

    /// Only the comparison has the field.
    Extra,

    /// The field is in a different location on each side. The path uses the baseline's location.
    Location,
}

/// A single difference between two paired logs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldDiff {
    /// Location-qualified path of the field, such as `envelope.status` or `content.otel.trace_id`.
    pub path: String,

    /// The kind of difference.
    pub kind: DiffKind,

    /// The baseline value, if present.
    pub baseline: Option<Value>,

    /// The comparison value, if present.
    pub comparison: Option<Value>,
}

impl fmt::Display for FieldDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |value: &Option<Value>| value.as_ref().map_or_else(|| "<absent>".to_string(), Value::to_string);
        write!(
            f,
            "{}: {:?} (baseline: {}, comparison: {})",
            self.path,
            self.kind,
            show(&self.baseline),
            show(&self.comparison)
        )
    }
}

/// Diffs two logs field by field.
///
/// Envelope fields are compared as raw values, except that the raw `message` is left out when both logs have content,
/// since the content properties are compared instead. Encode-time `timestamp` values within
/// `encode_timestamp_tolerance_ms` of each other count as equal. `ddtags` are compared as sets, one difference per tag.
/// Properties are compared per location, a property found in a different location on each side is a location
/// difference, and objects are compared key by key.
pub fn diff_logs(baseline: &Log, comparison: &Log, encode_timestamp_tolerance_ms: u64) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();

    // When both sides have content, the raw `message` string is just its serialization, so compare the content
    // properties instead to avoid reporting every content difference twice.
    let both_have_content = baseline.content_properties().is_some() && comparison.content_properties().is_some();
    for field in RAW_ENVELOPE_FIELDS {
        if *field == "message" && both_have_content {
            continue;
        }
        let baseline_value = baseline.envelope_field(field);
        let comparison_value = comparison.envelope_field(field);
        if *field == "timestamp"
            && encode_timestamps_match(baseline_value, comparison_value, encode_timestamp_tolerance_ms)
        {
            continue;
        }
        diff_optional(
            format!("envelope.{}", field),
            baseline_value,
            comparison_value,
            &mut diffs,
        );
    }

    for tag in baseline.ddtags().difference(comparison.ddtags()) {
        diff_optional(
            "envelope.ddtags".to_string(),
            Some(&Value::String(tag.clone())),
            None,
            &mut diffs,
        );
    }
    for tag in comparison.ddtags().difference(baseline.ddtags()) {
        diff_optional(
            "envelope.ddtags".to_string(),
            None,
            Some(&Value::String(tag.clone())),
            &mut diffs,
        );
    }

    diff_properties(baseline, comparison, &mut diffs);
    diffs
}

type PropertyLocations<'a> = [(FieldLocation, &'a BTreeMap<String, Value>); 2];

fn property_locations<'a>(log: &'a Log, empty: &'a BTreeMap<String, Value>) -> PropertyLocations<'a> {
    [
        (FieldLocation::EnvelopeProperties, log.envelope_properties()),
        (FieldLocation::Content, log.content_properties().unwrap_or(empty)),
    ]
}

fn diff_properties(baseline: &Log, comparison: &Log, diffs: &mut Vec<FieldDiff>) {
    let empty = BTreeMap::new();
    let baseline_locations = property_locations(baseline, &empty);
    let comparison_locations = property_locations(comparison, &empty);

    let keys = baseline_locations
        .iter()
        .chain(comparison_locations.iter())
        .flat_map(|(_, props)| props.keys())
        .collect::<BTreeSet<_>>();

    for key in keys {
        let in_baseline = find_property(&baseline_locations, key);
        let in_comparison = find_property(&comparison_locations, key);

        // A key found in exactly one location on each side, but a different one, has moved.
        if let ([(baseline_location, baseline_value)], [(comparison_location, comparison_value)]) =
            (in_baseline.as_slice(), in_comparison.as_slice())
        {
            if baseline_location != comparison_location {
                let path = format!("{}.{}", baseline_location.prefix(), key);
                diffs.push(FieldDiff {
                    path: path.clone(),
                    kind: DiffKind::Location,
                    baseline: Some((*baseline_value).clone()),
                    comparison: Some((*comparison_value).clone()),
                });
                diff_values(path, baseline_value, comparison_value, diffs);
                continue;
            }
        }

        for location in [FieldLocation::EnvelopeProperties, FieldLocation::Content] {
            diff_optional(
                format!("{}.{}", location.prefix(), key),
                property_at(&in_baseline, location),
                property_at(&in_comparison, location),
                diffs,
            );
        }
    }
}

fn find_property<'a>(locations: &PropertyLocations<'a>, key: &str) -> Vec<(FieldLocation, &'a Value)> {
    locations
        .iter()
        .filter_map(|(location, props)| props.get(key).map(|value| (*location, value)))
        .collect()
}

fn property_at<'a>(found: &[(FieldLocation, &'a Value)], location: FieldLocation) -> Option<&'a Value> {
    found
        .iter()
        .find(|(found_location, _)| *found_location == location)
        .map(|(_, value)| *value)
}

/// Returns `true` if both encode-time timestamps are epoch milliseconds within `tolerance_ms` of each other.
fn encode_timestamps_match(baseline: Option<&Value>, comparison: Option<&Value>, tolerance_ms: u64) -> bool {
    match (baseline.and_then(Value::as_u64), comparison.and_then(Value::as_u64)) {
        (Some(baseline), Some(comparison)) => baseline.abs_diff(comparison) <= tolerance_ms,
        _ => false,
    }
}

fn diff_optional(path: String, baseline: Option<&Value>, comparison: Option<&Value>, diffs: &mut Vec<FieldDiff>) {
    let kind = match (baseline, comparison) {
        (None, None) => return,
        (Some(baseline), Some(comparison)) => return diff_values(path, baseline, comparison, diffs),
        (Some(_), None) => DiffKind::Missing,
        (None, Some(_)) => DiffKind::Extra,
    };
    diffs.push(FieldDiff {
        path,
        kind,
        baseline: baseline.cloned(),
        comparison: comparison.cloned(),
    });
}

/// Diffs two values, descending into objects. Other values, including arrays, are compared as a whole.
fn diff_values(path: String, baseline: &Value, comparison: &Value, diffs: &mut Vec<FieldDiff>) {
    match (baseline, comparison) {
        (Value::Object(baseline), Value::Object(comparison)) => {
            let keys = baseline.keys().chain(comparison.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                diff_optional(
                    format!("{}.{}", path, key),
                    baseline.get(key),
                    comparison.get(key),
                    diffs,
                );
            }
        }
        _ if baseline != comparison => diffs.push(FieldDiff {
            path,
            kind: DiffKind::Value,
            baseline: Some(baseline.clone()),
            comparison: Some(comparison.clone()),
        }),
        _ => {}
    }
}

/// Returns `true` if `path` matches the glob `pattern`.
///
/// Both are split into segments on `.`. A segment of `*` matches any one segment, a segment of `**` matches any number
/// of segments, including none, and any other segment matches only itself.
pub fn field_path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.split('.').collect::<Vec<_>>();
    let path = path.split('.').collect::<Vec<_>>();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, _) => path.is_empty(),
        (Some((&"**", rest)), _) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        (Some((segment, rest)), Some((path_segment, path_rest))) => {
            (*segment == "*" || segment == path_segment) && segments_match(rest, path_rest)
        }
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Tolerance for encode-time timestamps in these tests.
    const TOLERANCE_MS: u64 = 60_000;

    fn log(value: Value) -> Log {
        let payload = Value::Array(vec![value]).to_string();
        Log::try_from_payload(payload.as_bytes())
            .expect("test log should parse")
            .remove(0)
    }

    fn paths(diffs: &[FieldDiff]) -> Vec<&str> {
        diffs.iter().map(|diff| diff.path.as_str()).collect()
    }

    #[test]
    fn envelope_fields_and_tags_are_diffed() {
        let baseline =
            log(json!({"message": "{\"message\":\"a\"}", "status": "info", "hostname": "h", "ddtags": "a:1,b:2"}));
        let comparison =
            log(json!({"message": "{\"message\":\"a\"}", "status": "Info", "service": "s", "ddtags": "b:2, c:3"}));

        let diffs = diff_logs(&baseline, &comparison, 0);

        let summary = diffs
            .iter()
            .map(|diff| (diff.path.as_str(), diff.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("envelope.status", DiffKind::Value),
                ("envelope.hostname", DiffKind::Missing),
                ("envelope.service", DiffKind::Extra),
                ("envelope.ddtags", DiffKind::Missing),
                ("envelope.ddtags", DiffKind::Extra),
            ]
        );
        assert_eq!(diffs[3].baseline, Some(json!("a:1")));
        assert_eq!(diffs[4].comparison, Some(json!("c:3")));
    }

    #[test]
    fn a_property_that_moved_out_of_the_content_is_a_location_diff() {
        let baseline = log(json!({"message": "{\"message\":\"a\",\"otel.trace_id\":\"1\",\"attr\":\"x\"}"}));
        let comparison = log(json!({"message": "{\"message\":\"a\"}", "otel.trace_id": "1", "attr": "y"}));

        let diffs = diff_logs(&baseline, &comparison, 0);

        let summary = diffs
            .iter()
            .map(|diff| (diff.path.as_str(), diff.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("content.attr", DiffKind::Location),
                // A moved property whose value also changed gets a second, value diff.
                ("content.attr", DiffKind::Value),
                ("content.otel.trace_id", DiffKind::Location),
            ]
        );
    }

    #[test]
    fn a_property_in_both_locations_on_one_side_is_diffed_per_location() {
        let baseline = log(json!({"message": "{\"k\":1}"}));
        let comparison = log(json!({"message": "{\"k\":1}", "k": 1}));

        let diffs = diff_logs(&baseline, &comparison, 0);

        assert_eq!(paths(&diffs), vec!["envelope_properties.k"]);
        assert_eq!(diffs[0].kind, DiffKind::Extra);
    }

    #[test]
    fn raw_message_is_compared_unless_both_sides_have_content() {
        let content = log(json!({"message": "{\"message\":\"a\"}"}));
        let plain = log(json!({"message": "a"}));

        let diffs = diff_logs(&content, &plain, 0);
        assert_eq!(paths(&diffs), vec!["envelope.message", "content.message"]);
        assert_eq!(diffs[0].kind, DiffKind::Value);
        assert_eq!(diffs[1].kind, DiffKind::Missing);

        assert!(diff_logs(&plain, &plain.clone(), 0).is_empty());
    }

    #[test]
    fn encode_timestamps_match_within_the_tolerance() {
        let with_timestamp = |timestamp: Value| log(json!({"message": "a", "timestamp": timestamp}));
        let base = 1_793_268_000_000u64;

        let cases = [
            ("equal", json!(base), true),
            ("just inside", json!(base + TOLERANCE_MS), true),
            ("just outside", json!(base + TOLERANCE_MS + 1), false),
            ("earlier, just outside", json!(base - TOLERANCE_MS - 1), false),
            ("not a number", json!(base.to_string()), false),
        ];
        for (name, comparison, matches) in cases {
            let diffs = diff_logs(&with_timestamp(json!(base)), &with_timestamp(comparison), TOLERANCE_MS);
            assert_eq!(diffs.is_empty(), matches, "{}: {:?}", name, diffs);
        }

        // A missing timestamp is still a difference.
        let diffs = diff_logs(
            &with_timestamp(json!(base)),
            &log(json!({"message": "a"})),
            TOLERANCE_MS,
        );
        assert_eq!(paths(&diffs), vec!["envelope.timestamp"]);
        assert_eq!(diffs[0].kind, DiffKind::Missing);
    }

    #[test]
    fn globs_match_segment_by_segment() {
        let cases = [
            ("envelope.status", "envelope.status", true),
            ("envelope.status", "envelope.status.extra", false),
            ("content.*", "content.attr", true),
            ("content.*", "content.attr.nested", false),
            ("content.otel.*", "content.otel.trace_id", true),
            ("content.otel.*", "content.otelx.trace_id", false),
            ("content.**", "content.attr.nested.deep", true),
            ("content.**", "content", true),
            ("content.**", "envelope_properties.attr", false),
            ("**.trace_id", "envelope_properties.otel.trace_id", true),
        ];

        for (pattern, path, expected) in cases {
            assert_eq!(field_path_matches(pattern, path), expected, "{} vs {}", pattern, path);
        }
    }

    #[test]
    fn raw_envelope_fields_cover_every_envelope_field_but_ddtags() {
        // `ddtags` is compared as a set of tags rather than as a raw value, so it's the only envelope field left out.
        let raw = RAW_ENVELOPE_FIELDS.iter().copied().collect::<BTreeSet<_>>();
        let all = crate::LOG_ENVELOPE_FIELDS
            .iter()
            .copied()
            .filter(|name| *name != "ddtags")
            .collect::<BTreeSet<_>>();
        assert_eq!(raw, all);
    }
}
