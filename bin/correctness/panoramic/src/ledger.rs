//! Tracks test resources for cleanup by the runner.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use airlock::driver::{Driver, DriverDetails};
use airlock::unix::UnixProcess;
use futures::future::join_all;
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, DeleteParams};
use kube::Client;
use rand::RngExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::time::{sleep, timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use crate::reporter::{Issue, IssueKind, ReclaimEntry, ReclaimResult};

/// How long the runner spends stopping one case's tasks and removing its resources.
pub(crate) const RECLAIM_TIME: Duration = Duration::from_secs(30);

/// How often reclaim checks whether a deleted namespace is gone.
const NAMESPACE_POLL_INTERVAL: Duration = Duration::from_millis(500);

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
/// The body registers resources before it creates them; the runner calls [`ResourceLedger::reclaim`] to remove them in
/// reverse registration order. Dropping the last ledger handle sends SIGKILL to registered process groups but leaves
/// directories, Docker resources, and namespaces behind.
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
enum Resource {
    /// The containers, shared volume, and network labeled with this isolation-group ID.
    IsolationGroup(String),
    /// A host process and its process group.
    Process(UnixProcess),
    /// A Kubernetes namespace, removed through the client that created it.
    Namespace { name: String, client: Client },
    /// A host directory, removed only if every resource registered after it was reclaimed.
    Directory(PathBuf),
}

/// A namespace whose deletion the API server accepted. It is reclaimed once it is gone.
struct Terminating {
    api: Api<Namespace>,
    name: String,
}

/// A removal's result, and the detail behind anything short of reclaimed.
type Settled = (ReclaimResult, Option<String>);

/// How far one resource's removal got during the reclaim walk.
enum Removal {
    Settled(Settled),
    Terminating(Terminating),
}

impl Resource {
    /// Removes the resource, returning a namespace that is still terminating.
    ///
    /// A directory or namespace that is already gone counts as removed.
    async fn remove(self) -> Result<Option<Terminating>, GenericError> {
        match self {
            Self::IsolationGroup(id) => Driver::clean_related_resources(id).await.map(|()| None),
            Self::Process(process) => process.cleanup().await.map(|()| None),
            Self::Namespace { name, client } => {
                let api = Api::<Namespace>::all(client);
                match api.delete(&name, &DeleteParams::default()).await {
                    Ok(_) => Ok(Some(Terminating { api, name })),
                    Err(kube::Error::Api(status)) if status.is_not_found() => Ok(None),
                    Err(e) => Err(generic_error!("Failed to delete namespace '{}': {}", name, e)),
                }
            }
            Self::Directory(path) => match std::fs::remove_dir_all(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                removed => removed
                    .map(|()| None)
                    .with_error_context(|| format!("Failed to remove '{}'", path.display())),
            },
        }
    }
}

impl fmt::Debug for Resource {
    /// Omits a namespace's client, which isn't `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IsolationGroup(id) => f.debug_tuple("IsolationGroup").field(id).finish(),
            Self::Process(process) => f.debug_tuple("Process").field(process).finish(),
            Self::Namespace { name, .. } => f.debug_struct("Namespace").field("name", name).finish_non_exhaustive(),
            Self::Directory(path) => f.debug_tuple("Directory").field(path).finish(),
        }
    }
}

impl fmt::Display for Resource {
    /// Writes `isolation_group:<id>`, `process:<name>`, `namespace:<name>`, or `directory:<path>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IsolationGroup(id) => write!(f, "isolation_group:{}", id),
            Self::Process(process) => write!(f, "process:{}", process.name()),
            Self::Namespace { name, .. } => write!(f, "namespace:{}", name),
            Self::Directory(path) => write!(f, "directory:{}", path.display()),
        }
    }
}

impl Terminating {
    /// Polls until the namespace is gone.
    async fn gone(self) -> Result<(), GenericError> {
        loop {
            match self.api.get_opt(&self.name).await {
                Ok(None) => return Ok(()),
                Ok(Some(_)) => sleep(NAMESPACE_POLL_INTERVAL).await,
                Err(e) => {
                    return Err(generic_error!(
                        "Failed to check whether namespace '{}' is gone: {}",
                        self.name,
                        e
                    ))
                }
            }
        }
    }
}

/// Settles a removal that finished, which a cut-short creation leaves unsettled.
fn removed(unsettled: bool) -> Settled {
    if unsettled {
        (
            ReclaimResult::Unsettled,
            Some("a creation request was cut short, so something created after removal may remain".to_string()),
        )
    } else {
        (ReclaimResult::Reclaimed, None)
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

    /// Registers a namespace, which reclaim deletes through `client`. Call it before creating the namespace.
    pub(crate) fn namespace(&self, client: Client, name: String) {
        self.register(Resource::Namespace { name, client });
    }

    /// Registers a directory the case created.
    pub(crate) fn directory(&self, path: PathBuf) {
        self.register(Resource::Directory(path));
    }

    /// Marks a creation request cut short: the runtime may finish creating the resource after removal.
    fn mark_unsettled(&self, name: &str) {
        for entry in self.lock().iter_mut() {
            let named = match &entry.resource {
                Resource::IsolationGroup(id) => id == name,
                Resource::Namespace { name: namespace, .. } => namespace == name,
                Resource::Process(_) | Resource::Directory(_) => false,
            };
            if named {
                entry.unsettled = true;
            }
        }
    }

    /// Runs `request`, which creates part of the resource registered as `name`, unless `cancel` fires first.
    ///
    /// Returns `None` on cancellation and marks the resource unsettled.
    pub(crate) async fn create<F>(&self, name: &str, cancel: &CancellationToken, request: F) -> Option<F::Output>
    where
        F: Future,
    {
        let created = cancel.run_until_cancelled(request).await;
        if created.is_none() {
            self.mark_unsettled(name);
        }
        created
    }

    /// Creates and starts `driver`'s container in `isolation_group`, returning early when `cancel` fires.
    ///
    /// Cancellation during creation marks the group unsettled; starting creates nothing, so cancellation after creation
    /// leaves it settled.
    pub(crate) async fn start_container(
        &self, driver: &Driver, isolation_group: &str, cancel: &CancellationToken,
    ) -> Result<DriverDetails, GenericError> {
        self.create(isolation_group, cancel, driver.create())
            .await
            .ok_or_else(|| {
                generic_error!(
                    "Cancelled while creating a container in isolation group '{}'.",
                    isolation_group
                )
            })??;
        cancel
            .run_until_cancelled(driver.start_container())
            .await
            .unwrap_or_else(|| {
                Err(generic_error!(
                    "Cancelled while starting a container in isolation group '{}'.",
                    isolation_group
                ))
            })
    }

    /// Attempts cleanup in reverse registration order, finishing by `deadline`.
    ///
    /// Namespaces are deleted during the walk, then awaited together until they are gone, so each deletion starts
    /// without waiting for an earlier namespace's pods to stop. Returns a result per resource, in reverse registration
    /// order, and an issue for each failed or unconfirmed removal. At the deadline, drops pending removal futures;
    /// dropping a process sends SIGKILL if cleanup hasn't sent it yet.
    pub(crate) async fn reclaim(&self, deadline: Instant) -> (Vec<ReclaimEntry>, Vec<Issue>) {
        let entries = std::mem::take(&mut *self.lock());
        let mut removals = Vec::with_capacity(entries.len());
        // Preserve earlier directories that an unreclaimed resource may still need. A terminating namespace holds no
        // host directory, so it keeps none.
        let mut kept_by: Option<String> = None;

        for Entry { resource, unsettled } in entries.into_iter().rev() {
            let label = resource.to_string();
            let removal = match (&resource, &kept_by) {
                (Resource::Directory(_), Some(kept_by)) => Removal::Settled((
                    ReclaimResult::Leaked,
                    Some(format!("kept because {} was not reclaimed", kept_by)),
                )),
                _ => match timeout_at(deadline, resource.remove()).await {
                    Ok(Ok(Some(terminating))) => Removal::Terminating(terminating),
                    Ok(Ok(None)) => Removal::Settled(removed(unsettled)),
                    Ok(Err(e)) => Removal::Settled((ReclaimResult::Leaked, Some(format!("{:#}", e)))),
                    Err(_) => Removal::Settled((
                        ReclaimResult::Unsettled,
                        Some(format!(
                            "still being removed when the {:?} reclaim budget expired",
                            RECLAIM_TIME
                        )),
                    )),
                },
            };
            if let Removal::Settled((_, Some(_))) = removal {
                kept_by.get_or_insert_with(|| label.clone());
            }
            removals.push((label, unsettled, removal));
        }

        let settled = join_all(removals.into_iter().map(|(label, unsettled, removal)| async move {
            let settled = match removal {
                Removal::Settled(settled) => settled,
                Removal::Terminating(terminating) => match timeout_at(deadline, terminating.gone()).await {
                    Ok(Ok(())) => removed(unsettled),
                    Ok(Err(e)) => (ReclaimResult::Leaked, Some(format!("{:#}", e))),
                    Err(_) => (
                        ReclaimResult::Unsettled,
                        Some(format!(
                            "still terminating when the {:?} reclaim budget expired",
                            RECLAIM_TIME
                        )),
                    ),
                },
            };
            (label, settled)
        }))
        .await;

        let mut reclaim = Vec::with_capacity(settled.len());
        let mut issues = Vec::new();
        for (label, (result, detail)) in settled {
            if let Some(detail) = detail {
                let kind = match result {
                    ReclaimResult::Unsettled => IssueKind::ResourceUnsettled,
                    _ => IssueKind::ResourceLeaked,
                };
                issues.push(Issue {
                    kind,
                    message: format!("{}: {}", label, detail),
                });
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
    use std::collections::HashMap;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::intake::test_server::install_crypto_provider;
    use crate::tasks::{CaseTasks, TASK_STOP_TIME};

    /// Bounds every reclaim in these tests.
    const BOUND: Duration = Duration::from_secs(10);

    /// A reclaim deadline that allows several namespace polls.
    const BUDGET: Duration = Duration::from_secs(5);

    /// A reclaim deadline for namespaces that never disappear.
    const SHORT_BUDGET: Duration = Duration::from_secs(1);

    const NAMESPACE: &str = "airlock-ab12cd34-baseline";

    /// A fake Kubernetes API with canned responses and recorded requests.
    struct FakeApi {
        client: Client,
        /// Each request's method and path.
        requests: Arc<Mutex<Vec<String>>>,
        server: JoinHandle<()>,
    }

    /// Maps a request's method, path, and how many earlier requests had the same method and path, to an HTTP status
    /// and JSON body.
    type Respond = fn(&str, &str, usize) -> (u16, String);

    impl FakeApi {
        async fn start(respond: Respond) -> Self {
            install_crypto_provider();
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("the fake API should bind");
            let address = listener.local_addr().expect("the listener should have an address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let server = tokio::spawn(async move {
                let mut seen = HashMap::<String, usize>::new();
                loop {
                    let (mut stream, _) = listener.accept().await.expect("the fake API should accept");
                    let mut request = Vec::new();
                    let mut buf = [0; 1024];
                    let head_end = loop {
                        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                            break end + 4;
                        }
                        let read = stream.read(&mut buf).await.expect("the request should be readable");
                        if read == 0 {
                            break request.len();
                        }
                        request.extend_from_slice(&buf[..read]);
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
                    // Read the body too, so closing the connection doesn't reset it before the client reads the
                    // response.
                    let body_len = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    while request.len() < head_end + body_len {
                        let read = stream.read(&mut buf).await.expect("the body should be readable");
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buf[..read]);
                    }
                    let mut request_line = head.lines().next().unwrap_or_default().split(' ');
                    let method = request_line.next().unwrap_or_default();
                    let path = request_line.next().unwrap_or_default();
                    let path = path.split('?').next().unwrap_or_default();
                    let key = format!("{} {}", method, path);
                    let count = seen.entry(key.clone()).or_default();
                    let (status, body) = respond(method, path, *count);
                    *count += 1;
                    recorded.lock().unwrap().push(key);
                    let response = format!(
                        "HTTP/1.1 {} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n{}",
                        status,
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            });
            let config = kube::Config::new(
                format!("http://{}", address)
                    .parse()
                    .expect("the fake API's address should be a URI"),
            );
            let client = Client::try_from(config).expect("kube should build a client for the fake API");
            Self {
                client,
                requests,
                server,
            }
        }

        /// Registers `names` in a new ledger through the fake's client.
        fn ledger(&self, names: &[&str]) -> ResourceLedger {
            let ledger = ResourceLedger::default();
            for name in names {
                ledger.namespace(self.client.clone(), name.to_string());
            }
            ledger
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for FakeApi {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// A namespace in `phase`, named by the last segment of `path`.
    fn namespace_json(path: &str, phase: &str) -> String {
        let name = path.rsplit('/').next().unwrap_or_default();
        format!(
            r#"{{"apiVersion":"v1","kind":"Namespace","metadata":{{"name":"{}"}},"status":{{"phase":"{}"}}}}"#,
            name, phase
        )
    }

    /// A failure `Status` with `code`, `reason`, and `message`.
    fn status_json(code: u16, reason: &str, message: &str) -> String {
        format!(
            r#"{{"apiVersion":"v1","kind":"Status","metadata":{{}},"status":"Failure","message":"{}","reason":"{}","code":{}}}"#,
            message, reason, code
        )
    }

    fn not_found(path: &str) -> (u16, String) {
        (404, status_json(404, "NotFound", &format!("{} not found", path)))
    }

    /// Accepts every deletion and reports every namespace as terminating.
    fn always_terminating(method: &str, path: &str, _: usize) -> (u16, String) {
        match method {
            "DELETE" | "GET" => (200, namespace_json(path, "Terminating")),
            _ => (500, status_json(500, "InternalError", "unexpected request")),
        }
    }

    async fn reclaim(ledger: &ResourceLedger, budget: Duration) -> (Vec<ReclaimEntry>, Vec<Issue>) {
        tokio::time::timeout(BOUND, ledger.reclaim(Instant::now() + budget))
            .await
            .expect("reclaim should finish within the bound")
    }

    fn entry(resource: &str, result: ReclaimResult) -> ReclaimEntry {
        ReclaimEntry {
            resource: resource.to_string(),
            result,
        }
    }

    #[tokio::test]
    async fn a_directory_that_is_already_gone_counts_as_reclaimed() {
        let parent = tempfile::tempdir().expect("temp dir should be creatable");
        let missing = parent.path().join("missing");
        let ledger = ResourceLedger::default();
        ledger.directory(missing.clone());

        let (reclaim, issues) = reclaim(&ledger, BUDGET).await;

        assert_eq!(
            reclaim,
            [entry(
                &format!("directory:{}", missing.display()),
                ReclaimResult::Reclaimed
            )]
        );
        assert_eq!(issues, []);
    }

    #[tokio::test]
    async fn a_namespace_is_reclaimed_once_it_is_gone() {
        let api = FakeApi::start(|method, path, seen| match (method, seen) {
            ("DELETE", _) => (200, namespace_json(path, "Terminating")),
            ("GET", 0) => (200, namespace_json(path, "Terminating")),
            ("GET", _) => not_found(path),
            _ => (500, status_json(500, "InternalError", "unexpected request")),
        })
        .await;
        let ledger = api.ledger(&[NAMESPACE]);

        let (reclaim, issues) = reclaim(&ledger, BUDGET).await;

        assert_eq!(
            reclaim,
            [entry(&format!("namespace:{}", NAMESPACE), ReclaimResult::Reclaimed)]
        );
        assert_eq!(issues, []);
        let path = format!("/api/v1/namespaces/{}", NAMESPACE);
        assert_eq!(
            api.requests(),
            [
                format!("DELETE {}", path),
                format!("GET {}", path),
                format!("GET {}", path)
            ]
        );
    }

    #[tokio::test]
    async fn a_namespace_still_terminating_at_the_deadline_is_unsettled() {
        let api = FakeApi::start(always_terminating).await;
        let ledger = api.ledger(&[NAMESPACE]);

        let (reclaim, issues) = reclaim(&ledger, SHORT_BUDGET).await;

        let resource = format!("namespace:{}", NAMESPACE);
        assert_eq!(reclaim, [entry(&resource, ReclaimResult::Unsettled)]);
        assert_eq!(
            issues,
            [Issue {
                kind: IssueKind::ResourceUnsettled,
                message: format!(
                    "{}: still terminating when the {:?} reclaim budget expired",
                    resource, RECLAIM_TIME
                ),
            }]
        );
    }

    #[tokio::test]
    async fn a_namespace_that_is_already_gone_counts_as_reclaimed() {
        let api = FakeApi::start(|method, path, _| match method {
            "DELETE" => not_found(path),
            _ => (500, status_json(500, "InternalError", "unexpected request")),
        })
        .await;
        let ledger = api.ledger(&[NAMESPACE]);

        let (reclaim, issues) = reclaim(&ledger, BUDGET).await;

        assert_eq!(
            reclaim,
            [entry(&format!("namespace:{}", NAMESPACE), ReclaimResult::Reclaimed)]
        );
        assert_eq!(issues, []);
    }

    #[tokio::test]
    async fn a_failed_namespace_delete_is_leaked() {
        let api = FakeApi::start(|_, _, _| (500, status_json(500, "InternalError", "boom"))).await;
        let ledger = api.ledger(&[NAMESPACE]);

        let (reclaim, issues) = reclaim(&ledger, BUDGET).await;

        assert_eq!(
            reclaim,
            [entry(&format!("namespace:{}", NAMESPACE), ReclaimResult::Leaked)]
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].kind, IssueKind::ResourceLeaked);
        assert!(
            issues[0].message.starts_with(&format!(
                "namespace:{}: Failed to delete namespace '{}': ",
                NAMESPACE, NAMESPACE
            )) && issues[0].message.contains("boom"),
            "{}",
            issues[0].message
        );
        assert_eq!(api.requests(), [format!("DELETE /api/v1/namespaces/{}", NAMESPACE)]);
    }

    #[tokio::test]
    async fn every_namespace_is_deleted_before_any_wait() {
        let api = FakeApi::start(always_terminating).await;
        let names = [
            "airlock-ab12cd34-baseline",
            "airlock-ab12cd34-comparison",
            "airlock-ab12cd34-millstone",
        ];
        let ledger = api.ledger(&names);

        let (reclaim, _) = reclaim(&ledger, SHORT_BUDGET).await;

        let newest_first: Vec<_> = names.iter().rev().collect();
        assert_eq!(
            reclaim,
            newest_first
                .iter()
                .map(|name| entry(&format!("namespace:{}", name), ReclaimResult::Unsettled))
                .collect::<Vec<_>>()
        );
        let requests = api.requests();
        assert_eq!(
            requests[..3],
            newest_first
                .iter()
                .map(|name| format!("DELETE /api/v1/namespaces/{}", name))
                .collect::<Vec<_>>()
        );
        assert!(
            requests[3..].iter().all(|request| request.starts_with("GET ")),
            "{requests:?}"
        );
    }

    #[tokio::test]
    async fn a_creation_cut_short_by_the_task_stop_is_unsettled() {
        let api = FakeApi::start(|_, path, _| not_found(path)).await;
        let ledger = api.ledger(&[NAMESPACE]);
        let tasks = CaseTasks::new(&CancellationToken::new());
        let entered = Arc::new(Notify::new());
        let (task_ledger, task_entered, stop) = (ledger.clone(), entered.clone(), tasks.stop.clone());
        tasks.spawn(async move {
            let request = async {
                task_entered.notify_one();
                std::future::pending::<()>().await
            };
            task_ledger.create(NAMESPACE, &stop, request).await
        });
        tokio::time::timeout(BOUND, entered.notified())
            .await
            .expect("the creation should start within the bound");

        let running = tasks.stop_all(Instant::now() + TASK_STOP_TIME).await;
        let (reclaim, issues) = reclaim(&ledger, BUDGET).await;

        assert_eq!(running, 0);
        let resource = format!("namespace:{}", NAMESPACE);
        assert_eq!(reclaim, [entry(&resource, ReclaimResult::Unsettled)]);
        assert_eq!(
            issues,
            [Issue {
                kind: IssueKind::ResourceUnsettled,
                message: format!(
                    "{}: a creation request was cut short, so something created after removal may remain",
                    resource
                ),
            }]
        );
    }
}
