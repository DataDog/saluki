//! HTTP API handler for DogStatsD replay sessions.
//!
//! A replay client starts a session before sending replay packets and finishes the same session when replay ends. The
//! ADP process keeps the captured tagger snapshot only for the active session.

use axum::body::Bytes;
use datadog_protos::agent::TaggerState;
use prost::Message as _;
use saluki_api::{
    extract::{Path, State},
    routing::{delete, post, Router},
    APIHandler, Json, StatusCode,
};
use serde::Serialize;

use super::DogStatsDReplayControl;

/// API handler for the DogStatsD replay session control surface.
#[derive(Clone)]
pub struct DogStatsDReplayAPIHandler {
    replay_control: DogStatsDReplayControl,
}

impl DogStatsDReplayAPIHandler {
    /// Creates a new handler bound to the given replay control.
    pub fn new(replay_control: DogStatsDReplayControl) -> Self {
        Self { replay_control }
    }

    async fn start_session_handler(
        State(replay_control): State<DogStatsDReplayControl>, body: Bytes,
    ) -> Result<Json<ReplaySessionResponseBody>, (StatusCode, String)> {
        let state = if body.is_empty() {
            None
        } else {
            Some(
                TaggerState::decode(body)
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid tagger state: {}", e)))?,
            )
        };

        let session = replay_control.start_session(state).map_err(map_replay_control_error)?;

        Ok(Json(ReplaySessionResponseBody { session_id: session.id }))
    }

    async fn finish_session_handler(
        State(replay_control): State<DogStatsDReplayControl>, Path(session_id): Path<String>,
    ) -> Result<StatusCode, (StatusCode, String)> {
        replay_control
            .finish_session(&session_id)
            .map_err(map_replay_control_error)?;

        Ok(StatusCode::NO_CONTENT)
    }
}

/// Response body for `POST /dogstatsd/replay/session`.
#[derive(Debug, Serialize)]
pub struct ReplaySessionResponseBody {
    /// Opaque session identifier the replay client must use when finishing the replay.
    pub session_id: String,
}

fn map_replay_control_error(err: saluki_error::GenericError) -> (StatusCode, String) {
    let message = err.to_string();
    let status = if message.contains("replay already in progress") || message.contains("does not own") {
        StatusCode::CONFLICT
    } else {
        StatusCode::PRECONDITION_FAILED
    };
    (status, message)
}

impl APIHandler for DogStatsDReplayAPIHandler {
    type State = DogStatsDReplayControl;

    fn generate_initial_state(&self) -> Self::State {
        self.replay_control.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/dogstatsd/replay/session", post(Self::start_session_handler))
            .route(
                "/dogstatsd/replay/session/{session_id}",
                delete(Self::finish_session_handler),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use datadog_protos::agent::{Entity as ProtoEntity, TaggerState};
    use prost::Message as _;
    use saluki_context::origin::OriginTagCardinality;

    use super::*;
    use crate::sources::dogstatsd::replay::CapturedTaggerHandle;

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

    #[tokio::test]
    async fn start_session_rejects_invalid_protobuf() {
        let control = DogStatsDReplayControl::new();
        let err = DogStatsDReplayAPIHandler::start_session_handler(State(control), Bytes::from_static(b"bad"))
            .await
            .expect_err("invalid protobuf should fail");

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn start_and_finish_session_update_control() {
        let captured_tagger = CapturedTaggerHandle::new();
        let control = DogStatsDReplayControl::new();
        control.bind(captured_tagger.clone());

        let body = Bytes::from(make_state().encode_to_vec());
        let Json(session) = DogStatsDReplayAPIHandler::start_session_handler(State(control.clone()), body)
            .await
            .expect("session should start");
        assert!(!session.session_id.is_empty());

        let store = captured_tagger.current().expect("state should be set");
        assert!(store.lookup(99, OriginTagCardinality::Low).is_some());

        let finish_status = DogStatsDReplayAPIHandler::finish_session_handler(State(control), Path(session.session_id))
            .await
            .expect("finish should succeed");
        assert_eq!(finish_status, StatusCode::NO_CONTENT);
        assert!(captured_tagger.current().is_none());
    }

    #[tokio::test]
    async fn start_session_rejects_concurrent_session() {
        let captured_tagger = CapturedTaggerHandle::new();
        let control = DogStatsDReplayControl::new();
        control.bind(captured_tagger);

        let body = Bytes::from(make_state().encode_to_vec());
        let _ = DogStatsDReplayAPIHandler::start_session_handler(State(control.clone()), body)
            .await
            .expect("first session should start");

        let err =
            DogStatsDReplayAPIHandler::start_session_handler(State(control), Bytes::from(make_state().encode_to_vec()))
                .await
                .expect_err("second session should fail");
        assert_eq!(err.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn finish_session_rejects_non_owner() {
        let captured_tagger = CapturedTaggerHandle::new();
        let control = DogStatsDReplayControl::new();
        control.bind(captured_tagger.clone());

        let body = Bytes::from(make_state().encode_to_vec());
        let Json(session) = DogStatsDReplayAPIHandler::start_session_handler(State(control.clone()), body)
            .await
            .expect("session should start");

        let err = DogStatsDReplayAPIHandler::finish_session_handler(State(control.clone()), Path("wrong".to_string()))
            .await
            .expect_err("non-owner should fail");
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(captured_tagger.current().is_some());

        DogStatsDReplayAPIHandler::finish_session_handler(State(control), Path(session.session_id))
            .await
            .expect("owner should finish");
        assert!(captured_tagger.current().is_none());
    }
}
