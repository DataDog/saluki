//! Replay session control surface for the DogStatsD source.
//!
//! The ADP process owns only the active replay session and its captured tagger snapshot. A separate replay client is
//! responsible for reading capture files and sending replay packets into the DogStatsD UDS listener.

use std::sync::{Arc, Mutex};

use datadog_protos::agent::TaggerState;
use rand::RngExt as _;
use saluki_error::{generic_error, GenericError};

use super::{CapturedTaggerHandle, CapturedTaggerStore};

const UNAVAILABLE_REPLAY_CONTROL_ERROR: &str =
    "DogStatsD replay control is unavailable because the source is not running.";

const REPLAY_ALREADY_IN_PROGRESS_ERROR: &str = "DogStatsD replay already in progress.";

/// Default number of times to replay a capture file when the CLI omits an explicit count.
pub const DEFAULT_REPLAY_LOOPS: u32 = 1;

/// Active DogStatsD replay session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySession {
    /// Opaque identifier the replay client must use to finish the session.
    pub id: String,
}

/// Shared control handle for DogStatsD replay sessions.
///
/// Created before the source is built and bound to the live captured tagger handle during source construction. The
/// HTTP API handler holds a clone of this handle so a replay client can acquire and release the single active session.
#[derive(Clone, Default)]
pub struct DogStatsDReplayControl {
    inner: Arc<Mutex<ReplayControlState>>,
}

#[derive(Default)]
struct ReplayControlState {
    captured_tagger: Option<CapturedTaggerHandle>,
    active_session_id: Option<String>,
}

impl DogStatsDReplayControl {
    /// Creates a new, unbound replay control handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the control handle to the captured tagger store used by the running DogStatsD source.
    pub(crate) fn bind(&self, captured_tagger: CapturedTaggerHandle) {
        let mut state = self.inner.lock().expect("replay control mutex poisoned");
        state.captured_tagger = Some(captured_tagger);
    }

    /// Starts a replay session and sets the captured tagger state for replay-origin resolution.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source hasn't been built and bound to this control handle yet, or if another
    /// replay session is already active.
    pub fn start_session(&self, tagger_state: Option<TaggerState>) -> Result<ReplaySession, GenericError> {
        let mut state = self.inner.lock().expect("replay control mutex poisoned");
        let captured_tagger = state
            .captured_tagger
            .clone()
            .ok_or_else(|| generic_error!("{}", UNAVAILABLE_REPLAY_CONTROL_ERROR))?;

        if state.active_session_id.is_some() {
            return Err(generic_error!("{}", REPLAY_ALREADY_IN_PROGRESS_ERROR));
        }

        let session = ReplaySession {
            id: new_replay_session_id(),
        };

        let captured_store = tagger_state.map(CapturedTaggerStore::from_tagger_state);
        captured_tagger.set_current(captured_store);
        state.active_session_id = Some(session.id.clone());

        Ok(session)
    }

    /// Finishes a replay session and removes the captured tagger state it owns.
    ///
    /// # Errors
    ///
    /// Returns an error if the DogStatsD source hasn't been built and bound to this control handle yet, or if the given
    /// session does not own the active replay.
    pub fn finish_session(&self, session_id: &str) -> Result<(), GenericError> {
        let mut state = self.inner.lock().expect("replay control mutex poisoned");
        let captured_tagger = state
            .captured_tagger
            .clone()
            .ok_or_else(|| generic_error!("{}", UNAVAILABLE_REPLAY_CONTROL_ERROR))?;

        match state.active_session_id.as_deref() {
            Some(active_session_id) if active_session_id == session_id => {
                captured_tagger.set_current(None);
                state.active_session_id = None;
                Ok(())
            }
            Some(active_session_id) => Err(generic_error!(
                "DogStatsD replay session '{}' does not own active replay session '{}'.",
                session_id,
                active_session_id
            )),
            None => Ok(()),
        }
    }
}

fn new_replay_session_id() -> String {
    format!("{:032x}", rand::rng().random::<u128>())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use datadog_protos::agent::{Entity as ProtoEntity, TaggerState};
    use saluki_context::origin::OriginTagCardinality;

    use super::*;

    fn make_state() -> TaggerState {
        let entity = ProtoEntity {
            low_cardinality_tags: vec!["env:prod".into()],
            ..Default::default()
        };

        let mut entities = HashMap::new();
        entities.insert("container_id://container-xyz".to_string(), entity);

        let mut pid_map = HashMap::new();
        pid_map.insert(99, "container_id://container-xyz".to_string());

        TaggerState {
            state: entities,
            pid_map,
            duration: 0,
        }
    }

    #[test]
    fn control_requires_bound_store() {
        let control = DogStatsDReplayControl::new();
        let err = control
            .start_session(Some(make_state()))
            .expect_err("unbound control should fail");
        assert_eq!(err.to_string(), UNAVAILABLE_REPLAY_CONTROL_ERROR);
    }

    #[test]
    fn session_start_and_finish_update_bound_store() {
        let captured_tagger = CapturedTaggerHandle::new();
        let control = DogStatsDReplayControl::new();
        control.bind(captured_tagger.clone());

        let session = control.start_session(Some(make_state())).expect("session should start");
        let store = captured_tagger.current().expect("state should be set");
        assert!(store.lookup(99, OriginTagCardinality::Low).is_some());

        control
            .finish_session(&session.id)
            .expect("matching session should finish");
        assert!(captured_tagger.current().is_none());
    }

    #[test]
    fn session_rejects_concurrent_start() {
        let captured_tagger = CapturedTaggerHandle::new();
        let control = DogStatsDReplayControl::new();
        control.bind(captured_tagger);

        let _session = control
            .start_session(Some(make_state()))
            .expect("first session should start");
        let err = control
            .start_session(Some(make_state()))
            .expect_err("second session should fail");
        assert!(err.to_string().contains("replay already in progress"));
    }

    #[test]
    fn finish_session_rejects_non_owner() {
        let captured_tagger = CapturedTaggerHandle::new();
        let control = DogStatsDReplayControl::new();
        control.bind(captured_tagger.clone());

        let session = control.start_session(Some(make_state())).expect("session should start");
        let err = control
            .finish_session("wrong-session")
            .expect_err("non-owner should fail");
        assert!(err.to_string().contains("does not own"));

        assert!(captured_tagger.current().is_some());
        control
            .finish_session(&session.id)
            .expect("matching session should finish");
        assert!(captured_tagger.current().is_none());
    }
}
