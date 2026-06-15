use std::io::{self, Write as _};

use argh::{FromArgValue, FromArgs};
use resource_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_core::health::HealthRegistry;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

use crate::{
    cli::run::{active_pipelines, check_and_warn_config, create_topology},
    config::DataPlaneConfiguration,
    internal::env::ADPEnvironmentProvider,
};

/// Exports the configured topology as JSON or Mermaid.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "topology")]
pub struct TopologyCommand {
    /// output format ('json' or 'mermaid')
    #[argh(option, short = 'f', long = "format", default = "TopologyOutputFormat::Json")]
    format: TopologyOutputFormat,
}

#[derive(Clone, Copy, Debug)]
enum TopologyOutputFormat {
    Json,
    Mermaid,
}

impl FromArgValue for TopologyOutputFormat {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        let value_lc = value.to_lowercase();
        match value_lc.as_str() {
            "json" => Ok(Self::Json),
            "mermaid" => Ok(Self::Mermaid),
            other => Err(format!(
                "invalid topology output format '{other}': expected 'json' or 'mermaid'"
            )),
        }
    }
}

/// Entrypoint for the `debug topology` command.
pub async fn handle_topology_command(config: &GenericConfiguration, cmd: TopologyCommand) -> Result<(), GenericError> {
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
    match cmd.format {
        TopologyOutputFormat::Json => {
            serde_json::to_writer_pretty(&mut output, &snapshot)
                .error_context("Failed to serialize topology snapshot.")?;
            output
                .write_all(b"\n")
                .error_context("Failed to write topology snapshot.")?;
        }
        TopologyOutputFormat::Mermaid => output
            .write_all(snapshot.to_mermaid().as_bytes())
            .error_context("Failed to write Mermaid topology snapshot.")?,
    }

    Ok(())
}
