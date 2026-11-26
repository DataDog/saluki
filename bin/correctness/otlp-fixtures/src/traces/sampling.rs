//! The fixture of the `otlp-traces-sampling` case: one probe for each sampling decision.
//!
//! The OTLP receiver of the Agent and of ADP keeps a trace when a hash of the low 64 bits of its trace ID falls below
//! the sampling rate of the receiver. A fixture picks the trace ID of each probe, so that the decision of the receiver
//! is fixed. A probe in the keep range is one that the receiver keeps at the rate of the case, and a probe in the drop
//! range is one that it drops. A probe of a user decision, or of a sampler that runs after the receiver, sits in the
//! range that makes its decision the most telling.
//!
//! Each probe has a span name of its own, `otlpprobe.tsamp.<id>.<arm>`, so that a decision shows as one named span, one
//! group of stats, or both. A span that a trace keeps beside the probe has the name of the probe with a suffix. Spans
//! are `SERVER` roots, or `INTERNAL` children of a root, and each one has an `operation.name` that matches its name.
//! The spans of a probe start and end at fixed times in the past.

use otlp_protos::opentelemetry::proto::{
    common::v1::KeyValue,
    trace::v1::span::{Event, SpanKind},
};

use super::{probe_request_with_seeds, ProbeResource, ProbeSpan, BASE_TIME_NANOS};
use crate::common::{boolean, double, int, kv, string};

/// The first seed of the traces of `sampling_fixture`. A trace has the seed of the first one that follows the seed of the
/// trace before it, and that has the decision of the receiver that the probe needs.
const SAMPLING_SEED: u32 = 0x0d00;

/// The service of most spans of `sampling_fixture`.
const SAMPLING_SERVICE: &str = "probe-sampling";

/// The service of the spans that a configured rate of event extraction names.
const ANALYTICS_SERVICE: &str = "probe-analytics";

/// The sampling rate of the OTLP receiver in both cases, which is a sampling percentage of 50.
const RECEIVER_RATE: f64 = 0.5;

/// The rate that the error sampler applies to a trace that the receiver dropped, through the `_sample_rate` of the root
/// span.
const RESCUE_RATE: f64 = 0.1;

/// The attribute that sets the sampling priority of a trace.
const PRIORITY: &str = "sampling.priority";

/// The attribute that marks a span for single span sampling.
const SPAN_SAMPLING_MECHANISM: &str = "_dd.span_sampling.mechanism";

/// Returns whether the receiver keeps a trace with the low 64 bits `low` of its ID, at `rate`.
///
/// This is `SampleByRate` of the Agent in `pkg/trace/sampler/sampler.go`, which the OTLP receiver and the error sampler
/// both use.
fn sample_by_rate(low: u64, rate: f64) -> bool {
    if rate < 1.0 {
        low.wrapping_mul(1_111_111_111_111_111_111) < (rate * u64::MAX as f64) as u64
    } else {
        true
    }
}

/// The traces of a fixture, and the seed that fixes the sampling decision of the receiver for each.
struct Probes {
    /// The last seed that a probe took.
    last_seed: u32,
    seeds: Vec<u32>,
    traces: Vec<Vec<ProbeSpan>>,

    /// The rate and the decision that each seed was picked for.
    decisions: Vec<(f64, bool)>,
}

impl Probes {
    /// Starts a fixture whose seeds come after `seed_base`.
    fn new(seed_base: u32) -> Self {
        Self {
            last_seed: seed_base,
            seeds: Vec::new(),
            traces: Vec::new(),
            decisions: Vec::new(),
        }
    }

    /// Adds a trace with the next seed for which `sample_by_rate` at `rate` gives `keeps`.
    fn add(&mut self, rate: f64, keeps: bool, trace: Vec<ProbeSpan>) {
        let seed = (self.last_seed + 1..)
            .find(|&seed| sample_by_rate(u64::from(seed), rate) == keeps)
            .expect("a seed has each decision");
        self.last_seed = seed;
        self.seeds.push(seed);
        self.traces.push(trace);
        self.decisions.push((rate, keeps));
    }

    /// Adds a trace in the keep range: the receiver keeps it at `RECEIVER_RATE`.
    fn in_keep_range(&mut self, trace: Vec<ProbeSpan>) {
        self.add(RECEIVER_RATE, true, trace);
    }

    /// Adds a trace in the drop range: the receiver drops it at `RECEIVER_RATE`.
    fn in_drop_range(&mut self, trace: Vec<ProbeSpan>) {
        self.add(RECEIVER_RATE, false, trace);
    }

    /// Builds the request, with one resource for each of `services`.
    fn request(&self, services: &[&'static str]) -> Vec<u8> {
        let resources: Vec<ProbeResource> = services.iter().map(|&service| ProbeResource::new(service)).collect();
        probe_request_with_seeds(&resources, &self.seeds, &self.traces)
    }
}

/// A span of `service` with the name `name`, which is also its operation name.
fn span_in(service: &'static str, name: &'static str, kind: SpanKind) -> ProbeSpan {
    ProbeSpan::new(service, name, kind).with_attribute("operation.name", string(name))
}

/// A `SERVER` span of `SAMPLING_SERVICE` with no parent.
fn root(name: &'static str) -> ProbeSpan {
    span_in(SAMPLING_SERVICE, name, SpanKind::Server)
}

/// An `INTERNAL` span of `SAMPLING_SERVICE` that is a child of the first span of its trace.
fn child(name: &'static str) -> ProbeSpan {
    span_in(SAMPLING_SERVICE, name, SpanKind::Internal).child_of(1)
}

/// A span with the sampling priority `priority`.
fn with_priority(span: ProbeSpan, priority: i64) -> ProbeSpan {
    span.with_attribute(PRIORITY, int(priority))
}

/// A span that asks for an event of analytics.
fn with_analytics_event(span: ProbeSpan) -> ProbeSpan {
    span.with_attribute("analytics.event", boolean(true))
}

/// An event of a span with the name `name`, and attributes `attributes`.
fn event(name: &str, attributes: Vec<KeyValue>) -> Event {
    Event {
        time_unix_nano: BASE_TIME_NANOS + 1_000_000,
        name: name.to_string(),
        attributes,
        dropped_attributes_count: 0,
    }
}

/// An exception event, which the Agent and ADP turn into the tag `_dd.span_events.has_exception`.
fn exception() -> Event {
    event(
        "exception",
        vec![
            kv("exception.type", string("ProbeError")),
            kv("exception.message", string("probe exception")),
        ],
    )
}

/// The probes of `sampling_fixture`.
fn sampling_probes() -> Probes {
    let mut probes = Probes::new(SAMPLING_SEED);

    // TSAMP-001, TSAMP-002: the receiver keeps a trace in the keep range, and drops a trace in the drop range. Two
    // traces share each name, so that a side that decides by the span count and not by the trace ID fails.
    for _ in 0..2 {
        probes.in_keep_range(vec![root("otlpprobe.tsamp.002.keep")]);
    }
    for _ in 0..2 {
        probes.in_drop_range(vec![root("otlpprobe.tsamp.002.drop")]);
    }

    // TSAMP-004: the last span with a priority gives the priority of the trace.
    probes.in_drop_range(vec![
        with_priority(root("otlpprobe.tsamp.004.last"), 0),
        with_priority(child("otlpprobe.tsamp.004.last_child"), 2),
    ]);

    // TSAMP-005: a user keep beats the drop of the receiver.
    probes.in_drop_range(vec![with_priority(root("otlpprobe.tsamp.005.user"), 1)]);

    // TSAMP-006: the priority -128 is no priority. The Agent keeps the trace as a trace without a priority.
    probes.in_drop_range(vec![with_priority(root("otlpprobe.tsamp.006.none"), -128)]);

    // TSAMP-035: a user drop of a trace in the keep range skips the rescue of an error and the extraction of an event.
    probes.in_keep_range(vec![with_analytics_event(with_priority(
        root("otlpprobe.tsamp.035.drop").failed(),
        -1,
    ))]);

    // TSAMP-035: a user drop does not skip single span sampling, which keeps the span that asks for it.
    probes.in_keep_range(vec![
        with_priority(root("otlpprobe.tsamp.035.sss_root"), -1),
        child("otlpprobe.tsamp.035.sss").with_attribute(SPAN_SAMPLING_MECHANISM, int(8)),
    ]);

    // TSAMP-011: the error sampler keeps an error trace that the receiver dropped.
    probes.in_drop_range(vec![root("otlpprobe.tsamp.011.rescue").failed()]);

    // TSAMP-012: a priority of 0 is a user decision, and the error sampler keeps the error trace still.
    probes.in_drop_range(vec![with_priority(root("otlpprobe.tsamp.012.user_zero").failed(), 0)]);

    // TSAMP-013: the error sampler applies the `_sample_rate` of the root to the hash of the trace ID. A hash below
    // that rate keeps the trace. The receiver is out of play, because the user priority 0 drops both traces.
    probes.add(
        RESCUE_RATE,
        true,
        vec![with_priority(
            root("otlpprobe.tsamp.013.rescued").with_attribute("_sample_rate", double(RESCUE_RATE)),
            0,
        )
        .failed()],
    );
    probes.add(
        RESCUE_RATE,
        false,
        vec![with_priority(
            root("otlpprobe.tsamp.013.not_rescued").with_attribute("_sample_rate", double(RESCUE_RATE)),
            0,
        )
        .failed()],
    );

    // TSAMP-021: an exception event does not make the trace an error trace, unless error tracking standalone is on.
    probes.in_drop_range(vec![root("otlpprobe.tsamp.021.exception").with_event(exception())]);

    // TSAMP-028: the receiver removes the rate that a probabilistic sampler upstream put on a trace that it keeps, and
    // the rescue of the error sampler leaves it.
    probes.in_keep_range(vec![
        root("otlpprobe.tsamp.028.prob_sr").with_attribute("_dd.prob_sr", double(0.3))
    ]);
    probes.in_drop_range(vec![root("otlpprobe.tsamp.028.prob_sr_rescue")
        .with_attribute("_dd.prob_sr", double(0.3))
        .failed()]);

    // TSAMP-037: single span sampling keeps the span with the mechanism as a number. A mechanism that is a string is
    // not one, to the Agent.
    probes.in_drop_range(vec![
        root("otlpprobe.tsamp.037.root"),
        child("otlpprobe.tsamp.037.int").with_attribute(SPAN_SAMPLING_MECHANISM, int(8)),
    ]);
    probes.in_drop_range(vec![
        root("otlpprobe.tsamp.037.string_root"),
        child("otlpprobe.tsamp.037.string").with_attribute(SPAN_SAMPLING_MECHANISM, string("8")),
    ]);

    // TSAMP-038: a span with `analytics.event` is an event, and a dropped trace sends it alone, marked as dropped.
    probes.in_drop_range(vec![
        root("otlpprobe.tsamp.038.metric_root"),
        with_analytics_event(child("otlpprobe.tsamp.038.metric")),
    ]);

    // TSAMP-038: the configured rate for the service and name of a span makes it an event. This is the only name with
    // a configured rate.
    probes.in_drop_range(vec![
        span_in(ANALYTICS_SERVICE, "otlpprobe.tsamp.038.fixed", SpanKind::Server),
        span_in(ANALYTICS_SERVICE, "otlpprobe.tsamp.038.fixed", SpanKind::Internal).child_of(1),
    ]);

    // TSAMP-039: a kept trace keeps its events with the other spans, not marked as dropped.
    probes.in_keep_range(vec![
        root("otlpprobe.tsamp.039.kept_root"),
        with_analytics_event(child("otlpprobe.tsamp.039.kept_event")),
    ]);

    // TSAMP-040: a client that marks a span with `_dd.analyzed` does not make it an event.
    probes.in_drop_range(vec![
        root("otlpprobe.tsamp.040.client_root"),
        child("otlpprobe.tsamp.040.client").with_attribute("_dd.analyzed", int(1)),
    ]);

    // TSAMP-003: a kept trace holds its decision maker in the first span that it lists. The Agent and ADP differ when
    // that span is not the root.
    probes.in_keep_range(vec![
        span_in(
            SAMPLING_SERVICE,
            "otlpprobe.tsamp.003.child_first_child",
            SpanKind::Internal,
        )
        .child_of(2),
        root("otlpprobe.tsamp.003.child_first"),
    ]);

    // TSAMP-045: the stats count a trace that the sampler drops, and one that it keeps, with one name.
    probes.in_keep_range(vec![root("otlpprobe.tsamp.045.mixed")]);
    probes.in_drop_range(vec![root("otlpprobe.tsamp.045.mixed")]);

    // TSTAT-009: a trace with two roots. The weight of the stats comes from the root of the trace, which is the first
    // root to one side and the last to the other.
    probes.in_keep_range(vec![
        root("otlpprobe.tsamp.tstat009.first"),
        root("otlpprobe.tsamp.tstat009.second").with_attribute("_sample_rate", double(0.5)),
    ]);

    probes
}

/// Spans of the services `probe-sampling` and `probe-analytics`, with one probe for each decision that the OTLP
/// receiver and the samplers of the trace agent make.
///
/// The cases that use it set the sampling percentage of the receiver to 50. The trace of each probe has a trace ID that
/// the receiver keeps or drops at that percentage, and the traces of TSAMP-013 have trace IDs that the error sampler
/// keeps or drops at a rate of 0.1.
pub fn sampling_fixture() -> Vec<u8> {
    sampling_probes().request(&[SAMPLING_SERVICE, ANALYTICS_SERVICE])
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use otlp_protos::opentelemetry::proto::{
        collector::trace::v1::ExportTraceServiceRequest, common::v1::any_value::Value, trace::v1::Span,
    };
    use prost::Message as _;

    use super::*;

    #[test]
    fn sample_by_rate_matches_the_vectors_of_the_agent() {
        // The Agent's `TestCreateChunks` expects the receiver to keep the first trace ID and drop the second at 50 %.
        assert!(sample_by_rate(123, 0.5));
        assert!(!sample_by_rate(1_237_892_138_897, 0.5));
        // A rate of 1 keeps every trace.
        assert!(sample_by_rate(1_237_892_138_897, 1.0));
    }

    #[test]
    fn every_seed_has_the_decision_that_its_probe_needs() {
        let probes = sampling_probes();
        assert_eq!(probes.seeds.len(), probes.traces.len());
        assert_eq!(probes.seeds.len(), probes.decisions.len());
        for (seed, (rate, keeps)) in probes.seeds.iter().zip(&probes.decisions) {
            assert_eq!(
                sample_by_rate(u64::from(*seed), *rate),
                *keeps,
                "seed {seed:#x} at {rate}"
            );
        }

        let distinct: HashSet<u32> = probes.seeds.iter().copied().collect();
        assert_eq!(distinct.len(), probes.seeds.len(), "two traces share a seed");
    }

    /// Returns the spans of `request` in order, with the `service.name` of the resource that holds each.
    fn spans_of(request: &[u8]) -> Vec<(String, Span)> {
        let request = ExportTraceServiceRequest::decode(request).unwrap();
        request
            .resource_spans
            .iter()
            .flat_map(|resource_spans| {
                let service = match resource_spans
                    .resource
                    .as_ref()
                    .unwrap()
                    .attributes
                    .iter()
                    .find(|attribute| attribute.key == "service.name")
                    .and_then(|attribute| attribute.value.as_ref()?.value.clone())
                {
                    Some(Value::StringValue(service)) => service,
                    other => panic!("unexpected service {other:?}"),
                };
                resource_spans.scope_spans[0]
                    .spans
                    .iter()
                    .map(move |span| (service.clone(), span.clone()))
            })
            .collect()
    }

    #[test]
    fn the_request_carries_the_seeds_as_the_low_halves_of_the_trace_ids() {
        let mut trace_ids: Vec<Vec<u8>> = Vec::new();
        for (_, span) in spans_of(&sampling_fixture()) {
            if !trace_ids.contains(&span.trace_id) {
                trace_ids.push(span.trace_id);
            }
        }
        let expected: Vec<Vec<u8>> = sampling_probes()
            .seeds
            .iter()
            .map(|&seed| super::super::trace_id(seed))
            .collect();

        trace_ids.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(trace_ids, expected);
    }

    /// The number of spans of each name in `request`.
    fn names_of(request: &[u8]) -> BTreeMap<String, usize> {
        let mut names = BTreeMap::new();
        for (_, span) in spans_of(request) {
            *names.entry(span.name).or_default() += 1;
        }
        names
    }

    #[test]
    fn sampling_fixture_counts_match_the_case() {
        // The `otlp-traces-sampling` case checks these counts of the fixture, and the names that it lists.
        let spans = spans_of(&sampling_fixture());
        let traces: HashSet<Vec<u8>> = spans.iter().map(|(_, span)| span.trace_id.clone()).collect();
        assert_eq!(traces.len(), 26);
        assert_eq!(spans.len(), 36);
        assert_eq!(
            spans.iter().filter(|(service, _)| service == ANALYTICS_SERVICE).count(),
            2
        );
        assert!(spans
            .iter()
            .all(|(service, _)| service == SAMPLING_SERVICE || service == ANALYTICS_SERVICE));
    }

    #[test]
    fn each_probe_has_a_name_of_its_own() {
        let names = names_of(&sampling_fixture());
        assert!(
            names.keys().all(|name| name.starts_with("otlpprobe.tsamp.")),
            "{names:?}"
        );

        // These names repeat on purpose: two traces of a decision, or a span and its child with one configured name.
        let repeated: Vec<(&str, usize)> = names
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(name, count)| (name.as_str(), *count))
            .collect();
        assert_eq!(
            repeated,
            [
                ("otlpprobe.tsamp.002.drop", 2),
                ("otlpprobe.tsamp.002.keep", 2),
                ("otlpprobe.tsamp.038.fixed", 2),
                ("otlpprobe.tsamp.045.mixed", 2),
            ]
        );
    }

    #[test]
    fn the_probes_carry_the_attributes_that_they_test() {
        let spans = spans_of(&sampling_fixture());
        let span = |name: &str| spans.iter().find(|(_, span)| span.name == name).unwrap().1.clone();
        let attribute = |span: &Span, key: &str| {
            span.attributes
                .iter()
                .find(|attribute| attribute.key == key)
                .and_then(|attribute| attribute.value.as_ref()?.value.clone())
        };

        assert_eq!(
            attribute(&span("otlpprobe.tsamp.006.none"), PRIORITY),
            Some(Value::IntValue(-128))
        );
        assert_eq!(
            attribute(&span("otlpprobe.tsamp.037.int"), SPAN_SAMPLING_MECHANISM),
            Some(Value::IntValue(8))
        );
        assert_eq!(
            attribute(&span("otlpprobe.tsamp.037.string"), SPAN_SAMPLING_MECHANISM),
            Some(Value::StringValue("8".to_string()))
        );
        assert_eq!(
            attribute(&span("otlpprobe.tsamp.013.rescued"), "_sample_rate"),
            Some(Value::DoubleValue(RESCUE_RATE))
        );
        // Only the probes of TSAMP-028 carry `_dd.prob_sr`.
        for (_, span) in &spans {
            let carries = attribute(span, "_dd.prob_sr").is_some();
            assert_eq!(carries, span.name.starts_with("otlpprobe.tsamp.028."), "{}", span.name);
        }
        assert_eq!(span("otlpprobe.tsamp.021.exception").events[0].name, "exception");
        assert_eq!(span("otlpprobe.tsamp.011.rescue").status.unwrap().code, 2);
    }

    #[test]
    fn the_child_of_the_child_first_probe_comes_before_its_root() {
        let spans = spans_of(&sampling_fixture());
        let position = |name: &str| spans.iter().position(|(_, span)| span.name == name).unwrap();
        let child = &spans[position("otlpprobe.tsamp.003.child_first_child")].1;
        let root = &spans[position("otlpprobe.tsamp.003.child_first")].1;

        assert!(position("otlpprobe.tsamp.003.child_first_child") < position("otlpprobe.tsamp.003.child_first"));
        assert_eq!(child.parent_span_id, root.span_id);
        assert!(root.parent_span_id.is_empty());
    }

    #[test]
    fn the_two_roots_of_the_tstat_probe_have_no_parent() {
        let spans = spans_of(&sampling_fixture());
        let first = &spans
            .iter()
            .find(|(_, span)| span.name == "otlpprobe.tsamp.tstat009.first")
            .unwrap()
            .1;
        let second = &spans
            .iter()
            .find(|(_, span)| span.name == "otlpprobe.tsamp.tstat009.second")
            .unwrap()
            .1;

        assert!(first.parent_span_id.is_empty() && second.parent_span_id.is_empty());
        assert_eq!(first.trace_id, second.trace_id);
        assert!(first.attributes.iter().all(|attribute| attribute.key != "_sample_rate"));
        assert!(second
            .attributes
            .iter()
            .any(|attribute| attribute.key == "_sample_rate"));
    }
}
