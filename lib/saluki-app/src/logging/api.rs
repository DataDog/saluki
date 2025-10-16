use std::{sync::Mutex, time::Duration};

use saluki_api::{
    extract::{Query, State},
    response::IntoResponse,
    routing::{post, Router},
    APIHandler, StatusCode,
};
use saluki_common::task::spawn_traced_named;
use serde::Deserialize;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{error, info};
use tracing_subscriber::{reload::Handle, EnvFilter, Registry};

static API_HANDLER: Mutex<Option<LoggingAPIHandler>> = Mutex::new(None);

pub(super) fn set_logging_api_handler(handler: LoggingAPIHandler) {
    API_HANDLER.lock().unwrap().replace(handler);
}

/// Acquires the logging API handler.
///
/// This function is mutable, and consumes the handler if it's present. This means it should only be called once, and
/// only after logging has been initialized via `initialize_dynamic_logging`.
///
/// The logging API handler can be used to install API routes which allow dynamically controlling the logging level
/// filtering. See [`LoggingAPIHandler`] for more information.
pub fn acquire_logging_api_handler() -> Option<LoggingAPIHandler> {
    API_HANDLER.lock().unwrap().take()
}

#[derive(Deserialize)]
struct OverrideQueryParams {
    time_secs: u64,
}

/// State used for the logging API handler.
#[derive(Clone)]
pub struct LoggingHandlerState {
    override_tx: mpsc::Sender<Option<(Duration, EnvFilter)>>,
}

/// An API handler for updating log filtering directives at runtime.
///
/// This handler exposes two main routes -- `/logging/override` and `/logging/reset` -- which allow for overriding the
/// default log filtering directives (configured at startup) at runtime, and then resetting them once the override is no
/// longer needed.
///
/// As this has the potential for incredibly verbose logging at runtime, the override is set with a specific duration in
/// which it will apply. Once an override has been active for the configured duration, it will automatically be reset
/// unless the override is refreshed before the duration elapses.
///
/// The maximum duration for an override is 10 minutes.
pub struct LoggingAPIHandler {
    state: LoggingHandlerState,
}

impl LoggingAPIHandler {
    pub(super) fn new(original_filter: EnvFilter, reload_handle: Handle<EnvFilter, Registry>) -> Self {
        // Spawn our background task that will handle override requests.
        let (override_tx, override_rx) = mpsc::channel(1);
        spawn_traced_named(
            "dynamic-logging-override-processor",
            process_override_requests(original_filter, reload_handle, override_rx),
        );

        Self {
            state: LoggingHandlerState { override_tx },
        }
    }

    async fn override_handler(
        State(state): State<LoggingHandlerState>, params: Query<OverrideQueryParams>, body: String,
    ) -> impl IntoResponse {
        // Make sure the override length is within the acceptable range.
        const MAXIMUM_OVERRIDE_LENGTH_SECS: u64 = 600;
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
        let new_filter = match EnvFilter::try_new(body) {
            Ok(filter) => filter,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("failed to parse override filter: {}", e),
                )
            }
        };

        // Instruct the override processor to apply the new log filtering directives for the given duration.
        let _ = state.override_tx.send(Some((duration, new_filter))).await;

        (StatusCode::OK, "acknowledged".to_string())
    }

    async fn reset_handler(State(state): State<LoggingHandlerState>) {
        // Instruct the override processor to immediately reset back to the original log filtering directives.
        let _ = state.override_tx.send(None).await;
    }
}

impl APIHandler for LoggingAPIHandler {
    type State = LoggingHandlerState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/logging/override", post(Self::override_handler))
            .route("/logging/reset", post(Self::reset_handler))
    }
}

async fn process_override_requests(
    original_filter: EnvFilter, reload_handle: Handle<EnvFilter, Registry>,
    mut rx: mpsc::Receiver<Option<(Duration, EnvFilter)>>,
) {
    let mut override_active = false;
    let override_timeout = sleep(Duration::from_secs(3600));

    tokio::pin!(override_timeout);

    loop {
        select! {
            maybe_override = rx.recv() => match maybe_override {
                Some(Some((duration, new_filter))) => {
                    info!(directives = %new_filter, "Overriding existing log filtering directives for {} seconds...", duration.as_secs());

                    match reload_handle.reload(new_filter) {
                        Ok(()) => {
                            // We were able to successfully reload the filter, so mark ourselves as having an active
                            // override and update the override timeout.
                            override_active = true;
                            override_timeout.as_mut().reset(tokio::time::Instant::now() + duration);
                        },
                        Err(e) => error!(error = %e, "Failed to override log filtering directives."),
                    }
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
                // Our override timeout has fired. If we have an active override, reset it back to the original filter.
                //
                // Otherwise, this just means that we've been running for a while without any overrides, so we can just
                // reset the timeout with a long duration.
                if override_active {
                    override_active = false;

                    if let Err(e) = reload_handle.reload(original_filter.clone()) {
                        error!(error = %e, "Failed to reset log filtering directives.");
                    }

                    info!(directives = %original_filter, "Restored original log filtering directives.");
                }

                override_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3600));
            }
        }
    }

    // Reset our filter to the original one before we exit.
    if let Err(e) = reload_handle.reload(original_filter) {
        error!(error = %e, "Failed to reset log filtering directives before override handler shutdown.");
    }
}
