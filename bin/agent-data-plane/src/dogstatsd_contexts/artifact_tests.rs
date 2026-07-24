use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateMetricType};
use saluki_context::{
    tags::{Tag, TagSet},
    Context,
};
use serde_json::json;
use stringtheory::MetaString;

use super::{
    for_each_record, publish_context_dump, write_snapshot_records, AgentContextRecord, ArtifactError,
    CONTEXT_DUMP_FILENAME,
};
use crate::dogstatsd_contexts::read_report;

impl ArtifactError {
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn operation(&self) -> &'static str {
        self.operation
    }

    fn record_index(&self) -> usize {
        self.record_index
    }
}

fn collect_records(path: &std::path::Path) -> Result<Vec<AgentContextRecord>, ArtifactError> {
    let mut records = Vec::new();
    for_each_record(path, |record| records.push(record))?;
    Ok(records)
}

const PLAIN_FIXTURE_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/dogstatsd_contexts_agent.ndjson"
));
const COMPRESSED_FIXTURE_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/dogstatsd_contexts_agent.ndjson.zstd"
));
const PLAIN_FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/dogstatsd_contexts_agent.ndjson"
);
const COMPRESSED_FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/dogstatsd_contexts_agent.ndjson.zstd"
);

// These fixtures and expectations mirror ContextDebugRepr and the `dogstatsd top` command at Datadog Agent
// commit 9de2a8371cf8d95794f238939709491907893288.
fn expected_fixture_records() -> Vec<AgentContextRecord> {
    vec![
        AgentContextRecord {
            name: "z.metric".to_owned(),
            host: "node-a".to_owned(),
            metric_type: "Gauge".to_owned(),
            tagger_tags: vec!["pod_name:web-a".to_owned()],
            metric_tags: vec![
                "env:prod".to_owned(),
                "image:registry/repo:v1".to_owned(),
                "bare".to_owned(),
            ],
            no_index: false,
            source: 1,
        },
        AgentContextRecord {
            name: "a.metric".to_owned(),
            host: "node-a".to_owned(),
            metric_type: "Counter".to_owned(),
            tagger_tags: vec!["pod_name:web-a".to_owned()],
            metric_tags: vec!["env:prod".to_owned(), "service:web".to_owned()],
            no_index: false,
            source: 1,
        },
        AgentContextRecord {
            name: "a.metric".to_owned(),
            host: "node-b".to_owned(),
            metric_type: "Counter".to_owned(),
            tagger_tags: vec!["pod_name:web-b".to_owned()],
            metric_tags: vec!["env:prod".to_owned(), "service:web".to_owned()],
            no_index: false,
            source: 1,
        },
        AgentContextRecord {
            name: "a.metric".to_owned(),
            host: "node-c".to_owned(),
            metric_type: "Counter".to_owned(),
            tagger_tags: vec!["pod_name:web-c".to_owned()],
            metric_tags: vec!["env:staging".to_owned(), "service:web".to_owned()],
            no_index: false,
            source: 1,
        },
    ]
}

#[test]
fn streams_exact_agent_schema_for_every_metric_type() {
    let cases = [
        (AggregateMetricType::Counter, None, "Counter"),
        (AggregateMetricType::Rate, None, "Rate"),
        (AggregateMetricType::Gauge, None, "Gauge"),
        (AggregateMetricType::Set, None, "Set"),
        (AggregateMetricType::Distribution, None, "Distribution"),
        (AggregateMetricType::Histogram, None, "Histogram"),
        (AggregateMetricType::Histogram, Some("second"), "Histogram"),
        (AggregateMetricType::Histogram, Some("millisecond"), "Historate"),
    ];
    let snapshot: Vec<_> = cases
        .iter()
        .enumerate()
        .map(|(index, (metric_type, unit, _))| {
            snapshot_entry(
                Box::leak(format!("metric.{index}").into_boxed_str()),
                (index == 0).then_some("node-a"),
                *metric_type,
                unit.unwrap_or_default(),
                &["client:value", "bare"],
                &["origin:value"],
            )
        })
        .collect();
    let mut output = Vec::new();

    write_snapshot_records(&mut output, &snapshot, PathBuf::from("plain-stream").as_path())
        .expect("snapshot should serialize");

    let output = String::from_utf8(output).expect("serialized output should be UTF-8");
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), cases.len());
    assert_eq!(output.bytes().filter(|byte| *byte == b'\n').count(), cases.len());
    assert!(output.ends_with('\n'));
    for (index, ((_, _, expected_type), line)) in cases.iter().zip(&lines).enumerate() {
        let expected_host = if index == 0 { "node-a" } else { "" };
        let expected = json!({
            "Name": format!("metric.{index}"),
            "Host": expected_host,
            "Type": expected_type,
            "TaggerTags": ["origin:value"],
            "MetricTags": ["client:value", "bare"],
            "NoIndex": false,
            "Source": 1,
        });
        assert_eq!(serde_json::from_str::<serde_json::Value>(line).unwrap(), expected);
        assert_eq!(
            *line,
            format!(
                "{{\"Name\":\"metric.{index}\",\"Host\":\"{expected_host}\",\"Type\":\"{expected_type}\",\"TaggerTags\":[\"origin:value\"],\"MetricTags\":[\"client:value\",\"bare\"],\"NoIndex\":false,\"Source\":1}}"
            )
        );
    }
}

#[test]
fn plain_stream_round_trips_through_reader_and_report() {
    let snapshot = vec![
        snapshot_entry(
            "shared.metric",
            Some("node-a"),
            AggregateMetricType::Gauge,
            "",
            &["env:prod", "service:web"],
            &["pod_name:web-a"],
        ),
        snapshot_entry(
            "shared.metric",
            None,
            AggregateMetricType::Counter,
            "",
            &["env:staging", "service:web"],
            &["pod_name:web-b"],
        ),
    ];
    let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
    let mut file = artifact.reopen().expect("temporary artifact should reopen");

    write_snapshot_records(&mut file, &snapshot, artifact.path()).expect("snapshot should serialize");
    file.flush().expect("plain stream should flush");

    assert_eq!(
        collect_records(artifact.path()).expect("plain stream should decode"),
        vec![
            AgentContextRecord {
                name: "shared.metric".to_owned(),
                host: "node-a".to_owned(),
                metric_type: "Gauge".to_owned(),
                tagger_tags: vec!["pod_name:web-a".to_owned()],
                metric_tags: vec!["env:prod".to_owned(), "service:web".to_owned()],
                no_index: false,
                source: 1,
            },
            AgentContextRecord {
                name: "shared.metric".to_owned(),
                host: String::new(),
                metric_type: "Counter".to_owned(),
                tagger_tags: vec!["pod_name:web-b".to_owned()],
                metric_tags: vec!["env:staging".to_owned(), "service:web".to_owned()],
                no_index: false,
                source: 1,
            },
        ]
    );
    assert_eq!(
        read_report(artifact.path())
            .expect("plain stream report should build")
            .render(10, 10),
        concat!(
            "   Contexts\tMetric name\t(number of unique values for each tag)\n",
            "          2\tshared.metric\t(2 env, 1 service)\n",
        )
    );
}

#[test]
fn surfaces_a_partial_writer_failure_with_record_and_path_context() {
    let snapshot = [snapshot_entry(
        "write.failure",
        None,
        AggregateMetricType::Gauge,
        "",
        &["env:test"],
        &[],
    )];
    let path = PathBuf::from("/run/test/dogstatsd_contexts.json.zstd");
    let mut writer = FailAfterBytes::new(12);

    let error = write_snapshot_records(&mut writer, &snapshot, &path).expect_err("write failure should surface");

    assert_eq!(writer.written.len(), 12);
    assert!(error.to_string().contains("serialize DogStatsD context dump record 1"));
    assert!(error.to_string().contains(&path.display().to_string()));
    assert!(format!("{error:#}").contains("injected write failure"));
}

#[test]
fn publishes_a_decodable_mode_0600_dump_and_replaces_the_canonical_artifact() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    fs::write(&target, b"old canonical artifact").expect("old canonical artifact should be written");
    let snapshot = vec![
        snapshot_entry(
            "published.metric",
            Some("node-a"),
            AggregateMetricType::Histogram,
            "millisecond",
            &["env:prod"],
            &["pod_name:web-a"],
        ),
        snapshot_entry(
            "published.metric",
            None,
            AggregateMetricType::Distribution,
            "",
            &["env:staging"],
            &["pod_name:web-b"],
        ),
    ];

    let published = publish_context_dump(run_directory.path(), &snapshot).expect("snapshot should publish");

    assert_eq!(published, target);
    assert_eq!(
        collect_records(&target)
            .expect("published artifact should decode")
            .len(),
        2
    );
    assert_eq!(
        read_report(&target)
            .expect("published report should build")
            .render(10, 10),
        concat!(
            "   Contexts\tMetric name\t(number of unique values for each tag)\n",
            "          2\tpublished.metric\t(2 env)\n",
        )
    );
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn rejects_empty_missing_and_non_directory_run_paths_with_target_and_operation_context() {
    let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
    let missing = temporary_directory.path().join("missing");
    let not_directory = temporary_directory.path().join("not-a-directory");
    fs::write(&not_directory, b"file").expect("non-directory fixture should be written");
    let cases = [
        (PathBuf::new(), "validate run path"),
        (missing, "create temporary"),
        (not_directory, "create temporary"),
    ];

    for (run_path, expected_operation) in cases {
        let error = publish_context_dump(&run_path, &[]).expect_err("invalid run path should fail");
        let message = format!("{error:#}");
        assert!(message.contains(CONTEXT_DUMP_FILENAME), "{message}");
        assert!(message.contains(expected_operation), "{message}");
    }
}

#[test]
fn persist_failure_is_contextual_and_removes_the_temporary_file() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    fs::create_dir(&target).expect("directory should block file persistence");

    let error = publish_context_dump(run_directory.path(), &[]).expect_err("persistence should fail");

    let message = format!("{error:#}");
    assert!(message.contains("atomically replace"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

fn directory_entries(path: &std::path::Path) -> Vec<String> {
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort_unstable();
    entries
}

struct FailAfterBytes {
    remaining: usize,
    written: Vec<u8>,
}

impl FailAfterBytes {
    fn new(remaining: usize) -> Self {
        Self {
            remaining,
            written: Vec::new(),
        }
    }
}

impl io::Write for FailAfterBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::other("injected write failure"));
        }
        let written = self.remaining.min(bytes.len());
        self.written.extend_from_slice(&bytes[..written]);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn snapshot_entry(
    name: &'static str, host: Option<&'static str>, metric_type: AggregateMetricType, unit: &'static str,
    metric_tags: &[&'static str], origin_tags: &[&'static str],
) -> AggregateContextSnapshotEntry {
    let context = Context::from_static_parts(name, metric_tags)
        .with_host(host.map(MetaString::from_static))
        .with_origin_tags(origin_tags.iter().map(|tag| Tag::from_static(tag)).collect::<TagSet>());
    AggregateContextSnapshotEntry::for_test(context, metric_type, MetaString::from_static(unit))
}

#[test]
fn detects_plain_and_compressed_artifacts_by_magic_bytes() {
    let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
    let compressed_without_suffix = temporary_directory.path().join("compressed-contexts");
    let plain_with_compressed_suffix = temporary_directory.path().join("plain-contexts.zstd");
    fs::write(&compressed_without_suffix, COMPRESSED_FIXTURE_BYTES).expect("compressed fixture should be copied");
    fs::write(&plain_with_compressed_suffix, PLAIN_FIXTURE_BYTES).expect("plain fixture should be copied");

    let cases = [
        PathBuf::from(PLAIN_FIXTURE_PATH),
        PathBuf::from(COMPRESSED_FIXTURE_PATH),
        compressed_without_suffix,
        plain_with_compressed_suffix,
    ];
    let expected = expected_fixture_records();

    for path in cases {
        assert_eq!(
            collect_records(&path).expect("artifact should decode"),
            expected,
            "{path:?}"
        );
    }
}

#[test]
fn checked_in_compressed_fixture_expands_to_the_plain_fixture_exactly() {
    let decompressed =
        zstd::stream::decode_all(COMPRESSED_FIXTURE_BYTES).expect("checked-in compressed fixture should decompress");

    assert_eq!(decompressed, PLAIN_FIXTURE_BYTES);
    assert_eq!(PLAIN_FIXTURE_BYTES.last(), Some(&b'\n'));
}

#[test]
fn rejects_invalid_artifacts_with_path_operation_and_record_index() {
    let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
    let valid_record = concat!(
        "{\"Name\":\"metric\",\"Host\":\"host\",\"Type\":\"Gauge\",",
        "\"TaggerTags\":[],\"MetricTags\":[],\"NoIndex\":false,\"Source\":1}"
    );
    let truncated_length = COMPRESSED_FIXTURE_BYTES.len() - 4;
    let cases = [
        InvalidArtifactCase {
            filename: "array.ndjson",
            bytes: format!("[{valid_record}]").into_bytes(),
            expected_record_index: 1,
        },
        InvalidArtifactCase {
            filename: "missing-field.ndjson",
            bytes: br#"{"Name":"metric"}"#.to_vec(),
            expected_record_index: 1,
        },
        InvalidArtifactCase {
            filename: "malformed-trailing.ndjson",
            bytes: format!("{valid_record}\nnot-json").into_bytes(),
            expected_record_index: 2,
        },
        InvalidArtifactCase {
            filename: "truncated.zstd",
            bytes: COMPRESSED_FIXTURE_BYTES[..truncated_length].to_vec(),
            expected_record_index: 5,
        },
    ];

    for case in cases {
        let path = temporary_directory.path().join(case.filename);
        fs::write(&path, case.bytes).expect("invalid artifact should be written");

        let error = collect_records(&path).expect_err("invalid artifact should be rejected");

        assert_eq!(error.path(), path.as_path(), "{}", case.filename);
        assert_eq!(error.operation(), "decode", "{}", case.filename);
        assert_eq!(error.record_index(), case.expected_record_index, "{}", case.filename);
        assert!(error.to_string().contains(&path.display().to_string()), "{error}");
        assert!(
            error
                .to_string()
                .contains(&format!("decode record {}", case.expected_record_index)),
            "{error}"
        );
        assert!(error.to_string().contains("JSON"), "{error}");
        assert!(error.to_string().contains("line"), "{error}");
        assert!(error.to_string().contains("column"), "{error}");
    }
}

struct InvalidArtifactCase {
    filename: &'static str,
    bytes: Vec<u8>,
    expected_record_index: usize,
}

#[test]
fn ignores_unknown_fields_but_requires_every_agent_field() {
    let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
    let record = concat!(
        "{\"Name\":\"metric\",\"Host\":\"host\",\"Type\":\"Gauge\",",
        "\"TaggerTags\":[],\"MetricTags\":[\"env:prod\"],\"NoIndex\":false,\"Source\":7,",
        "\"FutureField\":\"ignored\"}\n"
    );
    fs::write(artifact.path(), record).expect("artifact should be written");

    assert_eq!(
        collect_records(artifact.path()).expect("record with unknown field should decode"),
        vec![AgentContextRecord {
            name: "metric".to_owned(),
            host: "host".to_owned(),
            metric_type: "Gauge".to_owned(),
            tagger_tags: Vec::new(),
            metric_tags: vec!["env:prod".to_owned()],
            no_index: false,
            source: 7,
        }]
    );
}
