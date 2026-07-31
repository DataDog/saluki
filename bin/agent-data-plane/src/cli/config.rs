use argh::FromArgs;
use saluki_common::scrubber;
use saluki_config::GenericConfiguration;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
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

fn scrub_json_string(
    value: &str, key_context: Option<&str>, scrubber: &scrubber::Scrubber,
) -> Result<String, GenericError> {
    let wrapper = match key_context {
        Some(key) => serde_json::json!({ key: value }),
        None => serde_json::json!([value]),
    };
    let wrapper_bytes =
        serde_json::to_vec(&wrapper).error_context("Failed to serialize a configuration value for scrubbing.")?;
    let scrubbed_bytes = scrubber.scrub_bytes(&wrapper_bytes);
    let scrubbed_wrapper: serde_json::Value = serde_json::from_slice(&scrubbed_bytes)
        .error_context("Scrubber produced invalid JSON while redacting a configuration value.")?;

    let scrubbed_value = match (key_context, scrubbed_wrapper) {
        (Some(key), serde_json::Value::Object(mut values)) if values.len() == 1 => values.remove(key),
        (None, serde_json::Value::Array(mut values)) if values.len() == 1 => values.pop(),
        _ => None,
    };

    match scrubbed_value {
        Some(serde_json::Value::String(value)) => Ok(value),
        _ => Err(generic_error!(
            "Scrubber changed the JSON wrapper structure while redacting a configuration value."
        )),
    }
}

fn scrub_json_value(
    value: &mut serde_json::Value, key_context: Option<&str>, scrubber: &scrubber::Scrubber,
) -> Result<(), GenericError> {
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

fn parse_config_response(response_body: &[u8]) -> Result<serde_json::Value, GenericError> {
    let mut config_value =
        serde_json::from_slice(response_body).error_context("Configuration response is not valid JSON.")?;
    scrub_json_value(&mut config_value, None, scrubber::default_scrubber())?;
    Ok(config_value)
}

/// Entrypoint for the `config` command.
pub async fn handle_config_command(bootstrap_config: &GenericConfiguration, command: ConfigCommand) {
    let mut api_client = match DataPlaneAPIClient::from_config(bootstrap_config).await {
        Ok(client) => client,
        Err(e) => {
            error!("Failed to create data plane API client: {:#}", e);
            std::process::exit(1);
        }
    };

    let response_body = match if command.runtime {
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
    let formatted = match if command.json {
        serde_json::to_string(&config_value)
    } else {
        serde_json::to_string_pretty(&config_value)
    } {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to format configuration response as JSON: {:#}", e);
            std::process::exit(1);
        }
    };

    if command.json {
        println!("{}", formatted);
    } else {
        info!("Full configuration:\n{}", formatted);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_config_response;

    #[test]
    fn config_response_scrubbing_preserves_json_types_and_scrubs_string_secrets() {
        let config = parse_config_response(
            br#"{
                "nullable": null,
                "enabled": true,
                "retries": 3,
                "password": ["key-sensitive-secret"],
                "generic": [
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
                    "https://user:uri-secret@example.com/path"
                ]
            }"#,
        )
        .expect("valid configuration response should remain valid after scrubbing");

        assert_eq!(config["nullable"], serde_json::Value::Null);
        assert_eq!(config["enabled"], true);
        assert_eq!(config["retries"], 3);
        assert_eq!(config["password"][0], "********");
        assert_eq!(config["generic"][0], "***************************abbbb");
        assert_eq!(config["generic"][1], "https://user:********@example.com/path");
    }

    #[test]
    fn malformed_config_response_is_rejected_without_exposing_its_contents() {
        let error =
            parse_config_response(b"secret response that is not JSON").expect_err("malformed JSON should be rejected");
        let message = format!("{error:#}");

        assert!(message.contains("Configuration response is not valid JSON"));
        assert!(!message.contains("secret response"));
    }
}
