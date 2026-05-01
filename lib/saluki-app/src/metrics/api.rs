use std::{sync::Mutex, time::Duration};

use async_trait::async_trait;
use metrics::Level;
use saluki_api::{
    extract::{Query, State},
    response::IntoResponse,
    routing::{post, Router},
    APIHandler, StatusCode,
};
use saluki_core::{
    observability::metrics::FilterHandle,
    runtime::{InitializationError, ProcessShutdown, Supervisable, SupervisorFuture},
};
use saluki_error::generic_error;
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
    /// Creates a new `MetricsAPIHandler` and a paired [`MetricsOverrideWorker`].
    ///
    /// The worker must be added to a [`Supervisor`][saluki_core::runtime::Supervisor] for the handler's
    /// override/reset routes to take effect; without it, requests are accepted but never applied.
    pub(super) fn new(filter_handle: FilterHandle) -> (Self, MetricsOverrideWorker) {
        let (override_tx, override_rx) = mpsc::channel(1);
        let worker = MetricsOverrideWorker {
            state: Mutex::new(Some(MetricsOverrideWorkerState {
                filter_handle,
                override_rx,
            })),
        };

        (
            Self {
                state: MetricsHandlerState { override_tx },
            },
            worker,
        )
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

/// A worker that processes dynamic metric filter override requests sent via [`MetricsAPIHandler`].
///
/// Holds the receiving half of the override channel; the corresponding sender is held by the API handler
/// (which is itself stored in a static after the metrics subsystem is initialized). The worker exits cleanly
/// on either supervisor shutdown or channel close.
///
/// This worker is one-shot: a successful initialization consumes the receiver. Restart by the supervisor will
/// fail with an initialization error, propagating up to bring the supervisor down. This matches the historical
/// fire-and-forget semantics, since the corresponding sender held by [`MetricsAPIHandler`] would no longer have
/// a live receiver to deliver to.
pub struct MetricsOverrideWorker {
    state: Mutex<Option<MetricsOverrideWorkerState>>,
}

struct MetricsOverrideWorkerState {
    filter_handle: FilterHandle,
    override_rx: mpsc::Receiver<Option<(Duration, Level)>>,
}

#[async_trait]
impl Supervisable for MetricsOverrideWorker {
    fn name(&self) -> &str {
        "dynamic-metrics-override-processor"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let MetricsOverrideWorkerState {
            filter_handle,
            override_rx,
        } = self
            .state
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| InitializationError::Failed {
                source: generic_error!("metrics override worker has already been initialized"),
            })?;

        Ok(Box::pin(async move {
            process_override_requests(filter_handle, override_rx, process_shutdown).await;
            Ok(())
        }))
    }
}

async fn process_override_requests(
    filter_handle: FilterHandle, mut rx: mpsc::Receiver<Option<(Duration, Level)>>,
    mut process_shutdown: ProcessShutdown,
) {
    let mut override_active = false;
    let override_timeout = sleep(Duration::MAX);

    tokio::pin!(override_timeout);

    let shutdown = process_shutdown.wait_for_shutdown();
    tokio::pin!(shutdown);

    loop {
        select! {
            _ = &mut shutdown => break,
            maybe_override = rx.recv() => match maybe_override {
                Some(Some((duration, new_level))) => {
                    // TODO: Using the `Debug` representation of `Level` is noisy, and we should add a method upstream to
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
