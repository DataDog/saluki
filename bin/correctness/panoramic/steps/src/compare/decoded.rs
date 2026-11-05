//! Decoded traffic streams and sidecars for comparisons.

use std::{fs::File, io::BufWriter};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Serialize;
use serde_json::Value;

use super::intake::CollectedData;

const DECODED_FORMAT: &str = "panoramic-decoded-capture";

/// Bump whenever a decoded record or manifest field changes meaning.
const DECODED_FORMAT_VERSION: u32 = 1;

const ZSTD_LEVEL: i32 = 3;

/// How decoded records are ordered, as reported to a reader.
const DECODED_ORDERING: &str = "grouped_by_kind_then_intake_dump_order";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Which test target a decoded capture came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    /// The reference against which the subject is measured.
    Baseline,

    /// The subject being measured.
    Comparison,
}

impl Side {
    /// The reference and subject sides, in manifest order.
    pub const ALL: [Self; 2] = [Self::Baseline, Self::Comparison];

    /// Returns the side's name, used as its file-name prefix and reported in its manifest.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Comparison => "comparison",
        }
    }

    /// Name of the compressed record stream.
    pub fn records_file_name(&self) -> String {
        format!("{}-decoded.jsonl.zst", self.name())
    }

    /// Name of the decoded capture sidecar.
    pub fn manifest_file_name(&self) -> String {
        format!("{}-decoded-manifest.json", self.name())
    }

    /// Name of the marker explaining a missing capture.
    pub fn unavailable_file_name(&self) -> String {
        format!("{}-decoded-unavailable.txt", self.name())
    }
}

/// A single decoded telemetry item.
#[derive(Serialize)]
struct DecodedRecord<'a, T> {
    kind: &'static str,

    /// Position within this kind's collection, starting at zero. Not a global receive order.
    index: usize,

    value: &'a T,
}

/// What a decoded capture holds, written alongside it and later merged into `manifest.json`.
#[derive(Serialize)]
struct DecodedManifest {
    format: &'static str,
    version: u32,
    side: &'static str,
    role: String,
    records_file: String,
    compression: &'static str,
    ordering: &'static str,
    source: String,
    records: usize,

    /// Number of records per kind, in the order the kinds were written.
    records_by_kind: Vec<KindCount>,

    /// Uncompressed size of the record stream, in bytes.
    uncompressed_bytes: u64,

    digest: Digest,
}

/// How many records of one kind a capture holds.
#[derive(Serialize)]
struct KindCount {
    kind: &'static str,
    count: usize,
}

/// A named, non-cryptographic digest of the record stream. It identifies a stream cheaply, so two captures can be
/// compared without shipping both.
#[derive(Serialize)]
struct Digest {
    algorithm: &'static str,
    value: String,
}

/// Streams a side's decoded telemetry to a file and returns its manifest sidecar.
///
/// # Errors
///
/// Returns serialization, compression or file-write failures.
pub(crate) fn write_decoded_records(
    file: File, side: Side, role: &str, source: &str, data: &CollectedData,
) -> Result<Value, GenericError> {
    let mut writer = RecordWriter::new(
        zstd::stream::write::Encoder::new(BufWriter::new(file), ZSTD_LEVEL)
            .error_context("Failed to initialize zstd encoder for decoded capture.")?,
    );

    let counts = vec![
        writer.write_kind("event", data.events())?,
        writer.write_kind("metric", data.metrics())?,
        writer.write_kind("service_check", data.service_checks())?,
        writer.write_kind("span", data.spans())?,
        writer.write_kind("trace_stats", std::slice::from_ref(data.trace_stats()))?,
        writer.write_kind("dogstatsd_forwarded_packet", data.dogstatsd_forwarded_packets())?,
    ];

    let (records, uncompressed_bytes, digest, encoder) = writer.finish();
    encoder
        .finish()
        .error_context("Failed to finalize zstd stream for decoded capture.")?
        .into_inner()
        .map_err(|e| generic_error!("Failed to flush decoded capture: {}", e))?
        .sync_all()
        .error_context("Failed to sync decoded capture.")?;

    serde_json::to_value(DecodedManifest {
        format: DECODED_FORMAT,
        version: DECODED_FORMAT_VERSION,
        side: side.name(),
        role: role.into(),
        records_file: side.records_file_name(),
        compression: "zstd",
        ordering: DECODED_ORDERING,
        source: source.into(),
        records,
        records_by_kind: counts,
        uncompressed_bytes,
        digest: Digest {
            algorithm: "fnv1a64",
            value: format!("{:016x}", digest),
        },
    })
    .error_context("Failed to serialize decoded capture manifest.")
}

/// Serializes records into a writer, one JSON object per line, tallying what it wrote.
struct RecordWriter<W> {
    writer: W,
    scratch: Vec<u8>,
    records: usize,
    bytes: u64,
    digest: u64,
}

impl<W: std::io::Write> RecordWriter<W> {
    fn new(writer: W) -> Self {
        Self {
            writer,
            scratch: Vec::new(),
            records: 0,
            bytes: 0,
            digest: FNV_OFFSET_BASIS,
        }
    }

    /// Writes every item in `items` as a record of `kind`.
    ///
    /// Each record is serialized into a reused buffer and streamed out, so a large collection never becomes a
    /// large JSON string.
    fn write_kind<T: Serialize>(&mut self, kind: &'static str, items: &[T]) -> Result<KindCount, GenericError> {
        for (index, value) in items.iter().enumerate() {
            self.scratch.clear();
            serde_json::to_writer(&mut self.scratch, &DecodedRecord { kind, index, value })
                .with_error_context(|| format!("Failed to serialize decoded {}.", kind))?;
            self.scratch.push(b'\n');

            self.writer
                .write_all(&self.scratch)
                .with_error_context(|| format!("Failed to write decoded {}.", kind))?;

            self.records += 1;
            self.bytes += self.scratch.len() as u64;
            // The newline separator is part of what the digest covers, so record boundaries are too.
            self.digest = fnv1a64_update(self.digest, &self.scratch);
        }

        Ok(KindCount {
            kind,
            count: items.len(),
        })
    }

    /// Returns the tallies and the underlying writer, which the caller still has to finalize.
    fn finish(self) -> (usize, u64, u64, W) {
        (self.records, self.bytes, self.digest, self.writer)
    }
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
    #[test]
    fn decoded_records_are_grouped_by_kind_and_indexed_within_it() {
        let mut buffer = Vec::new();
        let mut writer = RecordWriter::new(&mut buffer);

        let events = writer
            .write_kind("event", &["first".to_string(), "second".to_string()])
            .expect("events should be written");
        let packets = writer
            .write_kind("dogstatsd_forwarded_packet", &["packet".to_string()])
            .expect("packets should be written");
        let (records, bytes, digest, _) = writer.finish();

        assert_eq!(events.count, 2);
        assert_eq!(packets.count, 1);
        assert_eq!(records, 3);
        assert_eq!(bytes, buffer.len() as u64);
        assert_ne!(digest, FNV_OFFSET_BASIS);

        let text = String::from_utf8(buffer).expect("records should be valid UTF-8");
        let parsed = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("record should be valid JSON"))
            .collect::<Vec<_>>();

        assert_eq!(parsed[0]["kind"], "event");
        assert_eq!(parsed[0]["index"], 0);
        assert_eq!(parsed[0]["value"], "first");
        assert_eq!(parsed[1]["kind"], "event");
        assert_eq!(parsed[1]["index"], 1);
        assert_eq!(parsed[2]["kind"], "dogstatsd_forwarded_packet");
        // Indexes restart per kind: they are positions within a kind, not a global receive order.
        assert_eq!(parsed[2]["index"], 0);
    }

    #[test]
    fn each_side_names_its_capture() {
        assert_eq!(Side::Baseline.records_file_name(), "baseline-decoded.jsonl.zst");
        assert_eq!(Side::Comparison.records_file_name(), "comparison-decoded.jsonl.zst");
    }
}
