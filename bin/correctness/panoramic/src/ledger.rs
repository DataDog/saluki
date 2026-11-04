//! Tracks test resources for cleanup by the runner.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use airlock::driver::Driver;
use airlock::unix::UnixProcess;
use rand::RngExt as _;
use saluki_error::{ErrorContext as _, GenericError};
use tokio::time::{timeout_at, Instant};

use crate::reporter::{Issue, IssueKind, ReclaimEntry, ReclaimResult};

/// How long the runner spends removing one case's resources.
pub(crate) const RECLAIM_TIME: Duration = Duration::from_secs(30);

/// Returns a random lowercase alphanumeric ID suitable for Docker and Kubernetes names.
pub(crate) fn random_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Shares a case's registered resources between its test body and the runner.
///
/// The body registers resources as it creates them; the runner calls [`ResourceLedger::reclaim`] to remove them in
/// reverse registration order. Dropping the last ledger handle sends SIGKILL to registered process groups but leaves
/// directories and Docker resources behind.
#[derive(Clone, Debug, Default)]
pub(crate) struct ResourceLedger {
    entries: Arc<Mutex<Vec<Entry>>>,
}

#[derive(Debug)]
struct Entry {
    resource: Resource,
    /// Whether a creation request for this resource was cut short.
    unsettled: bool,
}

/// A resource the runner knows how to remove.
#[derive(Debug)]
enum Resource {
    /// The containers, shared volume, and network labeled with this isolation-group ID.
    IsolationGroup(String),
    /// A host process and its process group.
    Process(UnixProcess),
    /// A host directory, removed only if every resource registered after it was reclaimed.
    Directory(PathBuf),
}

impl Resource {
    /// Removes the resource. A directory that is already gone counts as removed.
    async fn remove(self) -> Result<(), GenericError> {
        match self {
            Self::IsolationGroup(id) => Driver::clean_related_resources(id).await,
            Self::Process(process) => process.cleanup().await,
            Self::Directory(path) => match tokio::fs::remove_dir_all(&path).await {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                removed => removed.with_error_context(|| format!("Failed to remove '{}'", path.display())),
            },
        }
    }
}

impl fmt::Display for Resource {
    /// Writes `isolation_group:<id>`, `process:<name>`, or `directory:<path>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IsolationGroup(id) => write!(f, "isolation_group:{}", id),
            Self::Process(process) => write!(f, "process:{}", process.name()),
            Self::Directory(path) => write!(f, "directory:{}", path.display()),
        }
    }
}

impl ResourceLedger {
    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        // Every update is a single push or flag write, so the entries stay consistent if a holder panics.
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register(&self, resource: Resource) {
        self.lock().push(Entry {
            resource,
            unsettled: false,
        });
    }

    /// Registers a new isolation group and returns its ID. Call it before creating anything in the group.
    pub(crate) fn isolation_group(&self) -> String {
        let id = random_id();
        self.register(Resource::IsolationGroup(id.clone()));
        id
    }

    /// Takes ownership of a spawned process.
    pub(crate) fn process(&self, process: UnixProcess) {
        self.register(Resource::Process(process));
    }

    /// Registers a directory the case created.
    pub(crate) fn directory(&self, path: PathBuf) {
        self.register(Resource::Directory(path));
    }

    /// Marks an interrupted creation request: Docker may finish creating the resource after cleanup.
    pub(crate) fn mark_unsettled(&self, isolation_group: &str) {
        for entry in self.lock().iter_mut() {
            if matches!(&entry.resource, Resource::IsolationGroup(id) if id == isolation_group) {
                entry.unsettled = true;
            }
        }
    }

    /// Attempts cleanup in reverse registration order, sharing one [`RECLAIM_TIME`] budget.
    ///
    /// Returns a result per resource and an issue for each failed or unconfirmed removal. At the deadline, drops
    /// pending removal futures; dropping a process sends SIGKILL if cleanup hasn't sent it yet. Filesystem removal
    /// may continue on a blocking thread after its future is dropped.
    pub(crate) async fn reclaim(&self) -> (Vec<ReclaimEntry>, Vec<Issue>) {
        let entries = std::mem::take(&mut *self.lock());
        let deadline = Instant::now() + RECLAIM_TIME;
        let mut reclaim = Vec::with_capacity(entries.len());
        let mut issues = Vec::new();
        // Preserve earlier directories that an unreclaimed resource may still need.
        let mut kept_by: Option<String> = None;

        for Entry { resource, unsettled } in entries.into_iter().rev() {
            let label = resource.to_string();
            let (result, detail) = match (&resource, &kept_by) {
                (Resource::Directory(_), Some(kept_by)) => (
                    ReclaimResult::Leaked,
                    Some(format!("kept because {} was not reclaimed", kept_by)),
                ),
                _ => match timeout_at(deadline, resource.remove()).await {
                    Ok(Ok(())) if unsettled => (
                        ReclaimResult::Unsettled,
                        Some(
                            "a creation request was cut short, so something created after removal may remain"
                                .to_string(),
                        ),
                    ),
                    Ok(Ok(())) => (ReclaimResult::Reclaimed, None),
                    Ok(Err(e)) => (ReclaimResult::Leaked, Some(format!("{:#}", e))),
                    Err(_) => (
                        ReclaimResult::Unsettled,
                        Some(format!(
                            "still being removed when the {:?} reclaim budget expired",
                            RECLAIM_TIME
                        )),
                    ),
                },
            };

            if let Some(detail) = detail {
                let kind = match result {
                    ReclaimResult::Unsettled => IssueKind::ResourceUnsettled,
                    _ => IssueKind::ResourceLeaked,
                };
                issues.push(Issue {
                    kind,
                    message: format!("{}: {}", label, detail),
                });
                kept_by.get_or_insert_with(|| label.clone());
            }
            reclaim.push(ReclaimEntry {
                resource: label,
                result,
            });
        }

        (reclaim, issues)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;
    use std::task::Poll;

    use super::*;

    #[test]
    fn directory_removal_yields_to_the_reclaim_deadline() {
        // Synchronous removal in Resource::remove prevented the reclaim timer from running. Occupy the blocking
        // pool so async removal must wait, then expire the budget before any filesystem work can start.
        let parent = tempfile::tempdir().expect("temp dir should be creatable");
        let directory = parent.path().join("state");
        std::fs::create_dir(&directory).expect("the state directory should be creatable");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .max_blocking_threads(1)
            .build()
            .expect("the runtime should build");

        runtime.block_on(async {
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let (started, ready) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = wait.recv();
            });
            ready.await.expect("the blocking task should start");

            let ledger = ResourceLedger::default();
            ledger.directory(directory.clone());
            let reclaim = ledger.reclaim();
            tokio::pin!(reclaim);
            std::future::poll_fn(|cx| {
                assert!(reclaim.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            tokio::time::advance(RECLAIM_TIME).await;
            let (reclaim, issues) = reclaim.await;

            assert_eq!(
                reclaim,
                [ReclaimEntry {
                    resource: format!("directory:{}", directory.display()),
                    result: ReclaimResult::Unsettled,
                }]
            );
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].kind, IssueKind::ResourceUnsettled);
            assert!(issues[0].message.contains("reclaim budget expired"), "{:?}", issues[0]);
            assert!(directory.exists(), "removal should still be queued");
            drop(release);
            blocker.await.expect("the blocking task should finish");
        });
    }

    #[tokio::test]
    async fn a_directory_with_nested_files_is_reclaimed() {
        let parent = tempfile::tempdir().expect("temp dir should be creatable");
        let directory = parent.path().join("state");
        std::fs::create_dir_all(directory.join("nested")).expect("the nested directory should be creatable");
        std::fs::write(directory.join("nested/file"), b"state").expect("the nested file should be writable");
        let ledger = ResourceLedger::default();
        ledger.directory(directory.clone());

        let (reclaim, issues) = ledger.reclaim().await;

        assert_eq!(
            reclaim,
            [ReclaimEntry {
                resource: format!("directory:{}", directory.display()),
                result: ReclaimResult::Reclaimed,
            }]
        );
        assert_eq!(issues, []);
        assert!(!directory.exists());
        assert!(parent.path().exists());
    }

    #[tokio::test]
    async fn a_directory_that_is_already_gone_counts_as_reclaimed() {
        let parent = tempfile::tempdir().expect("temp dir should be creatable");
        let missing = parent.path().join("missing");
        let ledger = ResourceLedger::default();
        ledger.directory(missing.clone());

        let (reclaim, issues) = tokio::time::timeout(Duration::from_secs(5), ledger.reclaim())
            .await
            .expect("reclaim should finish within the bound");

        assert_eq!(
            reclaim,
            [ReclaimEntry {
                resource: format!("directory:{}", missing.display()),
                result: ReclaimResult::Reclaimed,
            }]
        );
        assert_eq!(issues, []);
    }
}
