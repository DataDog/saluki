use std::fs;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateMetricType};
use saluki_context::{
    tags::{Tag, TagSet},
    Context,
};
use saluki_error::GenericError;
use serde_json::json;
use stringtheory::MetaString;

use super::{
    finish_buffered_writer, finish_zstd_encoder, for_each_record, publish_buffered_temporary_unmanaged,
    publish_context_dump, publish_context_dump_with, publish_context_dump_with_services, run_with_temporary_cleanup,
    write_snapshot_records, AgentContextRecord, ArtifactError, AtomicReplace, FileSystemAtomicReplace,
    FileSystemCleanup, OwnerOnlyPermissions, SyncAll, TemporaryFileCleanup, TemporaryFileFactory, TemporaryPathCleanup,
    CONTEXT_DUMP_FILENAME,
};
#[cfg(windows)]
use super::{open_temporary_file, SecureTemporaryFileFactory};
#[cfg(unix)]
use super::{set_owner_only_permissions, PermissionNormalizer};
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

fn publish_open_temporary<W, R>(
    writer: W, temporary_path: &std::path::Path, target: &std::path::Path, snapshot: &[AggregateContextSnapshotEntry],
    replacer: &R,
) -> Result<(), GenericError>
where
    W: Write + SyncAll,
    R: AtomicReplace,
{
    run_with_temporary_cleanup(temporary_path, target, &FileSystemCleanup, || {
        publish_buffered_temporary_unmanaged(BufWriter::new(writer), temporary_path, target, snapshot, replacer)
    })
}

fn publish_buffered_temporary<W, R>(
    buffer: BufWriter<W>, temporary_path: &std::path::Path, target: &std::path::Path,
    snapshot: &[AggregateContextSnapshotEntry], replacer: &R,
) -> Result<(), GenericError>
where
    W: Write + SyncAll,
    R: AtomicReplace,
{
    run_with_temporary_cleanup(temporary_path, target, &FileSystemCleanup, || {
        publish_buffered_temporary_unmanaged(buffer, temporary_path, target, snapshot, replacer)
    })
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

#[cfg(windows)]
struct LocalAllocation(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};

            // SAFETY: This pointer was allocated by a Windows security API that transfers ownership to the caller.
            unsafe {
                let _ = LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

#[cfg(windows)]
fn windows_file_dacl_sddl(path: &std::path::Path) -> io::Result<(u16, String)> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::{ptr, slice};

    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        Security::{
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
                SE_FILE_OBJECT,
            },
            GetSecurityDescriptorControl, DACL_SECURITY_INFORMATION,
        },
    };

    let mut wide_path: Vec<_> = path.as_os_str().encode_wide().collect();
    if wide_path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    wide_path.push(0);

    let mut descriptor = ptr::null_mut();
    // SAFETY: The path is NUL-terminated and remains alive during the call. All unused output pointers are null,
    // and `descriptor` receives an allocation owned by the caller on success.
    let result = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let descriptor = LocalAllocation(descriptor);

    let mut control = 0;
    let mut revision = 0;
    // SAFETY: `descriptor` is a valid security descriptor allocation and both output pointers are valid writes.
    let result = unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut sddl = ptr::null_mut();
    let mut sddl_len = 0;
    // SAFETY: `descriptor` remains valid during the call, and both output pointers are valid writes. The returned
    // string allocation is owned by the caller on success.
    let result = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl,
            &mut sddl_len,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let sddl_allocation = LocalAllocation(sddl.cast());
    // SAFETY: The conversion API returned `sddl_len` initialized UTF-16 code units in the live `sddl` allocation.
    let wide_sddl = unsafe { slice::from_raw_parts(sddl, sddl_len as usize) };
    let wide_sddl = wide_sddl.strip_suffix(&[0]).unwrap_or(wide_sddl);
    let sddl = String::from_utf16(wide_sddl).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    drop(sddl_allocation);

    Ok((control, sddl))
}

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

#[cfg(windows)]
#[test]
fn temporary_dump_is_created_with_a_protected_restrictive_dacl() {
    use windows_sys::Win32::Security::SE_DACL_PROTECTED;

    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let (_file, staging_path) = SecureTemporaryFileFactory
        .create(run_directory.path())
        .expect("temporary dump should be created");

    let (control, sddl) = windows_file_dacl_sddl(&staging_path).expect("temporary dump DACL should be readable");

    assert_ne!(control & SE_DACL_PROTECTED, 0, "DACL must be protected: {sddl}");
    assert!(sddl.contains("D:P"), "DACL SDDL must carry the protected flag: {sddl}");
    for required_ace in ["(A;;FA;;;OW)", "(A;;FA;;;SY)", "(A;;FA;;;BA)"] {
        assert!(sddl.contains(required_ace), "DACL is missing {required_ace}: {sddl}");
    }
    assert_eq!(sddl.matches('(').count(), 3, "DACL contains an unexpected ACE: {sddl}");
    assert!(!sddl.contains(";;;WD)"), "DACL must not grant Everyone access: {sddl}");
    assert!(
        !sddl.contains(";;;BU)"),
        "DACL must not grant Builtin Users access: {sddl}"
    );
}

#[cfg(unix)]
#[test]
fn publication_normalizes_restrictive_temporary_permissions_to_exactly_0600() {
    use std::os::unix::fs::PermissionsExt as _;

    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let normalizer = RestrictiveThenOwnerOnly::default();

    let target = publish_context_dump_with_services(
        run_directory.path(),
        &[],
        &FileSystemAtomicReplace,
        &FixedTemporaryFileFactory,
        &normalizer,
        &FileSystemCleanup,
    )
    .expect("snapshot should publish");

    assert!(normalizer.called.load(Ordering::Relaxed));
    assert_eq!(fs::metadata(target).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn temporary_file_creation_failure_is_contextual_and_preserves_the_canonical_artifact() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let original = b"original canonical artifact";
    fs::write(&target, original).expect("canonical fixture should be written");

    let error = publish_context_dump_with_services(
        run_directory.path(),
        &[],
        &FileSystemAtomicReplace,
        &FailingTemporaryFileFactory,
        &OwnerOnlyPermissions,
        &FileSystemCleanup,
    )
    .expect_err("temporary file creation failure should surface");

    let message = format!("{error:#}");
    assert!(message.contains("create temporary"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(
        message.contains("injected temporary file creation failure"),
        "{message}"
    );
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

#[cfg(unix)]
#[test]
fn permission_failure_closes_and_removes_the_new_temporary_file() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let original = b"original canonical artifact";
    fs::write(&target, original).expect("canonical fixture should be written");

    let error = publish_context_dump_with_services(
        run_directory.path(),
        &[],
        &FileSystemAtomicReplace,
        &FixedTemporaryFileFactory,
        &FailingPermissions,
        &FileSystemCleanup,
    )
    .expect_err("permission failure should surface");

    let message = format!("{error:#}");
    assert!(message.contains("set owner-only permissions"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(message.contains("injected permission failure"), "{message}");
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

#[test]
fn cleanup_method_surfaces_removal_failure_and_remains_armed() {
    let temporary_directory = tempfile::tempdir().expect("temporary directory should be created");
    let temporary_path = temporary_directory.path().join("staged-contexts.tmp");
    fs::write(&temporary_path, b"staged").unwrap();
    let cleanup = FailingCleanup::default();
    let mut guard = TemporaryPathCleanup::new(&temporary_path, &cleanup);

    let error = guard.cleanup().expect_err("cleanup failure should surface");

    assert!(error.to_string().contains("injected cleanup failure"));
    assert!(temporary_path.exists());
    assert_eq!(cleanup.attempts.load(Ordering::Relaxed), 1);
}

#[test]
fn reports_primary_and_cleanup_failures_without_replacing_the_canonical_artifact() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let original = b"original canonical artifact";
    fs::write(&target, original).expect("canonical fixture should be written");
    let cleanup = FailingCleanup::default();

    let error = publish_context_dump_with_services(
        run_directory.path(),
        &[],
        &FailingReplace,
        &FixedTemporaryFileFactory,
        &OwnerOnlyPermissions,
        &cleanup,
    )
    .expect_err("replacement and cleanup should fail");

    let message = format!("{error:#}");
    assert!(message.contains("atomically replace"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(message.contains("remove temporary DogStatsD context dump"), "{message}");
    assert!(message.contains("injected cleanup failure"), "{message}");
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(directory_entries(run_directory.path()).len(), 2);
    assert!(directory_entries(run_directory.path())
        .iter()
        .any(|entry| entry.starts_with(&format!(".{CONTEXT_DUMP_FILENAME}.")) && entry.ends_with(".tmp")));
    assert!(cleanup.attempts.load(Ordering::Relaxed) >= 1);
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
fn injected_replace_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let original = b"original canonical artifact";
    fs::write(&target, original).expect("canonical fixture should be written");
    let snapshot = [snapshot_entry(
        "replacement.failure",
        None,
        AggregateMetricType::Gauge,
        "",
        &[],
        &[],
    )];

    let error = publish_context_dump_with(run_directory.path(), &snapshot, &FailingReplace)
        .expect_err("replacement should fail");

    let message = format!("{error:#}");
    assert!(message.contains("atomically replace"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

#[test]
fn zstd_finalization_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let temporary = run_directory.path().join(".injected-zstd-failure.tmp");
    let original = b"original canonical artifact";
    fs::write(&target, original).expect("canonical fixture should be written");
    let file = fs::File::create(&temporary).expect("temporary fixture should be created");
    let writer = InjectableFile {
        file,
        fail_writes: true,
        fail_sync: false,
    };
    let buffer = BufWriter::with_capacity(0, writer);
    let snapshot = [snapshot_entry(
        "zstd.failure",
        None,
        AggregateMetricType::Gauge,
        "",
        &[],
        &[],
    )];

    let error = publish_buffered_temporary(buffer, &temporary, &target, &snapshot, &FailingReplace)
        .expect_err("zstd finalization should fail");

    let message = format!("{error:#}");
    assert!(message.contains("finalize zstd stream"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(message.contains("injected write failure"), "{message}");
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

#[test]
fn buffered_finalization_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let temporary = run_directory.path().join(".injected-buffer-failure.tmp");
    let original = b"original canonical artifact";
    fs::write(&target, original).expect("canonical fixture should be written");
    let file = fs::File::create(&temporary).expect("temporary fixture should be created");
    let writer = InjectableFile {
        file,
        fail_writes: true,
        fail_sync: false,
    };
    let snapshot = [snapshot_entry(
        "buffer.failure",
        None,
        AggregateMetricType::Gauge,
        "",
        &[],
        &[],
    )];

    let error = publish_open_temporary(writer, &temporary, &target, &snapshot, &FailingReplace)
        .expect_err("buffer finalization should fail");

    let message = format!("{error:#}");
    assert!(message.contains("finalize buffered"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(message.contains("injected write failure"), "{message}");
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

#[test]
fn sync_failure_preserves_the_canonical_artifact_and_removes_the_temporary_file() {
    let run_directory = tempfile::tempdir().expect("temporary run directory should be created");
    let target = run_directory.path().join(CONTEXT_DUMP_FILENAME);
    let temporary = run_directory.path().join(".injected-sync-failure.tmp");
    let original = b"original canonical artifact";
    fs::write(&target, original).expect("canonical fixture should be written");
    let file = fs::File::create(&temporary).expect("temporary fixture should be created");
    let writer = InjectableFile {
        file,
        fail_writes: false,
        fail_sync: true,
    };

    let error =
        publish_open_temporary(writer, &temporary, &target, &[], &FailingReplace).expect_err("sync should fail");

    let message = format!("{error:#}");
    assert!(message.contains("sync temporary"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(message.contains("injected sync failure"), "{message}");
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        directory_entries(run_directory.path()),
        vec![CONTEXT_DUMP_FILENAME.to_owned()]
    );
}

#[test]
fn zstd_and_buffer_finalizers_surface_their_own_error_context() {
    let target = PathBuf::from("/run/test/dogstatsd_contexts.json.zstd");
    let snapshot = [snapshot_entry(
        "finalize.failure",
        None,
        AggregateMetricType::Gauge,
        "",
        &[],
        &[],
    )];

    let zstd_failure = Arc::new(AtomicBool::new(false));
    let mut encoder = zstd::stream::write::Encoder::new(ToggleFailureWriter::new(zstd_failure.clone()), 0)
        .expect("encoder should initialize");
    write_snapshot_records(&mut encoder, &snapshot, &target).expect("records should stream before failure");
    zstd_failure.store(true, Ordering::Relaxed);
    let error = finish_zstd_encoder(encoder, &target).expect_err("zstd finalization should fail");
    let message = format!("{error:#}");
    assert!(message.contains("finalize zstd stream"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(message.contains("injected finalization failure"), "{message}");

    let buffer_failure = Arc::new(AtomicBool::new(false));
    let buffer = BufWriter::new(ToggleFailureWriter::new(buffer_failure.clone()));
    let mut encoder = zstd::stream::write::Encoder::new(buffer, 0).expect("encoder should initialize");
    write_snapshot_records(&mut encoder, &snapshot, &target).expect("records should stream before failure");
    let buffer = finish_zstd_encoder(encoder, &target).expect("zstd stream should finalize");
    buffer_failure.store(true, Ordering::Relaxed);
    let error = finish_buffered_writer(buffer, &target).expect_err("buffer finalization should fail");
    let message = format!("{error:#}");
    assert!(message.contains("finalize buffered"), "{message}");
    assert!(message.contains(&target.display().to_string()), "{message}");
    assert!(message.contains("injected finalization failure"), "{message}");
}

struct InjectableFile {
    file: fs::File,
    fail_writes: bool,
    fail_sync: bool,
}

impl io::Write for InjectableFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_writes {
            Err(io::Error::other("injected write failure"))
        } else {
            self.file.write(bytes)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.fail_writes {
            Err(io::Error::other("injected write failure"))
        } else {
            self.file.flush()
        }
    }
}

impl SyncAll for InjectableFile {
    fn sync_all(&self) -> io::Result<()> {
        if self.fail_sync {
            Err(io::Error::other("injected sync failure"))
        } else {
            self.file.sync_all()
        }
    }
}

#[derive(Debug)]
struct ToggleFailureWriter {
    failure_enabled: Arc<AtomicBool>,
}

impl ToggleFailureWriter {
    fn new(failure_enabled: Arc<AtomicBool>) -> Self {
        Self { failure_enabled }
    }
}

impl io::Write for ToggleFailureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failure_enabled.load(Ordering::Relaxed) {
            Err(io::Error::other("injected finalization failure"))
        } else {
            Ok(bytes.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.failure_enabled.load(Ordering::Relaxed) {
            Err(io::Error::other("injected finalization failure"))
        } else {
            Ok(())
        }
    }
}

struct FixedTemporaryFileFactory;

impl TemporaryFileFactory for FixedTemporaryFileFactory {
    fn create(&self, run_path: &std::path::Path) -> io::Result<(fs::File, PathBuf)> {
        let path = run_path.join(format!(".{CONTEXT_DUMP_FILENAME}.fixed.tmp"));
        #[cfg(windows)]
        let file = open_temporary_file(&path)?;
        #[cfg(not(windows))]
        let file = {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            options.open(&path)?
        };
        Ok((file, path))
    }
}

struct FailingTemporaryFileFactory;

impl TemporaryFileFactory for FailingTemporaryFileFactory {
    fn create(&self, _run_path: &std::path::Path) -> io::Result<(fs::File, PathBuf)> {
        Err(io::Error::other("injected temporary file creation failure"))
    }
}

#[cfg(unix)]
struct FailingPermissions;

#[cfg(unix)]
impl PermissionNormalizer for FailingPermissions {
    fn normalize(&self, _file: &fs::File) -> io::Result<()> {
        Err(io::Error::other("injected permission failure"))
    }
}

#[cfg(unix)]
#[derive(Default)]
struct RestrictiveThenOwnerOnly {
    called: AtomicBool,
}

#[cfg(unix)]
impl PermissionNormalizer for RestrictiveThenOwnerOnly {
    fn normalize(&self, file: &fs::File) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        file.set_permissions(fs::Permissions::from_mode(0o000))?;
        set_owner_only_permissions(file)?;
        self.called.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Default)]
struct FailingCleanup {
    attempts: AtomicUsize,
}

impl TemporaryFileCleanup for FailingCleanup {
    fn remove(&self, _path: &std::path::Path) -> io::Result<()> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        Err(io::Error::other("injected cleanup failure"))
    }
}

struct FailingReplace;

impl AtomicReplace for FailingReplace {
    fn replace(&self, _source: &std::path::Path, _target: &std::path::Path) -> io::Result<()> {
        Err(io::Error::other("injected replacement failure"))
    }
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
