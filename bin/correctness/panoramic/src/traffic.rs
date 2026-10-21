//! Traffic artifacts for a test: what went in, and what the intake decoded out of it.
//!
//! A failed test says what went wrong, but not what the system was fed or what it produced. These artifacts answer
//! both, under a test's `traffic/` directory:
//!
//! - `input.jsonl.zst` and `input-manifest.json`: the input payload stream of a correctness test, written by one
//!   Millstone run. Both arms are fed byte-identical payloads from the same seed, so capturing one arm describes both.
//! - `<side>-decoded.jsonl.zst`: the telemetry a side's `datadog-intake` decoded. A correctness test writes one per
//!   arm, as the analysis saw it. An integration test writes one for its target, collected after its assertions
//!   finish, so it is not necessarily the snapshot an assertion decided on.
//! - `manifest.json`: what each artifact is, whether it is still on disk, and why any capture is missing.
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
    time::Duration,
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::intake::{CollectedData, IntakeClient};
use crate::reporter::TestOutcome;

const TRAFFIC_DIR_NAME: &str = "traffic";

/// Must match `millstone::capture::INPUT_RECORDS_FILE_NAME`.
const INPUT_RECORDS_FILE_NAME: &str = "input.jsonl.zst";

/// Must match `millstone::capture::INPUT_MANIFEST_FILE_NAME`.
const INPUT_MANIFEST_FILE_NAME: &str = "input-manifest.json";

/// File a runtime writes to explain why it produced no input capture.
const INPUT_UNAVAILABLE_FILE_NAME: &str = "input-unavailable.txt";

/// Why an integration test has no input capture.
const INTEGRATION_INPUT_UNAVAILABLE_REASON: &str =
    "Integration procedures send input through actions, so no input capture is written.";

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

/// Which test target a decoded capture came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Side {
    /// The core Agent alone, which the comparison side is measured against.
    Baseline,

    /// The Agent and ADP working together.
    Comparison,

    /// The target of an integration test.
    Integration,
}

impl Side {
    const ALL: [Self; 3] = [Self::Baseline, Self::Comparison, Self::Integration];

    /// Returns the side's name, used as its file-name prefix and reported in its manifest.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Comparison => "comparison",
            Self::Integration => "target",
        }
    }

    /// Returns what the side runs, so a reader doesn't have to know the convention.
    const fn role(&self) -> &'static str {
        match self {
            Self::Baseline => "core Agent alone (reference side)",
            Self::Comparison => "core Agent with ADP (side under test)",
            Self::Integration => "integration test target (system under test)",
        }
    }

    /// Returns where and when the side's capture was collected.
    const fn source(&self) -> &'static str {
        match self {
            Self::Baseline | Self::Comparison => "datadog-intake dump endpoints",
            Self::Integration => "datadog-intake dump endpoints, collected after assertions",
        }
    }

    fn records_file_name(&self) -> String {
        format!("{}-decoded.jsonl.zst", self.name())
    }

    fn manifest_file_name(&self) -> String {
        format!("{}-decoded-manifest.json", self.name())
    }

    fn unavailable_file_name(&self) -> String {
        format!("{}-decoded-unavailable.txt", self.name())
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

    /// One entry per side whose decoded capture is missing or unusable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    decoded_unavailable: Vec<DecodedUnavailable>,
}

/// Why a side has no usable decoded capture.
#[derive(Debug, Serialize)]
struct DecodedUnavailable {
    side: &'static str,
    reason: String,
}

/// Writes one side's decoded telemetry into `dir`, creating it if necessary.
///
/// A failure is logged and recorded as the side's unavailable reason, never fatal: a missing artifact is a lost
/// diagnostic, not a failed verdict.
pub(crate) fn write_decoded(dir: &Path, side: Side, data: &CollectedData) {
    if let Err(e) = try_write_decoded(dir, side, data) {
        warn!(side = side.name(), error = %format!("{:#}", e), "Failed to write decoded telemetry capture.");
        record_decoded_unavailable(dir, side, &format!("Failed to write the decoded capture: {:#}", e));
    }
}

fn try_write_decoded(dir: &Path, side: Side, data: &CollectedData) -> Result<(), GenericError> {
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
        source: side.source(),
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
    write_marker(dir, INPUT_UNAVAILABLE_FILE_NAME, reason);
}

/// Records why `side` has no decoded capture, for [`finalize`] to fold into the manifest.
///
/// Failures are logged, never fatal.
fn record_decoded_unavailable(dir: &Path, side: Side, reason: &str) {
    write_marker(dir, &side.unavailable_file_name(), reason);
}

fn write_marker(dir: &Path, file_name: &str, reason: &str) {
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(dir.join(file_name), reason)) {
        warn!(path = %dir.display(), file = file_name, error = %e, "Failed to record missing traffic capture.");
    }
}

/// Captures an integration test's intake output into `dir`, after its assertions finish.
///
/// Records that the test has no input capture, then collects every telemetry kind from the intake within `budget` and
/// writes it as the [`Side::Integration`] decoded capture. The capture is skipped if `cancel` is already cancelled, and
/// abandoned if it is cancelled or `budget` runs out first. Whatever stops it is recorded as the capture's unavailable
/// reason. Failures are logged, never fatal: the capture is a diagnostic and must not change a verdict.
pub(crate) async fn capture_intake(dir: &Path, client: &IntakeClient, cancel: &CancellationToken, budget: Duration) {
    record_input_unavailable(dir, INTEGRATION_INPUT_UNAVAILABLE_REASON);

    let collected = if cancel.is_cancelled() {
        Err(
            "The test was cancelled before the post-assertion intake capture, so the intake was not queried."
                .to_string(),
        )
    } else {
        tokio::select! {
            result = tokio::time::timeout(budget, CollectedData::collect(client)) => match result {
                Ok(Ok(data)) => Ok(data),
                Ok(Err(e)) => Err(format!("Failed to collect the post-assertion intake capture: {:#}", e)),
                Err(_) => Err(format!(
                    "The post-assertion intake capture did not finish within {:?}, so it was abandoned.",
                    budget
                )),
            },
            _ = cancel.cancelled() => Err(
                "The test was cancelled during the post-assertion intake capture, so it was abandoned.".to_string()
            ),
        }
    };

    match collected {
        Ok(data) => write_decoded(dir, Side::Integration, &data),
        Err(reason) => {
            warn!(path = %dir.display(), reason = %reason, "No post-assertion intake capture.");
            record_decoded_unavailable(dir, Side::Integration, &reason);
        }
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
    let mut decoded_unavailable = Vec::new();
    for side in Side::ALL {
        let sidecar = dir.join(side.manifest_file_name());
        if let Some(manifest) = read_json_file(&sidecar) {
            decoded_manifests.push((side, manifest));
            sidecars.push(sidecar);
        } else if let Some(reason) = missing_decoded_reason(&dir, side) {
            decoded_unavailable.push(DecodedUnavailable {
                side: side.name(),
                reason,
            });
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
        for side in Side::ALL {
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
        decoded_unavailable,
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
    for side in Side::ALL {
        remove_file(&dir.join(side.unavailable_file_name()));
    }
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

/// Explains a side with no readable decoded manifest: its runtime's note if it left one, otherwise what the directory
/// shows. Returns `None` for a side that left nothing behind.
fn missing_decoded_reason(dir: &Path, side: Side) -> Option<String> {
    match std::fs::read_to_string(dir.join(side.unavailable_file_name())) {
        Ok(reason) => Some(reason.trim().to_string()),
        Err(_) if dir.join(side.records_file_name()).exists() => Some(format!(
            "'{}' has no readable '{}', so the capture cannot be interpreted.",
            side.records_file_name(),
            side.manifest_file_name()
        )),
        Err(_) => None,
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
        assert_eq!(manifest["version"], MANIFEST_FORMAT_VERSION);
        assert_eq!(manifest["decoded"][0]["side"], "baseline");
        assert_eq!(manifest["decoded"][1]["side"], "comparison");
        assert_eq!(manifest["decoded"][0]["file"]["present"], true);
        // Captures that were written leave no unavailable reasons behind.
        assert!(manifest.get("decoded_unavailable").is_none(), "{}", manifest);

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
        assert_eq!(Side::Integration.records_file_name(), "target-decoded.jsonl.zst");
        assert_ne!(Side::Baseline.role(), Side::Comparison.role());
        assert!(Side::Baseline.role().contains("reference"));
        assert!(Side::Comparison.role().contains("ADP"));
        assert_eq!(Side::Integration.role(), "integration test target (system under test)");
    }

    fn read_json(path: &Path) -> Value {
        let raw = std::fs::read_to_string(path).expect("file should be readable");
        serde_json::from_str(&raw).expect("file should be valid JSON")
    }

    #[test]
    fn each_side_writes_its_own_decoded_capture() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = traffic_dir(log_dir.path());

        write_decoded(&dir, Side::Baseline, &CollectedData::default());
        write_decoded(&dir, Side::Integration, &CollectedData::default());

        let baseline = read_json(&dir.join("baseline-decoded-manifest.json"));
        assert_eq!(baseline["format"], DECODED_FORMAT);
        assert_eq!(baseline["version"], DECODED_FORMAT_VERSION);
        assert_eq!(baseline["side"], "baseline");
        assert_eq!(baseline["source"], "datadog-intake dump endpoints");
        assert!(dir.join("baseline-decoded.jsonl.zst").exists());

        let target = read_json(&dir.join("target-decoded-manifest.json"));
        assert_eq!(target["format"], DECODED_FORMAT);
        assert_eq!(target["version"], DECODED_FORMAT_VERSION);
        assert_eq!(target["side"], "target");
        assert_eq!(target["role"], "integration test target (system under test)");
        assert_eq!(target["records_file"], "target-decoded.jsonl.zst");
        assert_eq!(
            target["source"],
            "datadog-intake dump endpoints, collected after assertions"
        );
        assert!(dir.join("target-decoded.jsonl.zst").exists());
    }

    #[test]
    fn an_integration_capture_is_kept_on_failure_and_deleted_on_pass() {
        for (outcome, retained) in [(TestOutcome::Failed, true), (TestOutcome::Passed, false)] {
            let log_dir = tempfile::tempdir().expect("temp dir should be created");
            let dir = traffic_dir(log_dir.path());
            write_decoded(&dir, Side::Integration, &CollectedData::default());

            finalize(log_dir.path(), outcome);

            let manifest = read_manifest(&dir);
            assert_eq!(manifest["captures_retained"], retained);
            assert_eq!(manifest["decoded"].as_array().map(Vec::len), Some(1));
            assert_eq!(manifest["decoded"][0]["side"], "target");
            assert_eq!(manifest["decoded"][0]["file"]["name"], "target-decoded.jsonl.zst");
            assert_eq!(manifest["decoded"][0]["file"]["present"], retained);
            assert!(manifest.get("decoded_unavailable").is_none(), "{}", manifest);
            assert_eq!(dir.join("target-decoded.jsonl.zst").exists(), retained);
            assert!(!dir.join("target-decoded-manifest.json").exists());
        }
    }

    #[test]
    fn a_failed_decoded_write_is_reported_as_unavailable() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = traffic_dir(log_dir.path());
        // A directory where the sidecar belongs makes the sidecar write fail after the records are written.
        std::fs::create_dir_all(dir.join(Side::Integration.manifest_file_name())).expect("dir should be created");

        write_decoded(&dir, Side::Integration, &CollectedData::default());
        assert!(dir.join(Side::Integration.unavailable_file_name()).exists());

        finalize(log_dir.path(), TestOutcome::Failed);

        let manifest = read_manifest(&dir);
        assert_eq!(manifest["decoded"].as_array().map(Vec::len), Some(0));
        assert_eq!(manifest["decoded_unavailable"][0]["side"], "target");
        let reason = manifest["decoded_unavailable"][0]["reason"]
            .as_str()
            .expect("reason should be a string");
        assert!(
            reason.starts_with("Failed to write the decoded capture: Failed to write '"),
            "{}",
            reason
        );
        // The chain continues past the outer context to the I/O cause.
        assert!(reason.contains("target-decoded-manifest.json'.: "), "{}", reason);
        assert!(dir.join("target-decoded.jsonl.zst").exists());
        assert!(!dir.join(Side::Integration.unavailable_file_name()).exists());
    }

    #[test]
    fn an_orphaned_decoded_capture_is_reported_as_uninterpretable() {
        for outcome in [TestOutcome::Failed, TestOutcome::Passed] {
            let log_dir = tempfile::tempdir().expect("temp dir should be created");
            let dir = seed_traffic_dir(log_dir.path(), true);
            std::fs::remove_file(dir.join(Side::Comparison.manifest_file_name())).expect("sidecar should be removed");

            finalize(log_dir.path(), outcome);

            let manifest = read_manifest(&dir);
            assert_eq!(manifest["decoded"].as_array().map(Vec::len), Some(1));
            assert_eq!(manifest["decoded"][0]["side"], "baseline");
            assert_eq!(
                manifest["decoded_unavailable"],
                serde_json::json!([{
                    "side": "comparison",
                    "reason": "'comparison-decoded.jsonl.zst' has no readable 'comparison-decoded-manifest.json', so \
                               the capture cannot be interpreted.",
                }])
            );
            assert_eq!(
                dir.join(Side::Comparison.records_file_name()).exists(),
                !outcome.is_passed()
            );
        }
    }

    #[test]
    fn unavailable_markers_stay_until_the_manifest_is_written() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = traffic_dir(log_dir.path());
        record_decoded_unavailable(&dir, Side::Integration, "collection failed");
        // A directory where the merged manifest belongs makes its write fail.
        std::fs::create_dir_all(dir.join(MANIFEST_FILE_NAME)).expect("dir should be created");

        finalize(log_dir.path(), TestOutcome::Failed);
        assert!(dir.join(Side::Integration.unavailable_file_name()).exists());

        std::fs::remove_dir(dir.join(MANIFEST_FILE_NAME)).expect("dir should be removed");
        finalize(log_dir.path(), TestOutcome::Failed);

        let manifest = read_manifest(&dir);
        assert_eq!(
            manifest["decoded_unavailable"],
            serde_json::json!([{"side": "target", "reason": "collection failed"}])
        );
        assert!(!dir.join(Side::Integration.unavailable_file_name()).exists());
    }

    mod capture_intake {
        use std::time::Duration;

        use serde_json::json;
        use tokio_util::sync::CancellationToken;

        use super::super::{capture_intake, finalize, traffic_dir, Side, INTEGRATION_INPUT_UNAVAILABLE_REASON};
        use super::{read_json, read_manifest};
        use crate::intake::test_server::{closed_port, install_crypto_provider, MockIntake};
        use crate::intake::IntakeClient;
        use crate::reporter::TestOutcome;

        /// Runs a capture into a fresh log directory, finalizes it as failed, and returns the merged manifest.
        async fn capture(port: u16, cancel: &CancellationToken, budget: Duration) -> serde_json::Value {
            install_crypto_provider();
            let log_dir = tempfile::tempdir().expect("temp dir should be created");
            let dir = traffic_dir(log_dir.path());

            tokio::time::timeout(
                Duration::from_secs(5),
                capture_intake(&dir, &IntakeClient::new(port), cancel, budget),
            )
            .await
            .expect("capture should finish within its budget");

            finalize(log_dir.path(), TestOutcome::Failed);
            let manifest = read_manifest(&dir);
            assert_eq!(manifest["input"], serde_json::Value::Null);
            assert_eq!(
                manifest["input_unavailable_reason"],
                INTEGRATION_INPUT_UNAVAILABLE_REASON
            );
            manifest
        }

        fn unavailable_reason(manifest: &serde_json::Value) -> String {
            assert_eq!(manifest["decoded"].as_array().map(Vec::len), Some(0), "{}", manifest);
            assert_eq!(manifest["decoded_unavailable"][0]["side"], Side::Integration.name());
            manifest["decoded_unavailable"][0]["reason"]
                .as_str()
                .expect("reason should be a string")
                .to_string()
        }

        #[tokio::test]
        async fn a_reachable_intake_is_captured_for_the_target() {
            let intake = MockIntake::serving(json!([{
                "context": {"name": "some.metric", "tags": []},
                "values": [[10, {"mtype": "Count", "value": 1.0}]],
            }]))
            .await;
            install_crypto_provider();
            let log_dir = tempfile::tempdir().expect("temp dir should be created");
            let dir = traffic_dir(log_dir.path());

            capture_intake(
                &dir,
                &IntakeClient::new(intake.port()),
                &CancellationToken::new(),
                Duration::from_secs(5),
            )
            .await;

            let decoded = read_json(&dir.join(Side::Integration.manifest_file_name()));
            assert_eq!(decoded["side"], "target");
            assert_eq!(decoded["records_by_kind"][1], json!({"kind": "metric", "count": 1}));

            finalize(log_dir.path(), TestOutcome::Failed);
            let manifest = read_manifest(&dir);
            assert_eq!(manifest["decoded"][0]["side"], "target");
            assert_eq!(manifest["decoded"][0]["file"]["present"], true);
            assert!(manifest.get("decoded_unavailable").is_none(), "{}", manifest);
            assert_eq!(
                manifest["input_unavailable_reason"],
                INTEGRATION_INPUT_UNAVAILABLE_REASON
            );
        }

        #[tokio::test]
        async fn an_unreachable_intake_records_the_full_collection_error() {
            let manifest = capture(closed_port(), &CancellationToken::new(), Duration::from_secs(5)).await;

            let reason = unavailable_reason(&manifest);
            assert!(
                reason.starts_with(
                    "Failed to collect the post-assertion intake capture: Failed to call events dump endpoint on \
                     datadog-intake server.: "
                ),
                "{}",
                reason
            );
        }

        #[tokio::test]
        async fn an_unresponsive_intake_is_abandoned_when_the_budget_runs_out() {
            let intake = MockIntake::unresponsive().await;

            let manifest = capture(intake.port(), &CancellationToken::new(), Duration::from_millis(50)).await;

            assert_eq!(
                unavailable_reason(&manifest),
                "The post-assertion intake capture did not finish within 50ms, so it was abandoned."
            );
        }

        #[tokio::test]
        async fn an_already_cancelled_test_skips_the_capture() {
            let intake = MockIntake::unresponsive().await;
            let cancel = CancellationToken::new();
            cancel.cancel();

            let manifest = capture(intake.port(), &cancel, Duration::from_secs(10)).await;

            assert_eq!(
                unavailable_reason(&manifest),
                "The test was cancelled before the post-assertion intake capture, so the intake was not queried."
            );
            assert_eq!(intake.connections(), 0);
        }

        #[tokio::test]
        async fn cancellation_during_the_capture_abandons_it() {
            let intake = MockIntake::unresponsive().await;
            let cancel = CancellationToken::new();
            // Cancel only once the intake holds the capture's request, so the capture is underway.
            let cancel_once_requested = async {
                while intake.connections() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                cancel.cancel();
            };

            let (manifest, ()) = tokio::join!(
                capture(intake.port(), &cancel, Duration::from_secs(10)),
                cancel_once_requested
            );

            assert_eq!(
                unavailable_reason(&manifest),
                "The test was cancelled during the post-assertion intake capture, so it was abandoned."
            );
        }
    }
}
