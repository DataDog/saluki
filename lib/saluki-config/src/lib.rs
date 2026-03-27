//! Primitives for working with typed and untyped configuration data.
#![deny(warnings)]
#![deny(missing_docs)]

use std::sync::{Arc, OnceLock, RwLock};
use std::{borrow::Cow, collections::HashSet};

use saluki_error::GenericError;
use serde::de::DeserializeOwned;
use snafu::{ResultExt as _, Snafu};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tracing::{debug, error};

pub mod dynamic;
mod provider;
mod secrets;

/// Re-exports of `facet_value` types used in the public API.
///
/// These types appear in [`dynamic::ConfigUpdate`], [`dynamic::ConfigChangeEvent`], [`upsert`], and other public
/// interfaces. Consumers should use this module instead of depending on `facet_value` directly.
pub mod value {
    pub use facet_value::{value, VArray, VObject, Value};
}

use facet_value::{VObject, Value};

pub use self::dynamic::FieldUpdateWatcher;
use self::dynamic::{ConfigChangeEvent, ConfigUpdate};

enum LayerSource {
    Static(Value),
    Dynamic(Option<mpsc::Receiver<ConfigUpdate>>),
}

impl Clone for LayerSource {
    fn clone(&self) -> Self {
        match self {
            Self::Static(v) => Self::Static(v.clone()),
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
    #[snafu(transparent)]
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
/// This loader provides a wrapper to expose a simpler and focused API for both loading configuration data from various
/// sources, as well as querying it.
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
    key_aliases: &'static [(&'static str, &'static str)],
    lookup_sources: HashSet<LookupSource>,
    layer_sources: Vec<LayerSource>,
}

impl ConfigurationLoader {
    /// Sets key aliases to apply when loading file-based configuration sources.
    ///
    /// Each entry is `(nested_path, flat_key)`. When a YAML or JSON file contains a value at `nested_path`
    /// (dot-separated), that value is also emitted under `flat_key` at the top level — but only if `flat_key`
    /// is not already explicitly set at the top level. This ensures that both YAML nested format and flat env var
    /// format produce the same Figment key, so source precedence (env vars > file) works correctly.
    ///
    /// Must be called before any file-loading methods ([`from_yaml`][Self::from_yaml], etc.) to take effect.
    pub fn with_key_aliases(mut self, aliases: &'static [(&'static str, &'static str)]) -> Self {
        self.key_aliases = aliases;
        self
    }

    /// Appends one or more static layers to the configuration chain.
    ///
    /// Sources are merged in the order they are added: later sources take precedence over earlier ones. Call
    /// this method after any file-loading methods and before [`from_environment`][Self::from_environment] to
    /// place the added layers at the correct intermediate precedence level:
    ///
    /// ```text
    /// file layers  <  add_layers(...)  <  from_environment(...)
    /// ```
    pub fn add_layers<I>(mut self, layers: I) -> Self
    where
        I: IntoIterator<Item = Value>,
    {
        for value in layers {
            self.layer_sources.push(LayerSource::Static(value));
        }
        self
    }

    /// Loads the given YAML configuration file.
    ///
    /// # Errors
    ///
    /// If the file could not be read, or if the file is not valid YAML, an error will be returned.
    pub fn from_yaml<P>(mut self, path: P) -> Result<Self, ConfigurationError>
    where
        P: AsRef<std::path::Path>,
    {
        let value = provider::load_yaml(&path, self.key_aliases)?;
        self.layer_sources.push(LayerSource::Static(value));
        Ok(self)
    }

    /// Attempts to load the given YAML configuration file, ignoring any errors.
    ///
    /// Errors include the file not existing, not being readable/accessible, and not being valid YAML.
    pub fn try_from_yaml<P>(mut self, path: P) -> Self
    where
        P: AsRef<std::path::Path>,
    {
        match provider::load_yaml(&path, self.key_aliases) {
            Ok(value) => {
                self.layer_sources.push(LayerSource::Static(value));
            }
            Err(e) => {
                println!(
                    "Unable to read YAML configuration file '{}': {}. Ignoring.",
                    path.as_ref().to_string_lossy(),
                    e
                );
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
        let value = provider::load_json(&path, self.key_aliases)?;
        self.layer_sources.push(LayerSource::Static(value));
        Ok(self)
    }

    /// Attempts to load the given JSON configuration file, ignoring any errors.
    ///
    /// Errors include the file not existing, not being readable/accessible, and not being valid JSON.
    pub fn try_from_json<P>(mut self, path: P) -> Self
    where
        P: AsRef<std::path::Path>,
    {
        match provider::load_json(&path, self.key_aliases) {
            Ok(value) => {
                self.layer_sources.push(LayerSource::Static(value));
            }
            Err(e) => {
                println!(
                    "Unable to read JSON configuration file '{}': {}. Ignoring.",
                    path.as_ref().to_string_lossy(),
                    e
                );
            }
        }
        self
    }

    /// Loads configuration from environment variables.
    ///
    /// The prefix given will have an underscore appended to it if it does not already end with one. For
    /// example, with a prefix of `app`, any environment variable starting with `app_` would be matched. The
    /// prefix is case-insensitive.
    ///
    /// Sources are merged in the order they are added, with later sources taking precedence over earlier ones.
    /// Sources added after this call will have higher precedence than environment variables.
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

        // Read environment variables with the given prefix and build a nested Value tree.
        //
        // Environment variable names are split on `__` (double underscore) to create nested objects.
        // For example, with prefix `DD_`, the variable `DD_FOO__BAR=baz` becomes `{ "foo": { "bar": "baz" } }`.
        //
        // TODO(env-var-munching): The current approach requires `__` for nesting, which is awkward for deeply nested
        // config. A future improvement should use `facet_solver::Schema::build(T::SHAPE).known_paths()` to
        // enable "opportunistic segment munching" — where a flat env var like `DD_FOO_BAR_QUACK_QUACK` can be
        // automatically matched to `foo.bar.quack_quack` or `foo_bar.quack_quack` by trying all possible segment
        // groupings against the known paths of the target type. This requires the target type's Shape to be available
        // at the point of env var loading, which means it must happen during typed extraction (`as_typed::<T>()`) or
        // a new method that accepts a type parameter. See `facet_solver::Schema` and `Resolution::known_paths()` for
        // the building blocks.
        let prefix_upper = prefix.to_uppercase();
        let mut obj = VObject::new();

        for (key, value) in std::env::vars() {
            let key_upper = key.to_uppercase();
            if !key_upper.starts_with(&prefix_upper) {
                continue;
            }

            // Strip the prefix and split on `__` for nesting.
            let stripped = &key[prefix.len()..];
            let segments: Vec<&str> = stripped.split("__").collect();

            // Build nested object structure.
            insert_nested_env_var(&mut obj, &segments, Value::from(value));
        }

        if !obj.is_empty() {
            self.layer_sources.push(LayerSource::Static(Value::from(obj)));
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
        let configuration = build_merged_value(&self.layer_sources);

        // If no secrets backend is set, we can't resolve secrets, so just return early.
        if !has_valid_secret_backend_command(&configuration) {
            debug!("No secrets backend configured; skipping secrets resolution.");
            return Ok(self);
        }

        let resolver_config: secrets::resolver::ExternalProcessResolverConfiguration =
            facet_value::from_value(configuration.clone())
                .map_err(|e| ConfigurationError::Generic { source: e.into() })?;
        let resolver = secrets::resolver::ExternalProcessResolver::from_configuration(resolver_config)
            .await
            .context(Secrets)?;

        let overlay = secrets::SecretsOverlay::new(resolver, &configuration)
            .await
            .context(Secrets)?;

        self.layer_sources.push(LayerSource::Static(overlay.into_value()));
        Ok(self)
    }

    /// Enables dynamic configuration.
    ///
    /// The receiver is used in `run_dynamic_config_updater` to handle retrieving the initial snapshot and subsequent updates.
    pub fn with_dynamic_configuration(mut self, receiver: mpsc::Receiver<ConfigUpdate>) -> Self {
        self.layer_sources.push(LayerSource::Dynamic(Some(receiver)));
        self
    }

    /// Consumes the configuration loader, deserializing it as `T`.
    ///
    /// ## Errors
    ///
    /// If the configuration could not be deserialized into `T`, an error will be returned.
    pub fn into_typed<T>(self) -> Result<T, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        let merged = build_merged_value(&self.layer_sources);
        let json = value_to_json(&merged);
        serde_json::from_value(json).map_err(|e| ConfigurationError::Generic { source: e.into() })
    }

    /// Creates a bootstrap `GenericConfiguration` without consuming the loader.
    ///
    /// This creates a static snapshot of the configuration loaded so far. As this is intended for bootstrapping
    /// before dynamic configuration is active, the dynamic provider is ignored.
    pub fn bootstrap_generic(&self) -> GenericConfiguration {
        let merged = build_merged_value(&self.layer_sources);

        GenericConfiguration {
            inner: Arc::new(Inner {
                config: RwLock::new(merged),
                lookup_sources: self.lookup_sources.clone(),
                event_sender: None,
                ready_signal: Mutex::new(None),
            }),
        }
    }

    /// Consumes the configuration loader and wraps it in a generic wrapper.
    pub async fn into_generic(mut self) -> Result<GenericConfiguration, ConfigurationError> {
        let has_dynamic_provider = self.layer_sources.iter().any(|s| matches!(s, LayerSource::Dynamic(_)));

        if has_dynamic_provider {
            let mut receiver_opt = None;
            for source in self.layer_sources.iter_mut() {
                if let LayerSource::Dynamic(ref mut receiver) = source {
                    receiver_opt = receiver.take();
                    break;
                }
            }
            let receiver = receiver_opt.expect("Dynamic receiver should exist but was not found");

            // Build the initial merged value from the static layers. The dynamic layer is empty for now.
            let merged = build_merged_value(&self.layer_sources);

            let (event_sender, _) = broadcast::channel(100);
            let (ready_sender, ready_receiver) = oneshot::channel();

            let generic_config = GenericConfiguration {
                inner: Arc::new(Inner {
                    config: RwLock::new(merged),
                    lookup_sources: self.lookup_sources,
                    event_sender: Some(event_sender.clone()),
                    ready_signal: Mutex::new(Some(ready_receiver)),
                }),
            };

            // Spawn the background task to handle retrieving the initial snapshot and subsequent updates.
            tokio::spawn(run_dynamic_config_updater(
                generic_config.inner.clone(),
                receiver,
                self.layer_sources,
                event_sender,
                ready_sender,
            ));

            Ok(generic_config)
        } else {
            // Otherwise, just build the static configuration.
            let merged = build_merged_value(&self.layer_sources);

            Ok(GenericConfiguration {
                inner: Arc::new(Inner {
                    config: RwLock::new(merged),
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
        file_values: Option<Value>, env_vars: Option<&[(String, String)]>, enable_dynamic_configuration: bool,
    ) -> (GenericConfiguration, Option<tokio::sync::mpsc::Sender<ConfigUpdate>>) {
        Self::for_tests_with_layer_factory(file_values, env_vars, enable_dynamic_configuration, &[], || {
            Value::from(VObject::new())
        })
        .await
    }

    /// Like [`for_tests`][Self::for_tests], but applies `key_aliases` during file loading and calls
    /// `layer_factory` to build an additional layer inserted between the file layers and the
    /// environment layer.
    ///
    /// The factory is called after test environment variables have been set, so any env var reads it performs
    /// (e.g. in `DatadogRemapper`) are consistent with the test's env setup.
    ///
    /// This is generally only useful for testing purposes, and is exposed publicly in order to be used in cross-crate testing scenarios.
    pub async fn for_tests_with_layer_factory<F>(
        file_values: Option<Value>, env_vars: Option<&[(String, String)]>,
        enable_dynamic_configuration: bool, key_aliases: &'static [(&'static str, &'static str)], layer_factory: F,
    ) -> (GenericConfiguration, Option<tokio::sync::mpsc::Sender<ConfigUpdate>>)
    where
        F: FnOnce() -> Value,
    {
        let json_file = tempfile::NamedTempFile::new().expect("should not fail to create temp file.");
        let path = &json_file.path();
        let value_to_write = file_values.unwrap_or_else(|| Value::from(VObject::new()));
        let json_str = facet_json::to_string(&value_to_write).expect("should not fail to serialize test value");
        std::fs::write(path, json_str).expect("should not fail to write to temp file.");

        let mut loader = ConfigurationLoader::default()
            .with_key_aliases(key_aliases)
            .try_from_json(path);
        let mut maybe_sender = None;
        if enable_dynamic_configuration {
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            loader = loader.with_dynamic_configuration(receiver);
            maybe_sender = Some(sender);
        }

        static ENV_MUTEX: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

        let guard = ENV_MUTEX.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap();

        if let Some(pairs) = env_vars.as_ref() {
            for (k, v) in pairs.iter() {
                // Set under both the raw name and the TEST_ prefix:
                //   - Raw name: available to any env-reading providers (e.g. DatadogRemapper)
                //   - TEST_ prefix: read by from_environment("TEST") (simulates DD_ prefix)
                std::env::set_var(k, v);
                std::env::set_var(format!("TEST_{}", k), v);
            }
        }

        // Build and insert the extra layer while env vars are set so it can snapshot them.
        let loader = loader.add_layers([layer_factory()]);

        // Add environment provider last so it has the highest precedence.
        let loader = loader
            .from_environment("TEST")
            .expect("should not fail to add environment provider");

        // Clean up test-provided env vars now that all providers have been built.
        if let Some(pairs) = env_vars.as_ref() {
            for (k, _) in pairs.iter() {
                std::env::remove_var(k);
                std::env::remove_var(format!("TEST_{}", k));
            }
        }

        drop(guard);

        let cfg = loader
            .into_generic()
            .await
            .expect("should not fail to build generic configuration");

        (cfg, maybe_sender)
    }
}

/// Inserts an environment variable value into a nested object structure.
///
/// The `segments` slice represents the path through the object hierarchy (split on `__` from the env var name),
/// with all segments lowercased for consistency with configuration key conventions.
fn insert_nested_env_var(obj: &mut VObject, segments: &[&str], value: Value) {
    if segments.is_empty() {
        return;
    }

    let key = segments[0].to_lowercase();

    if segments.len() == 1 {
        obj.insert(&key, value);
    } else {
        // Get or create the nested object.
        let nested = if let Some(existing) = obj.get_mut(&key) {
            if existing.as_object_mut().is_none() {
                *existing = Value::from(VObject::new());
            }
            existing.as_object_mut().unwrap()
        } else {
            obj.insert(&key, Value::from(VObject::new()));
            obj.get_mut(&key).unwrap().as_object_mut().unwrap()
        };

        insert_nested_env_var(nested, &segments[1..], value);
    }
}

fn build_merged_value(sources: &[LayerSource]) -> Value {
    let mut merged = Value::from(VObject::new());
    for source in sources {
        match source {
            LayerSource::Static(value) => deep_merge(&mut merged, value),
            // No-op. The merging is handled by the updater task.
            LayerSource::Dynamic(_) => {}
        }
    }
    merged
}

/// Recursively merges `overlay` into `base`.
///
/// When both values are objects, keys from `overlay` are recursively merged into `base`.
/// For all other cases, the overlay value replaces the base value.
fn deep_merge(base: &mut Value, overlay: &Value) {
    if let (Some(base_obj), Some(overlay_obj)) = (base.as_object_mut(), overlay.as_object()) {
        for (key, overlay_val) in overlay_obj.iter() {
            let key_str = key.as_str();
            if let Some(base_val) = base_obj.get_mut(key_str) {
                deep_merge(base_val, overlay_val);
            } else {
                base_obj.insert(key_str, overlay_val.clone());
            }
        }
    } else {
        *base = overlay.clone();
    }
}

/// Extracts a sub-value from a `Value` by following a dot-separated key path.
fn extract_at_path<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in key.split('.') {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

/// Inserts or updates a value for a key.
///
/// Intermediate objects are created if they don't exist.
pub fn upsert(root: &mut Value, key: &str, value: Value) {
    if root.as_object().is_none() {
        *root = Value::from(VObject::new());
    }

    let mut current = root;
    // Create a new node for each segment if the key is dotted.
    let mut segments = key.split('.').peekable();

    while let Some(seg) = segments.next() {
        let is_leaf = segments.peek().is_none();

        // Ensure current is an object before operating
        if current.as_object().is_none() {
            *current = Value::from(VObject::new());
        }
        let node = current.as_object_mut().expect("current node should be an object");

        if is_leaf {
            node.insert(seg, value);
            break;
        } else {
            // Ensure child exists and is an object
            let should_create_node = match node.get(seg) {
                Some(v) => v.as_object().is_none(),
                None => true,
            };
            // Check if we need to create an intermediate node if it doesn't exist.
            if should_create_node {
                node.insert(seg, Value::from(VObject::new()));
            }

            // Advance the current node to the next level.
            current = node.get_mut(seg).expect("should not fail to get nested object");
        }
    }
}

async fn run_dynamic_config_updater(
    inner: Arc<Inner>, mut receiver: mpsc::Receiver<ConfigUpdate>, layer_sources: Vec<LayerSource>,
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
            Value::NULL
        }
    };

    // Rebuild the configuration with the initial snapshot.
    let new_merged = rebuild_with_dynamic(&layer_sources, &dynamic_state);

    // Update the main config and then release the lock.
    {
        let mut config_guard = inner.config.write().unwrap();
        *config_guard = new_merged.clone();
    }

    // Signal that the initial snapshot has been processed and the configuration is ready.
    if ready_sender.send(()).is_err() {
        debug!("Configuration readiness receiver dropped. Updater task shutting down.");
        return;
    }

    // Set our "current" state for the main loop.
    let mut current_config = new_merged;

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
                debug!("Received configuration snapshot update.");
                dynamic_state = new_state;
            }
            ConfigUpdate::Partial { key, value } => {
                debug!(%key, "Received partial configuration update.");
                if dynamic_state.is_null() {
                    dynamic_state = Value::from(VObject::new());
                }
                if dynamic_state.as_object().is_some() {
                    upsert(&mut dynamic_state, &key, value);
                } else {
                    error!(
                        "Received partial update but current dynamic state is not an object. This should not happen."
                    );
                }
            }
        }

        // Rebuild the merged config on every update, respecting the original layer order.
        let new_merged = rebuild_with_dynamic(&layer_sources, &dynamic_state);

        if current_config != new_merged {
            for change in dynamic::diff_config(&current_config, &new_merged) {
                // Send the change event to any receivers of the dynamic handler.
                // If there are no receivers, `send` will fail. This is expected and fine,
                // so we can ignore the error to avoid log spam.
                let _ = sender.send(change);
            }

            let mut config_guard = inner.config.write().unwrap_or_else(|e| {
                error!("Failed to acquire write lock for dynamic configuration: {}", e);
                e.into_inner()
            });
            *config_guard = new_merged.clone();

            // Update our "current" state for the next iteration.
            current_config = new_merged;
        }
    }
}

/// Rebuilds the merged configuration from all layer sources, inserting the dynamic state at the
/// position of the dynamic layer.
fn rebuild_with_dynamic(layer_sources: &[LayerSource], dynamic_state: &Value) -> Value {
    let mut merged = Value::from(VObject::new());
    for source in layer_sources {
        match source {
            LayerSource::Static(value) => deep_merge(&mut merged, value),
            LayerSource::Dynamic(_) => deep_merge(&mut merged, dynamic_state),
        }
    }
    merged
}

#[derive(Debug)]
struct Inner {
    config: RwLock<Value>,
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

    fn get<T>(&self, key: &str) -> Result<T, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        let config_guard = self.inner.config.read().unwrap();
        match extract_at_path(&config_guard, key) {
            Some(value) => {
                let json = value_to_json(value);
                serde_json::from_value(json).map_err(|e| from_serde_error(&self.inner.lookup_sources, key, e))
            }
            None => {
                // We might have been given a key that uses nested notation -- `foo.bar` -- but is only present in the
                // environment variables. We specifically don't want to use a different separator in environment
                // variables to map to nested key separators, so we simply try again here but with all nested key
                // separators (`.`) replaced with `_`, to match environment variables.
                let fallback_key = key.replace('.', "_");
                match extract_at_path(&config_guard, &fallback_key) {
                    Some(value) => {
                        let json = value_to_json(value);
                        serde_json::from_value(json).map_err(|e| from_serde_error(&self.inner.lookup_sources, key, e))
                    }
                    None => {
                        let help_text = build_missing_field_help(&self.inner.lookup_sources, key);
                        Err(ConfigurationError::MissingField {
                            help_text,
                            field: Cow::Owned(key.to_string()),
                        })
                    }
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
    pub fn get_typed<T>(&self, key: &str) -> Result<T, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        self.get(key)
    }

    /// Gets a configuration value by key, or the default value if a key does not exist or could not be deserialized.
    ///
    /// The `Default` implementation of `T` will be used both if the key could not be found, as well as for any error
    /// during deserialization. This effectively swallows any errors and should generally be used sparingly.
    ///
    /// The key must be in the form of `a.b.c`, where periods (`.`) are used to indicate a nested lookup.
    pub fn get_typed_or_default<T>(&self, key: &str) -> T
    where
        T: Default + DeserializeOwned,
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
    pub fn try_get_typed<T>(&self, key: &str) -> Result<Option<T>, ConfigurationError>
    where
        T: DeserializeOwned,
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
    ///
    // TODO(env-var-munching): When type-aware env var resolution is implemented, this method should
    // use `facet_solver::Schema::build(T::SHAPE)` to resolve environment variable names against the
    // target type's known paths before deserialization. This would allow flat env var names like
    // `DD_FOO_BAR_QUACK_QUACK` to be matched against nested paths like `foo.bar.quack_quack` without
    // requiring `__` separators.
    pub fn as_typed<T>(&self) -> Result<T, ConfigurationError>
    where
        T: DeserializeOwned,
    {
        let config_guard = self.inner.config.read().unwrap();
        let json = value_to_json(&config_guard);
        serde_json::from_value(json).map_err(|e| from_serde_error(&self.inner.lookup_sources, "", e))
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

fn build_missing_field_help(lookup_sources: &HashSet<LookupSource>, key: &str) -> String {
    let mut valid_keys: Vec<String> = lookup_sources.iter().map(|source| source.transform_key(key)).collect();

    // Always specify the original key as a valid key to try.
    valid_keys.insert(0, key.to_string());

    format!("Try setting `{}`.", valid_keys.join("` or `"))
}

fn from_serde_error(lookup_sources: &HashSet<LookupSource>, key: &str, e: serde_json::Error) -> ConfigurationError {
    let error_str = e.to_string();

    // Try to detect missing field errors from the error message
    if error_str.contains("missing field") {
        let help_text = build_missing_field_help(lookup_sources, key);
        return ConfigurationError::MissingField {
            help_text,
            field: Cow::Owned(key.to_string()),
        };
    }

    ConfigurationError::Generic { source: e.into() }
}

/// Converts a `facet_value::Value` to a `serde_json::Value`.
///
/// This is public within the crate so that the watcher module can use it.
///
/// This is used as a bridge to allow types that implement `serde::Deserialize` (but not yet `Facet`)
/// to be extracted from the facet-based configuration store.
pub(crate) fn value_to_json(value: &Value) -> serde_json::Value {
    if value.is_null() {
        serde_json::Value::Null
    } else if let Some(b) = value.as_bool() {
        serde_json::Value::Bool(b)
    } else if let Some(n) = value.as_number() {
        if let Some(i) = n.to_i64() {
            serde_json::Value::Number(i.into())
        } else if let Some(f) = n.to_f64() {
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        }
    } else if let Some(s) = value.as_string() {
        serde_json::Value::String(s.as_str().to_string())
    } else if let Some(arr) = value.as_array() {
        serde_json::Value::Array(arr.iter().map(value_to_json).collect())
    } else if let Some(obj) = value.as_object() {
        let map: serde_json::Map<String, serde_json::Value> = obj
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), value_to_json(v)))
            .collect();
        serde_json::Value::Object(map)
    } else {
        // For other facet_value types (bytes, datetime, etc.), convert to string representation.
        serde_json::Value::Null
    }
}

fn has_valid_secret_backend_command(configuration: &Value) -> bool {
    extract_at_path(configuration, "secret_backend_command")
        .and_then(|v| v.as_string())
        .is_some_and(|s| !s.as_str().is_empty())
}

#[cfg(test)]
mod tests {
    use facet_value::value;

    use super::*;

    #[tokio::test]
    async fn test_has_valid_secret_backend_command() {
        // When `secrets_backend_command` is not set at all, or is set to an empty string, or isn't even a string
        // value... then we should consider those scenarios as "secrets backend not configured".
        let config = value!({});
        assert!(!has_valid_secret_backend_command(&config));

        let config = value!({
            "secret_backend_command": ""
        });
        assert!(!has_valid_secret_backend_command(&config));

        let config = value!({
            "secret_backend_command": false
        });
        assert!(!has_valid_secret_backend_command(&config));

        // Otherwise, whether it's a valid path or not, then we should consider things enabled, which means we'll
        // at least attempt secrets resolution:
        let config = value!({
            "secret_backend_command": "/usr/bin/foo"
        });
        assert!(has_valid_secret_backend_command(&config));

        let config = value!({
            "secret_backend_command": "or anything else"
        });
        assert!(has_valid_secret_backend_command(&config));
    }

    #[tokio::test]
    async fn test_static_configuration() {
        let (cfg, _) = ConfigurationLoader::for_tests(
            Some(value!({
                "foo": "bar",
                "baz": 5,
                "foobar": { "a": false, "b": "c" }
            })),
            Some(&[("ENV_VAR".to_string(), "from_env".to_string())]),
            false,
        )
        .await;
        cfg.ready().await;

        assert_eq!(cfg.get_typed::<String>("foo").unwrap(), "bar");
        assert_eq!(cfg.get_typed::<i64>("baz").unwrap(), 5);
        assert!(!cfg.get_typed::<bool>("foobar.a").unwrap());
        assert_eq!(cfg.get_typed::<String>("env_var").unwrap(), "from_env");
        assert!(matches!(
            cfg.get::<String>("nonexistentKey"),
            Err(ConfigurationError::MissingField { .. })
        ));
    }

    #[tokio::test]
    async fn test_dynamic_configuration() {
        let (cfg, sender) = ConfigurationLoader::for_tests(
            Some(value!({
                "foo": "bar",
                "baz": 5,
                "foobar": { "a": false, "b": "c" }
            })),
            Some(&[("ENV_VAR".to_string(), "from_env".to_string())]),
            true,
        )
        .await;
        let sender = sender.expect("sender should exist");
        sender
            .send(ConfigUpdate::Snapshot(value!({
                "new": "from_snapshot"
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
                value: Value::from("from dynamic update"),
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
    }

    #[tokio::test]
    async fn test_environment_precedence_over_dynamic() {
        let (cfg, sender) = ConfigurationLoader::for_tests(
            Some(value!({
                "foo": "bar",
                "baz": 5,
                "foobar": { "a": false, "b": "c" }
            })),
            Some(&[("ENV_VAR".to_string(), "from_env".to_string())]),
            true,
        )
        .await;
        let sender = sender.expect("sender should exist");

        sender
            .send(ConfigUpdate::Snapshot(value!({
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
                value: Value::from("from_partial"),
            })
            .await
            .unwrap();

        // Also attempt to override the nested env-backed key via dynamic.
        sender
            .send(ConfigUpdate::Partial {
                key: "foobar.a".to_string(),
                value: Value::FALSE,
            })
            .await
            .unwrap();

        // Send a dummy partial update to ensure the updater has processed prior partials.
        sender
            .send(ConfigUpdate::Partial {
                key: "dummy".to_string(),
                value: Value::from(1),
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
        let (cfg, sender) = ConfigurationLoader::for_tests(
            Some(value!({
                "foo": "bar",
                "baz": 5,
                "foobar": { "a": false, "b": "c" }
            })),
            None,
            true,
        )
        .await;
        let sender = sender.expect("sender should exist");

        sender.send(ConfigUpdate::Snapshot(value!({}))).await.unwrap();
        cfg.ready().await;

        let mut rx = cfg.subscribe_for_updates().expect("dynamic updates should be enabled");

        sender
            .send(ConfigUpdate::Partial {
                key: "new_parent.new_child".to_string(),
                value: Value::from(42),
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

        assert_eq!(cfg.get_typed::<i64>("new_parent.new_child").unwrap(), 42);
    }

    #[tokio::test]
    async fn test_underscore_fallback_on_get() {
        let (cfg, _) = ConfigurationLoader::for_tests(
            Some(value!({})),
            Some(&[("RANDOM_KEY".to_string(), "from_env_only".to_string())]),
            false,
        )
        .await;
        cfg.ready().await;

        assert_eq!(cfg.get_typed::<String>("random.key").unwrap(), "from_env_only");
    }

    #[tokio::test]
    async fn test_static_configuration_ready_and_subscribe() {
        let (cfg, maybe_sender) = ConfigurationLoader::for_tests(Some(value!({})), None, false).await;
        assert!(maybe_sender.is_none());

        tokio::time::timeout(std::time::Duration::from_millis(500), cfg.ready())
            .await
            .expect("ready() should not block when dynamic is disabled");

        assert!(cfg.subscribe_for_updates().is_none());
    }

    #[tokio::test]
    async fn test_dynamic_configuration_ready_requires_initial_snapshot() {
        // Enable dynamic but do not send the initial snapshot.
        let (cfg, maybe_sender) = ConfigurationLoader::for_tests(Some(value!({})), None, true).await;
        assert!(maybe_sender.is_some());

        // ready() should not resolve until the initial snapshot is processed.
        let res = tokio::time::timeout(std::time::Duration::from_millis(1000), cfg.ready()).await;
        assert!(res.is_err(), "ready() should time out without an initial snapshot");
    }
}
