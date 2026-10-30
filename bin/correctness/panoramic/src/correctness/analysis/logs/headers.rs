//! Comparison of the headers of the requests that carry logs.
//!
//! Headers belong to requests, not to logs, and the two sides batch logs into requests differently, so requests can't
//! be paired one to one. Instead, each side is summarized, for each compared header, by the set of shapes the header
//! took across all of that side's logs requests, and the two sets are compared. A shape is the header value itself for
//! headers compared exactly, or a placeholder for headers whose values legitimately differ between the sides, such as
//! versions and timestamps. A request without the header contributes an absent shape, so a header sent on only some
//! requests is a difference from one sent on all of them.

use std::{collections::BTreeSet, ops::RangeInclusive};

use serde::Deserialize;
use serde_json::Value;
use stele::LogsRequest;

use super::{DiffKind, FieldDiff};

/// Path prefix used for header differences, followed by the lowercased header name.
pub const HEADER_PATH_PREFIX: &str = "headers";

/// Shape of a header whose presence is compared, but not its value.
const PRESENT: &str = "<present>";

/// Shape of a timestamp header holding epoch milliseconds within the plausible range.
const TIMESTAMP: &str = "<timestamp>";

/// Shape of a timestamp header that isn't a non-negative integer.
const NOT_A_NUMBER: &str = "<not a number>";

/// Shape of a timestamp header holding an integer outside the plausible range.
const OUT_OF_RANGE: &str = "<out of range>";

/// How a header is compared between the two sides.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HeaderCheck {
    /// The header values must be equal.
    Exact,

    /// The header must be present on the same requests, with any value.
    Presence,

    /// The header must be present on the same requests, holding epoch milliseconds within the plausible range of the
    /// run, with any value in that range.
    Timestamp,
}

/// A header compared between the two sides, set in the `logs.headers` section of a correctness case.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HeaderRule {
    /// The header name, matched case-insensitively.
    pub name: String,

    /// How the header is compared.
    pub check: HeaderCheck,
}

impl HeaderRule {
    fn new(name: &str, check: HeaderCheck) -> Self {
        Self {
            name: name.to_string(),
            check,
        }
    }
}

/// Returns the headers compared by default: those the logs intake reads, apart from transport headers.
///
/// `User-Agent` and `DD-Agent-Version` are left out, since they name the sending product, which differs by design.
pub fn default_header_rules() -> Vec<HeaderRule> {
    vec![
        HeaderRule::new("dd-protocol", HeaderCheck::Exact),
        HeaderRule::new("dd-evp-origin", HeaderCheck::Exact),
        HeaderRule::new("dd-evp-origin-version", HeaderCheck::Presence),
        HeaderRule::new("dd-message-timestamp", HeaderCheck::Timestamp),
        HeaderRule::new("dd-current-timestamp", HeaderCheck::Timestamp),
        HeaderRule::new("content-encoding", HeaderCheck::Exact),
    ]
}

/// Compares the headers of both sides' logs requests.
///
/// For each rule, the set of shapes the header took on each side is compared. When they differ, a single diff is
/// reported at `headers.<name>`, holding each side's shapes as a sorted array, with `null` for requests without the
/// header. The diff is `Missing` when the comparison never sends the header, `Extra` when the baseline never does, and
/// `Value` otherwise.
///
/// `timestamp_range` is the plausible range of timestamp headers, in epoch milliseconds.
pub fn compare_headers(
    baseline: &[LogsRequest], comparison: &[LogsRequest], rules: &[HeaderRule], timestamp_range: &RangeInclusive<u64>,
) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();
    for rule in rules {
        let baseline_shapes = header_shapes(baseline, rule, timestamp_range);
        let comparison_shapes = header_shapes(comparison, rule, timestamp_range);
        if baseline_shapes == comparison_shapes {
            continue;
        }

        let never_sent = |shapes: &BTreeSet<Option<String>>| shapes.iter().all(Option::is_none);
        let (kind, baseline, comparison) = if never_sent(&comparison_shapes) {
            (DiffKind::Missing, Some(shapes_value(baseline_shapes)), None)
        } else if never_sent(&baseline_shapes) {
            (DiffKind::Extra, None, Some(shapes_value(comparison_shapes)))
        } else {
            (
                DiffKind::Value,
                Some(shapes_value(baseline_shapes)),
                Some(shapes_value(comparison_shapes)),
            )
        };
        diffs.push(FieldDiff {
            path: format!("{}.{}", HEADER_PATH_PREFIX, rule.name.to_ascii_lowercase()),
            kind,
            baseline,
            comparison,
        });
    }
    diffs
}

/// Returns the shapes `rule`'s header took across `requests`, with `None` for requests without it.
///
/// A side with no requests has no shapes, so it never matches a side that sent some.
fn header_shapes(
    requests: &[LogsRequest], rule: &HeaderRule, timestamp_range: &RangeInclusive<u64>,
) -> BTreeSet<Option<String>> {
    requests
        .iter()
        .map(|request| {
            request.header(&rule.name).map(|value| match rule.check {
                HeaderCheck::Exact => value.to_string(),
                HeaderCheck::Presence => PRESENT.to_string(),
                HeaderCheck::Timestamp => timestamp_shape(value, timestamp_range).to_string(),
            })
        })
        .collect()
}

fn timestamp_shape(value: &str, timestamp_range: &RangeInclusive<u64>) -> &'static str {
    match value.parse::<u64>() {
        Ok(timestamp) if timestamp_range.contains(&timestamp) => TIMESTAMP,
        Ok(_) => OUT_OF_RANGE,
        Err(_) => NOT_A_NUMBER,
    }
}

fn shapes_value(shapes: BTreeSet<Option<String>>) -> Value {
    Value::Array(
        shapes
            .into_iter()
            .map(|shape| shape.map_or(Value::Null, Value::String))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: u64 = 1_793_268_000_000;
    const RANGE: RangeInclusive<u64> = (NOW - 3_600_000)..=(NOW + 60_000);

    fn request(headers: &[(&str, &str)]) -> LogsRequest {
        LogsRequest::new("/api/v2/logs", headers.iter().copied(), Vec::new())
    }

    fn compare(baseline: &[LogsRequest], comparison: &[LogsRequest]) -> Vec<FieldDiff> {
        compare_headers(baseline, comparison, &default_header_rules(), &RANGE)
    }

    fn agent_request(message_timestamp: u64, current_timestamp: u64) -> LogsRequest {
        request(&[
            ("DD-PROTOCOL", "agent-json"),
            ("DD-EVP-ORIGIN", "agent"),
            ("DD-EVP-ORIGIN-VERSION", "7.70.0"),
            ("dd-message-timestamp", &message_timestamp.to_string()),
            ("dd-current-timestamp", &current_timestamp.to_string()),
            ("Content-Encoding", "zstd"),
            ("User-Agent", "datadog-agent/7.70.0"),
            ("Content-Length", "123"),
        ])
    }

    #[test]
    fn identical_headers_match_across_different_batching() {
        // The baseline sent three requests and the comparison one, with different timestamps, versions, and transport
        // headers.
        let baseline = [
            agent_request(NOW - 2_000, NOW - 1_000),
            agent_request(NOW - 1_000, NOW),
            agent_request(NOW, NOW),
        ];
        let comparison = [request(&[
            ("dd-protocol", "agent-json"),
            ("dd-evp-origin", "agent"),
            ("dd-evp-origin-version", "0.1.0"),
            ("dd-message-timestamp", &(NOW - 30_000).to_string()),
            ("dd-current-timestamp", &(NOW + 1).to_string()),
            ("content-encoding", "zstd"),
            ("user-agent", "agent-data-plane/0.1.0"),
            ("dd-agent-version", "0.1.0"),
        ])];

        assert_eq!(compare(&baseline, &comparison), Vec::new());
        assert_eq!(compare(&[], &[]), Vec::new());
    }

    #[test]
    fn a_header_the_comparison_never_sends_is_missing() {
        let baseline = [agent_request(NOW, NOW)];
        let comparison = [request(&[("Content-Encoding", "zstd")])];

        let diffs = compare(&baseline, &comparison);

        let summary = diffs
            .iter()
            .map(|diff| (diff.path.as_str(), diff.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("headers.dd-protocol", DiffKind::Missing),
                ("headers.dd-evp-origin", DiffKind::Missing),
                ("headers.dd-evp-origin-version", DiffKind::Missing),
                ("headers.dd-message-timestamp", DiffKind::Missing),
                ("headers.dd-current-timestamp", DiffKind::Missing),
            ]
        );
        assert_eq!(diffs[0].baseline, Some(json!(["agent-json"])));
        assert_eq!(diffs[0].comparison, None);
        assert_eq!(diffs[2].baseline, Some(json!([PRESENT])));
        assert_eq!(diffs[3].baseline, Some(json!([TIMESTAMP])));
    }

    #[test]
    fn a_header_the_baseline_never_sends_is_extra() {
        let baseline = [request(&[]), request(&[])];
        let comparison = [request(&[("DD-PROTOCOL", "agent-json")]), request(&[])];

        let diffs = compare(&baseline, &comparison);

        assert_eq!(diffs.len(), 1, "{:?}", diffs);
        assert_eq!(diffs[0].path, "headers.dd-protocol");
        assert_eq!(diffs[0].kind, DiffKind::Extra);
        assert_eq!(diffs[0].baseline, None);
        assert_eq!(diffs[0].comparison, Some(json!([null, "agent-json"])));
    }

    #[test]
    fn differing_values_and_partial_presence_are_value_diffs() {
        let cases = [
            ("exact value", "Content-Encoding", "zstd", "gzip"),
            ("exact value case", "DD-EVP-ORIGIN", "agent", "Agent"),
        ];
        for (name, header, baseline_value, comparison_value) in cases {
            let diffs = compare(
                &[request(&[(header, baseline_value)])],
                &[request(&[(header, comparison_value)])],
            );
            assert_eq!(diffs.len(), 1, "{}: {:?}", name, diffs);
            assert_eq!(diffs[0].kind, DiffKind::Value, "{}", name);
            assert_eq!(diffs[0].baseline, Some(json!([baseline_value])), "{}", name);
            assert_eq!(diffs[0].comparison, Some(json!([comparison_value])), "{}", name);
        }

        // A header sent on only some of the comparison's requests differs from one sent on all of the baseline's.
        let diffs = compare(
            &[request(&[("DD-EVP-ORIGIN-VERSION", "7.70.0")])],
            &[request(&[("DD-EVP-ORIGIN-VERSION", "0.1.0")]), request(&[])],
        );
        assert_eq!(diffs.len(), 1, "{:?}", diffs);
        assert_eq!(diffs[0].kind, DiffKind::Value);
        assert_eq!(diffs[0].baseline, Some(json!([PRESENT])));
        assert_eq!(diffs[0].comparison, Some(json!([null, PRESENT])));
    }

    #[test]
    fn timestamps_must_be_numeric_and_in_the_plausible_range() {
        let cases = [
            ("now", NOW.to_string(), None),
            ("range start", RANGE.start().to_string(), None),
            ("range end", RANGE.end().to_string(), None),
            ("before the range", (RANGE.start() - 1).to_string(), Some(OUT_OF_RANGE)),
            ("after the range", (RANGE.end() + 1).to_string(), Some(OUT_OF_RANGE)),
            ("seconds", (NOW / 1_000).to_string(), Some(OUT_OF_RANGE)),
            ("nanoseconds", (NOW * 1_000_000).to_string(), Some(OUT_OF_RANGE)),
            ("too large", u64::MAX.to_string() + "0", Some(NOT_A_NUMBER)),
            ("zero", "0".to_string(), Some(OUT_OF_RANGE)),
            ("negative", "-1".to_string(), Some(NOT_A_NUMBER)),
            ("fractional", format!("{}.5", NOW), Some(NOT_A_NUMBER)),
            ("empty", String::new(), Some(NOT_A_NUMBER)),
        ];
        for (name, value, expected) in cases {
            let baseline = [request(&[("dd-current-timestamp", &NOW.to_string())])];
            let comparison = [request(&[("dd-current-timestamp", &value)])];

            let diffs = compare(&baseline, &comparison);

            match expected {
                None => assert!(diffs.is_empty(), "{}: {:?}", name, diffs),
                Some(shape) => {
                    assert_eq!(diffs.len(), 1, "{}: {:?}", name, diffs);
                    assert_eq!(diffs[0].path, "headers.dd-current-timestamp", "{}", name);
                    assert_eq!(diffs[0].kind, DiffKind::Value, "{}", name);
                    assert_eq!(diffs[0].comparison, Some(json!([shape])), "{}", name);
                }
            }
        }
    }

    #[test]
    fn only_configured_headers_are_compared() {
        let rules = [HeaderRule::new("X-Custom", HeaderCheck::Exact)];
        let baseline = [agent_request(NOW, NOW)];
        let comparison = [request(&[("x-custom", "1")])];

        let diffs = compare_headers(&baseline, &comparison, &rules, &RANGE);

        assert_eq!(diffs.len(), 1, "{:?}", diffs);
        assert_eq!(diffs[0].path, "headers.x-custom");
        assert_eq!(diffs[0].kind, DiffKind::Extra);
    }

    #[test]
    fn rules_parse_from_case_yaml() {
        let yaml = r#"
- {name: DD-PROTOCOL, check: exact}
- {name: dd-evp-origin-version, check: presence}
- {name: dd-current-timestamp, check: timestamp}
"#;
        let rules: Vec<HeaderRule> = serde_saphyr::from_str(yaml).expect("rules should parse");

        assert_eq!(
            rules,
            vec![
                HeaderRule::new("DD-PROTOCOL", HeaderCheck::Exact),
                HeaderRule::new("dd-evp-origin-version", HeaderCheck::Presence),
                HeaderRule::new("dd-current-timestamp", HeaderCheck::Timestamp),
            ]
        );
        assert!(serde_saphyr::from_str::<Vec<HeaderRule>>("- {name: a, check: approximate}").is_err());
    }
}
