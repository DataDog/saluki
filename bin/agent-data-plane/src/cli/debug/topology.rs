use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use argh::{FromArgValue, FromArgs};
use resource_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_core::{
    health::HealthRegistry,
    topology::{diff_topology_snapshots, topology_diff_to_mermaid, TopologySnapshot},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};

use crate::{
    cli::run::{active_pipelines, check_and_warn_config, create_topology},
    config::DataPlaneConfiguration,
    internal::env::ADPEnvironmentProvider,
};

const TOPOLOGY_DIFF_JSON_OUTPUT: &str = "target/adp-topology/topology-diff.json";
const TOPOLOGY_DIFF_MERMAID_OUTPUT: &str = "target/adp-topology/topology-diff.mmd";

/// Exports the configured topology as JSON or Mermaid.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "topology")]
pub struct TopologyCommand {
    /// output format for topology export ('json' or 'mermaid')
    #[argh(option, short = 'f', long = "format", default = "TopologyOutputFormat::Json")]
    format: TopologyOutputFormat,

    /// base topology snapshot JSON to compare
    #[argh(option, long = "diff-base")]
    diff_base: Option<PathBuf>,

    /// head topology snapshot JSON to compare
    #[argh(option, long = "diff-head")]
    diff_head: Option<PathBuf>,

    /// file to write Mermaid output to; only valid with '--format mermaid'
    #[argh(option, short = 'o', long = "output")]
    output: Option<PathBuf>,
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
    if cmd.diff_base.is_some() || cmd.diff_head.is_some() {
        return handle_topology_diff_command(cmd);
    }

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

    match cmd.format {
        TopologyOutputFormat::Json => {
            if cmd.output.is_some() {
                return Err(generic_error!("'--output' is only valid with '--format mermaid'."));
            }

            write_output(None, |output| {
                write_topology_snapshot_output(output, cmd.format, &snapshot)
            })?;
        }
        TopologyOutputFormat::Mermaid => {
            write_output(cmd.output.as_deref(), |output| {
                write_topology_snapshot_output(output, cmd.format, &snapshot)
            })?;
        }
    }

    Ok(())
}

fn handle_topology_diff_command(cmd: TopologyCommand) -> Result<(), GenericError> {
    let base_path = cmd
        .diff_base
        .as_deref()
        .ok_or_else(|| generic_error!("Topology diff requires '--diff-base <path>'."))?;
    let head_path = cmd
        .diff_head
        .as_deref()
        .ok_or_else(|| generic_error!("Topology diff requires '--diff-head <path>'."))?;

    let base_snapshot = load_topology_snapshot(base_path).error_context(format!(
        "Failed to load base topology snapshot '{}'.",
        base_path.display()
    ))?;
    let head_snapshot = load_topology_snapshot(head_path).error_context(format!(
        "Failed to load head topology snapshot '{}'.",
        head_path.display()
    ))?;
    let output_path = match cmd.format {
        TopologyOutputFormat::Json => {
            if cmd.output.is_some() {
                return Err(generic_error!("'--output' is only valid with '--format mermaid'."));
            }
            PathBuf::from(TOPOLOGY_DIFF_JSON_OUTPUT)
        }
        TopologyOutputFormat::Mermaid => cmd
            .output
            .unwrap_or_else(|| PathBuf::from(TOPOLOGY_DIFF_MERMAID_OUTPUT)),
    };

    write_output(Some(&output_path), |output| {
        write_topology_diff_output(output, cmd.format, &base_snapshot, &head_snapshot)
    })?;
    eprintln!("Wrote topology diff to '{}'.", output_path.display());

    Ok(())
}

fn write_output(
    path: Option<&Path>, write: impl FnOnce(&mut dyn Write) -> Result<(), GenericError>,
) -> Result<(), GenericError> {
    match path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).error_context(format!(
                    "Failed to create topology output directory '{}'.",
                    parent.display()
                ))?;
            }

            let file =
                File::create(path).error_context(format!("Failed to create topology output '{}'.", path.display()))?;
            let mut output = BufWriter::new(file);
            write(&mut output)
        }
        None => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            write(&mut output)
        }
    }
}

fn write_json_pretty(output: &mut (impl Write + ?Sized), value: &impl serde::Serialize) -> Result<(), GenericError> {
    serde_json::to_writer_pretty(&mut *output, value).error_context("Failed to serialize topology JSON.")?;
    output
        .write_all(b"\n")
        .error_context("Failed to write topology JSON.")?;
    Ok(())
}

fn write_topology_snapshot_output(
    output: &mut (impl Write + ?Sized), format: TopologyOutputFormat, snapshot: &TopologySnapshot,
) -> Result<(), GenericError> {
    match format {
        TopologyOutputFormat::Json => write_json_pretty(output, snapshot),
        TopologyOutputFormat::Mermaid => output
            .write_all(snapshot.to_mermaid().as_bytes())
            .error_context("Failed to write Mermaid topology snapshot."),
    }
}

fn write_topology_diff_output(
    output: &mut (impl Write + ?Sized), format: TopologyOutputFormat, base_snapshot: &TopologySnapshot,
    head_snapshot: &TopologySnapshot,
) -> Result<(), GenericError> {
    match format {
        TopologyOutputFormat::Json => {
            let diff = diff_topology_snapshots(base_snapshot, head_snapshot);
            write_json_pretty(output, &diff)?;
        }
        TopologyOutputFormat::Mermaid => output
            .write_all(topology_diff_to_mermaid(base_snapshot, head_snapshot).as_bytes())
            .error_context("Failed to write Mermaid topology diff.")?,
    }

    Ok(())
}

fn load_topology_snapshot(path: &Path) -> Result<TopologySnapshot, GenericError> {
    let file = File::open(path).error_context("Failed to open topology snapshot.")?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).error_context("Failed to parse topology snapshot JSON.")
}
