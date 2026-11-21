//! Comparison of the requests sent to the intake against a wire golden (`v1_<case>_wire.json`).

use std::collections::{BTreeMap, BTreeSet};

use datadog_protos::traces::{AgentPayload, StatsPayload};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;

use super::{
    compare_stats, compare_traces, stats::GoldenStatsPayload, traces::GoldenAgentPayload, CompareOptions, Mismatches,
    StatsSummary, TraceSummary,
};

/// Intake path for APM stats.
pub const STATS_PATH: &str = "/api/v0.2/stats";

/// Intake path for trace payloads.
pub const TRACES_PATH: &str = "/api/v0.2/traces";

/// The request headers pinned by the wire golden files; all others are build- or runtime-dependent and ignored.
pub const PINNED_HEADERS: [&str; 4] = ["Content-Type", "Content-Encoding", "DD-API-KEY", "Datadog-Languages"];

/// One request recorded in a wire golden.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WireGoldenRequest {
    /// Request path.
    pub path: String,

    /// Pinned request headers that were present.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    /// Decoded, normalized payload, as `protojson`.
    pub payload: serde_json::Value,
}

/// A wire golden: every request the Go agent sent to the intake for one case.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WireGolden {
    /// Recorded requests, in send order.
    pub requests: Vec<WireGoldenRequest>,
}

impl WireGolden {
    /// Parses a wire golden file.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON doesn't have the wire golden shape.
    pub fn from_json(json: &str) -> Result<Self, GenericError> {
        serde_json::from_str(json).error_context("Failed to parse wire golden.")
    }

    fn payloads<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, GenericError> {
        self.requests
            .iter()
            .filter(|r| r.path == path)
            .map(|r| {
                T::deserialize(&r.payload).with_error_context(|| format!("Failed to parse golden {} payload.", path))
            })
            .collect()
    }

    /// Builds the stats summary of every stats request in the golden.
    ///
    /// # Errors
    ///
    /// Returns an error if a stats payload doesn't match the expected `protojson` shape.
    pub fn stats_summary(&self) -> Result<StatsSummary, GenericError> {
        StatsSummary::from_golden_payloads(self.payloads::<GoldenStatsPayload>(STATS_PATH)?)
    }

    /// Builds the trace summary of every trace request in the golden.
    ///
    /// # Errors
    ///
    /// Returns an error if a trace payload doesn't match the expected `protojson` shape or holds an invalid ref.
    pub fn trace_summary(&self, opts: &CompareOptions) -> Result<TraceSummary, GenericError> {
        TraceSummary::from_golden_payloads(self.payloads::<GoldenAgentPayload>(TRACES_PATH)?, opts)
    }
}

/// A decoded request payload captured from the system under test.
#[derive(Clone, Debug, PartialEq)]
pub enum CapturedPayload {
    /// A stats payload sent to [`STATS_PATH`].
    Stats(StatsPayload),

    /// A trace payload sent to [`TRACES_PATH`].
    Traces(AgentPayload),
}

/// A request captured from the system under test, with its body already decompressed and decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedRequest {
    /// Request path.
    pub path: String,

    /// Request headers.
    pub headers: Vec<(String, String)>,

    /// Decoded payload.
    pub payload: CapturedPayload,
}

fn header<'a>(headers: impl IntoIterator<Item = (&'a String, &'a String)>, name: &str) -> Option<&'a str> {
    headers
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// Compares captured requests against a wire golden.
///
/// - Every path in the golden must have been requested, and no other path may have been requested.
/// - All stats requests are aggregated and compared with [`compare_stats`]; all trace requests are aggregated and
///   compared with [`compare_traces`], normalizing span starts against `epoch_unix_nanos`.
/// - For every captured request, each of the [`PINNED_HEADERS`] must equal the golden value for that path, compared by
///   name case-insensitively. A header the golden doesn't record must be absent or empty.
///
/// # Errors
///
/// Returns every difference found, including golden payloads that fail to parse.
pub fn compare_wire(
    golden: &WireGolden, actual: &[CapturedRequest], epoch_unix_nanos: u64, opts: &CompareOptions,
) -> Result<(), Mismatches> {
    let mut out = Mismatches::default();

    let golden_paths: BTreeSet<&str> = golden.requests.iter().map(|r| r.path.as_str()).collect();
    let actual_paths: BTreeSet<&str> = actual.iter().map(|r| r.path.as_str()).collect();
    for path in golden_paths.difference(&actual_paths) {
        out.push(*path, "request sent", "no request");
    }
    for path in actual_paths.difference(&golden_paths) {
        out.push(*path, "no request", "unexpected request");
    }

    // Pinned headers, per path.
    for req in actual {
        let Some(g) = golden.requests.iter().find(|g| g.path == req.path) else {
            continue;
        };
        for name in PINNED_HEADERS {
            let expected = header(&g.headers, name);
            let actual = header(req.headers.iter().map(|(n, v)| (n, v)), name);
            if expected != actual {
                out.push(
                    format!("{}: header {}", req.path, name),
                    format!("{:?}", expected),
                    format!("{:?}", actual),
                );
            }
        }
    }

    if golden_paths.contains(STATS_PATH) && actual_paths.contains(STATS_PATH) {
        let payloads: Vec<StatsPayload> = actual
            .iter()
            .filter_map(|r| match (&r.payload, r.path.as_str()) {
                (CapturedPayload::Stats(p), STATS_PATH) => Some(p.clone()),
                _ => None,
            })
            .collect();
        match (golden.stats_summary(), StatsSummary::from_payloads(&payloads)) {
            (Ok(e), Ok(a)) => {
                if let Err(m) = compare_stats(&e, &a, opts) {
                    out.extend_prefixed(STATS_PATH, m);
                }
            }
            (Err(e), _) => out.push(STATS_PATH, "a valid golden", e.to_string()),
            (_, Err(e)) => out.push(STATS_PATH, "decodable stats payloads", e.to_string()),
        }
    }

    if golden_paths.contains(TRACES_PATH) && actual_paths.contains(TRACES_PATH) {
        let payloads: Vec<AgentPayload> = actual
            .iter()
            .filter_map(|r| match (&r.payload, r.path.as_str()) {
                (CapturedPayload::Traces(p), TRACES_PATH) => Some(p.clone()),
                _ => None,
            })
            .collect();
        match golden.trace_summary(opts) {
            Ok(e) => match TraceSummary::from_payloads(&payloads, epoch_unix_nanos, opts) {
                Ok(a) => {
                    if let Err(m) = compare_traces(&e, &a, opts) {
                        out.extend_prefixed(TRACES_PATH, m);
                    }
                }
                Err(m) => out.extend_prefixed(TRACES_PATH, m),
            },
            Err(e) => out.push(TRACES_PATH, "a valid golden", e.to_string()),
        }
    }

    // A captured payload whose type doesn't match its path can't be compared.
    for req in actual {
        let ok = matches!(
            (&req.payload, req.path.as_str()),
            (CapturedPayload::Stats(_), STATS_PATH) | (CapturedPayload::Traces(_), TRACES_PATH)
        );
        if !ok && golden_paths.contains(req.path.as_str()) {
            out.push(
                req.path.clone(),
                "a payload matching the path",
                "a payload of another type",
            );
        }
    }

    out.into_result()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compare::traces::tests as trace_tests, golden_wire_path};

    fn wire(case: &str) -> WireGolden {
        WireGolden::from_json(&std::fs::read_to_string(golden_wire_path(case)).unwrap()).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    fn default_capture() -> Vec<CapturedRequest> {
        let golden = wire("default");
        let stats: Vec<GoldenStatsPayload> = golden.payloads(STATS_PATH).unwrap();
        let stats = stats.into_iter().next().unwrap().into_proto().unwrap();
        vec![
            CapturedRequest {
                path: TRACES_PATH.into(),
                headers: headers(&[
                    ("content-type", "application/x-protobuf"),
                    ("Content-Encoding", "gzip"),
                    ("Dd-Api-Key", "test"),
                    ("User-Agent", "ignored"),
                ]),
                payload: CapturedPayload::Traces(trace_tests::default_agent_payload()),
            },
            CapturedRequest {
                path: STATS_PATH.into(),
                headers: headers(&[
                    ("Content-Type", "application/msgpack"),
                    ("Content-Encoding", "gzip"),
                    ("DD-API-KEY", "test"),
                    ("Datadog-Languages", ""),
                ]),
                payload: CapturedPayload::Stats(stats),
            },
        ]
    }

    fn run(case: &str, actual: &[CapturedRequest]) -> Result<(), Mismatches> {
        compare_wire(&wire(case), actual, trace_tests::EPOCH, &CompareOptions::default())
    }

    fn expect_mismatch(result: Result<(), Mismatches>, needle: &str) {
        let err = result.expect_err("comparison should fail");
        assert!(
            err.0.iter().any(|m| m.to_string().contains(needle)),
            "no mismatch containing {:?} in:\n{}",
            needle,
            err
        );
    }

    #[test]
    fn default_capture_matches() {
        run("default", &default_capture()).unwrap();
    }

    #[test]
    fn missing_path_is_reported() {
        let mut actual = default_capture();
        actual.retain(|r| r.path != STATS_PATH);
        expect_mismatch(run("default", &actual), "/api/v0.2/stats: expected request sent");
    }

    #[test]
    fn extra_path_is_reported() {
        // `user_drop` sends no traces, so a trace request is unexpected.
        let actual = default_capture();
        expect_mismatch(run("user_drop", &actual), "/api/v0.2/traces: expected no request");
    }

    #[test]
    fn header_mismatch_is_reported() {
        let mut actual = default_capture();
        actual[0].headers.retain(|(n, _)| n != "Content-Encoding");
        expect_mismatch(run("default", &actual), "header Content-Encoding");

        let mut actual = default_capture();
        actual[1].headers.push(("Datadog-Languages".into(), "python".into()));
        actual[1].headers.remove(3);
        expect_mismatch(run("default", &actual), "header Datadog-Languages");
    }

    #[test]
    fn payload_differences_are_prefixed_with_path() {
        let mut actual = default_capture();
        if let CapturedPayload::Stats(p) = &mut actual[1].payload {
            p.stats[0].stats[0].stats[0].hits += 1;
        }
        expect_mismatch(run("default", &actual), "/api/v0.2/stats: group{");
    }
}
