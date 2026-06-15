use std::io::{self, Write as _};

use argh::FromArgs;
use resource_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_core::health::HealthRegistry;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

use crate::{
    cli::run::{active_pipelines, check_and_warn_config, create_topology},
    config::DataPlaneConfiguration,
    internal::env::ADPEnvironmentProvider,
};

/// Exports the configured topology as JSON.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "topology")]
pub struct TopologyCommand {}

/// Entrypoint for the `debug topology` command.
pub async fn handle_topology_command(config: &GenericConfiguration, _cmd: TopologyCommand) -> Result<(), GenericError> {
    let dp_config =
        DataPlaneConfiguration::from_configuration(config).error_context("Failed to load data plane configuration.")?;

    if !dp_config.standalone_mode() && !dp_config.enabled() {
        return Err(generic_error!("Agent Data Plane is not enabled."));
    }

    let active_pipelines = active_pipelines(&dp_config);
    check_and_warn_config(config, &active_pipelines).error_context("Incompatible configuration detected.")?;

    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();
    let (env_provider, _maybe_env_supervisor) =
        ADPEnvironmentProvider::from_configuration(config, &dp_config, &component_registry, &health_registry).await?;
    let (blueprint, _control_surfaces) =
        create_topology(config, &dp_config, &env_provider, &component_registry).await?;
    let snapshot = blueprint.snapshot()?;

    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &snapshot).error_context("Failed to serialize topology snapshot.")?;
    output
        .write_all(b"\n")
        .error_context("Failed to write topology snapshot.")?;

    Ok(())
}
