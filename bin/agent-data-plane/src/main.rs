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
use self::config::{Action, Cli, DebugConfig, RunConfig};

mod env_provider;
mod internal;

mod run_subcommand;
use self::run_subcommand::run;

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

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    match cli.action {
        Some(Action::Run(config)) => match run(started, config).await {
            Ok(()) => info!("Agent Data Plane stopped."),
            Err(e) => {
                error!("{:?}", e);
                std::process::exit(1);
            }
        },
        Some(Action::Debug(DebugConfig::ResetLogLevel)) => {
            let response = match client.post("https://localhost:5101/logging/reset").send().await {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to send request: {}", e);
                    std::process::exit(1);
                }
            };

            if response.status().is_success() {
                println!("Log level reset successful");
            } else {
                eprintln!("Failed to reset log level: {}", response.status());
            }
        }
        Some(Action::Debug(DebugConfig::SetLogLevel(config))) => {
            let filter_directives = config.filter_directives;
            let duration_secs = config.duration_secs;

            let response = match client
                .post("https://localhost:5101/logging/override")
                .query(&[("time_secs", duration_secs)])
                .body(filter_directives)
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to send request: {}", e);
                    std::process::exit(1);
                }
            };

            if response.status().is_success() {
                println!("Log level override successful");
            } else {
                eprintln!("Failed to override log level: {}", response.status());
            }
        }
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
