//! Runtime-resolved dynamic variables for panoramic integration tests.
//!
//! Some integration tests need values that only exist at container runtime—for example, the
//! container's Docker-assigned IP address. These values aren't known when the test config is
//! written, so they can't be hardcoded in YAML.
//!
//! Dynamic variables solve this with a two-sided mechanism:
//!
//! ## Defining a dynamic variable
//!
//! In a test's `config.yaml`, add a `PANORAMIC_DYNAMIC_<KEY>` env var whose value is a shell
//! command. Reference the resolved value anywhere in the config with `{{PANORAMIC_DYNAMIC_<KEY>}}`:
//!
//! ```yaml
//! container:
//!   env:
//!     PANORAMIC_DYNAMIC_CONTAINER_IP: "hostname -i | awk '{print $1}'"
//!     DD_BIND_HOST: "{{PANORAMIC_DYNAMIC_CONTAINER_IP}}"
//!
//! procedure:
//!   - assertion: log_contains
//!     pattern: "listen_addr:{{PANORAMIC_DYNAMIC_CONTAINER_IP}}:8125"
//!     timeout: 15s
//! ```
//!
//! ## How resolution works
//!
//! Two independent resolvers perform the same substitution:
//!
//! **Inside the container**: the `00-panoramic-dynamic.sh` cont-init.d script runs before any
//! services start. It's bind-mounted into the container by panoramic's read-only mounts overlay
//! (see [`crate::mounts`]), not baked into the production ADP image. The script evaluates each
//! `PANORAMIC_DYNAMIC_*` command, writes the result to `/airlock/dynamic/<KEY>`, resolves
//! `{{PANORAMIC_DYNAMIC_*}}` references in `DD_*` env vars, and writes the resolved values to
//! `/run/adp/env/` for s6-envdir. ADP never sees placeholder strings.
//!
//! **Outside the container**: after the container starts, panoramic polls for
//! `/airlock/dynamic/.ready`, reads resolved values from `/airlock/dynamic/<KEY>`, and substitutes
//! `{{PANORAMIC_DYNAMIC_*}}` in assertion patterns before evaluating them.
//!
//! Both sides derive values from the same commands in the same container, so they match.
//!
//! ## Naming conventions
//!
//! - `PANORAMIC_DYNAMIC_*`: test infrastructure, not application config. Consumed by the init
//!   script; never visible to ADP or the core agent.
//! - `DD_*`: Datadog Agent and ADP config keys. May contain `{{PANORAMIC_DYNAMIC_*}}` references
//!   that get resolved before ADP starts.
//!
//! ## Error handling
//!
//! - If a `PANORAMIC_DYNAMIC_*` command produces an empty result, panoramic fails the test
//!   immediately with a clear message (the shell command likely failed).
//! - If an assertion pattern still contains `{{PANORAMIC_DYNAMIC_*}}` after substitution, panoramic
//!   fails the test (the variable was referenced but never defined).
//! - The init script writes `/airlock/dynamic/.ready` via a bash `trap EXIT`, so the sentinel is
//!   always written regardless of how the script exits.

use std::{collections::HashMap, time::Duration, time::Instant};

use airlock::driver::Driver;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tracing::debug;

use crate::config::IntegrationConfig;

/// Prefix for dynamic variable env vars in the test config.
pub const ENV_PREFIX: &str = "PANORAMIC_DYNAMIC_";

/// Placeholder pattern: `{{PANORAMIC_DYNAMIC_<KEY>}}`.
const PLACEHOLDER_NEEDLE: &str = "{{PANORAMIC_DYNAMIC_";

/// Returns `true` if the test case defines any `PANORAMIC_DYNAMIC_*` env vars.
pub fn has_dynamic_vars(test_case: &IntegrationConfig) -> bool {
    test_case.env.keys().any(|k| k.starts_with(ENV_PREFIX))
}

/// Resolves dynamic variables for Windows containers.
///
/// Windows test images do not run the Linux `cont-init.d` dynamic-variable script. Instead, Panoramic resolves the
/// small set of currently supported Windows dynamic variables directly after the container starts.
pub async fn resolve_windows_vars(
    test_case: &IntegrationConfig, container_ip: Option<&str>,
) -> Result<HashMap<String, String>, GenericError> {
    let mut vars = HashMap::new();

    for env_key in test_case.env.keys().filter(|key| key.starts_with(ENV_PREFIX)) {
        let key = env_key.trim_start_matches(ENV_PREFIX);
        match key {
            "CONTAINER_IP" => {
                let container_ip = container_ip.ok_or_else(|| generic_error!("Container IP unavailable."))?;
                vars.insert(key.to_string(), container_ip.to_string());
            }
            "CUSTOM_HOSTNAME" => {
                vars.insert(key.to_string(), "foo.local".to_string());
            }
            other => {
                return Err(generic_error!(
                    "Unsupported Windows dynamic variable PANORAMIC_DYNAMIC_{}.",
                    other
                ));
            }
        }
    }

    Ok(vars)
}

/// Reads resolved dynamic variable values from `/airlock/dynamic/` inside the container.
///
/// Polls for the `/airlock/dynamic/.ready` sentinel (up to 30 seconds), then reads each key file.
pub async fn read_resolved_vars(driver: &Driver) -> Result<HashMap<String, String>, GenericError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = driver
            .exec_in_container(vec!["cat".to_string(), "/airlock/dynamic/.ready".to_string()])
            .await;

        if result.is_ok() {
            break;
        }

        if Instant::now() > deadline {
            return Err(generic_error!(
                "Timed out waiting for /airlock/dynamic/.ready after 30s."
            ));
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let listing = driver
        .exec_in_container(vec!["ls".to_string(), "/airlock/dynamic/".to_string()])
        .await
        .error_context("Failed to list /airlock/dynamic/.")?;

    let mut vars = HashMap::new();
    for filename in listing.lines() {
        let filename = filename.trim();
        if filename.is_empty() || filename == ".ready" {
            continue;
        }

        let value = driver
            .exec_in_container(vec!["cat".to_string(), format!("/airlock/dynamic/{}", filename)])
            .await
            .error_context(format!("Failed to read /airlock/dynamic/{}.", filename))?;

        debug!(key = filename, value = %value.trim(), "Resolved dynamic variable.");
        vars.insert(filename.to_string(), value.trim().to_string());
    }

    Ok(vars)
}

/// Replace all `{{PANORAMIC_DYNAMIC_*}}` placeholders in a string with resolved values.
pub fn resolve_placeholders(s: &mut String, vars: &HashMap<String, String>) {
    for (key, value) in vars {
        *s = s.replace(&format!("{{{{PANORAMIC_DYNAMIC_{key}}}}}"), value);
    }
}

/// Collect any `{{PANORAMIC_DYNAMIC_*}}` placeholders still present in a string.
pub fn find_unresolved(s: &str, out: &mut Vec<String>) {
    let mut remaining = s;
    while let Some(start) = remaining.find(PLACEHOLDER_NEEDLE) {
        if let Some(end) = remaining[start..].find("}}") {
            out.push(remaining[start..start + end + 2].to_string());
            remaining = &remaining[start + end + 2..];
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal integration config whose `env` map contains exactly the given keys (each with a
    /// placeholder command value, which the Windows resolver never actually executes).
    fn config_with_env_keys(keys: &[&str]) -> IntegrationConfig {
        let env_block = keys.iter().map(|k| format!("  {k}: \"cmd\"\n")).collect::<String>();
        let yaml = format!("name: t\ntimeout: 10s\nprocedure: []\nenv:\n{env_block}");
        serde_yaml::from_str(&yaml).expect("minimal integration config should deserialize")
    }

    #[tokio::test]
    async fn resolve_windows_vars_supports_container_ip_and_custom_hostname() {
        // The two documented Windows variants: CONTAINER_IP echoes the provided container IP, and
        // CUSTOM_HOSTNAME resolves to the fixed sentinel value.
        let config = config_with_env_keys(&["PANORAMIC_DYNAMIC_CONTAINER_IP", "PANORAMIC_DYNAMIC_CUSTOM_HOSTNAME"]);

        let vars = resolve_windows_vars(&config, Some("172.17.0.2"))
            .await
            .expect("supported variables should resolve");

        assert_eq!(vars.get("CONTAINER_IP"), Some(&"172.17.0.2".to_string()));
        assert_eq!(vars.get("CUSTOM_HOSTNAME"), Some(&"foo.local".to_string()));
    }

    #[tokio::test]
    async fn resolve_windows_vars_errors_when_container_ip_unavailable() {
        let config = config_with_env_keys(&["PANORAMIC_DYNAMIC_CONTAINER_IP"]);

        let err = resolve_windows_vars(&config, None)
            .await
            .expect_err("CONTAINER_IP with no available IP should error");
        assert!(
            err.to_string().contains("Container IP unavailable"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_windows_vars_rejects_unsupported_variables() {
        let config = config_with_env_keys(&["PANORAMIC_DYNAMIC_SOMETHING_ELSE"]);

        let err = resolve_windows_vars(&config, Some("172.17.0.2"))
            .await
            .expect_err("an unsupported Windows dynamic variable should error");
        assert!(
            err.to_string()
                .contains("Unsupported Windows dynamic variable PANORAMIC_DYNAMIC_SOMETHING_ELSE"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_placeholders_replaces_every_occurrence() {
        let vars = HashMap::from([("IP".to_string(), "10.0.0.5".to_string())]);
        let mut s = "a={{PANORAMIC_DYNAMIC_IP}} b={{PANORAMIC_DYNAMIC_IP}}".to_string();

        resolve_placeholders(&mut s, &vars);

        assert_eq!(s, "a=10.0.0.5 b=10.0.0.5");
    }

    #[test]
    fn find_unresolved_collects_only_the_remaining_placeholders() {
        let vars = HashMap::from([("KNOWN".to_string(), "v".to_string())]);
        let mut s = "known={{PANORAMIC_DYNAMIC_KNOWN}} missing={{PANORAMIC_DYNAMIC_MISSING}}".to_string();
        resolve_placeholders(&mut s, &vars);

        let mut out = Vec::new();
        find_unresolved(&s, &mut out);

        assert_eq!(out, vec!["{{PANORAMIC_DYNAMIC_MISSING}}".to_string()]);
    }
}
