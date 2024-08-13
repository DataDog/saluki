use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler, StatusCode,
};
use serde::{ser::SerializeMap as _, Serialize};
use stringtheory::MetaString;

use crate::ComponentHealth;

/// State used for the healthy registry API handler.
#[derive(Clone)]
pub struct HealthRegistryState {
    inner: Arc<Mutex<HashMap<MetaString, ComponentHealth>>>,
}

impl HealthRegistryState {
    fn get_response(&self, check_ready: bool) -> (StatusCode, String) {
        // We specifically do this all here because we want to ensure the state is locked for both determining if the
        // ready/live state is passing/failing, as well as serializing that same state data to JSON, to avoid
        // inconsistencies between the two.

        let inner = self.inner.lock().unwrap();

        let passing = inner
            .values()
            .all(|health| if check_ready { health.ready } else { health.live });
        let status = if passing {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };

        let rendered = serde_json::to_string(&*inner).unwrap();

        (status, rendered)
    }

    fn get_ready_response(&self) -> (StatusCode, String) {
        self.get_response(true)
    }

    fn get_live_response(&self) -> (StatusCode, String) {
        self.get_response(false)
    }
}

impl Serialize for HealthRegistryState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let inner = self.inner.lock().unwrap();

        let mut map = serializer.serialize_map(Some(inner.len()))?;
        for (name, health) in inner.iter() {
            map.serialize_entry(name, health)?;
        }
        map.end()
    }
}

/// An API handler for reporting the health of all components.
///
/// This handler exposes two main routes -- `/ready` and `/live` -- which return the overall readiness and liveness of
/// all registered components, respectively. Each route will return a successful response (200 OK) if all components are
/// ready/live, or a failure response (503 Service Unavailable) if any (or all) of the components are not ready/live,
/// respectively.
///
/// In both cases, the response body will be a JSON object with all registered components, each with their individual
/// readiness and liveness status, as well as the response latency (in seconds) for the component to respond to the
/// latest liveness probe.
pub struct HealthAPIHandler {
    state: HealthRegistryState,
}

impl HealthAPIHandler {
    pub(crate) fn from_state(state: Arc<Mutex<HashMap<MetaString, ComponentHealth>>>) -> Self {
        Self {
            state: HealthRegistryState { inner: state },
        }
    }

    async fn ready_handler(State(state): State<HealthRegistryState>) -> impl IntoResponse {
        state.get_ready_response()
    }

    async fn live_handler(State(state): State<HealthRegistryState>) -> impl IntoResponse {
        state.get_live_response()
    }
}

impl APIHandler for HealthAPIHandler {
    type State = HealthRegistryState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/health/ready", get(Self::ready_handler))
            .route("/health/live", get(Self::live_handler))
    }
}
