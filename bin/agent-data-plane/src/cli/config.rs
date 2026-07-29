use std::fmt;

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

#[derive(Debug)]
enum ConfigResponseError {
    InvalidResponse(serde_json::Error),
    WrapperSerialization(serde_json::Error),
    InvalidScrubbedValue(serde_json::Error),
    UnexpectedScrubbedStructure,
}

impl fmt::Display for ConfigResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResponse(error) => write!(f, "configuration response is not valid JSON: {error}"),
            Self::WrapperSerialization(error) => {
                write!(f, "failed to serialize a configuration string for scrubbing: {error}")
            }
            Self::InvalidScrubbedValue(error) => {
                write!(
                    f,
                    "scrubber produced invalid JSON while redacting a configuration string: {error}"
                )
            }
            Self::UnexpectedScrubbedStructure => {
                f.write_str("scrubber changed the JSON wrapper structure while redacting a configuration string")
            }
        }
    }
}

impl std::error::Error for ConfigResponseError {}

fn scrub_json_string(
    value: &str, key_context: Option<&str>, scrubber: &scrubber::Scrubber,
) -> Result<String, ConfigResponseError> {
    let wrapper = match key_context {
        Some(key) => serde_json::json!({ key: value }),
        None => serde_json::json!([value]),
    };
    let wrapper_bytes = serde_json::to_vec(&wrapper).map_err(ConfigResponseError::WrapperSerialization)?;
    let scrubbed_bytes = scrubber.scrub_bytes(&wrapper_bytes);
    let scrubbed_wrapper =
        serde_json::from_slice(&scrubbed_bytes).map_err(ConfigResponseError::InvalidScrubbedValue)?;

    let scrubbed_value = match (key_context, scrubbed_wrapper) {
        (Some(_), serde_json::Value::Object(values)) if values.len() == 1 => values.into_iter().next().map(|(_, v)| v),
        (None, serde_json::Value::Array(mut values)) if values.len() == 1 => values.pop(),
        _ => None,
    };

    match scrubbed_value {
        Some(serde_json::Value::String(value)) => Ok(value),
        _ => Err(ConfigResponseError::UnexpectedScrubbedStructure),
    }
}

fn scrub_json_value(
    value: &mut serde_json::Value, key_context: Option<&str>, scrubber: &scrubber::Scrubber,
) -> Result<(), ConfigResponseError> {
    match value {
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                scrub_json_value(value, Some(key), scrubber)?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                scrub_json_value(value, key_context, scrubber)?;
            }
        }
        serde_json::Value::String(value) => {
            *value = scrub_json_string(value, key_context, scrubber)?;
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }

    Ok(())
}

fn parse_config_response(response_body: &[u8]) -> Result<serde_json::Value, ConfigResponseError> {
    let mut config_value = serde_json::from_slice(response_body).map_err(ConfigResponseError::InvalidResponse)?;
    scrub_json_value(&mut config_value, None, scrubber::default_scrubber())?;
    Ok(config_value)
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

    // Both privileged configuration views return JSON. Scrub their parsed string leaves so non-string values retain
    // their JSON types and key-sensitive rules still receive the surrounding object-key context.
    let config_value = match parse_config_response(response_body.as_bytes()) {
        Ok(v) => v,
        Err(e) => {
            error!(
                "Failed to scrub configuration response safely; refusing to emit configuration: {:#}",
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

    use super::{format_config_value, parse_config_response, ConfigOutputFormat, ConfigResponseError};
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
    fn config_response_scrubbing_preserves_json_types_and_scrubs_string_secrets() {
        let config = parse_config_response(
            br#"{
                "nullable": {"auth_token": null, "password": null},
                "non_strings": {"enabled": true, "retries": 3},
                "secrets": {
                    "auth_token": "token-secret",
                    "password": "password-secret",
                    "api_key": "aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
                    "uri": "https://user:uri-secret@example.com/path"
                },
                "password": ["array-password"],
                "generic_array": [
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
                    "https://user:array-uri-secret@example.com/path",
                    "Bearer array-bearer-secret"
                ]
            }"#,
        )
        .expect("valid configuration response should remain valid after scrubbing");

        assert_eq!(config["nullable"]["auth_token"], serde_json::Value::Null);
        assert_eq!(config["nullable"]["password"], serde_json::Value::Null);
        assert_eq!(config["non_strings"]["enabled"], true);
        assert_eq!(config["non_strings"]["retries"], 3);
        assert_eq!(config["secrets"]["auth_token"], "********");
        assert_eq!(config["secrets"]["password"], "********");
        assert_eq!(config["secrets"]["api_key"], "***************************abbbb");
        assert_eq!(config["secrets"]["uri"], "https://user:********@example.com/path");
        assert_eq!(config["password"][0], "********");
        assert_eq!(config["generic_array"][0], "***************************abbbb");
        assert_eq!(config["generic_array"][1], "https://user:********@example.com/path");
        assert_eq!(config["generic_array"][2], "Bearer ********");
    }

    #[test]
    fn malformed_config_response_is_rejected_before_formatting() {
        let error = parse_config_response(b"not JSON").expect_err("malformed JSON should be rejected");

        assert!(
            matches!(&error, ConfigResponseError::InvalidResponse(error) if error.is_syntax()),
            "unexpected parse error: {error}"
        );
    }
}
