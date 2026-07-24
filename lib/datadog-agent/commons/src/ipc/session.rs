//! IPC session management.

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use saluki_error::{ErrorContext as _, GenericError};
use tokio::sync::Notify;
use tonic::metadata::{Ascii, MetadataValue};

/// Remote agent session ID.
///
/// Session IDs are acquired when registering as a remote agent with the Datadog Agent. This session ID must be provided
/// when refreshing a remote agent's registration, as well as when streaming configuration updates. To ease passing this
/// in the necessary format, and with the required validation, this struct provides a type-safe to carry around the
/// session ID compared to a raw string.
#[derive(Debug)]
pub struct SessionId(MetadataValue<Ascii>);

impl SessionId {
    /// Creates a new `SessionId` from the given string.
    ///
    /// # Errors
    ///
    /// If the given string isn't valid ASCII, an error is returned.
    pub fn new(session_id: &str) -> Result<Self, GenericError> {
        MetadataValue::try_from(session_id)
            .map(Self)
            .error_context("Session ID is not valid ASCII")
    }

    /// Returns a reference to the string representation of the session ID.
    pub fn as_str(&self) -> &str {
        self.0
            .to_str()
            .expect("session ID is ensured to be valid ASCII on creation")
    }

    /// Returns the session ID as a metadata header value.
    pub fn to_grpc_header_value(&self) -> MetadataValue<Ascii> {
        self.0.clone()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Default)]
struct SessionIdHandleInner {
    session_id: Mutex<Option<SessionId>>,
    change_notify: Notify,
}

/// A handle for a dynamically updated session ID.
///
/// This handle allows sharing a session ID across multiple components while allowing it to be centrally updated in the
/// case of refresh/re-registration.
#[derive(Clone, Debug)]
pub struct SessionIdHandle {
    inner: Arc<SessionIdHandleInner>,
}

impl SessionIdHandle {
    /// Creates a new `SessionIdHandle` with no session ID.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(SessionIdHandleInner::default()),
        }
    }

    /// Updates the current session ID to the given value.
    pub fn update(&self, new_session_id: Option<SessionId>) {
        if let Ok(mut session_id) = self.inner.session_id.lock() {
            *session_id = new_session_id;
            self.inner.change_notify.notify_waiters();
        }
    }

    /// Gets the current session ID.
    pub fn get(&self) -> Option<SessionId> {
        self.inner
            .session_id
            .lock()
            .ok()
            .and_then(|s| (*s).as_ref().map(|session_id| SessionId(session_id.0.clone())))
    }

    /// Waits until the session ID is set to a non-empty value and returns it.
    pub async fn wait_for_update(&self) -> SessionId {
        loop {
            let updated = self.inner.change_notify.notified();
            if let Some(session_id) = self.get() {
                return session_id;
            }

            updated.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::{SessionId, SessionIdHandle};

    // Bound every await so that a regression reintroducing the lost-wakeup race fails fast instead of
    // hanging the test suite forever.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn wait_for_update_returns_immediately_when_value_already_set() {
        // When a value is already present, `wait_for_update` must return it on the first loop iteration
        // without ever awaiting a change notification.
        let handle = SessionIdHandle::empty();
        handle.update(Some(SessionId::new("already-set").unwrap()));

        let session_id = timeout(TEST_TIMEOUT, handle.wait_for_update())
            .await
            .expect("should return without waiting on a notification");
        assert_eq!(session_id.as_str(), "already-set");
    }

    #[tokio::test]
    async fn wait_for_update_wakes_on_concurrent_update() {
        // This exercises the documented race-avoidance idiom: `wait_for_update` registers the change
        // notification *before* it reads the current value, so an update applied while a caller is parked
        // is delivered rather than lost.
        let handle = SessionIdHandle::empty();
        let waiter = handle.clone();
        let waiter_task = tokio::spawn(async move { waiter.wait_for_update().await });

        // Let the spawned task advance to (and park on) its notification await while the value is still
        // empty, then apply the update.
        tokio::task::yield_now().await;
        handle.update(Some(SessionId::new("session-123").unwrap()));

        let session_id = timeout(TEST_TIMEOUT, waiter_task)
            .await
            .expect("waiter should wake within the timeout")
            .expect("waiter task should not panic");
        assert_eq!(session_id.as_str(), "session-123");
    }

    #[tokio::test]
    async fn wait_for_update_ignores_clears_and_returns_first_non_empty_value() {
        // `update(None)` still notifies waiters, but `wait_for_update` documents that it waits for a
        // *non-empty* value, so a clear must not cause it to return early — it must keep waiting until a
        // real session ID arrives.
        let handle = SessionIdHandle::empty();
        let waiter = handle.clone();
        let waiter_task = tokio::spawn(async move { waiter.wait_for_update().await });

        tokio::task::yield_now().await;
        // Notifies waiters, but leaves the value empty: the waiter must loop and keep waiting.
        handle.update(None);
        tokio::task::yield_now().await;
        handle.update(Some(SessionId::new("finally").unwrap()));

        let session_id = timeout(TEST_TIMEOUT, waiter_task)
            .await
            .expect("waiter should wake within the timeout")
            .expect("waiter task should not panic");
        assert_eq!(session_id.as_str(), "finally");
    }
}
