use std::{collections::HashMap, io, path::PathBuf, process::Stdio, time::Duration};

use figment::{
    value::{Dict, Map, Value},
    Figment, Metadata, Profile, Source,
};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt as _, Snafu};
use tokio::{io::AsyncWriteExt as _, process::Command, time::timeout};
use tracing::{debug, error};

const SECRET_REF_PREFIX: &str = "ENC[";
const SECRET_REF_SUFFIX: &str = "]";

fn default_secret_backend_timeout() -> u64 {
    30
}

/// Secrets resolution error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum Error {
    /// The provided secrets backend command was invalid.
    #[snafu(display("the provided backend command is invalid: {source}"))]
    BackendCommandInvalid {
        /// Error source.
        source: io::Error,
    },

    /// Failed to call the secrets backend command.
    #[snafu(display("failed to call secrets backend command: {source}"))]
    FailedToCallBackend {
        /// Error source.
        source: io::Error,
    },

    /// The secrets backend command exited with a non-zero status code.
    #[snafu(display(
        "backend command '{}' failed with exit code {}: {}",
        backend_command,
        exit_code,
        error
    ))]
    BackendFailed {
        /// Backend command path.
        backend_command: String,

        /// Exit code of the backend command.
        exit_code: i32,

        /// Error description.
        error: String,
    },

    /// Timed out waiting for the secrets backend command to return.
    #[snafu(display("secrets backend command failed to return within {timeout} seconds"))]
    TimedOutCallingBackend {
        /// Timeout duration, in seconds.
        timeout: u64,
    },

    /// Failed to deserialize the response from the secrets backend command
    #[snafu(display("failed to deserialize response from backend: {source}"))]
    FailedToDeserializeResponse {
        /// Error source.
        source: serde_json::Error,
    },

    /// Failed to resolve secrets.
    #[snafu(display("encountered an error when resolving secret '{}': {}", secret_ref, error))]
    FailedToResolve {
        /// Secret reference that the error relates to.
        secret_ref: String,

        /// Error description.
        error: String,
    },
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Deserialize)]
struct ResolvedSecret {
    value: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResolveResponse(HashMap<String, ResolvedSecret>);

#[derive(Deserialize)]
pub(crate) struct ResolverConfiguration {
    #[serde(default)]
    secret_backend_command: PathBuf,

    #[serde(default = "default_secret_backend_timeout")]
    secret_backend_timeout: u64,
}

pub(crate) struct Resolver {
    config: ResolverConfiguration,
}

impl Resolver {
    pub async fn from_configuration(config: ResolverConfiguration) -> Result<Self, Error> {
        // Make sure the backend command points to a real path we can access.
        let _ = tokio::fs::metadata(&config.secret_backend_command)
            .await
            .context(BackendCommandInvalid)?;

        Ok(Self { config })
    }

    async fn resolve(&self, secrets: HashMap<String, String>) -> Result<HashMap<String, String>, Error> {
        // Extract a list of the secret refs that we need to resolve.
        let mut secret_refs = Vec::new();
        for value in secrets.values() {
            debug!(secret_ref = value, "Resolving secret reference.");
            secret_refs.push(value.to_string());
        }

        // Generate our resolve request and serialize it.
        let request = ResolveRequest::new(secret_refs);
        let request = serde_json::to_vec(&request).expect("should never fail to serialize secret resolve request");

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
            .take()
            .expect("should always be able to acquire stdin for backend command process");
        command_stdin.write_all(&request).await.context(FailedToCallBackend)?;

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

        let parsed_output: ResolveResponse =
            serde_json::from_slice(&output.stdout).context(FailedToDeserializeResponse)?;

        let mut resolved_secrets = HashMap::new();

        for (source_key, secret_ref) in secrets {
            let resolved_secret = match parsed_output.0.get(secret_ref.as_str()) {
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

pub(crate) struct Provider {
    metadata: Metadata,
    secrets: Dict,
}

impl Provider {
    pub fn new() -> Self {
        Self {
            metadata: Metadata::from("Secrets", "<unset>"),
            secrets: Dict::default(),
        }
    }

    pub async fn resolve(&mut self, resolver: &Resolver, source: &Figment) -> Result<(), Error> {
        // Extract any secret references from the input data, simply returning early if we find none.
        let secret_refs = extract_secret_refs(source);
        if secret_refs.is_empty() {
            return Ok(());
        }

        // With our secret references in hand, we can now resolve them. Once we've done so, we'll construct our actual
        // data by building a nested data structure since our generated keys mapping to the secret refs are in the
        // nested `a.b.c` format.
        let resolved_secrets = resolver.resolve(secret_refs).await?;

        for (key, value) in resolved_secrets {
            set_nested_dict_entry(&mut self.secrets, key.as_str(), value);
        }

        // Update our metadata source based on the resolver we used.
        self.metadata = Metadata::from(
            "Secrets",
            Source::Custom(resolver.config.secret_backend_command.display().to_string()),
        );

        Ok(())
    }
}

impl figment::Provider for Provider {
    fn metadata(&self) -> Metadata {
        self.metadata.clone()
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut data = Map::new();
        data.insert(Profile::default(), self.secrets.clone());

        Ok(data)
    }
}

fn set_nested_dict_entry(dict: &mut Dict, key: &str, value: String) {
    fn get_or_create<'a>(dict: &'a mut Dict, key: &str) -> Option<&'a mut Dict> {
        let entry = dict.entry(key.to_string()).or_insert_with(|| Dict::default().into());
        if let Value::Dict(_, dict) = entry {
            Some(dict)
        } else {
            None
        }
    }

    let mut parts = key.split('.').collect::<Vec<_>>();
    let mut current_dict = dict;

    for intermediate_key in parts.drain(..parts.len() - 1) {
        match get_or_create(current_dict, intermediate_key) {
            Some(dict) => current_dict = dict,
            None => return,
        }
    }

    let leaf_key = parts.pop().expect("parts should always have at least one element left");
    current_dict.insert(leaf_key.to_string(), value.into());
}

fn extract_secret_refs(source: &Figment) -> HashMap<String, String> {
    let mut secrets = HashMap::new();

    match source.extract::<Value>() {
        Ok(value) => match value.as_dict() {
            Some(dict) => extract_secret_refs_inner("", dict, &mut secrets),
            None => {
                error!("Failed to extract configuration values as a dictionary during secrets resolution. No secrets will be resolved.");
            }
        },
        Err(e) => {
            error!(error = %e, "Failed to iterate over existing configuration values during secrets resolution. No secrets will be resolved.");

            return secrets;
        }
    };

    secrets
}

fn extract_secret_refs_inner(prefix: &str, dict: &Dict, secrets: &mut HashMap<String, String>) {
    for (key, value) in dict.iter() {
        let prefixed_key = format!("{}{}", prefix, key);

        match value {
            Value::String(_, value) => {
                if let Some(secret_ref) = parse_secret_ref(value) {
                    secrets.insert(prefixed_key, secret_ref.to_string());
                }
            }
            Value::Dict(_, dict) => extract_secret_refs_inner(&format!("{}.", prefixed_key), dict, secrets),
            _ => {}
        }
    }
}

fn parse_secret_ref(value: &str) -> Option<&str> {
    // We get our the reference extraction and validity check all in one: as we strip the
    // prefix/suffix away, we only get `Some(...)` if both the prefix and suffix were present.
    value
        .strip_prefix(SECRET_REF_PREFIX)
        .and_then(|s| s.strip_suffix(SECRET_REF_SUFFIX))
}
