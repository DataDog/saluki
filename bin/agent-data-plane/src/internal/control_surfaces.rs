use saluki_app::dynamic_api::DynamicAPIBuilder;
use saluki_components::{
    destinations::DogStatsDStatsAPIHandler,
    sources::{DogStatsDCaptureAPIHandler, DogStatsDReplayAPIHandler},
};

use crate::dogstatsd_contexts::DogStatsDContextDumpAPIHandler;

/// Combined set of control surfaces to expose from the privileged API endpoint.
#[derive(Default)]
pub struct TopologyControlSurfaces {
    dogstatsd: Option<DogStatsDControlSurface>,
}

impl TopologyControlSurfaces {
    /// Attaches the DogStatsD-specific control surfaces to this set of topology control surfaces.
    pub fn attach_dogstatsd(&mut self, dsd_control_surface: DogStatsDControlSurface) {
        self.dogstatsd = Some(dsd_control_surface);
    }

    /// Registers all configured control surfaces with the given API builder.
    pub fn register_control_surfaces(self, mut builder: DynamicAPIBuilder) -> DynamicAPIBuilder {
        if let Some(dogstatsd) = self.dogstatsd {
            builder = dogstatsd.register_control_surfaces(builder);
        }

        builder
    }
}

/// DogStatsD-specific control surfaces.
pub struct DogStatsDControlSurface {
    /// API handler for the `/dogstatsd/stats` endpoint.
    pub(crate) stats_api_handler: DogStatsDStatsAPIHandler,
    /// API handler for the `/dogstatsd/capture/trigger` endpoint.
    pub(crate) capture_api_handler: DogStatsDCaptureAPIHandler,
    /// API handler for the `/dogstatsd/replay/session` endpoints.
    pub(crate) replay_api_handler: DogStatsDReplayAPIHandler,
    /// API handler for the Agent-compatible `/agent/dogstatsd-contexts-dump` endpoint.
    pub(crate) context_dump_api_handler: DogStatsDContextDumpAPIHandler,
}

impl DogStatsDControlSurface {
    fn register_control_surfaces(self, builder: DynamicAPIBuilder) -> DynamicAPIBuilder {
        builder
            .with_handler(self.stats_api_handler)
            .with_handler(self.capture_api_handler)
            .with_handler(self.replay_api_handler)
            .with_handler(self.context_dump_api_handler)
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, time::Duration};

    use http::StatusCode;
    use saluki_api::EndpointType;
    use saluki_app::dynamic_api::DynamicAPIBuilder;
    use saluki_components::{destinations::DogStatsDStatisticsConfiguration, sources::DogStatsDConfiguration};
    use saluki_config::config_from;
    use saluki_core::runtime::Supervisor;
    use saluki_io::net::ListenAddress;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpStream,
        sync::oneshot,
        time::Instant,
    };

    use super::DogStatsDControlSurface;
    use crate::dogstatsd_contexts::DogStatsDContextDumpAPIHandler;

    #[tokio::test]
    async fn dogstatsd_control_surface_registers_context_dump_on_privileged_dynamic_api() {
        let address = reserve_tcp_address();
        let dogstatsd_config = DogStatsDConfiguration::from_configuration(&config_from(serde_json::json!({})).await)
            .expect("default DogStatsD configuration should build");
        let surface = DogStatsDControlSurface {
            stats_api_handler: DogStatsDStatisticsConfiguration::new().api_handler(),
            capture_api_handler: dogstatsd_config.capture_api_handler(),
            replay_api_handler: dogstatsd_config.replay_api_handler(),
            context_dump_api_handler: DogStatsDContextDumpAPIHandler::new(
                "configured-agent-token",
                Vec::new(),
                std::path::PathBuf::new(),
            )
            .unwrap(),
        };
        let api = surface.register_control_surfaces(DynamicAPIBuilder::new(
            EndpointType::Privileged,
            ListenAddress::Tcp(address),
        ));
        let mut supervisor = Supervisor::new("dogstatsd-control-surface-test").unwrap();
        supervisor.add_worker(api);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

        let status = post_context_dump_eventually(address).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let _ = shutdown_tx.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "timed out waiting for DogStatsD context dump route response")]
    async fn context_dump_request_exchange_times_out_when_peer_does_not_close() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test listener should bind");
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("test listener should accept");
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let stream = TcpStream::connect(address).await.expect("test client should connect");

        exchange_context_dump_request(stream, Instant::now() + Duration::from_millis(20)).await;
    }

    fn reserve_tcp_address() -> std::net::SocketAddr {
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("ephemeral TCP address should bind")
            .local_addr()
            .unwrap()
    }

    async fn post_context_dump_eventually(address: std::net::SocketAddr) -> StatusCode {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            if let Ok(stream) = TcpStream::connect(address).await {
                break stream;
            }
            assert!(Instant::now() < deadline, "dynamic API did not start before deadline");
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let response = exchange_context_dump_request(stream, deadline).await;
        let status_digits = response
            .get(9..12)
            .expect("HTTP response should have a three-digit status code");
        let status = status_digits.iter().try_fold(0_u16, |status, digit| {
            digit.is_ascii_digit().then(|| status * 10 + u16::from(*digit - b'0'))
        });
        StatusCode::from_u16(status.expect("HTTP response status should be numeric")).unwrap()
    }

    async fn exchange_context_dump_request(mut stream: TcpStream, deadline: Instant) -> Vec<u8> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::timeout(remaining, async {
            stream
                .write_all(
                    b"POST /agent/dogstatsd-contexts-dump HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer \
                      configured-agent-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await?;
            Ok::<_, std::io::Error>(response)
        })
        .await
        .expect("timed out waiting for DogStatsD context dump route response")
        .expect("DogStatsD context dump route request should complete")
    }
}
