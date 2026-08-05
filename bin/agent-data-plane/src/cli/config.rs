use agent_data_plane_config_system::LoadedConfiguration;
use argh::FromArgs;
use saluki_common::scrubber;
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{error, info};

use crate::cli::utils::get_api_client_or_exit;

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

fn parse_config_response(response_body: &[u8]) -> Result<serde_json::Value, GenericError> {
    let scrubbed_bytes = scrubber::default_scrubber().scrub_bytes(response_body);
    serde_json::from_slice(&scrubbed_bytes)
        .error_context("Configuration response is not valid JSON after redacting sensitive values.")
}

/// Entrypoint for the `config` command.
pub async fn handle_config_command(local_config: LoadedConfiguration, command: ConfigCommand) {
    let mut api_client = get_api_client_or_exit(&local_config).await;

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

    // Both privileged configuration views return JSON. Scrub the response before parsing it so malformed scrubber
    // output fails closed instead of exposing an unredacted response.
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
                "auth_token": null,
                "password": false,
                "jwt": 3,
                "database_password": "password-secret",
                "refresh_token": "token-secret",
                "api_key": "aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
                "uri": "https://user:uri-secret@example.com/path",
                "authorization": "Bearer bearer-secret"
            }"#,
        )
        .expect("valid configuration response should remain valid after scrubbing");

        assert_eq!(config["auth_token"], serde_json::Value::Null);
        assert_eq!(config["password"], false);
        assert_eq!(config["jwt"], 3);
        assert_eq!(config["database_password"], "********");
        assert_eq!(config["refresh_token"], "********");
        assert_eq!(config["api_key"], "***************************abbbb");
        assert_eq!(config["uri"], "https://user:********@example.com/path");
        assert_eq!(config["authorization"], "Bearer ********");
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
