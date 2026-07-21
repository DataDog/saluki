use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

const ZSTD_MAGIC: &[u8; 4] = b"\x28\xb5\x2f\xfd";

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub(super) struct AgentContextRecord {
    pub(super) name: String,
    pub(super) host: String,
    #[serde(rename = "Type")]
    pub(super) metric_type: String,
    pub(super) tagger_tags: Vec<String>,
    pub(super) metric_tags: Vec<String>,
    pub(super) no_index: bool,
    pub(super) source: u32,
}

#[derive(Debug)]
pub(super) struct ArtifactError {
    path: PathBuf,
    operation: &'static str,
    record_index: usize,
    source: Box<dyn Error + Send + Sync>,
}

impl ArtifactError {
    fn new(
        path: &Path, operation: &'static str, record_index: usize, source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            path: path.to_owned(),
            operation,
            record_index,
            source: Box::new(source),
        }
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(super) fn operation(&self) -> &'static str {
        self.operation
    }

    #[cfg(test)]
    pub(super) fn record_index(&self) -> usize {
        self.record_index
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: failed to {} record {}: {}",
            self.path.display(),
            self.operation,
            self.record_index,
            self.source
        )
    }
}

impl Error for ArtifactError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(super) fn for_each_record(path: &Path, mut consume: impl FnMut(AgentContextRecord)) -> Result<(), ArtifactError> {
    let file = File::open(path).map_err(|error| ArtifactError::new(path, "open", 0, error))?;
    let mut reader = BufReader::new(file);
    let is_compressed = reader
        .fill_buf()
        .map_err(|error| ArtifactError::new(path, "inspect compression magic for", 0, error))?
        .starts_with(ZSTD_MAGIC);

    if is_compressed {
        let decoder = zstd::stream::read::Decoder::with_buffer(reader)
            .map_err(|error| ArtifactError::new(path, "initialize zstd decoder for", 0, error))?;
        decode_records(path, decoder, &mut consume)
    } else {
        decode_records(path, reader, &mut consume)
    }
}

fn decode_records(
    path: &Path, reader: impl Read, consume: &mut impl FnMut(AgentContextRecord),
) -> Result<(), ArtifactError> {
    let records = serde_json::Deserializer::from_reader(reader).into_iter::<AgentContextRecord>();
    for (index, record) in records.enumerate() {
        let record_index = index + 1;
        let record = record.map_err(|error| ArtifactError::new(path, "decode", record_index, error))?;
        consume(record);
    }
    Ok(())
}

#[cfg(test)]
fn collect_records(path: &Path) -> Result<Vec<AgentContextRecord>, ArtifactError> {
    let mut records = Vec::new();
    for_each_record(path, |record| records.push(record))?;
    Ok(records)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{collect_records, AgentContextRecord};

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
        let decompressed = zstd::stream::decode_all(COMPRESSED_FIXTURE_BYTES)
            .expect("checked-in compressed fixture should decompress");

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
}
