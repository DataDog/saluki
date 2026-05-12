mod app;
use std::net::SocketAddr;

use axum::Router;
use saluki_error::GenericError;
use tokio::net::TcpListener;
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

/// Serve a pre-built intake router on `listener` until `shutdown` resolves.
pub async fn serve_intake(
    listener: TcpListener, router: Router, shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), GenericError> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(Into::into)
}

/// Bind on `addr` and serve the Datadog intake API until `shutdown` resolves.
pub async fn serve(
    addr: SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), GenericError> {
    let listener = TcpListener::bind(addr).await?;
    let (router, _signals) = build_intake();
    serve_intake(listener, router, shutdown).await
}
