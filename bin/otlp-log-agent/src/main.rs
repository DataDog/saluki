//! Minimal OTLP → Datadog Logs agent.
//!
//! Accepts OTLP log payloads over gRPC (default: 0.0.0.0:4317) and HTTP
//! (default: 0.0.0.0:4318), translates them from the OTLP data model into
//! the Datadog Logs format, and forwards them to the Datadog Logs intake
//! (`POST /api/v2/logs`).
//!
//! Metrics and trace payloads that arrive on the OTLP endpoints are silently
//! dropped. All other OTLP signal handling logic from agent-data-plane (DogStatsD,
//! aggregation, APM, enrichment, etc.) is excluded.
//!
//! # Configuration (environment variables)
//!
//! All configuration is read from `DD_`-prefixed environment variables, matching
//! the Datadog Agent convention. No YAML config file is required.
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `DD_API_KEY` | *(required)* | Datadog API key |
//! | `DD_SITE` | `datadoghq.com` | Datadog site |
//! | `DD_DD_URL` | *(derived from site)* | Override intake URL |
//! | `DD_HOSTNAME` | *(system hostname)* | Host tag on logs |
//! | `DD_LOG_LEVEL` | `info` | Log verbosity |
//! | `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT` | `0.0.0.0:4317` | gRPC listen address |
//! | `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT` | `0.0.0.0:4318` | HTTP listen address |
//!
//! # Lambda extension integration notes
//!
//! For a Lambda extension, set the gRPC/HTTP endpoints to `127.0.0.1:4317` /
//! `127.0.0.1:4318` so only the Lambda function itself can reach the agent.
//! SIGTERM (Lambda shutdown signal) is handled alongside SIGINT.

use std::time::{Duration, Instant};

use memory_accounting::{ComponentRegistry, MemoryLimiter};
use saluki_app::bootstrap::AppBootstrapper;
use saluki_components::{
    destinations::BlackholeConfiguration,
    encoders::{BufferedIncrementalConfiguration, DatadogLogsConfiguration},
    forwarders::DatadogConfiguration,
    sources::OtlpConfiguration,
};
use saluki_config::ConfigurationLoader;
use saluki_core::topology::TopologyBlueprint;
use saluki_error::{ErrorContext as _, GenericError};
use saluki_health::HealthRegistry;
use tokio::{select, time::interval};
use tracing::{error, info};

// On Linux, use jemalloc (same as agent-data-plane) for better allocator
// performance under the Tokio work-stealing scheduler.
// On other platforms (macOS for local dev, etc.) use the system allocator.
#[cfg(all(target_os = "linux", not(system_allocator)))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<tikv_jemallocator::Jemalloc> =
    memory_accounting::allocator::TrackingAllocator::new(tikv_jemallocator::Jemalloc);

#[cfg(any(not(target_os = "linux"), system_allocator))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() -> Result<(), GenericError> {
    let started = Instant::now();

    // -------------------------------------------------------------------------
    // Configuration
    //
    // Load entirely from DD_* environment variables — no YAML file required.
    // This makes the binary self-contained for Lambda / container deployments.
    // -------------------------------------------------------------------------
    let config = ConfigurationLoader::default()
        .from_environment("DD")
        .error_context("Failed to load configuration from environment variables.")?
        .with_default_secrets_resolution()
        .await
        .error_context("Failed to resolve secrets.")?
        .bootstrap_generic();

    // -------------------------------------------------------------------------
    // Bootstrap
    //
    // Initialises: structured logging, TLS root certificates, internal metrics
    // recorder, and the allocator telemetry background task.
    // -------------------------------------------------------------------------
    let _bootstrap_guard = AppBootstrapper::from_configuration(&config)
        .error_context("Failed to parse bootstrap configuration.")?
        .with_metrics_prefix("otlp_log_agent")
        .bootstrap()
        .await
        .error_context("Failed to complete bootstrap phase.")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        init_time_ms = started.elapsed().as_millis(),
        "OTLP Log Agent starting..."
    );

    // -------------------------------------------------------------------------
    // Component configuration
    // -------------------------------------------------------------------------
    let otlp_source = OtlpConfiguration::from_configuration(&config)
        .error_context("Failed to configure OTLP source.")?;
    // Note: OtlpConfiguration defaults to all signals enabled (metrics, logs,
    // traces). We only wire the "logs" output downstream; the other outputs
    // are connected to BlackholeConfiguration so the converter never panics
    // when it tries to dispatch to an unconnected output.

    let dd_logs_encoder = DatadogLogsConfiguration::from_configuration(&config)
        .map(BufferedIncrementalConfiguration::from_encoder_builder)
        .error_context("Failed to configure Datadog Logs encoder.")?;

    let dd_forwarder = DatadogConfiguration::from_configuration(&config)
        .error_context("Failed to configure Datadog forwarder.")?;
    // Reads DD_API_KEY, DD_SITE / DD_DD_URL from the config automatically.

    // -------------------------------------------------------------------------
    // Topology
    //
    //  otlp_in ──[logs]──▶ dd_logs_encode ──▶ dd_out (Datadog /api/v2/logs)
    //          ──[metrics]──▶ noop_metrics   (dropped silently)
    //          ──[traces]───▶ noop_traces    (dropped silently)
    // -------------------------------------------------------------------------
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let mut blueprint = TopologyBlueprint::new("otlp-log-agent", &component_registry);

    blueprint
        // ── Components ───────────────────────────────────────────────────────
        .add_source("otlp_in", otlp_source)?
        .add_encoder("dd_logs_encode", dd_logs_encoder)?
        .add_forwarder("dd_out", dd_forwarder)?
        .add_destination("noop_metrics", BlackholeConfiguration)?
        .add_destination("noop_traces", BlackholeConfiguration)?
        // ── Log path ─────────────────────────────────────────────────────────
        .connect_component("dd_logs_encode", ["otlp_in.logs"])?
        .connect_component("dd_out", ["dd_logs_encode"])?
        // ── Drop unhandled signals ────────────────────────────────────────────
        .connect_component("noop_metrics", ["otlp_in.metrics"])?
        .connect_component("noop_traces", ["otlp_in.traces"])?;

    // Skip memory bounds verification — not needed in Lambda where the runtime
    // already enforces a hard memory ceiling per invocation environment.
    let memory_limiter = MemoryLimiter::noop();

    let built = blueprint
        .build()
        .await
        .error_context("Failed to build topology.")?;

    let mut running = built
        .spawn(&health_registry, memory_limiter)
        .await
        .error_context("Failed to spawn topology.")?;

    // -------------------------------------------------------------------------
    // Health reporting
    // -------------------------------------------------------------------------
    {
        let health_registry = health_registry.clone();
        tokio::spawn(async move {
            let mut check = interval(Duration::from_millis(100));
            loop {
                check.tick().await;
                if health_registry.all_ready() {
                    break;
                }
            }
            info!(
                ready_time_ms = started.elapsed().as_millis(),
                "All components healthy. Accepting OTLP log payloads."
            );
        });
    }

    // -------------------------------------------------------------------------
    // Run until shutdown signal
    //
    // Handles both SIGINT (Ctrl-C) and SIGTERM (Lambda shutdown signal).
    // -------------------------------------------------------------------------
    select! {
        _ = running.wait_for_unexpected_finish() => {
            error!("A topology component finished unexpectedly. Initiating shutdown...");
        }
        _ = shutdown_signal() => {}
    }

    info!("Shutting down (30 s timeout)...");

    running
        .shutdown_with_timeout(Duration::from_secs(30))
        .await
        .error_context("Graceful shutdown timed out.")?;

    info!("OTLP Log Agent stopped.");
    Ok(())
}

/// Waits for SIGINT (Ctrl-C) or SIGTERM (Lambda shutdown signal).
async fn shutdown_signal() {
    // SIGTERM is only available on Unix-like systems.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

        select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM.");
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT.");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.expect("failed to listen for SIGINT");
        info!("Received SIGINT.");
    }
}
