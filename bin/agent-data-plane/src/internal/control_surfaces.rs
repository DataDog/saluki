use async_trait::async_trait;
use saluki_api::{DynamicRoute, EndpointType};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_components::{
    destinations::DogStatsDStatsAPIHandler,
    sources::{DogStatsDCaptureAPIHandler, DogStatsDReplayAPIHandler},
};
use saluki_core::runtime::{state::DataspaceRegistry, InitializationError, Supervisable, SupervisorFuture};
use saluki_error::generic_error;

use crate::dogstatsd_contexts::DogStatsDContextDumpAPIHandler;

/// A worker for exposing the DogStatsD control surfaces.
///
/// When running, the worker asserts the routes of each DogStatsD API handler on the privileged API endpoint.
pub struct DogStatsDAPIWorker {
    /// API handler for the `/dogstatsd/stats` endpoint.
    pub(crate) stats_api_handler: DogStatsDStatsAPIHandler,
    /// API handler for the `/dogstatsd/capture/trigger` endpoint.
    pub(crate) capture_api_handler: DogStatsDCaptureAPIHandler,
    /// API handler for the `/dogstatsd/replay/session` endpoints.
    pub(crate) replay_api_handler: DogStatsDReplayAPIHandler,
    /// API handler for the Agent-compatible `/agent/dogstatsd-contexts-dump` endpoint.
    pub(crate) context_dump_api_handler: DogStatsDContextDumpAPIHandler,
}

#[async_trait]
impl Supervisable for DogStatsDAPIWorker {
    fn name(&self) -> &str {
        "dogstatsd-api"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let routes = [
            (
                "dogstatsd-stats-api",
                DynamicRoute::http(EndpointType::Privileged, &self.stats_api_handler),
            ),
            (
                "dogstatsd-capture-api",
                DynamicRoute::http(EndpointType::Privileged, &self.capture_api_handler),
            ),
            (
                "dogstatsd-replay-api",
                DynamicRoute::http(EndpointType::Privileged, &self.replay_api_handler),
            ),
            (
                "dogstatsd-contexts-dump-api",
                DynamicRoute::http(EndpointType::Privileged, &self.context_dump_api_handler),
            ),
        ];

        Ok(Box::pin(async move {
            let dataspace =
                DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;

            for (id, route) in routes {
                dataspace.assert(route, id);
            }

            process_shutdown.await;
            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };

    use datadog_agent_commons::ipc::tls::build_ipc_server_tls_config;
    use futures::future::BoxFuture;
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use saluki_app::api::APIBuilder;
    use saluki_components::{
        destinations::DogStatsDStatisticsConfiguration,
        sources::{DogStatsDCaptureControl, DogStatsDReplayControl},
    };
    use saluki_core::runtime::{
        state::{DataspaceUpdate, Identifier, IdentifierFilter},
        FnWorker, Supervisor,
    };
    use saluki_error::GenericError;
    use saluki_io::net::{BoundListenAddress, ListenAddress};
    use tokio::{
        sync::oneshot,
        time::{sleep, timeout, Instant},
    };

    use super::*;
    use crate::cli::utils::DataPlaneAPIClient;

    const TEST_TIMEOUT: Duration = Duration::from_secs(15);
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

    type ClientRequest = for<'a> fn(&'a mut DataPlaneAPIClient) -> BoxFuture<'a, Result<(), GenericError>>;

    /// Retries `request` until its handler rejects it with `expected`, since routes are served shortly after they're
    /// asserted.
    async fn wait_for_handler_error(client: &mut DataPlaneAPIClient, request: ClientRequest, expected: &str) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let mut last_error = String::new();
        while Instant::now() < deadline {
            match timeout(REQUEST_TIMEOUT, request(client)).await {
                Ok(Ok(())) => panic!("request should be rejected by an unbound handler"),
                Ok(Err(e)) => last_error = e.to_string(),
                Err(_) => last_error = format!("request timed out after {REQUEST_TIMEOUT:?}"),
            }
            if last_error.contains(expected) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("expected a handler error containing {expected:?}, last error: {last_error:?}");
    }

    fn write_ipc_cert(dir: &Path) -> PathBuf {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["localhost".to_owned()]).expect("self-signed certificate should be generated");
        let cert_path = dir.join("ipc-cert.pem");
        fs::write(&cert_path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
            .expect("certificate and private key should be written");
        cert_path
    }

    #[tokio::test]
    async fn dogstatsd_routes_are_served_on_privileged_api() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
        let cert_path = write_ipc_cert(temp_dir.path());
        let tls_config = build_ipc_server_tls_config(&cert_path)
            .await
            .expect("IPC server TLS config should build");

        // Unbound controls and no context snapshot owners make every handler reject requests with its own error.
        let worker = DogStatsDAPIWorker {
            stats_api_handler: DogStatsDStatisticsConfiguration::new().api_handler(),
            capture_api_handler: DogStatsDCaptureAPIHandler::new(DogStatsDCaptureControl::new()),
            replay_api_handler: DogStatsDReplayAPIHandler::new(DogStatsDReplayControl::new()),
            context_dump_api_handler: DogStatsDContextDumpAPIHandler::new(Vec::new(), temp_dir.path()),
        };

        let (dataspace_tx, dataspace_rx) = oneshot::channel();
        let mut supervisor = Supervisor::new("dogstatsd-api-test").expect("test supervisor name should be valid");
        supervisor.add_worker(worker);
        supervisor.add_worker(
            APIBuilder::new(EndpointType::Privileged, ListenAddress::tcp_loopback(0))
                .with_tls_config(tls_config)
                .into_supervisor(),
        );
        supervisor.add_worker(FnWorker::with_shutdown(
            "dataspace-capture",
            move |shutdown| async move {
                let _ = dataspace_tx.send(DataspaceRegistry::try_current());
                shutdown.await;
            },
        ));

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let supervisor_task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

        let dataspace = timeout(TEST_TIMEOUT, dataspace_rx)
            .await
            .expect("dataspace should be captured before timeout")
            .expect("dataspace capture worker should send the registry")
            .expect("dataspace should be available under supervision");
        let mut bound_addrs = dataspace
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact(Identifier::named("http-server-privileged-api")));
        let listen_address = match timeout(TEST_TIMEOUT, bound_addrs.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(addr)))) => ListenAddress::Tcp(addr),
            update => panic!("expected a bound TCP address for the privileged API, got {update:?}"),
        };

        let mut client = DataPlaneAPIClient::new(&cert_path, &listen_address)
            .await
            .expect("API client should build");

        wait_for_handler_error(
            &mut client,
            |c| Box::pin(async move { c.dogstatsd_stats(601).await.map(drop) }),
            "Collection duration cannot be greater than 600 seconds.",
        )
        .await;
        wait_for_handler_error(
            &mut client,
            |c| Box::pin(async move { c.dogstatsd_capture("1s", None, false).await.map(drop) }),
            "DogStatsD capture control is unavailable",
        )
        .await;
        wait_for_handler_error(
            &mut client,
            |c| Box::pin(async move { c.dogstatsd_replay_start_session(None).await.map(drop) }),
            "DogStatsD replay control is unavailable",
        )
        .await;
        wait_for_handler_error(
            &mut client,
            |c| Box::pin(c.dogstatsd_replay_finish_session("session")),
            "DogStatsD replay control is unavailable",
        )
        .await;
        wait_for_handler_error(
            &mut client,
            |c| Box::pin(async move { c.dogstatsd_contexts_dump().await.map(drop) }),
            "DogStatsD context snapshot is unavailable",
        )
        .await;

        let route_ids = [
            "dogstatsd-stats-api",
            "dogstatsd-capture-api",
            "dogstatsd-replay-api",
            "dogstatsd-contexts-dump-api",
        ];
        assert_eq!(
            dataspace.current_values::<DynamicRoute>(IdentifierFilter::All).len(),
            route_ids.len()
        );
        for id in route_ids {
            let routes = dataspace.current_values::<DynamicRoute>(IdentifierFilter::exact(Identifier::named(id)));
            assert_eq!(routes.len(), 1, "expected one route asserted as {id:?}");
            assert_eq!(routes[0].endpoint_type(), EndpointType::Privileged);
        }

        let _ = shutdown_tx.send(());
        timeout(TEST_TIMEOUT, supervisor_task)
            .await
            .expect("supervisor should shut down before timeout")
            .expect("supervisor task should not panic")
            .expect("supervisor should stop cleanly");
    }
}
