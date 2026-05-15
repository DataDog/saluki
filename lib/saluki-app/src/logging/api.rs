use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use saluki_api::{
    extract::{Query, State},
    response::IntoResponse,
    routing::{post, Router},
    APIHandler, DynamicRoute, EndpointType, StatusCode,
};
use saluki_core::runtime::{
    state::DataspaceRegistry, InitializationError, ProcessShutdown, Supervisable, SupervisorFuture,
};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use tokio::{
    select,
    sync::{mpsc, Mutex},
    time::sleep,
};
use tracing::{error, info};
use tracing_subscriber::{reload::Handle, EnvFilter, Registry};

#[derive(Deserialize)]
struct OverrideQueryParams {
    time_secs: u64,
}

/// An action driven through a [`LoggingOverrideController`] and processed by [`LoggingOverrideWorker`].
enum LoggingOverrideAction {
    /// Apply `filter` as a temporary override on top of the current base, lasting `duration`.
    ///
    /// When the duration elapses, or a [`Reset`][LoggingOverrideAction::Reset] arrives, the worker restores the
    /// current base filter.
    Override { duration: Duration, filter: EnvFilter },

    /// Clear any active override, immediately restoring the current base filter.
    Reset,

    /// Replace the current base filter.
    ///
    /// Applied immediately if no override is active. If an override is active, the new base is stored and applied
    /// once the override expires (or is reset), so we don't clobber an in-flight override.
    UpdateBase(EnvFilter),
}

/// Controls the dynamic logging filter at runtime.
///
/// All filter mutations—temporary overrides, resets, and base updates—flow through here to the
/// [`LoggingOverrideWorker`] that owns the underlying `tracing` reload handle. Cloning is cheap; hand clones to
/// any caller that needs to drive filter changes (HTTP handlers, configuration watchers, etc.).
#[derive(Clone)]
pub struct LoggingOverrideController {
    tx: mpsc::Sender<LoggingOverrideAction>,
}

impl LoggingOverrideController {
    /// Applies `filter` as a temporary override for `duration`, after which the base filter is restored.
    pub(crate) async fn override_for(&self, duration: Duration, filter: EnvFilter) -> Result<(), GenericError> {
        self.send(LoggingOverrideAction::Override { duration, filter }).await
    }

    /// Clears any active override, immediately restoring the current base filter.
    pub(crate) async fn reset(&self) -> Result<(), GenericError> {
        self.send(LoggingOverrideAction::Reset).await
    }

    /// Replaces the current base filter.
    ///
    /// Applied immediately if no override is active; otherwise stored and applied when the override expires.
    pub async fn update_base(&self, filter: EnvFilter) -> Result<(), GenericError> {
        self.send(LoggingOverrideAction::UpdateBase(filter)).await
    }

    async fn send(&self, action: LoggingOverrideAction) -> Result<(), GenericError> {
        self.tx
            .send(action)
            .await
            .map_err(|_| generic_error!("logging override worker is no longer running"))
    }
}

/// State used for the logging API handler.
#[derive(Clone)]
pub struct LoggingHandlerState {
    controller: LoggingOverrideController,
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
    /// Creates a new `LoggingAPIHandler` driven by the given controller.
    fn new(controller: LoggingOverrideController) -> Self {
        Self {
            state: LoggingHandlerState { controller },
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

        // Instruct the override worker to apply the new log filtering directives for the given duration.
        let _ = state.controller.override_for(duration, new_filter).await;

        (StatusCode::OK, "acknowledged".to_string())
    }

    async fn reset_handler(State(state): State<LoggingHandlerState>) {
        // Instruct the override worker to immediately reset back to the current base log filtering directives.
        let _ = state.controller.reset().await;
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

/// A worker that processes log filter directive overrides.
///
/// When running, the worker asserts a set of routes (based on [`LoggingAPIHandler`]) that allow triggering
/// an override of the current logging filter directives as well as clearing (resetting) the active override.
pub struct LoggingOverrideWorker {
    handler: LoggingAPIHandler,
    state: Arc<Mutex<LoggingOverrideWorkerState>>,
}

struct LoggingOverrideWorkerState {
    reload_handle: Handle<EnvFilter, Registry>,
    rx: mpsc::Receiver<LoggingOverrideAction>,
}

impl LoggingOverrideWorker {
    /// Creates a new `LoggingOverrideWorker` driving the given reload handle.
    ///
    /// When the worker starts, the filter directives present in the reload handle will be used as the "base" filter:
    /// the filter that is reapplied after an override expires or is reset. This base filter can then be subsequently
    /// updated through the [`LoggingOverrideController`] handle that is returned.
    pub(super) fn new(reload_handle: Handle<EnvFilter, Registry>) -> (Self, LoggingOverrideController) {
        let (tx, rx) = mpsc::channel(1);
        let controller = LoggingOverrideController { tx };
        let handler = LoggingAPIHandler::new(controller.clone());
        let worker = Self {
            handler,
            state: Arc::new(Mutex::new(LoggingOverrideWorkerState { reload_handle, rx })),
        };

        (worker, controller)
    }
}

#[async_trait]
impl Supervisable for LoggingOverrideWorker {
    fn name(&self) -> &str {
        "dynamic-logging-override-processor"
    }

    async fn initialize(&self, process_shutdown: ProcessShutdown) -> Result<SupervisorFuture, InitializationError> {
        let mut state = self.state.clone().lock_owned().await;
        let logging_route = DynamicRoute::http(EndpointType::Privileged, &self.handler);

        Ok(Box::pin(async move {
            DataspaceRegistry::try_current()
                .ok_or_else(|| generic_error!("Dataspace not available."))?
                .assert(logging_route, "logging-api");

            process_override_actions(&mut state, process_shutdown).await;
            Ok(())
        }))
    }
}

async fn process_override_actions(state: &mut LoggingOverrideWorkerState, mut process_shutdown: ProcessShutdown) {
    // Seed the canonical base filter from the reload handle. If the underlying reload layer has been dropped, we
    // cannot perform overrides or resets meaningfully: just emit an error and wait for shutdown so that we don't
    // exit prematurely and drive the supervisor into an infinite restart loop.
    let mut base_filter = match state.reload_handle.clone_current() {
        Some(filter) => filter,
        None => {
            error!("Logging subsystem is in an indeterminate state; dynamic log filtering will not be available.");

            process_shutdown.wait_for_shutdown().await;
            return;
        }
    };

    let mut override_active = false;
    let override_timeout = sleep(Duration::from_secs(3600));

    tokio::pin!(override_timeout);

    let shutdown = process_shutdown.wait_for_shutdown();
    tokio::pin!(shutdown);

    loop {
        select! {
            _ = &mut shutdown => break,
            maybe_action = state.rx.recv() => match maybe_action {
                Some(LoggingOverrideAction::Override { duration, filter }) => {
                    info!(directives = %filter, "Overriding existing log filtering directives for {} seconds...", duration.as_secs());

                    match state.reload_handle.reload(filter) {
                        Ok(()) => {
                            // We were able to successfully reload the filter, so mark ourselves as having an active
                            // override and update the override timeout.
                            override_active = true;
                            override_timeout.as_mut().reset(tokio::time::Instant::now() + duration);
                        },
                        Err(e) => error!(error = %e, "Failed to override log filtering directives."),
                    }
                },

                Some(LoggingOverrideAction::Reset) => {
                    // We've been instructed to immediately reset the filter back to the base one, so simply update
                    // the override timeout to fire as soon as possible.
                    override_timeout.as_mut().reset(tokio::time::Instant::now());
                },

                Some(LoggingOverrideAction::UpdateBase(new_base)) => {
                    // Before replacing the base filter, check if the new base is different from the current one before we trigger a reload
                    // and log a big, noisy message.
                    //
                    // We do this in a hacky way and compare the stringified version of each filter since `EnvFilter` can't be directly compared.
                    let existing_base_filter_str = base_filter.to_string();
                    let new_base_filter_str = new_base.to_string();
                    if new_base_filter_str == existing_base_filter_str {
                        continue;
                    }

                    base_filter = new_base;

                    if override_active {
                        // An override is currently active. Don't clobber it -- the new base will take effect when
                        // the override expires.
                        info!(
                            directives = %base_filter,
                            "Updated base log filtering directives; application deferred until active override expires."
                        );
                    } else {
                        let new_directives = base_filter.to_string();
                        match state.reload_handle.reload(base_filter.clone()) {
                            Ok(()) => info!(directives = %new_directives, "Updated base log filtering directives."),
                            Err(e) => error!(error = %e, "Failed to update base log filtering directives."),
                        }
                    }
                },

                // Our sender has dropped, so there's no more actions for us to handle.
                None => break,
            },
            _ = &mut override_timeout => {
                // Our override timeout has fired. If we have an active override, reset it back to the base filter.
                //
                // Otherwise, this just means that we've been running for a while without any overrides, so we can just
                // reset the timeout with a long duration.
                if override_active {
                    override_active = false;

                    let restore_directives = base_filter.to_string();
                    if let Err(e) = state.reload_handle.reload(base_filter.clone()) {
                        error!(error = %e, "Failed to reset log filtering directives.");
                    }

                    info!(directives = %restore_directives, "Restored base log filtering directives.");
                }

                override_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3600));
            }
        }
    }

    // Reset our filter to the base one before we exit.
    if let Err(e) = state.reload_handle.reload(base_filter) {
        error!(error = %e, "Failed to reset log filtering directives before override handler shutdown.");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{sync::mpsc, time::sleep};
    use tracing_subscriber::{reload, EnvFilter, Registry};

    use super::*;

    fn current_filter(handle: &reload::Handle<EnvFilter, Registry>) -> String {
        handle
            .clone_current()
            .expect("reload layer should be alive")
            .to_string()
    }

    async fn wait_for_filter(handle: &reload::Handle<EnvFilter, Registry>, expected: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);

        loop {
            let actual = current_filter(handle);
            if actual == expected {
                return;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "filter did not become `{expected}`; last value was `{actual}`"
            );

            sleep(Duration::from_millis(10)).await;
        }
    }

    fn spawn_processor(
        base_filter: EnvFilter,
    ) -> (
        LoggingOverrideController,
        reload::Handle<EnvFilter, Registry>,
        tokio::task::JoinHandle<()>,
        reload::Layer<EnvFilter, Registry>,
    ) {
        let (filter_layer, reload_handle) = reload::Layer::new(base_filter);
        let (tx, rx) = mpsc::channel(1);
        let controller = LoggingOverrideController { tx };
        let mut state = LoggingOverrideWorkerState {
            reload_handle: reload_handle.clone(),
            rx,
        };
        let processor = tokio::spawn(async move {
            process_override_actions(&mut state, ProcessShutdown::noop()).await;
        });

        // The reload layer must outlive the handles -- callers should bind it (e.g., `let _layer = ...`) to keep
        // the handle alive for the duration of the test.
        (controller, reload_handle, processor, filter_layer)
    }

    #[tokio::test]
    async fn reset_restores_current_base_filter() {
        let (controller, reload_handle, processor, _filter_layer) =
            spawn_processor(EnvFilter::new("agent_data_plane=info"));

        controller
            .override_for(Duration::from_secs(60), EnvFilter::new("hyper=warn"))
            .await
            .expect("send override");
        wait_for_filter(&reload_handle, "hyper=warn").await;

        controller
            .update_base(EnvFilter::new("agent_data_plane=debug"))
            .await
            .expect("send update_base");
        controller.reset().await.expect("send reset");
        wait_for_filter(&reload_handle, "agent_data_plane=debug").await;

        drop(controller);
        processor.await.expect("override processor should exit cleanly");
    }

    #[tokio::test]
    async fn override_expiration_restores_current_base_filter() {
        let (controller, reload_handle, processor, _filter_layer) =
            spawn_processor(EnvFilter::new("agent_data_plane=info"));

        controller
            .override_for(Duration::from_millis(100), EnvFilter::new("hyper=warn"))
            .await
            .expect("send override");
        wait_for_filter(&reload_handle, "hyper=warn").await;

        controller
            .update_base(EnvFilter::new("agent_data_plane=warn"))
            .await
            .expect("send update_base");
        wait_for_filter(&reload_handle, "agent_data_plane=warn").await;

        drop(controller);
        processor.await.expect("override processor should exit cleanly");
    }

    #[tokio::test]
    async fn update_base_applies_immediately_when_no_override_active() {
        let (controller, reload_handle, processor, _filter_layer) =
            spawn_processor(EnvFilter::new("agent_data_plane=info"));

        controller
            .update_base(EnvFilter::new("agent_data_plane=debug"))
            .await
            .expect("send update_base");
        wait_for_filter(&reload_handle, "agent_data_plane=debug").await;

        drop(controller);
        processor.await.expect("override processor should exit cleanly");
    }
}
