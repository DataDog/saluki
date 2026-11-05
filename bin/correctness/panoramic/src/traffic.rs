//! Traffic artifacts for a test: what went in, and what the intake decoded out of it.
//!
//! A failed test says what went wrong, but not what the system was fed or what it produced. These artifacts answer
//! both, under a test's `traffic/` directory:
//!
//! - `input.jsonl.zst` and `input-manifest.json`: the input payload stream of a correctness test, written by one
//!   Millstone run. Both arms are fed byte-identical payloads from the same seed, so capturing one arm describes both.
//! - `<side>-decoded.jsonl.zst`: the telemetry a side's `datadog-intake` decoded. A correctness test writes one per
//!   arm, as the analysis saw it.
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
    path::{Path, PathBuf},
};

use panoramic_core::result::TestOutcome;
pub(crate) use panoramic_steps::compare::decoded::Side;
use panoramic_steps::compare::{decoded::write_decoded_records, intake::CollectedData};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

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

/// Returns the traffic directory for a test's log directory.
pub(crate) fn traffic_dir(log_dir: &Path) -> PathBuf {
    log_dir.join(TRAFFIC_DIR_NAME)
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
    let file = File::create(&records_path)
        .with_error_context(|| format!("Failed to create decoded capture '{}'.", records_path.display()))?;
    let role = match side {
        Side::Baseline => "core Agent alone (reference side)",
        Side::Comparison => "core Agent with ADP (side under test)",
    };
    let manifest = match write_decoded_records(file, side, role, "datadog-intake dump endpoints", data) {
        Ok(manifest) => manifest,
        Err(e) => {
            // A half-written file can't be read back, and leaving it would look like a complete capture.
            let _ = std::fs::remove_file(&records_path);
            return Err(e);
        }
    };

    write_json_file(&dir.join(side.manifest_file_name()), &manifest)
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
    fn a_decoded_capture_is_kept_on_failure_and_deleted_on_pass() {
        for (outcome, retained) in [(TestOutcome::Failed, true), (TestOutcome::Passed, false)] {
            let log_dir = tempfile::tempdir().expect("temp dir should be created");
            let dir = traffic_dir(log_dir.path());
            write_decoded(&dir, Side::Comparison, &CollectedData::default());

            finalize(log_dir.path(), outcome);

            let manifest = read_manifest(&dir);
            assert_eq!(manifest["captures_retained"], retained);
            assert_eq!(manifest["decoded"].as_array().map(Vec::len), Some(1));
            assert_eq!(manifest["decoded"][0]["side"], "comparison");
            assert_eq!(manifest["decoded"][0]["file"]["name"], "comparison-decoded.jsonl.zst");
            assert_eq!(manifest["decoded"][0]["file"]["present"], retained);
            assert!(manifest.get("decoded_unavailable").is_none(), "{}", manifest);
            assert_eq!(dir.join("comparison-decoded.jsonl.zst").exists(), retained);
            assert!(!dir.join("comparison-decoded-manifest.json").exists());
        }
    }

    #[test]
    fn a_failed_decoded_write_is_reported_as_unavailable() {
        let log_dir = tempfile::tempdir().expect("temp dir should be created");
        let dir = traffic_dir(log_dir.path());
        // A directory where the sidecar belongs makes the sidecar write fail after the records are written.
        std::fs::create_dir_all(dir.join(Side::Comparison.manifest_file_name())).expect("dir should be created");

        write_decoded(&dir, Side::Comparison, &CollectedData::default());
        assert!(dir.join(Side::Comparison.unavailable_file_name()).exists());

        finalize(log_dir.path(), TestOutcome::Failed);

        let manifest = read_manifest(&dir);
        assert_eq!(manifest["decoded"].as_array().map(Vec::len), Some(0));
        assert_eq!(manifest["decoded_unavailable"][0]["side"], "comparison");
        let reason = manifest["decoded_unavailable"][0]["reason"]
            .as_str()
            .expect("reason should be a string");
        assert!(
            reason.starts_with("Failed to write the decoded capture: Failed to write '"),
            "{}",
            reason
        );
        // The chain continues past the outer context to the I/O cause.
        assert!(reason.contains("comparison-decoded-manifest.json'.: "), "{}", reason);
        assert!(dir.join("comparison-decoded.jsonl.zst").exists());
        assert!(!dir.join(Side::Comparison.unavailable_file_name()).exists());
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
        record_decoded_unavailable(&dir, Side::Comparison, "collection failed");
        // A directory where the merged manifest belongs makes its write fail.
        std::fs::create_dir_all(dir.join(MANIFEST_FILE_NAME)).expect("dir should be created");

        finalize(log_dir.path(), TestOutcome::Failed);
        assert!(dir.join(Side::Comparison.unavailable_file_name()).exists());

        std::fs::remove_dir(dir.join(MANIFEST_FILE_NAME)).expect("dir should be removed");
        finalize(log_dir.path(), TestOutcome::Failed);

        let manifest = read_manifest(&dir);
        assert_eq!(
            manifest["decoded_unavailable"],
            serde_json::json!([{"side": "comparison", "reason": "collection failed"}])
        );
        assert!(!dir.join(Side::Comparison.unavailable_file_name()).exists());
    }
}
