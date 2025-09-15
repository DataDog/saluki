//! Primitives for working with typed and untyped configuration data.
#![deny(warnings)]
#![deny(missing_docs)]

use std::sync::{Arc, RwLock};
use std::{borrow::Cow, collections::HashSet};

pub use figment::value;
use figment::{
    error::Kind,
    providers::{Env, Serialized},
    Figment, Provider,
};
use saluki_error::GenericError;
use serde::Deserialize;
use snafu::{ResultExt as _, Snafu};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tracing::{debug, error};

pub mod dynamic;
mod provider;
mod secrets;

pub use self::dynamic::FieldUpdateWatcher;
use self::dynamic::{ConfigChangeEvent, ConfigUpdate};
use self::provider::ResolvedProvider;

#[derive(Clone)]
struct ArcProvider(Arc<dyn Provider + Send + Sync>);

impl Provider for ArcProvider {
    fn metadata(&self) -> figment::Metadata {
        self.0.metadata()
    }

    fn data(&self) -> Result<figment::value::Map<figment::Profile, figment::value::Dict>, figment::Error> {
        self.0.data()
    }
}

enum ProviderSource {
    Static(ArcProvider),
    Dynamic(Option<mpsc::Receiver<ConfigUpdate>>),
}

impl Clone for ProviderSource {
    fn clone(&self) -> Self {
        match self {
            Self::Static(p) => Self::Static(p.clone()),
            Self::Dynamic(_) => Self::Dynamic(None),
        }
    }
}

/// A configuration error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ConfigurationError {
    /// Environment variable prefix was empty.
    #[snafu(display("Environment variable prefix must not be empty."))]
    EmptyPrefix,

    /// Requested field was missing from the configuration.
    #[snafu(display("Missing field '{}' in configuration. {}", field, help_text))]
    MissingField {
        /// Help text describing how to set the missing field.
        ///
        /// This is meant to be displayed to the user, and includes environment variable-specific text if environment
        /// variables had been loaded originally.
        help_text: String,

        /// Name of the missing field.
        field: Cow<'static, str>,
    },

    /// Requested field's data type was not the unexpected data type.
    #[snafu(display(
        "Expected value for field '{}' to be '{}', got '{}' instead.",
        field,
        expected_ty,
        actual_ty
    ))]
    InvalidFieldType {
        /// Name of the invalid field.
        ///
        /// This is a period-separated path to the field.
        field: String,

        /// Expected data type.
        expected_ty: String,

        /// Actual data type.
        actual_ty: String,
    },

    /// Generic configuration error.
    #[snafu(display("Failed to query configuration."))]
    Generic {
        /// Error source.
        source: GenericError,
    },

    /// Secrets resolution error.
    #[snafu(display("Failed to resolve secrets."))]
    Secrets {
        /// Error source.
        source: secrets::Error,
    },
}

impl From<figment::Error> for ConfigurationError {
    fn from(e: figment::Error) -> Self {
        match e.kind {
            Kind::InvalidType(actual_ty, expected_ty) => Self::InvalidFieldType {
                field: e.path.join("."),
                expected_ty,
                actual_ty: actual_ty.to_string(),
            },
            _ => Self::Generic { source: e.into() },
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum LookupSource {
    /// The configuration key is looked up in a form suitable for environment variables.
    Environment { prefix: String },
}

impl LookupSource {
    fn transform_key(&self, key: &str) -> String {
        match self {
            // The prefix should already be uppercased, with a trailing underscore, which is needed when we actually
            // configure the provider used for reading from the environment... so we don't need to re-do that here.
            LookupSource::Environment { prefix } => format!("{}{}", prefix, key.replace('.', "_").to_uppercase()),
        }
    }
}

/// A configuration loader that can pull from various sources.
///
/// This loader provides a wrapper around a lower-level library, `figment`, to expose a simpler and focused API for both
/// loading configuration data from various sources, as well as querying it.
///
/// A variety of configuration sources can be configured (see below), with an implicit priority based on the order in
/// which sources are added: sources added later take precedence over sources prior. Additionally, either a typed value
/// can be extracted from the configuration ([`into_typed`][Self::into_typed]), or the raw configuration data can be
/// accessed via a generic API ([`into_generic`][Self::into_generic]).
///
/// # Supported sources
///
/// - YAML file
/// - JSON file
/// - environment variables (must be prefixed; see [`from_environment`][Self::from_environment])
#[derive(Clone, Default)]
pub struct ConfigurationLoader {
    lookup_sources: HashSet<LookupSource>,
    provider_sources: Vec<ProviderSource>,
}

impl ConfigurationLoader {
    /// Loads the given YAML configuration file.
    ///
    /// # Errors
    ///
    /// If the file could not be read, or if the file is not valid YAML, an error will be returned.
    pub fn from_yaml<P>(mut self, path: P) -> Result<Self, ConfigurationError>
    where
        P: AsRef<std::path::Path>,
    {
        let resolved_provider = ResolvedProvider::from_yaml(&path).context(Generic)?;
        self.provider_sources
            .push(ProviderSource::Static(ArcProvider(Arc::new(resolved_provider))));
        Ok(self)
    }

    /// Attempts to load the given YAML configuration file, ignoring any errors.
    ///
    /// Errors include the file not existing, not being readable/accessible, and not being valid YAML.
    pub fn try_from_yaml<P>(mut self, path: P) -> Self
    where
        P: AsRef<std::path::Path>,
    {
        match ResolvedProvider::from_yaml(&path) {
            Ok(resolved_provider) => {
                self.provider_sources
                    .push(ProviderSource::Static(ArcProvider(Arc::new(resolved_provider))));
            }
            Err(e) => {
                tracing::debug!(error = %e, file_path = %path.as_ref().to_string_lossy(), "Unable to read YAML configuration file. Ignoring.");
            }
        }
        self
    }

    /// Loads the given JSON configuration file.
    ///
    /// # Errors
    ///
    /// If the file could not be read, or if the file is not valid JSON, an error will be returned.
    pub fn from_json<P>(mut self, path: P) -> Result<Self, ConfigurationError>
    where
        P: AsRef<std::path::Path>,
    {
        let resolved_provider = ResolvedProvider::from_json(&path).context(Generic)?;
        self.provider_sources
            .push(ProviderSource::Static(ArcProvider(Arc::new(resolved_provider))));
        Ok(self)
    }

    /// Attempts to load the given JSON configuration file, ignoring any errors.
    ///
    /// Errors include the file not existing, not being readable/accessible, and not being valid JSON.
    pub fn try_from_json<P>(mut self, path: P) -> Self
    where
        P: AsRef<std::path::Path>,
    {
        match ResolvedProvider::from_json(&path) {
            Ok(resolved_provider) => {
                self.provider_sources
                    .push(ProviderSource::Static(ArcProvider(Arc::new(resolved_provider))));
            }
            Err(e) => {
                tracing::debug!(error = %e, file_path = %path.as_ref().to_string_lossy(), "Unable to read JSON configuration file. Ignoring.");
            }
        }
        self
    }

    /// Loads configuration from environment variables.
    ///
    /// The prefix given will have an underscore appended to it if it does not already end with one. For example, with a
    /// prefix of `app`, any environment variable starting with `app_` would be matched.
    ///
    /// The prefix is case-insensitive.
    ///
    /// # Errors
    ///
    /// If the prefix is empty, an error will be returned.
    pub fn from_environment(mut self, prefix: &'static str) -> Result<Self, ConfigurationError> {
        if prefix.is_empty() {
            return Err(ConfigurationError::EmptyPrefix);
        }

        let prefix = if prefix.ends_with('_') {
            prefix.to_string()
        } else {
            format!("{}_", prefix)
        };

        // Convert to use Serialized::defaults since, Env isn't Send + Sync
        let env = Env::prefixed(&prefix);
        let values = env.data().unwrap();
        if let Some(default_dict) = values.get(&figment::Profile::Default) {
            self.provider_sources
                .push(ProviderSource::Static(ArcProvider(Arc::new(Serialized::defaults(
                    default_dict.clone(),
                )))));
            self.lookup_sources.insert(LookupSource::Environment { prefix });
        }
        Ok(self)
    }

    /// Resolves secrets in the configuration based on available secret backend configuration.
    ///
    /// This will attempt to resolve any secret references (format shown below) in the configuration by using a "secrets
    /// backend", which is a user-provided command that utilizes a simple JSON-based protocol to accept secrets to
    /// resolve, and return those resolved secrets, or the errors that occurred during resolving.
    ///
    /// # Configuration
    ///
    /// This method uses the existing configuration (see Caveats) to determine the secrets backend configuration. The
    /// following configuration settings are used:
    ///
    /// - `secret_backend_command`: The executable to resolve secrets. (required)
    /// - `secret_backend_timeout`: The timeout for the secrets backend command, in seconds. (optional, default: 30)
    ///
    /// # Usage
    ///
    /// For any value which should be resolved as a secret, the value should be a string in the format of
    /// `ENC[secret_reference]`. The `secret_reference` portion is the value that will be sent to the backend command
    /// during resolution. There is no limitation on the format of the `secret_reference` value, so long as it can be
    /// expressed through the existing configuration sources (YAML, environment variables, etc).
    ///
    /// The entire configuration value must match this pattern, and cannot be used to replace only part of a value, so
    /// values such as `db-ENC[secret_reference]` would not be detected as secrets and thus would not be resolved.
    ///
    /// # Protocol
    ///
    /// The executable is expected to accept a JSON object on stdin, with the following format:
    ///
    /// ```json
    /// {
    ///   "version": "1.0",
    ///   "secrets": ["key1", "key2"]
    /// }
    /// ```
    ///
    /// The executable is expected return a JSON object on stdout, with the following format:
    /// ```json
    /// {
    ///   "key1": {
    ///     "value": "my_secret_password",
    ///     "error": null
    ///   },
    ///   "key2": {
    ///     "value": null,
    ///     "error": "could not fetch the secret"
    ///   }
    /// }
    /// ```
    ///
    /// If any entry in the response has an `error` value that is anything but `null`, the overall resolution will be
    /// considered failed.
    ///
    /// # Caveats
    ///
    /// ## Time of resolution
    ///
    /// Secrets resolution happens at the time this method is called, and only resolves configuration values that are
    /// already present in the configuration, which means all calls to load configuration (`try_from_yaml`,
    /// `from_environment`, etc) must be made before calling this method.
    ///
    /// ## Sensitive data in error output
    ///
    /// Care should be taken to not return sensitive information in either the error output (standard error) of the
    /// backend command or the `error` field in the JSON response, as these values are logged in order to aid debugging.
    pub async fn with_default_secrets_resolution(mut self) -> Result<Self, ConfigurationError> {
        let initial_figment = build_figment_from_sources(&self.provider_sources);

        // If no secrets backend is set, we can't resolve secrets, so just return early.
        if !initial_figment.contains("secret_backend_command") {
            debug!("No secrets backend configured; skipping secrets resolution.");
            return Ok(self);
        }

        let resolver_config = initial_figment.extract::<secrets::resolver::ExternalProcessResolverConfiguration>()?;
        let resolver = secrets::resolver::ExternalProcessResolver::from_configuration(resolver_config)
            .await
            .context(Secrets)?;

        let provider = secrets::Provider::new(resolver, &initial_figment)
            .await
            .context(Secrets)?;

        self.provider_sources
            .push(ProviderSource::Static(ArcProvider(Arc::new(provider))));
        Ok(self)
    }

    /// Enables dynamic configuration.
    ///
    /// The receiver is used in `run_dynamic_config_updater` to handle retrieving the initial snapshot and subsequent updates.
    pub fn with_dynamic_configuration(mut self, receiver: mpsc::Receiver<ConfigUpdate>) -> Self {
        self.provider_sources.push(ProviderSource::Dynamic(Some(receiver)));
        self
    }

    /// Consumes the configuration loader, deserializing it as `T`.
    ///
    /// ## Errors
    ///
    /// If the configuration could not be deserialized into `T`, an error will be returned.
    pub fn into_typed<'a, T>(self) -> Result<T, ConfigurationError>
    where
        T: Deserialize<'a>,
    {
        let figment = build_figment_from_sources(&self.provider_sources);
        figment.extract().map_err(Into::into)
    }

    /// Creates a bootstrap `GenericConfiguration` without consuming the loader.
    ///
    /// This creates a static snapshot of the configuration loaded so far. As this is intended for bootstrapping
    /// before dynamic configuration is active, the dynamic provider is ignored.
    pub fn bootstrap_generic(&self) -> Result<GenericConfiguration, ConfigurationError> {
        let figment = build_figment_from_sources(&self.provider_sources);

        Ok(GenericConfiguration {
            inner: Arc::new(Inner {
                figment: RwLock::new(figment),
                lookup_sources: self.lookup_sources.clone(),
                event_sender: None,
                ready_signal: Mutex::new(None),
            }),
        })
    }

    /// Consumes the configuration loader and wraps it in a generic wrapper.
    pub async fn into_generic(mut self) -> Result<GenericConfiguration, ConfigurationError> {
        let has_dynamic_provider = self
            .provider_sources
            .iter()
            .any(|s| matches!(s, ProviderSource::Dynamic(_)));

        if has_dynamic_provider {
            let mut receiver_opt = None;
            for source in self.provider_sources.iter_mut() {
                if let ProviderSource::Dynamic(ref mut receiver) = source {
                    receiver_opt = receiver.take();
                    break;
                }
            }
            let receiver = receiver_opt.expect("Dynamic receiver should exist but was not found");

            // Build the initial figment object from the static providers. The dynamic provider is empty for now.
            let figment = build_figment_from_sources(&self.provider_sources);

            let (event_sender, _) = broadcast::channel(100);
            let (ready_sender, ready_receiver) = oneshot::channel();

            let generic_config = GenericConfiguration {
                inner: Arc::new(Inner {
                    figment: RwLock::new(figment),
                    lookup_sources: self.lookup_sources,
                    event_sender: Some(event_sender.clone()),
                    ready_signal: Mutex::new(Some(ready_receiver)),
                }),
            };

            // Spawn the background task to handle retrieving the initial snapshot and subsequent updates.
            tokio::spawn(run_dynamic_config_updater(
                generic_config.inner.clone(),
                receiver,
                self.provider_sources,
                event_sender,
                ready_sender,
            ));

            Ok(generic_config)
        } else {
            // Otherwise, just build the static configuration.
            let figment = build_figment_from_sources(&self.provider_sources);

            Ok(GenericConfiguration {
                inner: Arc::new(Inner {
                    figment: RwLock::new(figment),
                    lookup_sources: self.lookup_sources,
                    event_sender: None,
                    ready_signal: Mutex::new(None),
                }),
            })
        }
    }

    /// Configures a [`GenericConfiguration`] that is suitable for tests.
    ///
    /// This configures the loader with the following defaults:
    ///
    /// - configuration from a JSON file
    /// - configuration from environment variables
    ///
    /// If `enable_dynamic_configuration` is true, a dynamic configuration sender is returned.
    ///
    /// This is generally only useful for testing purposes, and is exposed publicly in order to be used in cross-crate testing scenarios.
    pub async fn for_tests(
        enable_dynamic_configuration: bool,
    ) -> (GenericConfiguration, Option<tokio::sync::mpsc::Sender<ConfigUpdate>>) {
        let json_file = tempfile::NamedTempFile::new().expect("should not fail to create temp file.");
        let path = &json_file.path();
        let json = serde_json::json!({
            "foo": "bar",
            "baz": 5,
            "foobar": {
                "a": false,
                "b": "c",
            }
        });
        serde_json::to_writer(&json_file, &json).expect("should not fail to write to temp file.");

        std::env::set_var("test_env_var", "from_env");
        let mut loader = ConfigurationLoader::default().try_from_json(path);
        let mut maybe_sender = None;
        if enable_dynamic_configuration {
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            loader = loader.with_dynamic_configuration(receiver);
            maybe_sender = Some(sender);
        }

        // Add environment provider last so it has the highest precedence.
        let cfg = loader
            .from_environment("test")
            .expect("should not fail to add environment provider")
            .into_generic()
            .await
            .expect("should not fail to build generic configuration");

        std::env::remove_var("test_env_var");

        (cfg, maybe_sender)
    }
}

fn build_figment_from_sources(sources: &[ProviderSource]) -> Figment {
    sources.iter().fold(Figment::new(), |figment, source| match source {
        ProviderSource::Static(p) => figment.admerge(p.clone()),
        // No-op. The merging is handled by the updater task.
        ProviderSource::Dynamic(_) => figment,
    })
}

/// Inserts or updates a value for a key.
///
/// Intermediate objects are created if they don't exist.
fn upsert(root: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if !root.is_object() {
        *root = serde_json::Value::Object(serde_json::Map::new());
    }

    let mut current = root;
    // Create a new node for each segment if the key is dotted.
    let mut segments = key.split('.').peekable();

    while let Some(seg) = segments.next() {
        let is_leaf = segments.peek().is_none();

        // Ensure current is an object before operating
        if !current.is_object() {
            *current = serde_json::Value::Object(serde_json::Map::new());
        }
        let node = current.as_object_mut().expect("current node should be an object");

        if is_leaf {
            node.insert(seg.to_string(), value);
            break;
        } else {
            // Ensure child exists and is an object
            let should_create_node = match node.get(seg) {
                Some(v) => !v.is_object(),
                None => true,
            };
            // Check if we need to create an intermediate node if it doesn't exist.
            if should_create_node {
                node.insert(seg.to_string(), serde_json::Value::Object(serde_json::Map::new()));
            }

            // Advance the current node to the next level.
            current = node.get_mut(seg).expect("should not fail to get nested object");
        }
    }
}

async fn run_dynamic_config_updater(
    inner: Arc<Inner>, mut receiver: mpsc::Receiver<ConfigUpdate>, provider_sources: Vec<ProviderSource>,
    sender: broadcast::Sender<ConfigChangeEvent>, ready_sender: oneshot::Sender<()>,
) {
    // The first message on the channel will be the initial snapshot.
    let initial_update = match receiver.recv().await {
        Some(update) => update,
        None => {
            // The channel was closed before we even received the initial snapshot.
            debug!("Dynamic configuration channel closed before initial snapshot.");
            return;
        }
    };

    let mut dynamic_state = match initial_update {
        ConfigUpdate::Snapshot(state) => state,
        ConfigUpdate::Partial { .. } => {
            // This is theoretically unreachable, as `configstream` should always send a snapshot first.
            error!("First dynamic config message was not a snapshot. Updater may be in an inconsistent state.");
            serde_json::Value::Null
        }
    };

    // Rebuild the configuration with the initial snapshot.
    let new_figment = provider_sources
        .iter()
        .fold(Figment::new(), |figment, source| match source {
            ProviderSource::Static(p) => figment.admerge(p.clone()),
            ProviderSource::Dynamic(_) => {
                figment.admerge(figment::providers::Serialized::defaults(dynamic_state.clone()))
            }
        });

    // Update the main figment object and then release the lock.
    {
        let mut figment_guard = inner.figment.write().unwrap();
        *figment_guard = new_figment.clone();
    }

    // Signal that the initial snapshot has been processed and the configuration is ready.
    if ready_sender.send(()).is_err() {
        debug!("Configuration readiness receiver dropped. Updater task shutting down.");
        return;
    }

    // Set our "current" state for the main loop.
    let mut current_config: figment::value::Value = new_figment.extract().unwrap();

    // Enter the main loop to process subsequent updates.
    loop {
        let update = match receiver.recv().await {
            Some(update) => update,
            None => {
                // The sender was dropped, which means the config stream has terminated. We can exit.
                debug!("Dynamic configuration update channel closed. Updater task shutting down.");
                return;
            }
        };

        // Update our local dynamic state based on the received message.
        match update {
            ConfigUpdate::Snapshot(new_state) => {
                dynamic_state = new_state;
            }
            ConfigUpdate::Partial { key, value } => {
                if dynamic_state.is_null() {
                    dynamic_state = serde_json::Value::Object(serde_json::Map::new());
                }
                if dynamic_state.is_object() {
                    upsert(&mut dynamic_state, &key, value);
                } else {
                    error!(
                        "Received partial update but current dynamic state is not an object. This should not happen."
                    );
                }
            }
        }

        // Rebuild the figment object on every update, respecting the original provider order.
        let new_figment = provider_sources
            .iter()
            .fold(Figment::new(), |figment, source| match source {
                ProviderSource::Static(p) => figment.admerge(p.clone()),
                ProviderSource::Dynamic(_) => {
                    figment.admerge(figment::providers::Serialized::defaults(dynamic_state.clone()))
                }
            });

        let new_config: figment::value::Value = new_figment.clone().extract().unwrap();

        if current_config != new_config {
            for change in dynamic::diff_config(&current_config, &new_config) {
                // Send the change event to any receivers of the dynamic handler.
                // If there are no receivers, `send` will fail. This is expected and fine,
                // so we can ignore the error to avoid log spam.
                let _ = sender.send(change);
            }

            let mut figment_guard = inner.figment.write().unwrap_or_else(|e| {
                error!("Failed to acquire write lock for dynamic configuration: {}", e);
                e.into_inner()
            });
            *figment_guard = new_figment;

            // Update our "current" state for the next iteration.
            current_config = new_config;
        }
    }
}

#[derive(Debug)]
struct Inner {
    figment: RwLock<Figment>,
    lookup_sources: HashSet<LookupSource>,
    event_sender: Option<broadcast::Sender<ConfigChangeEvent>>,
    ready_signal: Mutex<Option<oneshot::Receiver<()>>>,
}

/// A generic configuration object.
///
/// This represents the merged configuration derived from [`ConfigurationLoader`] in its raw form.  Values can be
/// queried by key, and can be extracted either as typed values or in their raw form.
///
/// Keys must be in the form of `a.b.c`, where periods (`.`) as used to indicate a nested value.
///
/// Using an example JSON configuration:
///
/// ```json
/// {
///   "a": {
///     "b": {
///       "c": "value"
///     }
///   }
/// }
/// ```
///
/// Querying for the value of `a.b.c` would return `"value"`, and querying for `a.b` would return the nested object `{
/// "c": "value" }`.
#[derive(Clone, Debug)]
pub struct GenericConfiguration {
    inner: Arc<Inner>,
}

impl GenericConfiguration {
    /// Waits for the configuration to be ready, if dynamic configuration is enabled.
    ///
    /// If dynamic configuration is in use, this method will asynchronously wait until the first snapshot has been
    /// received and applied.
    ///
    /// If dynamic configuration is not used, it returns immediately.
    pub async fn ready(&self) {
        // We need a lock to both ensure that multiple callers can race against this,
        // and to allow us mutable access to consume the receiver.
        let mut maybe_ready_rx = self.inner.ready_signal.lock().await;
        if let Some(ready_rx) = maybe_ready_rx.take() {
            // We're the first caller to wait for readiness.
            if ready_rx.await.is_err() {
                error!("Failed to receive configuration readiness signal; updater task may have panicked.");
            }
        }
    }

    fn get<'a, T>(&self, key: &str) -> Result<T, ConfigurationError>
    where
        T: Deserialize<'a>,
    {
        let figment_guard = self.inner.figment.read().unwrap();
        match figment_guard.extract_inner(key) {
            Ok(value) => Ok(value),
            Err(e) => {
                if matches!(e.kind, figment::error::Kind::MissingField(_)) {
                    // We might have been given a key that uses nested notation -- `foo.bar` -- but is only present in the
                    // environment variables. We specifically don't want to use a different separator in environment
                    // variables to map to nested key separators, so we simply try again here but with all nested key
                    // separators (`.`) replaced with `_`, to match environment variables.
                    let fallback_key = key.replace('.', "_");
                    figment_guard
                        .extract_inner(&fallback_key)
                        .map_err(|fallback_e| from_figment_error(&self.inner.lookup_sources, fallback_e))
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Gets a configuration value by key.
    ///
    /// The key must be in the form of `a.b.c`, where periods (`.`) are used to indicate a nested lookup.
    ///
    /// ## Errors
    ///
    /// If the key does not exist in the configuration, or if the value could not be deserialized into `T`, an error
    /// variant will be returned.
    pub fn get_typed<'a, T>(&self, key: &str) -> Result<T, ConfigurationError>
    where
        T: Deserialize<'a>,
    {
        self.get(key)
    }

    /// Gets a configuration value by key, or the default value if a key does not exist or could not be deserialized.
    ///
    /// The `Default` implementation of `T` will be used both if the key could not be found, as well as for any error
    /// during deserialization. This effectively swallows any errors and should generally be used sparingly.
    ///
    /// The key must be in the form of `a.b.c`, where periods (`.`) are used to indicate a nested lookup.
    pub fn get_typed_or_default<'a, T>(&self, key: &str) -> T
    where
        T: Default + Deserialize<'a>,
    {
        self.get(key).unwrap_or_default()
    }

    /// Gets a configuration value by key, if it exists.
    ///
    /// If the key exists in the configuration, and can be deserialized, `Ok(Some(value))` is returned. Otherwise,
    /// `Ok(None)` will be returned.
    ///
    /// The key must be in the form of `a.b.c`, where periods (`.`) are used to indicate a nested lookup.
    ///
    /// ## Errors
    ///
    /// If the value could not be deserialized into `T`, an error will be returned.
    pub fn try_get_typed<'a, T>(&self, key: &str) -> Result<Option<T>, ConfigurationError>
    where
        T: Deserialize<'a>,
    {
        match self.get(key) {
            Ok(value) => Ok(Some(value)),
            Err(ConfigurationError::MissingField { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Attempts to deserialize the entire configuration as `T`.
    ///
    /// ## Errors
    ///
    /// If the value could not be deserialized into `T`, an error will be returned.
    pub fn as_typed<'a, T>(&self) -> Result<T, ConfigurationError>
    where
        T: Deserialize<'a>,
    {
        self.inner
            .figment
            .read()
            .unwrap()
            .extract()
            .map_err(|e| from_figment_error(&self.inner.lookup_sources, e))
    }

    /// Subscribes for updates to the configuration.
    pub fn subscribe_for_updates(&self) -> Option<broadcast::Receiver<dynamic::ConfigChangeEvent>> {
        self.inner.event_sender.as_ref().map(|s| s.subscribe())
    }

    /// Creates a watcher that yields only when the given key changes.
    ///
    /// If dynamic configuration is disabled, the returned watcher's `changed()`
    /// will wait indefinitely.
    pub fn watch_for_updates(&self, key: &str) -> FieldUpdateWatcher {
        FieldUpdateWatcher {
            key: key.to_string(),
            rx: self.subscribe_for_updates(),
        }
    }
}

fn from_figment_error(lookup_sources: &HashSet<LookupSource>, e: figment::Error) -> ConfigurationError {
    match e.kind {
        Kind::MissingField(field) => {
            let mut valid_keys = lookup_sources
                .iter()
                .map(|source| source.transform_key(&field))
                .collect::<Vec<_>>();

            // Always specify the original key as a valid key to try.
            valid_keys.insert(0, field.to_string());

            let help_text = format!("Try setting `{}`.", valid_keys.join("` or `"));

            ConfigurationError::MissingField { help_text, field }
        }
        Kind::InvalidType(actual_ty, expected_ty) => ConfigurationError::InvalidFieldType {
            field: e.path.join("."),
            expected_ty,
            actual_ty: actual_ty.to_string(),
        },
        _ => ConfigurationError::Generic { source: e.into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_static_configuration() {
        let (cfg, _) = ConfigurationLoader::for_tests(false).await;
        cfg.ready().await;

        assert_eq!(cfg.get_typed::<String>("foo").unwrap(), "bar");
        assert_eq!(cfg.get_typed::<i32>("baz").unwrap(), 5);
        assert!(!cfg.get_typed::<bool>("foobar.a").unwrap());
        assert_eq!(cfg.get_typed::<String>("env_var").unwrap(), "from_env");
        assert!(matches!(
            cfg.get::<String>("nonexistentKey"),
            Err(ConfigurationError::MissingField { .. })
        ));
    }

    #[tokio::test]
    async fn test_dynamic_configuration() {
        let (cfg, sender) = ConfigurationLoader::for_tests(true).await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({
                "new": "from_snapshot",
            })))
            .await
            .unwrap();

        cfg.ready().await;

        // Test that existing values still exist.
        assert_eq!(cfg.get_typed::<String>("foo").unwrap(), "bar");

        // Test that new values from the snapshot exist.
        assert_eq!(cfg.get_typed::<String>("new").unwrap(), "from_snapshot");

        let mut rx = cfg.subscribe_for_updates().expect("dynamic updates should be enabled");

        sender
            .send(ConfigUpdate::Partial {
                key: "new_key".to_string(),
                value: "from dynamic update".to_string().into(),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.key == "new_key" => break ev,
                    Err(e) => panic!("updates channel closed: {e}"),
                    Ok(_) => continue,
                }
            }
        })
        .await
        .expect("timed out waiting for new_key update");

        assert_eq!(cfg.get_typed::<String>("new_key").unwrap(), "from dynamic update");

        // Test that an update with a nested key is applied.
        sender
            .send(ConfigUpdate::Partial {
                key: "foobar.a".to_string(),
                value: serde_json::json!(true),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.key == "foobar.a" => break ev,
                    Err(e) => panic!("updates channel closed: {e}"),
                    Ok(_) => continue,
                }
            }
        })
        .await
        .expect("timed out waiting for foobar.a update");

        assert!(cfg.get_typed::<bool>("foobar.a").unwrap());
        assert_eq!(cfg.get_typed::<String>("foobar.b").unwrap(), "c");
    }

    #[tokio::test]
    async fn test_environment_precedence_over_dynamic() {
        let (cfg, sender) = ConfigurationLoader::for_tests(true).await;
        let sender = sender.expect("sender should exist");

        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({
                "env_var": "from_snapshot_env_var"
            })))
            .await
            .unwrap();

        cfg.ready().await;

        // Env provider has highest precedence so the snapshot should not override it.
        assert_eq!(cfg.get_typed::<String>("env_var").unwrap(), "from_env");

        let mut rx = cfg.subscribe_for_updates().expect("dynamic updates should be enabled");

        // Send a partial update that attempts to override the env-backed key.
        sender
            .send(ConfigUpdate::Partial {
                key: "env_var".to_string(),
                value: serde_json::json!("from_partial"),
            })
            .await
            .unwrap();

        // Also attempt to override the nested env-backed key via dynamic.
        sender
            .send(ConfigUpdate::Partial {
                key: "foobar.a".to_string(),
                value: serde_json::json!(false),
            })
            .await
            .unwrap();

        // Send a dummy partial update to ensure the updater has processed prior partials.
        sender
            .send(ConfigUpdate::Partial {
                key: "dummy".to_string(),
                value: serde_json::json!(1),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.key == "dummy" => break,
                    Err(e) => panic!("updates channel closed: {e}"),
                    Ok(_) => continue,
                }
            }
        })
        .await
        .expect("timed out waiting for sync marker");

        assert_eq!(cfg.get_typed::<String>("env_var").unwrap(), "from_env");
    }

    #[tokio::test]
    async fn test_dynamic_configuration_add_new_nested_key() {
        let (cfg, sender) = ConfigurationLoader::for_tests(true).await;
        let sender = sender.expect("sender should exist");

        sender
            .send(ConfigUpdate::Snapshot(serde_json::json!({})))
            .await
            .unwrap();
        cfg.ready().await;

        let mut rx = cfg.subscribe_for_updates().expect("dynamic updates should be enabled");

        sender
            .send(ConfigUpdate::Partial {
                key: "new_parent.new_child".to_string(),
                value: serde_json::json!(42),
            })
            .await
            .unwrap();

        // new_parent object did not exist before, so the diff will emit the object "new_parent"
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.key == "new_parent" => break ev,
                    Err(e) => panic!("updates channel closed: {e}"),
                    Ok(_) => continue,
                }
            }
        })
        .await
        .expect("timed out waiting for new_parent.new_child update");

        assert_eq!(cfg.get_typed::<i32>("new_parent.new_child").unwrap(), 42);
    }

    #[tokio::test]
    async fn test_underscore_fallback_on_get() {
        std::env::set_var("TEST_RANDOM_KEY", "from_env_only");

        let (cfg, _) = ConfigurationLoader::for_tests(false).await;
        cfg.ready().await;

        assert_eq!(cfg.get_typed::<String>("random.key").unwrap(), "from_env_only");

        std::env::remove_var("TEST_RANDOM_KEY");
    }

    #[tokio::test]
    async fn test_static_configuration_ready_and_subscribe() {
        let (cfg, maybe_sender) = ConfigurationLoader::for_tests(false).await;
        assert!(maybe_sender.is_none());

        tokio::time::timeout(std::time::Duration::from_millis(500), cfg.ready())
            .await
            .expect("ready() should not block when dynamic is disabled");

        assert!(cfg.subscribe_for_updates().is_none());
    }

    #[tokio::test]
    async fn test_dynamic_configuration_ready_requires_initial_snapshot() {
        // Enable dynamic but do not send the initial snapshot.
        let (cfg, maybe_sender) = ConfigurationLoader::for_tests(true).await;
        assert!(maybe_sender.is_some());

        // ready() should not resolve until the initial snapshot is processed.
        let res = tokio::time::timeout(std::time::Duration::from_millis(1000), cfg.ready()).await;
        assert!(res.is_err(), "ready() should time out without an initial snapshot");
    }
}
