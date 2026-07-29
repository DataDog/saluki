use argh::FromArgs;
use saluki_common::scrubber;
use saluki_config::GenericConfiguration;
use tracing::{error, info};

use crate::cli::utils::DataPlaneAPIClient;

/// Prints the current configuration.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "config")]
pub struct ConfigCommand {
    /// Controls whether the command emits compact machine-readable JSON.
    #[argh(
        switch,
        description = "emit one compact JSON value to standard output for machine parsing"
    )]
    pub json: bool,

    /// Controls whether the command selects the translated runtime configuration.
    #[argh(switch, description = "select the translated runtime configuration view")]
    pub runtime: bool,
}

#[derive(Clone, Copy)]
enum ConfigOutputFormat {
    Human,
    Json,
}

fn parse_config_response(response_body: &[u8]) -> Result<serde_json::Value, serde_json::Error> {
    let scrubber = scrubber::default_scrubber();
    let scrubbed_bytes = scrubber.scrub_bytes(response_body);
    serde_json::from_slice(&scrubbed_bytes)
}

fn format_config_value(
    config_value: &serde_json::Value, output_format: ConfigOutputFormat,
) -> Result<String, serde_json::Error> {
    match output_format {
        ConfigOutputFormat::Human => serde_json::to_string_pretty(config_value),
        ConfigOutputFormat::Json => serde_json::to_string(config_value),
    }
}

/// Entrypoint for the `config` command.
pub async fn handle_config_command(bootstrap_config: &GenericConfiguration, json: bool, runtime: bool) {
    let mut api_client = match DataPlaneAPIClient::from_config(bootstrap_config).await {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create data plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    let response_body = match if runtime {
        api_client.config_runtime().await
    } else {
        api_client.config().await
    } {
        Ok(body) => body,
        Err(e) => {
            error!("Failed to get configuration: {:#}.", e);
            std::process::exit(1);
        }
    };

    // Both privileged configuration views return JSON; parse the selected response after scrubbing.
    let config_value = match parse_config_response(response_body.as_bytes()) {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Failed to parse configuration response as JSON after scrubbing (malformed payload or scrubber bug): {:#}",
                e
            );
            std::process::exit(1);
        }
    };
    let output_format = if json {
        ConfigOutputFormat::Json
    } else {
        ConfigOutputFormat::Human
    };
    let formatted = match format_config_value(&config_value, output_format) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to format configuration response as JSON: {:#}", e);
            std::process::exit(1);
        }
    };

    if json {
        println!("{}", formatted);
    } else {
        info!("Full configuration:\n{}", formatted);
    }
}

#[cfg(test)]
mod tests {
    use argh::FromArgs as _;

    use super::{format_config_value, parse_config_response, ConfigOutputFormat};
    use crate::cli::{Action, Cli};

    #[test]
    fn config_command_defaults_to_source_human_view() {
        let cli = Cli::from_args(&["agent-data-plane"], &["config"]).expect("config should parse");

        let Action::Config(command) = cli.action else {
            panic!("expected config action");
        };
        assert!(!command.json);
        assert!(!command.runtime);
    }

    #[test]
    fn json_and_runtime_switches_parse_together_for_config_command() {
        let cli = Cli::from_args(&["agent-data-plane"], &["config", "--json", "--runtime"])
            .expect("config --json --runtime should parse");

        let Action::Config(command) = cli.action else {
            panic!("expected config action");
        };
        assert!(command.json);
        assert!(command.runtime);
    }

    #[test]
    fn config_help_documents_runtime_view_without_user_facing_internal_language() {
        let help = match Cli::from_args(&["agent-data-plane"], &["config", "--help"]) {
            Ok(_) => panic!("config --help should exit after rendering help"),
            Err(early_exit) => early_exit.output,
        };

        assert!(help.contains("--runtime"), "runtime switch missing from help: {help}");
        assert!(
            help.contains("translated runtime configuration"),
            "runtime view meaning missing from help: {help}"
        );
        assert!(
            !help.contains("internal"),
            "help exposed internal API terminology: {help}"
        );
    }

    #[test]
    fn json_output_is_compact() {
        let config =
            parse_config_response(br#"{"outer":{"items":[1,2]}}"#).expect("configuration response should parse");

        let formatted = format_config_value(&config, ConfigOutputFormat::Json)
            .expect("configuration should format as compact JSON");

        assert_eq!(formatted, r#"{"outer":{"items":[1,2]}}"#);
    }

    #[test]
    fn human_output_remains_pretty_printed() {
        let config =
            parse_config_response(br#"{"outer":{"items":[1,2]}}"#).expect("configuration response should parse");

        let formatted = format_config_value(&config, ConfigOutputFormat::Human)
            .expect("configuration should format as pretty JSON");

        assert_eq!(
            formatted,
            "{\n  \"outer\": {\n    \"items\": [\n      1,\n      2\n    ]\n  }\n}"
        );
    }

    #[test]
    fn malformed_config_response_is_rejected_before_formatting() {
        let error = parse_config_response(b"not JSON").expect_err("malformed JSON should be rejected");

        assert!(error.is_syntax(), "unexpected parse error: {error}");
    }
}
