use std::{
    fmt,
    sync::{Arc, Mutex},
};

use saluki_error::{ErrorContext as _, GenericError};
use tokio::sync::Notify;
use tonic::metadata::{Ascii, MetadataValue};

/// A session ID for a register remote agent.
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
    /// If the given string is not valid ASCII, an error is returned.
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

/// A handle for a dynamically-updated session ID.
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
