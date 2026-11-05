//! Copies declared participant files into case artifacts without changing the procedure verdict.

use std::{fs::File, io::Write as _, path::Path, sync::Mutex, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    handle::Primitives,
    participant::{CollectDestination, CollectFile},
    result::{Collected, Issue, IssueKind},
};

const COLLECTION_TIME: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct CollectionReport {
    pub collected: Vec<Collected>,
    pub issues: Vec<Issue>,
}

pub(crate) fn artifact_file(directory: &Path, name: &str) -> Result<File, String> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', ':']) {
        return Err(format!("artifact name '{name}' must be a plain filename"));
    }
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create artifact directory '{}': {error}", directory.display()))?;
    let path = directory.join(name);
    File::options()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("cannot create artifact '{}': {error}", path.display()))
}

pub(crate) async fn collect(
    name: &str, participant: &dyn Primitives, files: &[CollectFile], log_dir: &Path, report: &Mutex<CollectionReport>,
    cancel: &CancellationToken,
) {
    for file in files {
        let directory = match file.destination {
            CollectDestination::Participant => log_dir.join(name).join("collected"),
            CollectDestination::Traffic => log_dir.join("traffic"),
        };
        let copy = async {
            let bytes = participant
                .read_file(file.path)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("file '{}' is missing", file.path))?;
            let mut output = artifact_file(&directory, file.name)?;
            let path = directory.join(file.name);
            if let Err(error) = output.write_all(&bytes) {
                drop(output);
                let _ = std::fs::remove_file(&path);
                return Err(format!("cannot write '{}': {error}", path.display()));
            }
            std::path::absolute(&path).map_err(|error| format!("cannot resolve '{}': {error}", path.display()))
        };
        let result = match cancel
            .run_until_cancelled(tokio::time::timeout(COLLECTION_TIME, copy))
            .await
        {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(format!("collection exceeded {COLLECTION_TIME:?}")),
            None => Err("collection was cancelled".into()),
        };
        let mut report = report.lock().unwrap();
        match result {
            Ok(path) => report.collected.push(Collected {
                participant: name.into(),
                kind: file.kind.into(),
                path,
            }),
            Err(error) => report.issues.push(Issue {
                kind: IssueKind::CollectionFailed,
                message: format!(
                    "participant '{name}', collection '{}' from '{}': {error}",
                    file.name, file.path
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{handle::HandleError, step::test_support::FakeParticipant};

    #[tokio::test]
    async fn collection_retains_successes_and_reports_missing_files_and_read_and_write_failures() {
        let root = tempfile::tempdir().unwrap();
        let fake = FakeParticipant::new(&[]);
        fake.set_file("/available", Ok(Some(vec![0, 255])));
        fake.set_file("/missing", Ok(None));
        fake.set_file(
            "/unreadable",
            Err(HandleError::Harness {
                message: "read mechanism failed".into(),
            }),
        );
        let files = [
            ("/available", "good"),
            ("/available", "good"),
            ("/missing", "missing"),
            ("/unreadable", "unreadable"),
            ("/available", "../outside"),
        ]
        .map(|(path, name)| CollectFile {
            path,
            name,
            kind: "sample",
            destination: CollectDestination::Traffic,
        });
        let report = Mutex::new(CollectionReport::default());
        collect(
            "subject",
            &fake,
            &files,
            root.path(),
            &report,
            &CancellationToken::new(),
        )
        .await;
        let report = report.into_inner().unwrap();
        assert_eq!(
            report.collected,
            [Collected {
                participant: "subject".into(),
                kind: "sample".into(),
                path: root.path().join("traffic/good"),
            }]
        );
        assert_eq!(std::fs::read(&report.collected[0].path).unwrap(), [0, 255]);
        assert_eq!(report.issues.len(), 4);
        for (issue, reason) in report.issues.iter().zip([
            "cannot create artifact",
            "is missing",
            "read mechanism failed",
            "plain filename",
        ]) {
            assert_eq!(issue.kind, IssueKind::CollectionFailed);
            assert!(issue.message.contains("participant 'subject'"), "{issue:?}");
            assert!(issue.message.contains(reason), "{issue:?}");
        }
        assert!(!root.path().join("outside").exists());
    }

    #[tokio::test]
    async fn cancelled_collection_records_an_issue_without_creating_an_artifact() {
        let root = tempfile::tempdir().unwrap();
        let fake = FakeParticipant::new(&[]);
        fake.set_file("/available", Ok(Some(vec![0, 255])));
        let files = [CollectFile {
            path: "/available",
            name: "output",
            kind: "sample",
            destination: CollectDestination::Participant,
        }];
        let report = Mutex::new(CollectionReport::default());
        let cancel = CancellationToken::new();
        cancel.cancel();
        collect("subject", &fake, &files, root.path(), &report, &cancel).await;
        let report = report.into_inner().unwrap();
        assert!(report.collected.is_empty());
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].kind, IssueKind::CollectionFailed);
        assert!(report.issues[0].message.contains("cancelled"));
        assert!(!root.path().join("subject/collected/output").exists());
    }

    struct PendingReader;

    #[async_trait::async_trait]
    impl Primitives for PendingReader {
        async fn read_file(&self, _: &str) -> Result<Option<Vec<u8>>, HandleError> {
            std::future::pending().await
        }
        async fn stat(&self, _: &str) -> Result<Option<crate::handle::FileStatus>, HandleError> {
            unreachable!()
        }
        async fn exec(
            &self, _: crate::handle::ExecCommand, _: Duration,
        ) -> Result<crate::handle::ExecOutput, HandleError> {
            unreachable!()
        }
        async fn probe(
            &self, _: &crate::endpoint::ResolvedAddress, _: &crate::probe::AttemptLimits,
        ) -> Result<crate::probe::ProbeAnswer, HandleError> {
            unreachable!()
        }
        async fn send(
            &self, _: &crate::endpoint::ResolvedAddress, _: &[u8], _: &crate::probe::AttemptLimits,
        ) -> Result<String, HandleError> {
            unreachable!()
        }
        async fn http(
            &self, _: &crate::endpoint::ResolvedAddress, _: &str, _: bool, _: &crate::probe::AttemptLimits,
        ) -> Result<crate::handle::HttpResponse, HandleError> {
            unreachable!()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn collection_times_out_a_stalled_read_and_keeps_the_report() {
        let root = tempfile::tempdir().unwrap();
        let report = Mutex::new(CollectionReport::default());
        let started = tokio::time::Instant::now();
        collect(
            "subject",
            &PendingReader,
            &[CollectFile {
                path: "/pending",
                name: "output",
                kind: "sample",
                destination: CollectDestination::Participant,
            }],
            root.path(),
            &report,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(started.elapsed(), COLLECTION_TIME);
        let report = report.into_inner().unwrap();
        assert!(report.collected.is_empty());
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].kind, IssueKind::CollectionFailed);
        assert!(report.issues[0].message.contains("exceeded 5s"));
    }
}
