//! Traffic artifacts for a correctness test: what went in, and what each side decoded out of it.
//!
//! A failed correctness test says two sides disagreed, but not what they were fed or what they produced. These
//! artifacts answer both, under a test's `traffic/` directory:
//!
//! - `input.jsonl.zst` and `input-manifest.json`: the input payload stream, written by one Millstone run. Both
//!   arms are fed byte-identical payloads from the same seed, so capturing one arm describes both.
//! - `baseline-decoded.jsonl.zst` and `comparison-decoded.jsonl.zst`: the telemetry each side's `datadog-intake`
//!   decoded, as the analysis saw it.
//! - `manifest.json`: what each artifact is and whether it is still on disk.
//!
//! # Retention
//!
//! The captures are large, so [`finalize`] deletes them once a test passes and keeps them for any other outcome.
//! `manifest.json` survives either way, written last so it reports the state a reader will find.
//!
//! # Ordering
//!
//! Input records are in send order. Decoded records are grouped by kind and ordered within each kind as
//! `datadog-intake` dumped them; there is no global receive order across kinds.

use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::correctness::analysis::CollectedData;
use crate::reporter::TestOutcome;

const TRAFFIC_DIR_NAME: &str = "traffic";

/// Must match `millstone::capture::INPUT_RECORDS_FILE_NAME`.
const INPUT_RECORDS_FILE_NAME: &str = "input.jsonl.zst";

/// Must match `millstone::capture::INPUT_MANIFEST_FILE_NAME`.
const INPUT_MANIFEST_FILE_NAME: &str = "input-manifest.json";

/// File a runtime writes to explain why it produced no input capture.
const INPUT_UNAVAILABLE_FILE_NAME: &str = "input-unavailable.txt";

/// The merged manifest, the one artifact every completed test keeps.
const MANIFEST_FILE_NAME: &str = "manifest.json";

const MANIFEST_FORMAT: &str = "panoramic-traffic-manifest";

/// Bump whenever a merged manifest field changes meaning.
const MANIFEST_FORMAT_VERSION: u32 = 1;

const DECODED_FORMAT: &str = "panoramic-decoded-capture";

/// Bump whenever a decoded record or manifest field changes meaning.
const DECODED_FORMAT_VERSION: u32 = 1;

const ZSTD_LEVEL: i32 = 3;

/// How decoded records are ordered, as reported to a reader.
const DECODED_ORDERING: &str = "grouped_by_kind_then_intake_dump_order";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Which side of a correctness comparison a decoded capture came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Side {
    /// The core Agent alone, which the comparison side is measured against.
    Baseline,

    /// The Agent and ADP working together.
    Comparison,
}

impl Side {
    /// Returns the side's name, used as its file-name prefix and reported in its manifest.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Comparison => "comparison",
        }
    }

    /// Returns what the side runs, so a reader doesn't have to know the convention.
    const fn role(&self) -> &'static str {
        match self {
            Self::Baseline => "core Agent alone (reference side)",
            Self::Comparison => "core Agent with ADP (side under test)",
        }
    }

    fn records_file_name(&self) -> String {
        format!("{}-decoded.jsonl.zst", self.name())
    }

    fn manifest_file_name(&self) -> String {
        format!("{}-decoded-manifest.json", self.name())
    }
}

/// Returns the traffic directory for a test's log directory.
pub(crate) fn traffic_dir(log_dir: &Path) -> PathBuf {
    log_dir.join(TRAFFIC_DIR_NAME)
}

/// A single decoded telemetry item.
#[derive(Serialize)]
struct DecodedRecord<'a, T> {
    kind: &'static str,

    /// Position within this kind's collection, starting at zero. Not a global receive order.
    index: usize,

    value: &'a T,
}

/// What a decoded capture holds, written alongside it and later merged into [`MANIFEST_FILE_NAME`].
#[derive(Serialize)]
struct DecodedManifest {
    format: &'static str,
    version: u32,
    side: &'static str,
    role: &'static str,
    records_file: String,
    compression: &'static str,
    ordering: &'static str,
    source: &'static str,
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

/// The merged manifest, describing every traffic artifact a test produced.
#[derive(Debug, Serialize)]
struct TrafficManifest {
    format: &'static str,
    version: u32,

    /// Outcome the retention decision was made against.
    outcome: TestOutcome,

    /// What the retention policy decided. Each artifact's own `file.present` reports what is on disk, which can
    /// differ if a deletion failed.
    captures_retained: bool,
    retention_reason: &'static str,

    /// The input capture's own manifest with its file's state folded in, or `null` when there is no usable input
    /// capture.
    input: Option<Value>,

    /// Why there is no usable input capture, when `input` is `null`.
    input_unavailable_reason: Option<String>,

    /// One entry per decoded capture, each with its file's state folded in.
    decoded: Vec<Value>,
}

/// Writes one side's decoded telemetry into `dir`, creating it if necessary.
///
/// # Errors
///
/// If the directory can't be created, or either file can't be written, an error is returned. Callers treat this as
/// non-fatal: a missing artifact is a lost diagnostic, not a failed verdict.
pub(crate) fn write_decoded(dir: &Path, side: Side, data: &CollectedData) -> Result<(), GenericError> {
    std::fs::create_dir_all(dir)
        .with_error_context(|| format!("Failed to create traffic directory '{}'.", dir.display()))?;

    let records_path = dir.join(side.records_file_name());
    let manifest = match write_decoded_records(&records_path, side, data) {
        Ok(manifest) => manifest,
        Err(e) => {
            // A half-written file can't be read back, and leaving it would look like a complete capture.
            let _ = std::fs::remove_file(&records_path);
            return Err(e);
        }
    };

    write_json_file(&dir.join(side.manifest_file_name()), &manifest)
}

fn write_decoded_records(path: &Path, side: Side, data: &CollectedData) -> Result<DecodedManifest, GenericError> {
    let file =
        File::create(path).with_error_context(|| format!("Failed to create decoded capture '{}'.", path.display()))?;

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

    Ok(DecodedManifest {
        format: DECODED_FORMAT,
        version: DECODED_FORMAT_VERSION,
        side: side.name(),
        role: side.role(),
        records_file: side.records_file_name(),
        compression: "zstd",
        ordering: DECODED_ORDERING,
        source: "datadog-intake dump endpoints",
        records,
        records_by_kind: counts,
        uncompressed_bytes,
        digest: Digest {
            algorithm: "fnv1a64",
            value: format!("{:016x}", digest),
        },
    })
}

/// Records why a runtime produced no input capture, for [`finalize`] to fold into the manifest.
///
/// Failures are logged, never fatal.
pub(crate) fn record_input_unavailable(dir: &Path, reason: &str) {
    if let Err(e) =
        std::fs::create_dir_all(dir).and_then(|()| std::fs::write(dir.join(INPUT_UNAVAILABLE_FILE_NAME), reason))
    {
        warn!(path = %dir.display(), error = %e, "Failed to record missing input capture.");
    }
}

/// Merges the traffic sidecars into [`MANIFEST_FILE_NAME`] and applies the outcome's retention policy.
///
/// Called for every test, and a no-op for a test that produced no traffic directory. Failures are logged, never
/// fatal, so an artifact problem can't change a verdict.
pub(crate) fn finalize(log_dir: &Path, outcome: TestOutcome) {
    let dir = traffic_dir(log_dir);
    if !dir.is_dir() {
        return;
    }

    let retain = !outcome.is_passed();
    let mut sidecars = Vec::new();

    let input_sidecar = dir.join(INPUT_MANIFEST_FILE_NAME);
    let input_manifest = read_json_file(&input_sidecar);
    if input_manifest.is_some() {
        sidecars.push(input_sidecar);
    }

    let mut decoded_manifests = Vec::new();
    for side in [Side::Baseline, Side::Comparison] {
        let sidecar = dir.join(side.manifest_file_name());
        if let Some(manifest) = read_json_file(&sidecar) {
            decoded_manifests.push((side, manifest));
            sidecars.push(sidecar);
        }
    }

    // Sizes come from before the deletion and presence from after it, so the manifest reports both what the
    // captures held and whether a reader can still open them.
    let input_records = dir.join(INPUT_RECORDS_FILE_NAME);
    let input_bytes = file_size(&input_records);
    let decoded_bytes = decoded_manifests
        .iter()
        .map(|(side, _)| file_size(&dir.join(side.records_file_name())))
        .collect::<Vec<_>>();

    if !retain {
        remove_file(&input_records);
        for side in [Side::Baseline, Side::Comparison] {
            remove_file(&dir.join(side.records_file_name()));
        }
    }

    let input_unavailable_reason = input_manifest
        .is_none()
        .then(|| missing_input_reason(&dir, input_bytes.is_some()));
    let input =
        input_manifest.map(|manifest| with_file_state(manifest, INPUT_RECORDS_FILE_NAME, input_bytes, &input_records));
    let decoded = decoded_manifests
        .into_iter()
        .zip(decoded_bytes)
        .map(|((side, manifest), bytes)| {
            let records_file = side.records_file_name();
            let path = dir.join(&records_file);
            with_file_state(manifest, &records_file, bytes, &path)
        })
        .collect();

    let manifest = TrafficManifest {
        format: MANIFEST_FORMAT,
        version: MANIFEST_FORMAT_VERSION,
        outcome,
        captures_retained: retain,
        retention_reason: if retain {
            "kept: the test did not pass"
        } else {
            "deleted: the test passed, so only this manifest remains"
        },
        input,
        input_unavailable_reason,
        decoded,
    };

    if let Err(e) = write_json_file(&dir.join(MANIFEST_FILE_NAME), &manifest) {
        warn!(path = %dir.display(), error = %e, "Failed to write traffic manifest.");
        // The sidecars are the only remaining record of the captures, so they stay.
        return;
    }

    for sidecar in sidecars {
        remove_file(&sidecar);
    }
    remove_file(&dir.join(INPUT_UNAVAILABLE_FILE_NAME));
}

/// Explains an unusable input capture: a runtime's own note if it left one, otherwise what the directory shows.
fn missing_input_reason(dir: &Path, has_orphaned_records: bool) -> String {
    match std::fs::read_to_string(dir.join(INPUT_UNAVAILABLE_FILE_NAME)) {
        Ok(reason) => reason.trim().to_string(),
        Err(_) if has_orphaned_records => format!(
            "'{}' has no readable '{}', so the capture cannot be interpreted.",
            INPUT_RECORDS_FILE_NAME, INPUT_MANIFEST_FILE_NAME
        ),
        Err(_) => "No input capture was written for this test.".to_string(),
    }
}

/// Folds a capture file's state into its producer's manifest, under a `file` key.
fn with_file_state(mut manifest: Value, name: &str, bytes: Option<u64>, path: &Path) -> Value {
    let file = serde_json::json!({
        "name": name,
        "compressed_bytes": bytes,
        "present": path.exists(),
    });

    match manifest.as_object_mut() {
        Some(object) => {
            object.insert("file".to_string(), file);
            manifest
        }
        // A manifest that isn't an object came from something other than a producer; keep both rather than
        // discarding either.
        None => serde_json::json!({ "manifest": manifest, "file": file }),
    }
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|meta| meta.len()).ok()
}

fn read_json_file(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to parse traffic manifest sidecar.");
            None
        }
    }
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), GenericError> {
    let json = serde_json::to_string_pretty(value)
        .with_error_context(|| format!("Failed to serialize '{}'.", path.display()))?;
    std::fs::write(path, format!("{}\n", json)).with_error_context(|| format!("Failed to write '{}'.", path.display()))
}

fn remove_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %path.display(), error = %e, "Failed to delete traffic capture."),
    }
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

    /// Writes the sidecars a finished capture would have left behind, plus stand-in record files.
    fn seed_traffic_dir(log_dir: &Path, with_input: bool) -> PathBuf {
        let dir = traffic_dir(log_dir);
        std::fs::create_dir_all(&dir).expect("traffic dir should be created");

        if with_input {
            std::fs::write(dir.join(INPUT_RECORDS_FILE_NAME), b"compressed-input").expect("input should be written");
            std::fs::write(
                dir.join(INPUT_MANIFEST_FILE_NAME),
                br#"{"format":"millstone-input-capture","records":5,"ordering":"logical_send_order"}"#,
            )
            .expect("input manifest should be written");
        }

        for side in [Side::Baseline, Side::Comparison] {
            std::fs::write(dir.join(side.records_file_name()), b"compressed-decoded")
                .expect("decoded records should be written");
            std::fs::write(
                dir.join(side.manifest_file_name()),
                format!(r#"{{"format":"panoramic-decoded-capture","side":"{}"}}"#, side.name()),
            )
            .expect("decoded manifest should be written");
        }

        dir
    }

    fn read_manifest(dir: &Path) -> Value {
        let raw = std::fs::read_to_string(dir.join(MANIFEST_FILE_NAME)).expect("manifest should be readable");
        serde_json::from_str(&raw).expect("manifest should be valid JSON")
    }

    #[test]
    fn a_failed_test_keeps_its_captures_and_merges_the_sidecars() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = seed_traffic_dir(log_dir.path(), true);

        finalize(log_dir.path(), TestOutcome::Failed);

        let manifest = read_manifest(&dir);
        assert_eq!(manifest["format"], MANIFEST_FORMAT);
        assert_eq!(manifest["outcome"], "failed");
        assert_eq!(manifest["captures_retained"], true);
        assert_eq!(manifest["input"]["records"], 5);
        assert_eq!(manifest["input"]["ordering"], "logical_send_order");
        assert_eq!(manifest["input"]["file"]["name"], INPUT_RECORDS_FILE_NAME);
        assert_eq!(manifest["input"]["file"]["compressed_bytes"], 16);
        assert_eq!(manifest["input"]["file"]["present"], true);
        assert!(manifest["input_unavailable_reason"].is_null());
        assert_eq!(manifest["decoded"][0]["side"], "baseline");
        assert_eq!(manifest["decoded"][1]["side"], "comparison");
        assert_eq!(manifest["decoded"][0]["file"]["present"], true);

        assert!(dir.join(INPUT_RECORDS_FILE_NAME).exists());
        assert!(dir.join(Side::Baseline.records_file_name()).exists());
        assert!(dir.join(Side::Comparison.records_file_name()).exists());
        // The sidecars are folded into the manifest, so they no longer stand on their own.
        assert!(!dir.join(INPUT_MANIFEST_FILE_NAME).exists());
        assert!(!dir.join(Side::Baseline.manifest_file_name()).exists());
    }

    #[test]
    fn a_passing_test_keeps_only_the_manifest() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = seed_traffic_dir(log_dir.path(), true);

        finalize(log_dir.path(), TestOutcome::Passed);

        let manifest = read_manifest(&dir);
        assert_eq!(manifest["outcome"], "passed");
        assert_eq!(manifest["captures_retained"], false);
        // The manifest still describes the sizes of the captures it outlived, and reports them as gone.
        assert_eq!(manifest["input"]["file"]["compressed_bytes"], 16);
        assert_eq!(manifest["input"]["file"]["present"], false);
        assert_eq!(manifest["decoded"][0]["file"]["present"], false);

        let remaining = std::fs::read_dir(&dir)
            .expect("traffic dir should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(remaining, vec![MANIFEST_FILE_NAME.to_string()]);
    }

    #[test]
    fn a_timed_out_test_keeps_its_captures() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = seed_traffic_dir(log_dir.path(), true);

        finalize(log_dir.path(), TestOutcome::TimedOut);

        assert_eq!(read_manifest(&dir)["captures_retained"], true);
        assert!(dir.join(INPUT_RECORDS_FILE_NAME).exists());
    }

    #[test]
    fn an_errored_test_keeps_its_captures() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = seed_traffic_dir(log_dir.path(), true);

        finalize(log_dir.path(), TestOutcome::Errored);

        assert_eq!(read_manifest(&dir)["captures_retained"], true);
        assert!(dir.join(INPUT_RECORDS_FILE_NAME).exists());
    }

    #[test]
    fn a_missing_input_capture_is_reported_with_its_reason() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = seed_traffic_dir(log_dir.path(), false);

        record_input_unavailable(&dir, "the kind runtime cannot extract it");
        finalize(log_dir.path(), TestOutcome::Failed);

        let manifest = read_manifest(&dir);
        assert!(manifest["input"].is_null());
        assert_eq!(
            manifest["input_unavailable_reason"],
            "the kind runtime cannot extract it"
        );
        // The decoded captures are unaffected by a missing input capture.
        assert_eq!(manifest["decoded"].as_array().map(Vec::len), Some(2));
        assert!(!dir.join(INPUT_UNAVAILABLE_FILE_NAME).exists());
    }

    #[test]
    fn an_input_capture_with_no_readable_manifest_is_reported_as_unusable() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = seed_traffic_dir(log_dir.path(), false);
        std::fs::write(dir.join(INPUT_RECORDS_FILE_NAME), b"partial").expect("records should be written");

        finalize(log_dir.path(), TestOutcome::Failed);

        let manifest = read_manifest(&dir);
        assert!(manifest["input"].is_null());
        let reason = manifest["input_unavailable_reason"]
            .as_str()
            .expect("reason should be a string");
        assert!(reason.contains("cannot be interpreted"), "{}", reason);
    }

    #[test]
    fn a_test_with_no_traffic_directory_is_left_alone() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");

        finalize(log_dir.path(), TestOutcome::Failed);

        assert!(!traffic_dir(log_dir.path()).exists());
    }

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
    fn each_side_names_itself_and_its_role() {
        assert_eq!(Side::Baseline.records_file_name(), "baseline-decoded.jsonl.zst");
        assert_eq!(Side::Comparison.records_file_name(), "comparison-decoded.jsonl.zst");
        assert_ne!(Side::Baseline.role(), Side::Comparison.role());
        assert!(Side::Baseline.role().contains("reference"));
        assert!(Side::Comparison.role().contains("ADP"));
    }
}
