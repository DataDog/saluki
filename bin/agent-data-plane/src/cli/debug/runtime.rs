//! `debug runtime` commands: inspecting ADP's own runtime.

use std::io::Write;

use argh::FromArgs;
use async_trait::async_trait;
use saluki_core::runtime::TreeSnapshot;
use saluki_error::{ErrorContext as _, GenericError};

use super::runtime_render::{render_dot, render_tree};
use crate::cli::utils::DataPlaneAPIClient;

/// Inspect the runtime.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "runtime")]
pub struct RuntimeCommand {
    #[argh(subcommand)]
    subcommand: RuntimeSubcommand,
}

#[derive(FromArgs, Debug)]
#[argh(subcommand)]
enum RuntimeSubcommand {
    ShowProcesses(ShowProcessesCommand),
}

/// Show the supervision tree of processes in the running data plane.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "show-processes")]
pub struct ShowProcessesCommand {
    /// output format: `tree` (default), `json`, or `dot` (Graphviz)
    #[argh(option, short = 'f', long = "format", default = "String::from(\"tree\")")]
    format: String,

    /// output in JSON format, equivalent to `--format json`
    #[argh(switch, short = 'j', long = "json")]
    json: bool,
}

/// How to render the supervision tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    /// An indented tree, for reading in a terminal.
    Tree,
    /// The endpoint's payload, passed through unchanged.
    Json,
    /// A Graphviz graph, for rendering with `dot`.
    Dot,
}

impl ShowProcessesCommand {
    /// Resolves the requested output format.
    ///
    /// # Errors
    ///
    /// If the format isn't one of the supported values, an error is returned.
    fn output_format(&self) -> Result<OutputFormat, GenericError> {
        // `--json` is the spelling used by the other commands here, so it stays supported and simply wins.
        if self.json {
            return Ok(OutputFormat::Json);
        }

        match self.format.as_str() {
            "tree" => Ok(OutputFormat::Tree),
            "json" => Ok(OutputFormat::Json),
            "dot" => Ok(OutputFormat::Dot),
            other => Err(saluki_error::generic_error!(
                "Unknown output format '{}'. Supported formats are `tree`, `json`, and `dot`.",
                other
            )),
        }
    }
}

/// Source of supervision tree snapshots.
///
/// Narrow on purpose: it lets the command be exercised against a fixture with no process to talk to.
#[async_trait(?Send)]
pub(super) trait SupervisionTreeRequester {
    async fn request_supervision_tree(&mut self) -> Result<String, GenericError>;
}

#[async_trait(?Send)]
impl SupervisionTreeRequester for DataPlaneAPIClient {
    async fn request_supervision_tree(&mut self) -> Result<String, GenericError> {
        self.runtime_processes().await
    }
}

/// Entrypoint for the `debug runtime` commands.
pub async fn handle_runtime_command(api_client: &mut DataPlaneAPIClient, cmd: RuntimeCommand) {
    let mut stdout = std::io::stdout();
    let result = match cmd.subcommand {
        RuntimeSubcommand::ShowProcesses(cmd) => handle_show_processes(api_client, cmd, &mut stdout).await,
    };

    if let Err(e) = result {
        tracing::error!("Failed to show processes: {:#}", e);
        std::process::exit(1);
    }
}

/// Fetches a snapshot of the supervision tree and writes it in the requested format.
pub(super) async fn handle_show_processes(
    requester: &mut dyn SupervisionTreeRequester, cmd: ShowProcessesCommand, output: &mut dyn Write,
) -> Result<(), GenericError> {
    let format = cmd.output_format()?;

    let body = requester
        .request_supervision_tree()
        .await
        .error_context("Failed to request the supervision tree.")?;

    let rendered = match format {
        // Passed through verbatim rather than re-serialized, so the output is exactly what the endpoint reported and
        // a future field the CLI doesn't know about still reaches whatever is consuming it.
        OutputFormat::Json => body,
        OutputFormat::Tree | OutputFormat::Dot => {
            let snapshot: TreeSnapshot =
                serde_json::from_str(&body).error_context("Failed to decode the supervision tree.")?;
            match format {
                OutputFormat::Tree => render_tree(&snapshot),
                OutputFormat::Dot => render_dot(&snapshot),
                OutputFormat::Json => unreachable!("handled above"),
            }
        }
    };

    output
        .write_all(rendered.as_bytes())
        .error_context("Failed to write the supervision tree.")?;
    if !rendered.ends_with('\n') {
        output
            .write_all(b"\n")
            .error_context("Failed to write a trailing newline.")?;
    }
    output.flush().error_context("Failed to flush the supervision tree.")?;

    Ok(())
}
