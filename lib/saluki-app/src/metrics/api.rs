use std::time::Duration;

use metrics::Level;
use saluki_api::{
    extract::{Query, State},
    response::IntoResponse,
    routing::{post, Router},
    APIHandler, StatusCode,
};
use saluki_common::task::spawn_traced_named;
use saluki_core::observability::metrics::FilterHandle;
use serde::Deserialize;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::info;

const MAXIMUM_OVERRIDE_LENGTH_SECS: u64 = 60 * 60;

#[derive(Deserialize)]
struct OverrideQueryParams {
    time_secs: u64,
}

/// State used for the metrics API handler.
#[derive(Clone)]
pub struct MetricsHandlerState {
    override_tx: mpsc::Sender<Option<(Duration, Level)>>,
}

/// An API handler for updating metric filtering directives at runtime.
///
/// This handler exposes two main routes -- `/metrics/override` and `/metrics/reset` -- which allow for
/// overriding the default metric filtering directives (configured at startup) at runtime, and then resetting them once
/// the override is no longer needed.
///
/// As this has the potential for incredibly high metrics cardinality at runtime, the override is set with a specific
/// duration in which it will apply. Once an override has been active for the configured duration, it will automatically
/// be reset unless the override is refreshed before the duration elapses.
///
/// The maximum duration for an override is 60 minutes.
pub struct MetricsAPIHandler {
    state: MetricsHandlerState,
}

impl MetricsAPIHandler {
    pub(super) fn new(filter_handle: FilterHandle) -> Self {
        // Spawn our background task that will handle override requests.
        let (override_tx, override_rx) = mpsc::channel(1);
        spawn_traced_named(
            "dynamic-metrics-override-processor",
            process_override_requests(filter_handle, override_rx),
        );

        Self {
            state: MetricsHandlerState { override_tx },
        }
    }

    async fn override_handler(
        State(state): State<MetricsHandlerState>, params: Query<OverrideQueryParams>, body: String,
    ) -> impl IntoResponse {
        // Make sure the override length is within the acceptable range.
        if params.time_secs > MAXIMUM_OVERRIDE_LENGTH_SECS {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "override time cannot be greater than {} seconds",
                    MAXIMUM_OVERRIDE_LENGTH_SECS
                ),
            );
        }

        // Parse the override duration and create a new filter from the body.
        let duration = Duration::from_secs(params.time_secs);
        let new_level = match Level::try_from(body.as_str()) {
            Ok(level) => level,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("failed to parse override filter: {}", e),
                )
            }
        };

        // Instruct the override processor to apply the new metric filtering directives for the given duration.
        let _ = state.override_tx.send(Some((duration, new_level))).await;

        (StatusCode::OK, "acknowledged".to_string())
    }

    async fn reset_handler(State(state): State<MetricsHandlerState>) {
        // Instruct the override processor to immediately reset back to the original metric filtering directives.
        let _ = state.override_tx.send(None).await;
    }
}

impl APIHandler for MetricsAPIHandler {
    type State = MetricsHandlerState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/metrics/override", post(Self::override_handler))
            .route("/metrics/reset", post(Self::reset_handler))
    }
}

async fn process_override_requests(filter_handle: FilterHandle, mut rx: mpsc::Receiver<Option<(Duration, Level)>>) {
    let mut override_active = false;
    let override_timeout = sleep(Duration::MAX);

    tokio::pin!(override_timeout);

    loop {
        select! {
            maybe_override = rx.recv() => match maybe_override {
                Some(Some((duration, new_level))) => {
                    // TODO: Using the `Debug` representation of `Leve` is noisy, and we should add a method upstream to
                    // just get the stringified representation of the level instead.
                    info!(level = ?new_level, "Overriding existing metric filtering directive for {} seconds...", duration.as_secs());

                    filter_handle.override_filter(new_level);

                    // Mark ourselves as having an active override and update the override timeout.
                    override_active = true;
                    override_timeout.as_mut().reset(tokio::time::Instant::now() + duration);
                },

                Some(None) => {
                    // We've been instructed to immediately reset the filter back to the original one, so simply update
                    // the override timeout to fire as soon as possible.
                    override_timeout.as_mut().reset(tokio::time::Instant::now());
                },

                // Our sender has dropped, so there's no more override requests for us to handle.
                None => break,
            },
            _ = &mut override_timeout => {
                // Our override timeout has fired. If we have an active override, reset it.
                //
                // Otherwise, this just means that we've been running for a while without any overrides, so we can just
                // reset the timeout with a long duration.
                if override_active {
                    override_active = false;

                    filter_handle.reset_filter();

                    info!("Restored original metric filtering directive.");
                }

                override_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(MAXIMUM_OVERRIDE_LENGTH_SECS));
            }
        }
    }
}
