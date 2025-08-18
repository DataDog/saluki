use axum::{extract::State, response::IntoResponse, routing::get, Router};
use http::StatusCode;
use saluki_io::net::{
    listener::ConnectionOrientedListener,
    server::http::{ErrorHandle, HttpServer, ShutdownHandle},
    util::hyper::TowerToHyperService,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
struct PayloadRequestor {
    payload_req_tx: mpsc::Sender<oneshot::Sender<String>>,
}

impl PayloadRequestor {
    async fn try_get_payload(&self) -> Option<String> {
        let (payload_resp_tx, payload_resp_rx) = oneshot::channel();
        match self.payload_req_tx.send(payload_resp_tx).await {
            Ok(()) => payload_resp_rx.await.ok(),
            Err(_) => None,
        }
    }
}

pub fn spawn_api_server(
    listener: ConnectionOrientedListener, payload_req_tx: mpsc::Sender<oneshot::Sender<String>>,
) -> (ShutdownHandle, ErrorHandle) {
    let payload_requestor = PayloadRequestor { payload_req_tx };
    let service = Router::new()
        .route("/metrics", get(handle_scrape_request))
        .with_state(payload_requestor)
        .into_service();

    let http_server = HttpServer::from_listener(listener, TowerToHyperService::new(service));
    http_server.listen()
}

async fn handle_scrape_request(State(payload_requestor): State<PayloadRequestor>) -> impl IntoResponse {
    match payload_requestor.try_get_payload().await {
        Some(payload) => (StatusCode::OK, payload),
        None => (StatusCode::SERVICE_UNAVAILABLE, "Metrics unavailable.".to_string()),
    }
}
