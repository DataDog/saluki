//! The fixtures of the `otlp-traces-hazards` case: requests that a receiver must survive, and a control.
//!
//! The SQL fixture holds statements that start with a `#` comment. All its spans share one request, so a receiver that
//! fails on one statement also loses the other spans. The control fixture follows the hazards on the same sender, so a
//! receiver that is dead or stuck cannot deliver it.

use otlp_protos::opentelemetry::proto::trace::v1::span::SpanKind;

use super::{probe_request, trace_fixture, ProbeResource, ProbeSpan};

/// The seed of the traces of `sql_fixture`: trace `n`, counted from 1, has the seed `HAZARDS_SEED + n`.
const HAZARDS_SEED: u32 = 0x0f00;

/// The seed of the trace of `control_fixture`.
const CONTROL_SEED: u32 = 0x0f10;

/// The service of the spans of `sql_fixture`.
const SQL_SERVICE: &str = "otlp-traces-hazards-sql";

/// The service of the spans of `control_fixture`.
const CONTROL_SERVICE: &str = "otlp-traces-hazards-control";

/// The number of spans in `control_fixture`.
const CONTROL_SPAN_COUNT: u32 = 2;

/// A `CLIENT` span of a database with the legacy attributes `db.system` and `db.statement`.
fn database(name: &'static str, system: &str, statement: &str) -> ProbeSpan {
    ProbeSpan::new(SQL_SERVICE, name, SpanKind::Client)
        .with_strings(&[("db.system", system), ("db.statement", statement)])
}

/// Three traces of four spans, all in one request, from `SQL_SERVICE`. Each trace has a SQL statement that starts with
/// a `#` comment.
///
/// - Trace 1 is a `SERVER` root with a `CLIENT` child. The child has the system `mysql` and the statement `#c`, a line
///   break and `SELECT 1`.
/// - Trace 2 is a `CLIENT` span with the system `postgresql` and the statement `#c`, a line break and `SELECT 2`.
/// - Trace 3 is a `CLIENT` span with the system `mysql` and the statement of a space, `#c`, a line break and `SELECT 3`.
pub fn sql_fixture() -> Vec<u8> {
    let traces = vec![
        vec![
            ProbeSpan::new(SQL_SERVICE, "otlpprobe.hazards.sql.root", SpanKind::Server),
            database("otlpprobe.hazards.sql.mysql", "mysql", "#c\nSELECT 1").child_of(1),
        ],
        vec![database(
            "otlpprobe.hazards.sql.postgresql",
            "postgresql",
            "#c\nSELECT 2",
        )],
        vec![database("otlpprobe.hazards.sql.space", "mysql", " #c\nSELECT 3")],
    ];
    probe_request(&[ProbeResource::new(SQL_SERVICE)], HAZARDS_SEED, &traces)
}

/// One trace of two spans from `CONTROL_SERVICE`. The case sends it after the hazards on the same sender, and a healthy
/// receiver delivers it.
pub fn control_fixture() -> Vec<u8> {
    trace_fixture(CONTROL_SEED, CONTROL_SERVICE, CONTROL_SPAN_COUNT)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use otlp_protos::opentelemetry::proto::{
        collector::trace::v1::ExportTraceServiceRequest,
        common::v1::{any_value::Value, KeyValue},
        trace::v1::Span,
    };
    use prost::Message as _;

    use super::*;

    fn spans(request: Vec<u8>) -> Vec<Span> {
        let request = ExportTraceServiceRequest::decode(request.as_slice()).unwrap();
        assert_eq!(request.resource_spans.len(), 1);
        request.resource_spans[0].scope_spans[0].spans.clone()
    }

    fn attribute<'a>(span: &'a Span, key: &str) -> &'a str {
        span.attributes
            .iter()
            .find_map(|KeyValue { key: found, value }| match &value.as_ref()?.value {
                Some(Value::StringValue(value)) if found == key => Some(value.as_str()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("span {} has no string attribute {key}", span.name))
    }

    #[test]
    fn the_sql_fixture_holds_three_traces_of_four_spans_in_one_request() {
        let spans = spans(sql_fixture());
        assert_eq!(spans.len(), 4);

        let traces: HashSet<&[u8]> = spans.iter().map(|span| span.trace_id.as_slice()).collect();
        assert_eq!(traces.len(), 3);
    }

    #[test]
    fn each_statement_of_the_sql_fixture_starts_with_a_comment() {
        let statements: Vec<(String, String)> = spans(sql_fixture())
            .iter()
            .filter(|span| span.attributes.iter().any(|attribute| attribute.key == "db.statement"))
            .map(|span| {
                (
                    attribute(span, "db.system").to_string(),
                    attribute(span, "db.statement").to_string(),
                )
            })
            .collect();
        let expected = [
            ("mysql", "#c\nSELECT 1"),
            ("postgresql", "#c\nSELECT 2"),
            ("mysql", " #c\nSELECT 3"),
        ];
        assert_eq!(statements.len(), expected.len());
        for ((system, statement), (expected_system, expected_statement)) in statements.iter().zip(expected) {
            assert_eq!(
                (system.as_str(), statement.as_str()),
                (expected_system, expected_statement)
            );
        }
    }

    #[test]
    fn the_control_is_one_trace_of_two_spans_in_its_own_service() {
        let request = ExportTraceServiceRequest::decode(control_fixture().as_slice()).unwrap();
        let spans = &request.resource_spans[0].scope_spans[0].spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].trace_id, spans[1].trace_id);
        assert_ne!(SQL_SERVICE, CONTROL_SERVICE);
    }

    #[test]
    fn no_span_of_the_control_shares_a_trace_with_the_hazards() {
        let hazards: HashSet<Vec<u8>> = spans(sql_fixture()).into_iter().map(|span| span.trace_id).collect();
        let control: HashSet<Vec<u8>> = spans(control_fixture()).into_iter().map(|span| span.trace_id).collect();
        assert!(hazards.is_disjoint(&control));
    }
}
