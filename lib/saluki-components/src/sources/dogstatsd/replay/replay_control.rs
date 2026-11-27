//! Replay session control surface for the DogStatsD source.
//!
//! The ADP process owns only the active replay session and its captured tagger snapshot. A separate replay client is
//! responsible for reading capture files and sending replay packets into the DogStatsD UDS listener.

use std::sync::{Arc, Mutex};

use datadog_protos::agent::TaggerState;
use rand::RngExt as _;
use snafu::Snafu;

use super::{CapturedTaggerHandle, CapturedTaggerStore};

/// Error returned by [`DogStatsDReplayControl`] operations.
#[derive(Debug, Eq, PartialEq, Snafu)]
pub enum ReplayControlError {
    /// Another replay session is already active.
    #[snafu(display("DogStatsD replay already in progress."))]
    AlreadyInProgress,

    /// The given session doesn't own the active replay session.
    #[snafu(display(
        "DogStatsD replay session '{}' does not own active replay session '{}'.",
        session_id,
        active_session_id
    ))]
    NotOwner {
        /// Session that tried to finish the replay.
        session_id: String,

        /// Session that owns the active replay.
        active_session_id: String,
    },
}

/// Default number of times to replay a capture file when the CLI omits an explicit count.
pub const DEFAULT_REPLAY_LOOPS: u32 = 1;

/// Active DogStatsD replay session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySession {
    /// Opaque identifier the replay client must use to finish the session.
    pub id: String,
}

/// Control handle for DogStatsD replay sessions.
///
/// The caller creates this handle, passes it to the source configuration, and keeps a clone to serve the replay API,
/// through which a replay client acquires and releases the single active session. The source resolves the origin of
/// replayed traffic from the captured tagger state that the active session sets.
#[derive(Clone, Default)]
pub struct DogStatsDReplayControl {
    captured_tagger: CapturedTaggerHandle,
    active_session_id: Arc<Mutex<Option<String>>>,
}

impl DogStatsDReplayControl {
    /// Creates a replay control handle with no active session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the captured tagger handle that replay sessions write to.
    pub(crate) fn captured_tagger(&self) -> &CapturedTaggerHandle {
        &self.captured_tagger
    }

    /// Starts a replay session and sets the captured tagger state for replay-origin resolution.
    ///
    /// # Errors
    ///
    /// Returns an error if another replay session is already active.
    pub fn start_session(&self, tagger_state: Option<TaggerState>) -> Result<ReplaySession, ReplayControlError> {
        let mut active_session_id = self.active_session_id.lock().expect("replay control mutex poisoned");
        if active_session_id.is_some() {
            return Err(ReplayControlError::AlreadyInProgress);
        }

        let session = ReplaySession {
            id: new_replay_session_id(),
        };

        let captured_store = tagger_state.map(CapturedTaggerStore::from_tagger_state);
        self.captured_tagger.set_current(captured_store);
        *active_session_id = Some(session.id.clone());

        Ok(session)
    }

    /// Finishes a replay session and removes the captured tagger state it owns.
    ///
    /// # Errors
    ///
    /// Returns an error if the given session does not own the active replay.
    pub fn finish_session(&self, session_id: &str) -> Result<(), ReplayControlError> {
        let mut active_session_id = self.active_session_id.lock().expect("replay control mutex poisoned");
        match active_session_id.as_deref() {
            Some(active) if active == session_id => {
                self.captured_tagger.set_current(None);
                *active_session_id = None;
                Ok(())
            }
            Some(active) => Err(ReplayControlError::NotOwner {
                session_id: session_id.to_string(),
                active_session_id: active.to_string(),
            }),
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
    use saluki_core::data_model::origin::OriginTagCardinality;

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
    fn session_start_and_finish_update_the_captured_tagger() {
        let control = DogStatsDReplayControl::new();
        let captured_tagger = control.captured_tagger();

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
        let control = DogStatsDReplayControl::new();

        let _session = control
            .start_session(Some(make_state()))
            .expect("first session should start");
        let err = control
            .start_session(Some(make_state()))
            .expect_err("second session should fail");
        assert_eq!(err, ReplayControlError::AlreadyInProgress);
        assert_eq!(err.to_string(), "DogStatsD replay already in progress.");
    }

    #[test]
    fn finish_session_rejects_non_owner() {
        let control = DogStatsDReplayControl::new();

        let session = control.start_session(Some(make_state())).expect("session should start");
        let err = control
            .finish_session("wrong-session")
            .expect_err("non-owner should fail");
        assert_eq!(
            err,
            ReplayControlError::NotOwner {
                session_id: "wrong-session".to_string(),
                active_session_id: session.id.clone(),
            }
        );
        assert_eq!(
            err.to_string(),
            format!(
                "DogStatsD replay session 'wrong-session' does not own active replay session '{}'.",
                session.id
            )
        );

        assert!(control.captured_tagger().current().is_some());
        control
            .finish_session(&session.id)
            .expect("matching session should finish");
        assert!(control.captured_tagger().current().is_none());
    }
}
