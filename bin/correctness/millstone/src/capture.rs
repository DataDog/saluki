//! Capture of the payload stream a run sent to its target.
//!
//! Diagnosing a failed correctness test starts with knowing what the target was fed, so the capture records the
//! payloads instead of leaving them to be re-derived from the seed.
//!
//! Two files are written into the capture directory:
//!
//! - `input.jsonl.zst`: one JSON object per payload, zstd-compressed, in the order the payloads were handed to the
//!   transport. That is not packet order: a datagram transport may reorder or drop what it was given, and a
//!   partially written payload still appears at its full length.
//! - `input-manifest.json`: the facts a reader needs to interpret the records.

use std::{
    borrow::Cow,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use base64::Engine as _;
use bytes::Bytes;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Serialize;
use tracing::info;

const INPUT_RECORDS_FILE_NAME: &str = "input.jsonl.zst";
const INPUT_MANIFEST_FILE_NAME: &str = "input-manifest.json";

/// Format identifier written into the manifest, so a reader can tell what it is holding.
const INPUT_FORMAT: &str = "millstone-input-capture";

/// Bump whenever a record or manifest field changes meaning.
const INPUT_FORMAT_VERSION: u32 = 1;

const ZSTD_LEVEL: i32 = 3;

/// How records are ordered, as reported to a reader.
const ORDERING: &str = "logical_send_order";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A single captured payload.
#[derive(Serialize)]
struct InputRecord<'a> {
    /// Position in the send order, starting at zero.
    seq: usize,

    /// Index into the generated corpus, which repeats when the volume exceeds the corpus size.
    corpus_index: usize,

    /// Length of the payload as handed to the transport. Any framing the corpus generated, such as the length
    /// prefix DogStatsD uses over a stream socket, is part of it.
    byte_len: usize,

    /// How `payload` is encoded: `utf8` or `base64`.
    encoding: &'static str,

    payload: Cow<'a, str>,
}

impl<'a> InputRecord<'a> {
    /// Builds a record for `payload`, preferring text and falling back to base64 for anything that isn't UTF-8.
    fn new(seq: usize, corpus_index: usize, payload: &'a [u8]) -> Self {
        let (encoding, encoded) = match simdutf8::basic::from_utf8(payload) {
            Ok(text) => ("utf8", Cow::Borrowed(text)),
            Err(_) => (
                "base64",
                Cow::Owned(base64::engine::general_purpose::STANDARD.encode(payload)),
            ),
        };

        Self {
            seq,
            corpus_index,
            byte_len: payload.len(),
            encoding,
            payload: encoded,
        }
    }
}

/// Facts a reader needs in order to interpret the records.
#[derive(Serialize)]
struct InputManifest {
    format: &'static str,
    version: u32,
    records_file: &'static str,
    compression: &'static str,
    ordering: &'static str,

    /// Hex-encoded RNG seed the corpus was generated from.
    seed: String,
    payload_kind: &'static str,
    target_kind: &'static str,
    send_delay_us: u64,

    /// Number of distinct payloads the corpus generated. Fewer than `records` means the corpus was cycled; more
    /// means its tail was never sent.
    corpus_payloads: usize,

    /// How many payloads the run was configured to send.
    volume: usize,

    /// Number of records in the capture.
    records: usize,

    /// Total payload bytes across every record.
    logical_bytes: u64,

    /// Bytes the transport reported as written, which differs from `logical_bytes` only when a send was partial.
    wire_bytes: u64,

    /// Number of sends the transport only partially wrote. When nonzero, the wire stream is not byte-identical to
    /// the payloads captured here.
    partial_sends: usize,

    digest: Digest,
}

/// A named, non-cryptographic digest of the record stream. It identifies a stream cheaply, so two runs can be
/// compared without shipping both captures.
#[derive(Serialize)]
struct Digest {
    algorithm: &'static str,
    value: String,
}

/// Everything about a finished run that isn't derivable from the payloads themselves.
pub(crate) struct RunFacts {
    /// Hex-encoded RNG seed the corpus was generated from.
    pub seed: String,
    pub payload_kind: &'static str,
    pub target_kind: &'static str,
    pub send_delay_us: u64,

    /// How many payloads the run was configured to send.
    pub volume: usize,
    pub payloads_sent: usize,

    /// Bytes the transport reported as written.
    pub wire_bytes: u64,
    pub partial_sends: usize,
}

/// Writes the capture for a finished run into `dir`, creating it if necessary.
///
/// `payloads` is the generated corpus and `facts.payloads_sent` is how many payloads the run took from it, cycling
/// when the volume exceeded the corpus size. Only that many records are written.
///
/// # Errors
///
/// If the directory can't be created, or either file can't be written, an error is returned. Callers treat this as
/// non-fatal: a missing capture is a lost diagnostic, not a failed run.
pub(crate) fn write_input_capture(dir: &Path, payloads: &[Bytes], facts: RunFacts) -> Result<(), GenericError> {
    std::fs::create_dir_all(dir)
        .with_error_context(|| format!("Failed to create traffic capture directory '{}'.", dir.display()))?;

    let records_path = dir.join(INPUT_RECORDS_FILE_NAME);
    let summary = match write_records_file(&records_path, payloads, facts.payloads_sent) {
        Ok(summary) => summary,
        Err(e) => {
            // A half-written file can't be read back, and leaving it would look like a complete capture.
            let _ = std::fs::remove_file(&records_path);
            return Err(e);
        }
    };

    let manifest = InputManifest {
        format: INPUT_FORMAT,
        version: INPUT_FORMAT_VERSION,
        records_file: INPUT_RECORDS_FILE_NAME,
        compression: "zstd",
        ordering: ORDERING,
        seed: facts.seed,
        payload_kind: facts.payload_kind,
        target_kind: facts.target_kind,
        send_delay_us: facts.send_delay_us,
        corpus_payloads: payloads.len(),
        volume: facts.volume,
        records: summary.records,
        logical_bytes: summary.logical_bytes,
        wire_bytes: facts.wire_bytes,
        partial_sends: facts.partial_sends,
        digest: Digest {
            algorithm: "fnv1a64",
            value: format!("{:016x}", summary.digest),
        },
    };

    let manifest_path = dir.join(INPUT_MANIFEST_FILE_NAME);
    let manifest_json =
        serde_json::to_string_pretty(&manifest).error_context("Failed to serialize traffic capture manifest.")?;
    std::fs::write(&manifest_path, format!("{}\n", manifest_json)).with_error_context(|| {
        format!(
            "Failed to write traffic capture manifest '{}'.",
            manifest_path.display()
        )
    })?;

    info!(
        records = summary.records,
        logical_bytes = summary.logical_bytes,
        path = %records_path.display(),
        "Wrote input traffic capture."
    );

    Ok(())
}

/// Totals describing what [`write_records`] wrote.
struct RecordSummary {
    records: usize,
    logical_bytes: u64,
    digest: u64,
}

fn write_records_file(path: &Path, payloads: &[Bytes], payloads_sent: usize) -> Result<RecordSummary, GenericError> {
    let file = File::create(path)
        .with_error_context(|| format!("Failed to create traffic capture file '{}'.", path.display()))?;

    let mut encoder = zstd::stream::write::Encoder::new(BufWriter::new(file), ZSTD_LEVEL)
        .error_context("Failed to initialize zstd encoder for traffic capture.")?;
    let summary = write_records(&mut encoder, payloads, payloads_sent)?;
    encoder
        .finish()
        .error_context("Failed to finalize zstd stream for traffic capture.")?
        .into_inner()
        .map_err(|e| saluki_error::generic_error!("Failed to flush traffic capture file: {}", e))?
        .sync_all()
        .error_context("Failed to sync traffic capture file.")?;

    Ok(summary)
}

/// Writes `payloads_sent` records to `writer` in send order, cycling the corpus as the send loop did.
fn write_records<W: Write>(
    writer: &mut W, payloads: &[Bytes], payloads_sent: usize,
) -> Result<RecordSummary, GenericError> {
    let mut summary = RecordSummary {
        records: 0,
        logical_bytes: 0,
        digest: FNV_OFFSET_BASIS,
    };

    if payloads.is_empty() {
        return Ok(summary);
    }

    for seq in 0..payloads_sent {
        let corpus_index = seq % payloads.len();
        let payload = &payloads[corpus_index];

        serde_json::to_writer(&mut *writer, &InputRecord::new(seq, corpus_index, payload))
            .error_context("Failed to serialize captured payload.")?;
        writer
            .write_all(b"\n")
            .error_context("Failed to write captured payload separator.")?;

        summary.records += 1;
        summary.logical_bytes += payload.len() as u64;
        // Each payload's length goes into the digest ahead of its bytes, so a stream can't collide with a
        // differently split stream of the same bytes.
        summary.digest = fnv1a64_update(summary.digest, &(payload.len() as u64).to_le_bytes());
        summary.digest = fnv1a64_update(summary.digest, payload);
    }

    Ok(summary)
}

fn fnv1a64_update(mut digest: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(FNV_PRIME);
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(payloads: &[&[u8]]) -> Vec<Bytes> {
        payloads.iter().map(|p| Bytes::copy_from_slice(p)).collect()
    }

    fn records_from(payloads: &[Bytes], payloads_sent: usize) -> (Vec<serde_json::Value>, RecordSummary) {
        let mut buffer = Vec::new();
        let summary = write_records(&mut buffer, payloads, payloads_sent).expect("records should be written");
        let text = String::from_utf8(buffer).expect("records should be valid UTF-8");
        let records = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
            .collect();

        (records, summary)
    }

    #[test]
    fn records_follow_send_order() {
        let payloads = corpus(&[b"first", b"second"]);
        let (records, summary) = records_from(&payloads, 2);

        assert_eq!(summary.records, 2);
        assert_eq!(summary.logical_bytes, 11);
        assert_eq!(records[0]["seq"], 0);
        assert_eq!(records[0]["corpus_index"], 0);
        assert_eq!(records[0]["payload"], "first");
        assert_eq!(records[1]["seq"], 1);
        assert_eq!(records[1]["corpus_index"], 1);
        assert_eq!(records[1]["payload"], "second");
    }

    #[test]
    fn volume_larger_than_corpus_cycles_the_corpus() {
        let payloads = corpus(&[b"a", b"bb"]);
        let (records, summary) = records_from(&payloads, 5);

        assert_eq!(summary.records, 5);
        // Three sends of "a" and two of "bb".
        assert_eq!(summary.logical_bytes, 7);
        let corpus_indexes = records
            .iter()
            .map(|record| record["corpus_index"].as_u64().expect("index should be a number"))
            .collect::<Vec<_>>();
        assert_eq!(corpus_indexes, vec![0, 1, 0, 1, 0]);
        let seqs = records
            .iter()
            .map(|record| record["seq"].as_u64().expect("seq should be a number"))
            .collect::<Vec<_>>();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn volume_smaller_than_corpus_stops_at_the_volume() {
        let payloads = corpus(&[b"a", b"bb", b"ccc"]);
        let (records, summary) = records_from(&payloads, 2);

        assert_eq!(summary.records, 2);
        assert_eq!(summary.logical_bytes, 3);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["corpus_index"], 1);
    }

    #[test]
    fn non_utf8_payloads_are_captured_as_base64() {
        let payloads = corpus(&[&[0x00, 0xff, 0x10]]);
        let (records, summary) = records_from(&payloads, 1);

        assert_eq!(summary.logical_bytes, 3);
        assert_eq!(records[0]["encoding"], "base64");
        assert_eq!(records[0]["byte_len"], 3);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(records[0]["payload"].as_str().expect("payload should be a string"))
            .expect("payload should decode");
        assert_eq!(decoded, vec![0x00, 0xff, 0x10]);
    }

    #[test]
    fn digest_covers_record_boundaries() {
        let (_, split) = records_from(&corpus(&[b"a", b"b"]), 2);
        let (_, joined) = records_from(&corpus(&[b"ab"]), 1);
        let (_, different_content) = records_from(&corpus(&[b"a", b"c"]), 2);
        let (_, same) = records_from(&corpus(&[b"a", b"b"]), 2);

        assert_ne!(split.digest, joined.digest);
        assert_ne!(split.digest, different_content.digest);
        assert_eq!(split.digest, same.digest);
    }

    #[test]
    fn an_empty_corpus_writes_no_records() {
        let (records, summary) = records_from(&[], 3);

        assert!(records.is_empty());
        assert_eq!(summary.records, 0);
        assert_eq!(summary.logical_bytes, 0);
    }

    #[test]
    fn the_manifest_describes_the_records_it_was_written_with() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let payloads = corpus(&[b"one", b"two"]);

        write_input_capture(
            dir.path(),
            &payloads,
            RunFacts {
                seed: "00".repeat(32),
                payload_kind: "DogStatsD",
                target_kind: "unixgram",
                send_delay_us: 500,
                volume: 5,
                payloads_sent: 5,
                wire_bytes: 14,
                partial_sends: 1,
            },
        )
        .expect("capture should be written");

        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(INPUT_MANIFEST_FILE_NAME)).expect("manifest should be readable"),
        )
        .expect("manifest should be valid JSON");

        assert_eq!(manifest["format"], INPUT_FORMAT);
        assert_eq!(manifest["version"], INPUT_FORMAT_VERSION);
        assert_eq!(manifest["ordering"], ORDERING);
        assert_eq!(manifest["records_file"], INPUT_RECORDS_FILE_NAME);
        assert_eq!(manifest["compression"], "zstd");
        assert_eq!(manifest["corpus_payloads"], 2);
        assert_eq!(manifest["volume"], 5);
        assert_eq!(manifest["records"], 5);
        assert_eq!(manifest["logical_bytes"], 15);
        assert_eq!(manifest["wire_bytes"], 14);
        assert_eq!(manifest["partial_sends"], 1);
        assert_eq!(manifest["digest"]["algorithm"], "fnv1a64");
        assert_eq!(manifest["target_kind"], "unixgram");
        assert_eq!(manifest["send_delay_us"], 500);

        // The record file is a real zstd stream holding one line per payload.
        let compressed = std::fs::read(dir.path().join(INPUT_RECORDS_FILE_NAME)).expect("records should be readable");
        let decompressed = zstd::stream::decode_all(&compressed[..]).expect("records should decompress");
        let text = String::from_utf8(decompressed).expect("records should be valid UTF-8");
        assert_eq!(text.lines().count(), 5);
    }

    #[test]
    fn a_failed_record_write_leaves_no_capture_behind() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        // A directory where the records file belongs makes `File::create` fail.
        std::fs::create_dir(dir.path().join(INPUT_RECORDS_FILE_NAME)).expect("blocker should be created");

        let error = write_input_capture(
            dir.path(),
            &corpus(&[b"one"]),
            RunFacts {
                seed: "00".to_string(),
                payload_kind: "DogStatsD",
                target_kind: "unixgram",
                send_delay_us: 0,
                volume: 1,
                payloads_sent: 1,
                wire_bytes: 3,
                partial_sends: 0,
            },
        );

        assert!(error.is_err());
        // No manifest, so a reader treats the capture as absent rather than partial.
        assert!(!dir.path().join(INPUT_MANIFEST_FILE_NAME).exists());
    }
}
