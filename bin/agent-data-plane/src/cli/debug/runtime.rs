//! `debug runtime` commands: inspecting ADP's own runtime.

use std::io::Write;

use argh::{FromArgValue, FromArgs};
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
    #[argh(option, short = 'f', long = "format", default = "OutputFormat::Tree")]
    format: OutputFormat,

    /// output in JSON format, equivalent to `--format json`
    #[argh(switch, short = 'j', long = "json")]
    json: bool,
}

impl ShowProcessesCommand {
    /// Resolves the requested output format.
    fn output_format(&self) -> OutputFormat {
        // `--json` is how the sibling `debug` commands spell it, so it stays supported and simply wins.
        if self.json {
            OutputFormat::Json
        } else {
            self.format
        }
    }
}

/// How to render the supervision tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OutputFormat {
    /// An indented tree, for reading in a terminal.
    #[default]
    Tree,
    /// The endpoint's payload, passed through unchanged.
    Json,
    /// A Graphviz graph, for rendering with `dot`.
    Dot,
}

impl FromArgValue for OutputFormat {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value.to_lowercase().as_str() {
            "tree" => Ok(Self::Tree),
            "json" => Ok(Self::Json),
            "dot" => Ok(Self::Dot),
            other => Err(format!(
                "invalid output format '{}': expected 'tree', 'json' or 'dot'",
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
    let body = requester
        .request_supervision_tree()
        .await
        .error_context("Failed to request the supervision tree.")?;

    let rendered = match cmd.output_format() {
        // Passed through verbatim rather than re-serialized, so the output is exactly what the endpoint reported and
        // a future field the CLI doesn't know about still reaches whatever is consuming it.
        OutputFormat::Json => body,
        OutputFormat::Tree => render_tree(&decode(&body)?),
        OutputFormat::Dot => render_dot(&decode(&body)?),
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

/// Decodes an endpoint payload into a snapshot.
fn decode(body: &str) -> Result<TreeSnapshot, GenericError> {
    serde_json::from_str(body).error_context("Failed to decode the supervision tree.")
}

#[cfg(test)]
mod tests {
    use saluki_error::generic_error;

    use super::*;

    /// A requester that returns a canned payload, so the command can be driven with no process to talk to.
    struct FakeRequester(Result<String, &'static str>);

    #[async_trait(?Send)]
    impl SupervisionTreeRequester for FakeRequester {
        async fn request_supervision_tree(&mut self) -> Result<String, GenericError> {
            self.0.clone().map_err(|e| generic_error!("{}", e))
        }
    }

    /// The smallest payload the endpoint can produce: a root that has been declared but never run.
    fn payload() -> String {
        serde_json::json!({
            "captured_at": 1_700_000_000_000u64,
            "resource_tracking_enabled": false,
            "totals": {
                "supervisors": 1, "workers": 0, "running": 0, "exited": 0, "registered": 1,
                "restarts": 0, "live_bytes": 0, "cpu_time_nanos": 0, "max_depth": 1
            },
            "root": {
                "name": "adp-root",
                "kind": "supervisor",
                "process_name": null,
                "process_id": null,
                "state": "registered",
                "restart": "permanent",
                "significant": false,
                "created_at": 1_700_000_000_000u64,
                "started_at": null,
                "uptime_ms": null,
                "restart_count": 0,
                "exited_at": null,
                "resource_group": null,
                "children": []
            }
        })
        .to_string()
    }

    async fn run(cmd: ShowProcessesCommand, body: Result<String, &'static str>) -> Result<String, GenericError> {
        let mut requester = FakeRequester(body);
        let mut output = Vec::new();
        handle_show_processes(&mut requester, cmd, &mut output).await?;
        Ok(String::from_utf8(output).expect("output is valid UTF-8"))
    }

    fn command(format: OutputFormat, json: bool) -> ShowProcessesCommand {
        ShowProcessesCommand { format, json }
    }

    #[tokio::test]
    async fn renders_a_tree_by_default() {
        let out = run(command(OutputFormat::Tree, false), Ok(payload())).await.unwrap();
        assert!(out.contains("Supervision tree for 'adp-root'"), "{out}");
        assert!(out.contains("adp-root  [sup] registered"), "{out}");
    }

    #[tokio::test]
    async fn json_is_passed_through_verbatim() {
        // Not re-serialized, so a field this CLI doesn't know about still reaches whatever consumes the output.
        let body = payload();
        let out = run(command(OutputFormat::Tree, true), Ok(body.clone())).await.unwrap();
        assert_eq!(out.trim_end(), body);

        let out = run(command(OutputFormat::Json, false), Ok(body.clone())).await.unwrap();
        assert_eq!(out.trim_end(), body);
    }

    #[tokio::test]
    async fn renders_a_graphviz_graph() {
        let out = run(command(OutputFormat::Dot, false), Ok(payload())).await.unwrap();
        assert!(out.starts_with("digraph supervision_tree {"), "{out}");
        assert!(out.contains(r#"n0 [label="adp-root\nsupervisor""#), "{out}");
    }

    #[tokio::test]
    async fn json_wins_over_an_explicit_format() {
        let body = payload();
        let out = run(command(OutputFormat::Dot, true), Ok(body.clone())).await.unwrap();
        assert_eq!(out.trim_end(), body);
    }

    #[tokio::test]
    async fn a_failed_request_is_reported_with_context() {
        let err = run(command(OutputFormat::Tree, false), Err("connection refused"))
            .await
            .expect_err("the request failed");
        let rendered = format!("{:#}", err);
        assert!(
            rendered.contains("Failed to request the supervision tree."),
            "{rendered}"
        );
        assert!(rendered.contains("connection refused"), "{rendered}");
    }

    #[tokio::test]
    async fn an_undecodable_payload_is_reported_rather_than_panicking() {
        let err = run(command(OutputFormat::Tree, false), Ok(String::from("not json")))
            .await
            .expect_err("the payload did not decode");
        assert!(
            format!("{:#}", err).contains("Failed to decode the supervision tree."),
            "{err:#}"
        );
    }

    #[test]
    fn output_format_rejects_an_unknown_value() {
        assert_eq!(OutputFormat::from_arg_value("tree"), Ok(OutputFormat::Tree));
        assert_eq!(OutputFormat::from_arg_value("DOT"), Ok(OutputFormat::Dot));
        assert!(OutputFormat::from_arg_value("svg").is_err());
    }
}
