//! Connects the trace pipeline to the core Agent's Remote Configuration service.
//!
//! The Agent supplies trace sampling settings (`APM_SAMPLING`) and mappings from trace concepts such
//! as HTTP status to attribute names (`APM_SEMANTIC_CORE_DD`). In connected mode with a local trace
//! pipeline, ADP subscribes to enabled products and runs a worker that polls for updates. Without a
//! subscription, components use configured sampling settings and the mappings embedded in ADP.

use std::time::Duration;

use agent_data_plane_config::SalukiConfiguration;
use datadog_agent_commons::ipc::{
    client::{client_name, RemoteAgentClient},
    config::RemoteAgentClientConfiguration,
};
use datadog_agent_remote_config::{AgentIdentity, ClientKind, RcClientConfiguration, RemoteConfigurationClient};
use saluki_components::remote_config::{SemanticRegistryProvider, TraceSamplingSubscription};
use saluki_core::runtime::{
    nested_supervisor, ChildSpecification, RestartStrategy, Supervisable, Supervisor, SupervisorError, SupervisorSpec,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metadata::AppDetails;

use crate::config::DataPlaneConfiguration;

/// The trace products ADP requests from the Agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RcProducts {
    /// Whether to request trace sampling settings.
    pub sampling: bool,

    /// Whether to request OTLP trace attribute mappings.
    pub semantics: bool,
}

impl RcProducts {
    /// Disables all products.
    pub(crate) const NONE: Self = Self {
        sampling: false,
        semantics: false,
    };

    /// Returns `true` if any product is enabled.
    pub(crate) const fn any(self) -> bool {
        self.sampling || self.semantics
    }
}

/// Chooses which trace products to subscribe to from `config`.
///
/// Subscriptions require connected mode, a local trace pipeline, the global Remote Configuration switch, and the
/// corresponding product switch.
pub(crate) fn enabled_products(config: &SalukiConfiguration) -> RcProducts {
    let dp = DataPlaneConfiguration::from_configuration(config);
    let rc = &config.domains.remote_configuration;
    // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e/comp/trace/config/impl/setup.go#L114-L116
    if dp.standalone_mode() || !rc.enabled || !dp.traces_pipeline_required() {
        return RcProducts::NONE;
    }

    RcProducts {
        sampling: rc.apm_sampling_enabled,
        semantics: rc.apm_semantics_enabled,
    }
}

/// Creates the identity ADP sends when polling the Agent for Remote Configuration.
///
/// Uses the same application name as ADP's separate configuration-update stream, plus this build's version.
pub(crate) fn rc_identity(app_details: &AppDetails) -> ClientKind {
    ClientKind::Agent(AgentIdentity::new(
        client_name(app_details),
        app_details.version().raw(),
    ))
}

/// Handles passed to trace components for Remote Configuration updates.
///
/// By default, the sampler uses its configured settings, and OTLP translation and APM stats use the attribute mappings
/// embedded in ADP. When subscribed, these components read updates from the same polling worker.
#[derive(Default)]
pub(crate) struct RemoteConfigSubscriptions {
    /// Sampling settings for the trace sampler, if `APM_SAMPLING` is enabled.
    pub trace_sampling: Option<TraceSamplingSubscription>,

    /// Attribute mappings shared by the OTLP source, OTLP decoder, and APM stats.
    pub semantic_registry: SemanticRegistryProvider,
}

impl RemoteConfigSubscriptions {
    /// Connects to the Agent and subscribes to the selected `products`.
    ///
    /// With no products or no Agent connection settings, returns default handles and no worker without connecting.
    /// Otherwise, returns a child supervisor for the polling worker. The subscriptions keep its shared state alive.
    ///
    /// # Errors
    ///
    /// Returns an error if the Agent connection, client setup, subscription, or supervisor creation fails.
    pub(crate) async fn subscribe(
        products: RcProducts, remote_agent: Option<&RemoteAgentClientConfiguration>,
    ) -> Result<(Self, Option<ChildSpecification<SupervisorSpec>>), GenericError> {
        let (true, Some(remote_agent)) = (products.any(), remote_agent) else {
            return Ok((Self::default(), None));
        };

        // Remote Configuration has its own Agent connection, separate from other ADP services.
        let agent = RemoteAgentClient::connect(remote_agent)
            .await
            .error_context("Failed to connect to the Datadog Agent for Remote Configuration.")?;
        let config = RcClientConfiguration::new(rc_identity(saluki_metadata::get_app_details()));
        let (client, worker) = RemoteConfigurationClient::new(agent, config)?;

        let mut subscriptions = Self::default();
        if products.sampling {
            subscriptions.trace_sampling = Some(TraceSamplingSubscription::new(&client)?);
        }
        if products.semantics {
            subscriptions.semantic_registry = SemanticRegistryProvider::subscribe(&client)?;
        }

        Ok((subscriptions, Some(remote_config_child(worker)?)))
    }
}

/// Runs the Remote Configuration polling worker under its own supervisor.
///
/// The worker can restart five times within 60 seconds. If it keeps failing, polling stops but the rest of ADP keeps
/// running with the last accepted settings. The worker's `initialize` must not fail: initialization errors stop ADP
/// rather than triggering a restart.
fn remote_config_child(
    worker: impl Supervisable + 'static,
) -> Result<ChildSpecification<SupervisorSpec>, SupervisorError> {
    let mut supervisor = Supervisor::new("remote-config")?
        .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(5, Duration::from_secs(60)));
    supervisor.add_worker(worker);
    Ok(nested_supervisor(supervisor).temporary().build())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use async_trait::async_trait;
    use datadog_agent_commons::ipc::config::IpcAuthConfiguration;
    use saluki_common::sync::shutdown::ShutdownHandle;
    use saluki_core::runtime::{InitializationError, RestartMode, SupervisorFuture};
    use tokio::sync::oneshot;

    use super::*;

    fn unreachable_remote_agent() -> RemoteAgentClientConfiguration {
        RemoteAgentClientConfiguration {
            cmd_port: 1,
            auth: IpcAuthConfiguration::new(
                PathBuf::from("/nonexistent/auth_token"),
                PathBuf::from("/nonexistent/ipc_cert.pem"),
            ),
            grpc_max_message_size: 1024,
            #[cfg(target_os = "linux")]
            vsock_cid: None,
        }
    }

    #[tokio::test]
    async fn subscribe_makes_nothing_when_no_product_is_enabled_or_there_is_no_agent() {
        let remote_agent = unreachable_remote_agent();

        // The unreachable endpoint makes an unexpected connection fail this test.
        let (subscriptions, child) = RemoteConfigSubscriptions::subscribe(RcProducts::NONE, Some(&remote_agent))
            .await
            .expect("subscribing to nothing should not connect");
        assert!(subscriptions.trace_sampling.is_none());
        assert!(child.is_none());

        let all = RcProducts {
            sampling: true,
            semantics: true,
        };
        let (subscriptions, child) = RemoteConfigSubscriptions::subscribe(all, None)
            .await
            .expect("standalone mode should not fail");
        assert!(subscriptions.trace_sampling.is_none());
        assert!(child.is_none());
    }

    /// Counts starts and runs until shutdown.
    struct LongRunning {
        starts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Supervisable for LongRunning {
        fn name(&self) -> &str {
            "long-running"
        }

        async fn initialize(&self, shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(async move {
                shutdown.await;
                Ok(())
            }))
        }
    }

    /// Counts starts and panics on every run.
    struct AlwaysPanics {
        starts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Supervisable for AlwaysPanics {
        fn name(&self) -> &str {
            "always-panics"
        }

        async fn initialize(&self, _shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(async move { panic!("worker bug") }))
        }
    }

    async fn wait_until(what: &str, condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
    }

    #[tokio::test]
    async fn exhausted_remote_config_child_stops_without_stopping_root() {
        let sibling_starts = Arc::new(AtomicUsize::new(0));
        let worker_starts = Arc::new(AtomicUsize::new(0));

        // Match the production root's no-restart strategy; the sibling stands in for the topology.
        let mut root = Supervisor::new("root")
            .unwrap()
            .with_restart_strategy(RestartStrategy::new(RestartMode::OneForOne, 0, Duration::from_secs(5)));
        root.add_worker(LongRunning {
            starts: Arc::clone(&sibling_starts),
        });
        root.add_worker(
            remote_config_child(AlwaysPanics {
                starts: Arc::clone(&worker_starts),
            })
            .unwrap(),
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let run = tokio::spawn(async move { root.run_with_shutdown(shutdown_rx).await });

        // The first run and five restarts, then the sixth failure stops the child without stopping root.
        wait_until("the worker has started six times", || {
            worker_starts.load(Ordering::SeqCst) >= 6
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(worker_starts.load(Ordering::SeqCst), 6);
        assert_eq!(sibling_starts.load(Ordering::SeqCst), 1);
        assert!(!run.is_finished(), "root stopped with the remote-config child");

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("root should shut down promptly")
            .expect("root task should not panic");
        assert!(result.is_ok(), "root shut down with {result:?}");
    }

    struct Case {
        name: &'static str,
        rc: bool,
        sampling: bool,
        semantics: bool,
        standalone: bool,
        otlp: bool,
        otlp_proxy: bool,
        otlp_proxy_traces: bool,
        expected: RcProducts,
    }

    const SAMPLING: RcProducts = RcProducts {
        sampling: true,
        semantics: false,
    };
    const SEMANTICS: RcProducts = RcProducts {
        sampling: false,
        semantics: true,
    };
    const BOTH: RcProducts = RcProducts {
        sampling: true,
        semantics: true,
    };

    // Baseline with both products and a local trace pipeline enabled.
    const ON: Case = Case {
        name: "",
        rc: true,
        sampling: true,
        semantics: true,
        standalone: false,
        otlp: true,
        otlp_proxy: false,
        otlp_proxy_traces: false,
        expected: BOTH,
    };

    #[test]
    fn enabled_products_requires_rc_a_product_key_and_the_traces_pipeline() {
        let cases = [
            Case { name: "all on", ..ON },
            Case {
                name: "sampling only",
                semantics: false,
                expected: SAMPLING,
                ..ON
            },
            Case {
                name: "semantics only",
                sampling: false,
                expected: SEMANTICS,
                ..ON
            },
            Case {
                name: "both product keys off",
                sampling: false,
                semantics: false,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "rc off",
                rc: false,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "rc off, sampling only",
                rc: false,
                semantics: false,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "standalone",
                standalone: true,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "otlp off: no traces pipeline",
                otlp: false,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "otlp traces proxied: no traces pipeline",
                otlp_proxy: true,
                otlp_proxy_traces: true,
                expected: RcProducts::NONE,
                ..ON
            },
            Case {
                name: "otlp proxy with traces kept: traces pipeline",
                otlp_proxy: true,
                otlp_proxy_traces: false,
                ..ON
            },
        ];

        for case in cases {
            let mut config = SalukiConfiguration::default();
            config.domains.remote_configuration.enabled = case.rc;
            config.domains.remote_configuration.apm_sampling_enabled = case.sampling;
            config.domains.remote_configuration.apm_semantics_enabled = case.semantics;
            config.control.standalone_mode = case.standalone;
            config.control.otlp = case.otlp;
            config.domains.otlp.proxy.enabled = case.otlp_proxy;
            config.domains.otlp.proxy.traces_enabled = case.otlp_proxy_traces;

            let products = enabled_products(&config);
            assert_eq!(products, case.expected, "{}", case.name);
            assert_eq!(
                products.any(),
                case.expected != RcProducts::NONE,
                "{}: any()",
                case.name
            );
        }
    }

    #[test]
    fn rc_identity_uses_the_config_stream_name_and_app_version() {
        let ClientKind::Agent(agent) = rc_identity(&crate::APP_DETAILS) else {
            panic!("expected an agent identity");
        };
        assert_eq!(agent.name, client_name(&crate::APP_DETAILS));
        assert_eq!(agent.name, "agent-data-plane");
        assert_eq!(agent.version, crate::APP_DETAILS.version().raw());
    }
}
