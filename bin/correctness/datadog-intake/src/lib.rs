mod app;

use axum::Router;
use tokio::sync::watch;

/// Signals produced by the intake server, available before the server starts.
pub struct IntakeSignals {
    /// Number of payloads successfully processed on `POST /api/v2/series`.
    pub series_v2_count: watch::Receiver<usize>,
}

/// Build the intake router and return it alongside its signals.
///
/// Pass the router to [`serve_intake`] to start serving. Signals are readable
/// immediately — no need to wait for the server to start.
pub fn build_intake() -> (Router, IntakeSignals) {
    let (router, app_signals) = app::initialize_app_router();
    (
        router,
        IntakeSignals {
            series_v2_count: app_signals.series_v2_count,
        },
    )
}
