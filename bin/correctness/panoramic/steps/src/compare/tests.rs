use std::{fs, path::Path};

use panoramic_core::step::{
    test_support::{run_step_with_participants, FakeParticipant},
    ExecCommand,
};
use serde_json::Value;

use super::*;
use crate::test_support::{output, valid_fields};

const KINDS: [&str; 6] = [
    "events",
    "metrics",
    "service_checks",
    "spans",
    "trace_stats",
    "dogstatsd_forwarded",
];

fn metric(value: f64) -> Value {
    json!([{"context": {"name": "example.counter", "tags": ["env:test"]},
        "values": [[10, {"mtype": "Count", "value": value}]]}])
}

fn snapshots(fake: &FakeParticipant, metrics: Value, packets: &[&[u8]]) {
    for value in [
        json!([]),
        metrics,
        json!([]),
        json!([]),
        json!({"groups": {}}),
        json!(packets),
    ] {
        fake.push_exec(Ok(output(value)));
    }
}

fn assert_fetched_once(fake: &FakeParticipant) {
    let commands = KINDS.map(|kind| {
        (
            ExecCommand {
                argv: vec!["dump".into(), kind.into()],
                env: vec![],
                stdin: None,
            },
            DUMP_TIMEOUT,
        )
    });
    fake.assert_executed(&commands);
}

async fn compare(fields: &StepFields, subject: &FakeParticipant, baseline: &FakeParticipant, dir: &Path) -> StepReport {
    run_step_with_participants(
        &CompareFactory,
        fields,
        &[("subject", subject), ("baseline", baseline)],
        dir,
    )
    .await
}

#[tokio::test]
async fn matching_metrics_are_fetched_once_and_written_with_complete_sidecars() {
    let subject = FakeParticipant::default();
    let baseline = FakeParticipant::default();
    snapshots(&subject, metric(3.0), &[b"example.counter:3|c"]);
    snapshots(&baseline, metric(3.0), &[b"example.counter:3|c"]);
    let dir = tempfile::tempdir().unwrap();
    let report = compare(&valid_fields(Compare::NAME), &subject, &baseline, dir.path()).await;
    assert_eq!(report.status, AssertionStatus::Passed, "{}", report.message);
    assert_eq!(report.message, "Telemetry matches the baseline.");
    assert_fetched_once(&subject);
    assert_fetched_once(&baseline);
    for side in Side::ALL {
        let traffic = dir.path().join("traffic");
        let compressed = fs::read(traffic.join(side.records_file_name())).unwrap();
        let decoded = zstd::stream::decode_all(compressed.as_slice()).unwrap();
        let records: Vec<Value> = String::from_utf8(decoded.clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records,
            vec![
                json!({"kind": "metric", "index": 0, "value": metric(3.0)[0]}),
                json!({"kind": "trace_stats", "index": 0, "value": {"groups": {}}}),
                json!({"kind": "dogstatsd_forwarded_packet", "index": 0, "value": "ZXhhbXBsZS5jb3VudGVyOjN8Yw=="}),
            ]
        );
        let manifest: Value =
            serde_json::from_slice(&fs::read(traffic.join(side.manifest_file_name())).unwrap()).unwrap();
        assert_eq!(manifest["format"], "panoramic-decoded-capture");
        assert_eq!(manifest["version"], 1);
        assert_eq!(manifest["side"], side.name());
        assert_eq!(
            manifest["role"],
            match side {
                Side::Baseline => "baseline participant 'baseline'",
                Side::Comparison => "subject participant 'subject'",
            }
        );
        assert_eq!(manifest["records_file"], side.records_file_name());
        assert_eq!(manifest["compression"], "zstd");
        assert_eq!(manifest["source"], "participant dump operation");
        assert_eq!(manifest["ordering"], "grouped_by_kind_then_intake_dump_order");
        assert_eq!(manifest["records"], 3);
        assert_eq!(manifest["uncompressed_bytes"], decoded.len());
        assert_eq!(
            manifest["records_by_kind"],
            json!([
                {"kind": "event", "count": 0}, {"kind": "metric", "count": 1},
                {"kind": "service_check", "count": 0}, {"kind": "span", "count": 0},
                {"kind": "trace_stats", "count": 1}, {"kind": "dogstatsd_forwarded_packet", "count": 1},
            ])
        );
        assert_eq!(manifest["digest"]["algorithm"], "fnv1a64");
        assert_eq!(manifest["digest"]["value"].as_str().unwrap().len(), 16);
    }
}

#[tokio::test]
async fn a_metric_mismatch_retains_values_and_context_in_the_report() {
    let subject = FakeParticipant::default();
    let baseline = FakeParticipant::default();
    snapshots(&subject, metric(4.0), &[]);
    snapshots(&baseline, metric(3.0), &[]);
    let dir = tempfile::tempdir().unwrap();
    let report = compare(&valid_fields(Compare::NAME), &subject, &baseline, dir.path()).await;
    assert_eq!(report.status, AssertionStatus::Failed);
    for detail in ["example.counter", "env:test", "count(3)", "count(4)"] {
        assert!(report.message.contains(detail), "{}", report.message);
    }
    assert!(dir.path().join("traffic/comparison-decoded.jsonl.zst").is_file());
}

#[tokio::test]
async fn forwarded_packets_are_compared_even_when_not_required() {
    for (required, packets, expected) in [
        (false, &[][..], AssertionStatus::Passed),
        (true, &[][..], AssertionStatus::Failed),
        (false, &[b"extra:1|c".as_slice()][..], AssertionStatus::Failed),
    ] {
        let subject = FakeParticipant::default();
        let baseline = FakeParticipant::default();
        snapshots(&subject, metric(1.0), packets);
        snapshots(&baseline, metric(1.0), &[]);
        let mut fields = valid_fields(Compare::NAME);
        fields.insert("require_forwarded_packets".into(), json!(required));
        let dir = tempfile::tempdir().unwrap();
        let report = compare(&fields, &subject, &baseline, dir.path()).await;
        assert_eq!(report.status, expected, "{}", report.message);
        if required {
            assert!(report.message.contains("No forwarded DogStatsD packets were captured."));
            assert!(report
                .message
                .contains("both baseline and comparison captures were empty"));
        } else if !packets.is_empty() {
            assert!(report
                .message
                .contains("extra forwarded DogStatsD message in comparison"));
            assert!(report
                .message
                .contains("message_base64=ZXh0cmE6MXxj, comparison_count=1"));
        }
    }
}

#[tokio::test]
async fn collection_errors_distinguish_the_harness_from_the_product() {
    for (error, expected) in [
        (
            HandleError::Harness {
                message: "request route lost".into(),
            },
            AssertionStatus::Errored,
        ),
        (
            HandleError::Product {
                message: "dump unavailable".into(),
            },
            AssertionStatus::Failed,
        ),
    ] {
        let subject = FakeParticipant::default();
        let baseline = FakeParticipant::default();
        baseline.push_exec(Err(error.clone()));
        snapshots(&subject, metric(1.0), &[]);
        let dir = tempfile::tempdir().unwrap();
        let report = compare(&valid_fields(Compare::NAME), &subject, &baseline, dir.path()).await;
        assert_eq!(report.status, expected);
        assert_eq!(report.message, format!("Failed to collect 'baseline': {error}"));
        assert_fetched_once(&subject);
    }
}

#[tokio::test]
async fn invalid_telemetry_shapes_fail_with_the_dump_kind() {
    let subject = FakeParticipant::default();
    let baseline = FakeParticipant::default();
    baseline.push_exec(Ok(output(json!({"unexpected": true}))));
    snapshots(&subject, metric(1.0), &[]);
    let dir = tempfile::tempdir().unwrap();
    let report = compare(&valid_fields(Compare::NAME), &subject, &baseline, dir.path()).await;
    assert_eq!(report.status, AssertionStatus::Failed);
    assert!(
        report
            .message
            .starts_with("Failed to collect 'baseline': Failed to decode dumped events from intake:"),
        "{}",
        report.message
    );
}

#[tokio::test]
async fn a_capture_failure_records_its_reason_without_changing_the_comparison() {
    let subject = FakeParticipant::default();
    let baseline = FakeParticipant::default();
    snapshots(&subject, metric(1.0), &[]);
    snapshots(&baseline, metric(1.0), &[]);
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("traffic/comparison-decoded-manifest.json")).unwrap();
    let report = compare(&valid_fields(Compare::NAME), &subject, &baseline, dir.path()).await;
    assert_eq!(report.status, AssertionStatus::Passed, "{}", report.message);
    let reason = fs::read_to_string(dir.path().join("traffic/comparison-decoded-unavailable.txt")).unwrap();
    assert!(reason.starts_with("Failed to write the decoded capture:"), "{reason}");
}

#[test]
fn comparison_requires_distinct_named_participants_and_known_kinds() {
    let mut fields = valid_fields(Compare::NAME);
    fields.insert("baseline".into(), json!("subject"));
    assert_eq!(
        CompareFactory.build(&fields).unwrap_err(),
        "compare requires distinct subject and baseline intake participants"
    );
    for field in ["kind", "subject", "baseline"] {
        let mut fields = valid_fields(Compare::NAME);
        fields.remove(field);
        assert!(CompareFactory
            .build(&fields)
            .unwrap_err()
            .contains(&format!("missing field `{field}`")));
    }
    for kind in ["metrics", "events", "service_checks", "traces"] {
        let mut fields = valid_fields(Compare::NAME);
        fields.insert("kind".into(), json!(kind));
        assert!(CompareFactory.build(&fields).is_ok());
    }
    fields = valid_fields(Compare::NAME);
    fields.insert("kind".into(), json!("dogstatsd_forwarded"));
    assert!(CompareFactory.build(&fields).unwrap_err().contains("unknown variant"));
}

#[test]
fn trace_options_are_typed_and_require_trace_comparison() {
    for (field, invalid) in [
        ("otlp_direct", json!("true")),
        ("ignore_fields", json!(true)),
        ("require_forwarded_packets", json!(1)),
    ] {
        let mut fields = valid_fields(Compare::NAME);
        fields.insert(field.into(), invalid);
        assert!(CompareFactory.build(&fields).is_err());
    }
    for (field, value) in [("otlp_direct", json!(true)), ("ignore_fields", json!(["meta.custom"]))] {
        let mut fields = valid_fields(Compare::NAME);
        fields.insert(field.into(), value);
        assert_eq!(
            CompareFactory.build(&fields).unwrap_err(),
            "compare otlp_direct and ignore_fields require kind: traces"
        );
        fields.insert("kind".into(), json!("traces"));
        assert!(CompareFactory.build(&fields).is_ok());
    }
}

#[tokio::test]
#[should_panic(expected = "calls operation 'dump' of participant 'baseline'")]
async fn both_participants_must_offer_dump_at_plan_compile() {
    let subject = FakeParticipant::default();
    let baseline = FakeParticipant::default().with_operations(&[]);
    let dir = tempfile::tempdir().unwrap();
    compare(&valid_fields(Compare::NAME), &subject, &baseline, dir.path()).await;
}

#[tokio::test]
async fn event_and_service_check_kinds_reach_their_analyzers() {
    let event = json!({"title": "title", "text": "text", "alert_type": "info", "aggregation_key": "",
        "hostname": "test", "priority": "normal", "source_type_name": "test", "tags": [], "timestamp": 1});
    let check = json!({"name": "example.check", "status": 0, "hostname": "test", "message": "healthy", "tags": [], "timestamp": 1});
    for (kind, index, record, key, changed) in [
        ("events", 0, event, "text", json!("different")),
        ("service_checks", 2, check, "status", json!(2)),
    ] {
        for matches in [true, false] {
            let subject = FakeParticipant::default();
            let baseline = FakeParticipant::default();
            let mut values = [
                json!([]),
                json!([]),
                json!([]),
                json!([]),
                json!({"groups": {}}),
                json!([]),
            ];
            values[index] = json!([record]);
            for value in &values {
                baseline.push_exec(Ok(output(value.clone())));
            }
            if !matches {
                values[index][0][key] = changed.clone();
            }
            for value in values {
                subject.push_exec(Ok(output(value)));
            }
            let mut fields = valid_fields(Compare::NAME);
            fields.insert("kind".into(), json!(kind));
            let dir = tempfile::tempdir().unwrap();
            let report = compare(&fields, &subject, &baseline, dir.path()).await;
            assert_eq!(
                report.status,
                if matches {
                    AssertionStatus::Passed
                } else {
                    AssertionStatus::Failed
                },
                "{}",
                report.message
            );
            if !matches {
                assert!(report.message.contains(key), "{}", report.message);
            }
        }
    }
}

#[tokio::test]
async fn trace_options_control_metadata_requirements_and_ignored_fields() {
    use datadog_protos::traces::{AgentPayload, Span, TraceChunk, TracerPayload};

    let mut span = Span::new();
    span.traceID = 1;
    span.spanID = 2;
    span.meta.insert("custom".into(), "baseline".into());
    let mut chunk = TraceChunk::new();
    chunk.spans.push(span);
    let mut tracer = TracerPayload::new();
    tracer.chunks.push(chunk);
    let mut payload = AgentPayload::new();
    payload.tracerPayloads.push(tracer);
    let spans = serde_json::to_value(stele::Span::get_spans_from_agent_payload(&payload)).unwrap();
    for (otlp_direct, ignore, status) in [
        (false, true, AssertionStatus::Failed),
        (true, false, AssertionStatus::Failed),
        (true, true, AssertionStatus::Passed),
    ] {
        let subject = FakeParticipant::default();
        let baseline = FakeParticipant::default();
        let mut values = [
            json!([]),
            json!([]),
            json!([]),
            spans.clone(),
            json!({"groups": {}}),
            json!([]),
        ];
        for value in &values {
            baseline.push_exec(Ok(output(value.clone())));
        }
        values[3][0]["meta"]["custom"] = json!("subject");
        for value in values {
            subject.push_exec(Ok(output(value)));
        }
        let mut fields = valid_fields(Compare::NAME);
        fields.insert("kind".into(), json!("traces"));
        fields.insert("otlp_direct".into(), json!(otlp_direct));
        fields.insert(
            "ignore_fields".into(),
            if ignore { json!(["meta.custom"]) } else { json!([]) },
        );
        let dir = tempfile::tempdir().unwrap();
        let report = compare(&fields, &subject, &baseline, dir.path()).await;
        assert_eq!(report.status, status, "{}", report.message);
        if otlp_direct && !ignore {
            assert!(report.message.contains("meta.custom"), "{}", report.message);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn both_snapshots_are_fetched_concurrently() {
    let mut subject = FakeParticipant::default();
    let mut baseline = FakeParticipant::default();
    subject.delay_exec(Duration::from_secs(1));
    baseline.delay_exec(Duration::from_secs(1));
    snapshots(&subject, metric(1.0), &[]);
    snapshots(&baseline, metric(1.0), &[]);
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let report = compare(&valid_fields(Compare::NAME), &subject, &baseline, dir.path()).await;
    assert_eq!(report.status, AssertionStatus::Passed, "{}", report.message);
    assert_eq!(started.elapsed(), Duration::from_secs(KINDS.len() as u64));
    assert_fetched_once(&subject);
    assert_fetched_once(&baseline);
}

#[tokio::test]
async fn required_metric_tag_keys_need_one_enriched_workload_context_on_each_side() {
    let mut enriched = metric(1.0);
    enriched[0]["context"]["tags"] = json!(["env:test", "origin:sender", "image:tools"]);
    let mut empty = enriched.clone();
    empty[0]["context"]["tags"] = json!(["origin:", "image:tools"]);
    let mut telemetry = enriched.clone();
    telemetry[0]["context"]["name"] = json!("system.cpu");
    let mut split = json!([metric(1.0)[0].clone(), metric(1.0)[0].clone()]);
    split[0]["context"]["tags"] = json!(["origin:sender"]);
    split[1]["context"]["tags"] = json!(["image:tools"]);
    for (baseline_metrics, subject_metrics, missing) in [
        (enriched.clone(), enriched.clone(), vec![]),
        (metric(1.0), metric(1.0), vec!["baseline", "comparison"]),
        (metric(1.0), enriched.clone(), vec!["baseline"]),
        (enriched.clone(), metric(1.0), vec!["comparison"]),
        (empty.clone(), empty, vec!["baseline", "comparison"]),
        (telemetry.clone(), telemetry, vec!["baseline", "comparison"]),
        (split.clone(), split, vec!["baseline", "comparison"]),
    ] {
        let subject = FakeParticipant::default();
        let baseline = FakeParticipant::default();
        snapshots(&subject, subject_metrics, &[]);
        snapshots(&baseline, baseline_metrics, &[]);
        let mut fields = valid_fields(Compare::NAME);
        fields.insert("required_metric_tag_keys".into(), json!(["origin", "image"]));
        let dir = tempfile::tempdir().unwrap();
        let report = compare(&fields, &subject, &baseline, dir.path()).await;
        if missing.is_empty() {
            assert_eq!(report.status, AssertionStatus::Passed, "{}", report.message);
        } else {
            assert_eq!(report.status, AssertionStatus::Failed, "{}", report.message);
            let mut message = "Missing required metric tags.".to_string();
            for side in missing {
                message.push_str(&format!(
                    "\n{side} has no workload metric context with nonempty tags: origin, image"
                ));
            }
            assert_eq!(report.message, message);
        }
        for side in Side::ALL {
            assert!(dir.path().join("traffic").join(side.records_file_name()).is_file());
        }
    }
}

#[test]
fn required_metric_tag_keys_accept_only_bare_keys_for_metrics() {
    let mut fields = valid_fields(Compare::NAME);
    for value in [
        json!(true),
        json!("origin"),
        json!([false]),
        json!([""]),
        json!(["origin:sender"]),
        json!([" origin"]),
    ] {
        fields.insert("required_metric_tag_keys".into(), value.clone());
        assert!(CompareFactory.build(&fields).is_err(), "{value}");
    }
    fields.insert("required_metric_tag_keys".into(), json!(["origin"]));
    assert!(CompareFactory.build(&fields).is_ok());
    for kind in ["events", "service_checks", "traces"] {
        fields.insert("kind".into(), json!(kind));
        assert_eq!(
            CompareFactory.build(&fields).unwrap_err(),
            "compare required_metric_tag_keys requires kind: metrics"
        );
    }
}
