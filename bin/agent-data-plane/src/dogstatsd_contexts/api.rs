use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::try_join_all;
use http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use saluki_api::{
    extract::State,
    response::{IntoResponse as _, Response},
    routing::{post, Router},
    APIHandler, Json,
};
use saluki_components::transforms::{AggregateContextSnapshotEntry, AggregateContextSnapshotHandle};
use saluki_error::{generic_error, GenericError};
use subtle::ConstantTimeEq as _;
use tokio::sync::Mutex;

use super::{publish_context_dump, CONTEXT_DUMP_ROUTE};

// The Agent has no owner-request timeout. ADP uses a finite bound so a stopped topology cannot hold an HTTP request
// indefinitely; 30 seconds is several orders of magnitude above the measured one-million-context snapshot time.
const PRODUCTION_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);
const AUTHENTICATION_REQUIRED: &str = "Authentication required.";
const SNAPSHOT_UNAVAILABLE: &str =
    "DogStatsD context snapshot is unavailable; retry after the aggregate owner is running.";
const SNAPSHOT_TIMED_OUT: &str = "Timed out waiting for the DogStatsD context snapshot; retry the request.";
const PUBLICATION_FAILED: &str =
    "Failed to publish DogStatsD context dump; check the configured run path and permissions.";
const PUBLICATION_TASK_FAILED: &str = "DogStatsD context dump publication did not complete; retry the request.";
const NON_UTF8_RESULT_PATH: &str =
    "The completed DogStatsD context dump path is not valid UTF-8; configure a UTF-8 run path.";

#[derive(Clone)]
struct AuthenticationSecret(Arc<[u8]>);

impl AuthenticationSecret {
    fn new(token: &[u8]) -> Result<Self, GenericError> {
        if token.is_empty() {
            return Err(generic_error!("Agent authentication token must not be empty."));
        }
        if !is_valid_bearer_token(token) {
            return Err(generic_error!(
                "Agent authentication token cannot be represented as an HTTP bearer credential."
            ));
        }
        Ok(Self(Arc::from(token)))
    }

    fn authenticates(&self, headers: &HeaderMap) -> bool {
        let mut authorization_values = headers.get_all(AUTHORIZATION).iter();
        let Some(authorization) = authorization_values.next() else {
            return false;
        };
        if authorization_values.next().is_some() {
            return false;
        }

        let value = authorization.as_bytes();
        let Some(separator) = value.iter().position(|byte| *byte == b' ') else {
            return false;
        };
        if !value[..separator].eq_ignore_ascii_case(b"bearer") {
            return false;
        }
        let supplied = &value[separator..];
        let Some(first_credential_byte) = supplied.iter().position(|byte| *byte != b' ') else {
            return false;
        };
        let supplied = &supplied[first_credential_byte..];
        if supplied.len() != self.0.len() {
            return false;
        }

        bool::from(supplied.ct_eq(self.0.as_ref()))
    }
}

fn is_valid_bearer_token(token: &[u8]) -> bool {
    let core_length = token
        .iter()
        .rposition(|byte| *byte != b'=')
        .map_or(0, |index| index + 1);
    core_length > 0
        && token[..core_length]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/'))
        && token[core_length..].iter().all(|byte| *byte == b'=')
}

#[derive(Clone)]
struct ContextSnapshotCoordinator {
    handles: Arc<[AggregateContextSnapshotHandle]>,
    timeout: Duration,
}

impl ContextSnapshotCoordinator {
    fn new(handles: Vec<AggregateContextSnapshotHandle>, timeout: Duration) -> Self {
        Self {
            handles: handles.into(),
            timeout,
        }
    }

    async fn snapshot(&self) -> Result<Vec<AggregateContextSnapshotEntry>, SnapshotError> {
        if self.handles.is_empty() {
            return Err(SnapshotError::SnapshotUnavailable);
        }

        let requests = self.handles.iter().map(AggregateContextSnapshotHandle::snapshot);
        let owner_snapshots = tokio::time::timeout(self.timeout, try_join_all(requests))
            .await
            .map_err(|_| SnapshotError::SnapshotTimedOut)?
            .map_err(|_| SnapshotError::SnapshotUnavailable)?;
        let total_len = owner_snapshots.iter().map(Vec::len).sum::<usize>();
        let mut owner_snapshots = owner_snapshots.into_iter();
        let mut combined = owner_snapshots
            .next()
            .expect("a non-empty snapshot handle set always returns at least one owner snapshot");
        combined.reserve(total_len - combined.len());
        for snapshot in owner_snapshots {
            combined.extend(snapshot);
        }
        Ok(combined)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotError {
    SnapshotUnavailable,
    SnapshotTimedOut,
}

trait ContextDumpPublisher: Send + Sync {
    fn publish(&self, run_path: PathBuf, snapshot: Vec<AggregateContextSnapshotEntry>)
        -> Result<PathBuf, GenericError>;
}

struct FileSystemContextDumpPublisher;

impl ContextDumpPublisher for FileSystemContextDumpPublisher {
    fn publish(
        &self, run_path: PathBuf, snapshot: Vec<AggregateContextSnapshotEntry>,
    ) -> Result<PathBuf, GenericError> {
        publish_context_dump(&run_path, &snapshot)
    }
}

#[derive(Clone)]
pub(crate) struct DogStatsDContextDumpAPIState {
    auth_secret: AuthenticationSecret,
    coordinator: ContextSnapshotCoordinator,
    run_path: PathBuf,
    publication_lock: Arc<Mutex<()>>,
    publisher: Arc<dyn ContextDumpPublisher>,
}

#[derive(Clone)]
pub(crate) struct DogStatsDContextDumpAPIHandler {
    state: DogStatsDContextDumpAPIState,
}

impl DogStatsDContextDumpAPIHandler {
    pub(crate) fn new(
        auth_token: impl AsRef<[u8]>, handles: Vec<AggregateContextSnapshotHandle>, run_path: impl Into<PathBuf>,
    ) -> Result<Self, GenericError> {
        Self::new_with_services(
            auth_token.as_ref(),
            handles,
            run_path.into(),
            PRODUCTION_SNAPSHOT_TIMEOUT,
            Arc::new(FileSystemContextDumpPublisher),
        )
    }

    #[cfg(test)]
    fn new_for_test(
        auth_token: impl AsRef<[u8]>, handles: Vec<AggregateContextSnapshotHandle>, run_path: PathBuf,
        snapshot_timeout: Duration, publisher: Arc<dyn ContextDumpPublisher>,
    ) -> Result<Self, GenericError> {
        Self::new_with_services(auth_token.as_ref(), handles, run_path, snapshot_timeout, publisher)
    }

    fn new_with_services(
        auth_token: &[u8], handles: Vec<AggregateContextSnapshotHandle>, run_path: PathBuf, snapshot_timeout: Duration,
        publisher: Arc<dyn ContextDumpPublisher>,
    ) -> Result<Self, GenericError> {
        let auth_secret = AuthenticationSecret::new(auth_token)?;
        Ok(Self {
            state: DogStatsDContextDumpAPIState {
                auth_secret,
                coordinator: ContextSnapshotCoordinator::new(handles, snapshot_timeout),
                run_path,
                publication_lock: Arc::new(Mutex::new(())),
                publisher,
            },
        })
    }

    async fn dump_handler(State(state): State<DogStatsDContextDumpAPIState>, headers: HeaderMap) -> Response {
        if !state.auth_secret.authenticates(&headers) {
            return (StatusCode::UNAUTHORIZED, AUTHENTICATION_REQUIRED).into_response();
        }

        let publication_guard = state.publication_lock.clone().lock_owned().await;
        let snapshot = match state.coordinator.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(SnapshotError::SnapshotUnavailable) => {
                return (StatusCode::SERVICE_UNAVAILABLE, SNAPSHOT_UNAVAILABLE).into_response();
            }
            Err(SnapshotError::SnapshotTimedOut) => {
                return (StatusCode::GATEWAY_TIMEOUT, SNAPSHOT_TIMED_OUT).into_response();
            }
        };

        let run_path = state.run_path.clone();
        let publisher = state.publisher.clone();
        let publication = tokio::task::spawn_blocking(move || {
            let _publication_guard = publication_guard;
            publisher.publish(run_path, snapshot)
        })
        .await;

        match publication {
            Ok(Ok(path)) => match path.into_os_string().into_string() {
                Ok(path) => Json(path).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, NON_UTF8_RESULT_PATH).into_response(),
            },
            Ok(Err(_)) => (StatusCode::INTERNAL_SERVER_ERROR, PUBLICATION_FAILED).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, PUBLICATION_TASK_FAILED).into_response(),
        }
    }
}

impl APIHandler for DogStatsDContextDumpAPIHandler {
    type State = DogStatsDContextDumpAPIState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route(CONTEXT_DUMP_ROUTE, post(Self::dump_handler))
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
