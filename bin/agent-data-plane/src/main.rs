//! Main benchmarking binary.
//!
//! This binary emulates the standalone DogStatsD binary, listening for DogStatsD over UDS, aggregating metrics over a
//! 10 second window, and shipping those metrics to the Datadog Platform.

#![deny(warnings)]
#![deny(missing_docs)]
use std::time::Instant;

use clap::Parser as _;
use saluki_app::prelude::*;
use tracing::{error, info};

mod components;

mod config;
use self::config::{Action, Cli, RunConfig};

mod env_provider;

mod internal;

mod cli;
use self::cli::{debug::handle_debug_command, run::run};

pub(crate) mod state;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<tikv_jemallocator::Jemalloc> =
    memory_accounting::allocator::TrackingAllocator::new(tikv_jemallocator::Jemalloc);

#[cfg(not(target_os = "linux"))]
#[global_allocator]
static ALLOC: memory_accounting::allocator::TrackingAllocator<std::alloc::System> =
    memory_accounting::allocator::TrackingAllocator::new(std::alloc::System);

#[tokio::main]
async fn main() {
    let started = Instant::now();
    let cli = Cli::parse();

    if let Err(e) = initialize_dynamic_logging(None).await {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
    }

    if let Err(e) = initialize_metrics("adp").await {
        fatal_and_exit(format!("failed to initialize metrics: {}", e));
    }

    if let Err(e) = initialize_allocator_telemetry().await {
        fatal_and_exit(format!("failed to initialize allocator telemetry: {}", e));
    }

    if let Err(e) = initialize_tls() {
        fatal_and_exit(format!("failed to initialize TLS: {}", e));
    }

    match cli.action {
        Some(Action::Run(config)) => match run(started, config).await {
            Ok(()) => info!("Agent Data Plane stopped."),
            Err(e) => {
                error!("{:?}", e);
                std::process::exit(1);
            }
        },
        Some(Action::Debug(debug_config)) => {
            handle_debug_command(debug_config).await;
        }
        // If no subcommand is provided, runs the run subcommand with the default configuration.
        None => {
            let default_config = RunConfig {
                config: std::path::PathBuf::from("/etc/datadog-agent/datadog.yaml"),
            };
            match run(started, default_config).await {
                Ok(()) => info!("Agent Data Plane stopped."),
                Err(e) => {
                    error!("{:?}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}
