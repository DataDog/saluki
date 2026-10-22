//! Connects a remote agent to the Datadog Agent's Remote Configuration service.

use std::time::Duration;

use datadog_agent_commons::ipc::{
    client::{client_name, RemoteAgentClient},
    config::RemoteAgentClientConfiguration,
};
use datadog_agent_remote_config::{
    AgentIdentity, ClientKind, RcClientConfiguration, RemoteConfigurationClient, RemoteConfigurationWorker,
};
use saluki_core::runtime::{
    nested_supervisor, ChildSpecification, RestartStrategy, Supervisable, Supervisor, SupervisorError, SupervisorSpec,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metadata::AppDetails;

/// Creates the identity a remote agent sends when polling the Datadog Agent for Remote Configuration.
///
/// Uses the same name as the remote agent's separate configuration stream, plus this build's version.
fn rc_identity(app_details: &AppDetails) -> ClientKind {
    ClientKind::Agent(AgentIdentity::new(
        client_name(app_details),
        app_details.version().raw(),
    ))
}

/// Connects to the Datadog Agent, and creates a Remote Configuration client and its polling worker.
///
/// # Errors
///
/// If the Datadog Agent can't be reached, or the client can't be created, an error is returned.
pub(super) async fn connect(
    client_config: &RemoteAgentClientConfiguration,
) -> Result<(RemoteConfigurationClient, RemoteConfigurationWorker), GenericError> {
    // Remote Configuration has its own Agent connection, separate from other remote agent services.
    let agent = RemoteAgentClient::connect(client_config)
        .await
        .error_context("Failed to connect to the Datadog Agent for Remote Configuration.")?;
    let config = RcClientConfiguration::new(rc_identity(saluki_metadata::get_app_details()));
    Ok(RemoteConfigurationClient::new(agent, config)?)
}

/// Runs the Remote Configuration polling worker under its own supervisor.
///
/// The worker can restart five times within 60 seconds. If it keeps failing, polling stops but the rest of the remote
/// agent keeps running with the last accepted settings. The worker's `initialize` must not fail: initialization errors
/// stop the remote agent rather than triggering a restart.
pub(super) fn remote_config_child(
    worker: impl Supervisable + 'static,
) -> Result<ChildSpecification<SupervisorSpec>, SupervisorError> {
    let mut supervisor = Supervisor::new("remote-config")?
        .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(5, Duration::from_secs(60)));
    supervisor.add_worker(worker);
    // `hosted()` would propagate this subtree's restart exhaustion to the root, which would stop the remote agent.
    // `temporary()` keeps the finite RC-only restart budget isolated.
    #[allow(deprecated)]
    let child = nested_supervisor(supervisor).temporary().build();
    Ok(child)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use async_trait::async_trait;
    use saluki_common::sync::shutdown::ShutdownHandle;
    use saluki_core::runtime::{InitializationError, RestartMode, SupervisorFuture};
    use saluki_metadata::Version;
    use tokio::sync::oneshot;

    use super::*;

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

        // Match the facade root's no-restart strategy; the sibling stands in for the topology.
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

    #[test]
    fn rc_identity_uses_the_config_stream_name_and_app_version() {
        let details = AppDetails::new("Agent Data Plane", "data-plane", "adp", Version::new("1.2.3", 1, 2, 3));
        let ClientKind::Agent(agent) = rc_identity(&details) else {
            panic!("expected an agent identity");
        };
        assert_eq!(agent.name, client_name(&details));
        assert_eq!(agent.name, "agent-data-plane");
        assert_eq!(agent.version, "1.2.3");
    }
}
