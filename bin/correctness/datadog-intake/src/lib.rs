mod app;
pub use self::app::initialize_app_router;

use std::net::SocketAddr;

use saluki_error::GenericError;
use tokio::net::TcpListener;

/// Bind on `addr` and serve the Datadog intake API until `shutdown` resolves.
pub async fn serve(
    addr: SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), GenericError> {
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, initialize_app_router())
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(Into::into)
}
