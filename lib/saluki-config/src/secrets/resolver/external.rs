use std::{collections::HashMap, path::PathBuf, process::Stdio, time::Duration};

use facet::Facet;
use saluki_error::GenericError;
use snafu::ResultExt as _;
use tokio::{io::AsyncWriteExt as _, process::Command, time::timeout};
use tracing::debug;

use super::Resolver;
use crate::secrets::{errors::*, Error, KeyPath};

const fn default_secret_backend_timeout() -> u64 {
    30
}

/// The JSON request sent to the secrets backend process.
#[derive(Debug, Facet)]
struct ResolveRequest {
    version: String,
    secrets: Vec<String>,
}

impl ResolveRequest {
    fn new(secrets: Vec<String>) -> Self {
        Self {
            version: "1.0".to_string(),
            secrets,
        }
    }
}

/// A single resolved secret from the backend response.
#[derive(Debug, Facet)]
struct ResolvedSecret {
    #[facet(default)]
    value: Option<String>,
    #[facet(default)]
    error: Option<String>,
}

/// External process resolver configuration.
#[derive(Facet)]
pub struct ExternalProcessResolverConfiguration {
    #[facet(default)]
    secret_backend_command: PathBuf,

    #[facet(default)]
    secret_backend_timeout: u64,
}

impl Default for ExternalProcessResolverConfiguration {
    fn default() -> Self {
        Self {
            secret_backend_command: PathBuf::new(),
            secret_backend_timeout: default_secret_backend_timeout(),
        }
    }
}

/// A secrets resolver based on an external process.
pub struct ExternalProcessResolver {
    config: ExternalProcessResolverConfiguration,
}

impl ExternalProcessResolver {
    /// Creates a new `ExternalProcessResolver` from the given configuration.
    ///
    /// # Errors
    ///
    /// If the backend command cannot be found at the given path, or if the process has insufficient permissions to
    /// access the backend command file, an error will be returned.
    pub async fn from_configuration(config: ExternalProcessResolverConfiguration) -> Result<Self, Error> {
        // Make sure the backend command points to a real path we can access.
        let _ = tokio::fs::metadata(&config.secret_backend_command)
            .await
            .context(BackendCommandInvalid)?;

        Ok(Self { config })
    }
}

impl Resolver for ExternalProcessResolver {
    async fn resolve(&self, secrets: HashMap<KeyPath, String>) -> Result<HashMap<KeyPath, String>, Error> {
        // Extract a list of the secret refs that we need to resolve.
        let mut secret_refs = Vec::new();
        for value in secrets.values() {
            debug!(secret_ref = value, "Resolving secret reference.");
            secret_refs.push(value.to_string());
        }

        // Generate our resolve request and serialize it.
        let request = ResolveRequest::new(secret_refs);
        let request_json = facet_json::to_string(&request)
            .map_err(GenericError::from)
            .context(FailedToDeserializeResponse)?;

        // Spawn the backend command as a subprocess, and write the serialized request to it.
        let mut command = Command::new(&self.config.secret_backend_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context(FailedToCallBackend)?;

        debug!(backend_command = ?self.config.secret_backend_command, "Spawned secrets backend command as subprocess.");

        let command_stdin = command
            .stdin
            .as_mut()
            .expect("should always be able to acquire stdin for backend command process");
        command_stdin
            .write_all(request_json.as_bytes())
            .await
            .context(FailedToCallBackend)?;

        debug!(backend_command = ?self.config.secret_backend_command, "Wrote resolve request to subprocess, waiting for response...");

        // Wait for the backend subprocess to response, returning early if it finished with an unexpected exit code.
        // After that, parse the response and either return the resolved secret or the error that was indicated during
        // resolution.
        let command_timeout = Duration::from_secs(self.config.secret_backend_timeout);
        let output = timeout(command_timeout, command.wait_with_output())
            .await
            .map_err(|_| Error::TimedOutCallingBackend {
                timeout: self.config.secret_backend_timeout,
            })?
            .context(FailedToCallBackend)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::BackendFailed {
                backend_command: self.config.secret_backend_command.display().to_string(),
                exit_code: output.status.code().unwrap_or(-1),
                error: stderr.to_string(),
            });
        }

        debug!(backend_command = ?self.config.secret_backend_command, "Resolve response received.");

        let response_str = std::str::from_utf8(&output.stdout)
            .map_err(GenericError::from)
            .context(FailedToDeserializeResponse)?;

        let parsed_output: HashMap<String, ResolvedSecret> = facet_json::from_str(response_str)
            .map_err(GenericError::from)
            .context(FailedToDeserializeResponse)?;

        let mut resolved_secrets = HashMap::new();

        for (source_key, secret_ref) in secrets {
            let resolved_secret = match parsed_output.get(secret_ref.as_str()) {
                Some(resolved) => {
                    if let Some(error) = resolved.error.clone() {
                        Err(Error::FailedToResolve { secret_ref, error })
                    } else if let Some(value) = resolved.value.clone() {
                        Ok(value)
                    } else {
                        Err(Error::FailedToResolve {
                            secret_ref,
                            error: "payload for secret had no value or error".to_string(),
                        })
                    }
                }
                None => Err(Error::FailedToResolve {
                    secret_ref,
                    error: "no entry for secret in response payload".to_string(),
                }),
            }?;

            resolved_secrets.insert(source_key, resolved_secret);
        }

        Ok(resolved_secrets)
    }
}
