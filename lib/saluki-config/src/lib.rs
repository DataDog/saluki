//! Primitives for working with typed and untyped configuration data.
#![deny(warnings)]
#![deny(missing_docs)]

use std::{borrow::Cow, collections::HashSet};

pub use figment::value;
use figment::{error::Kind, providers::Env, value::Value, Figment};
use saluki_error::GenericError;
use serde::Deserialize;
use snafu::{ResultExt as _, Snafu};
use tracing::debug;

mod provider;
mod secrets;
use self::provider::ResolvedProvider;

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

#[derive(Eq, Hash, PartialEq)]
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
/// ## Supported sources
///
/// - YAML file
/// - JSON file
/// - environment variables (must be prefixed; see [`from_environment`][Self::from_environment])
#[derive(Default)]
pub struct ConfigurationLoader {
    inner: Figment,
    lookup_sources: HashSet<LookupSource>,
}

impl ConfigurationLoader {
    /// Loads the given YAML configuration file.
    ///
    /// ## Errors
    ///
    /// If the file could not be read, or if the file is not valid YAML, an error will be returned.
    pub fn from_yaml<P>(mut self, path: P) -> Result<Self, ConfigurationError>
    where
        P: AsRef<std::path::Path>,
    {
        let resolved_provider = ResolvedProvider::from_yaml(&path)?;
        self.inner = self.inner.admerge(resolved_provider);
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
                self.inner = self.inner.admerge(resolved_provider);
            }
            Err(e) => {
                tracing::debug!(error = %e, file_path = %path.as_ref().to_string_lossy(), "Unable to read YAML configuration file. Ignoring.");
            }
        }
        self
    }

    /// Loads the given JSON configuration file.
    ///
    /// ## Errors
    ///
    /// If the file could not be read, or if the file is not valid JSON, an error will be returned.
    pub fn from_json<P>(mut self, path: P) -> Result<Self, ConfigurationError>
    where
        P: AsRef<std::path::Path>,
    {
        let resolved_provider = ResolvedProvider::from_json(&path)?;
        self.inner = self.inner.admerge(resolved_provider);
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
                self.inner = self.inner.admerge(resolved_provider);
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
    /// ## Errors
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

        self.inner = self.inner.admerge(Env::prefixed(&prefix));
        self.lookup_sources.insert(LookupSource::Environment { prefix });
        Ok(self)
    }

    /// Resolves secrets in the configuration based on available secret backend configuration.
    ///
    /// This will attempt to resolve any secret references (format shown below) in the configuration by using a "secrets
    /// backend", which is a user-provided command that utilizes a simple JSON-based protocol to accept secrets to
    /// resolve, and return those resolved secrets, or the errors that occurred during resolving.
    ///
    /// ## Configuration
    ///
    /// This method uses the existing configuration (see Caveats) to determine the secrets backend configuration. The
    /// following configuration settings are used:
    ///
    /// - `secret_backend_command`: The executable to resolve secrets. (required)
    /// - `secret_backend_timeout`: The timeout for the secrets backend command, in seconds. (optional, default: 30)
    ///
    /// ## Usage
    ///
    /// For any value which should be resolved as a secret, the value should be a string in the format of
    /// `ENC[secret_reference]`. The `secret_reference` portion is the value that will be sent to the backend command
    /// during resolution. There is no limitation on the format of the `secret_reference` value, so long as it can be
    /// expressed through the existing configuration sources (YAML, environment variables, etc).
    ///
    /// The entire configuration value must match this pattern, and cannot be used to replace only part of a value, so
    /// values such as `db-ENC[secret_reference]` would not be detected as secrets and thus would not be resolved.
    ///
    /// ## Protocol
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
    /// ## Caveats
    ///
    /// ### Time of resolution
    ///
    /// Secrets resolution happens at the time this method is called, and only resolves configuration values that are
    /// already present in the configuration, which means all calls to load configuration (`try_from_yaml`,
    /// `from_environment`, etc) must be made before calling this method.
    ///
    /// ### Sensitive data in error output
    ///
    /// Care should be taken to not return sensitive information in either the error output (standard error) of the
    /// backend command or the `error` field in the JSON response, as these values are logged in order to aid debugging.
    pub async fn with_default_secrets_resolution(mut self) -> Result<Self, ConfigurationError> {
        // If no secrets backend is set, we can't resolve secrets, so just return early.
        if !self.inner.contains("secret_backend_command") {
            debug!("No secrets backend configured; skipping secrets resolution.");
            return Ok(self);
        }

        let resolver_config = self.inner.extract::<secrets::ResolverConfiguration>()?;
        let resolver = secrets::Resolver::from_configuration(resolver_config)
            .await
            .context(Secrets)?;

        let mut provider = secrets::Provider::new();
        provider.resolve(&resolver, &self.inner).await.context(Secrets)?;

        self.inner = self.inner.admerge(provider);
        Ok(self)
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
        self.inner.extract().map_err(Into::into)
    }

    /// Consumes the configuration loader and wraps it in a generic wrapper.
    pub fn into_generic(self) -> Result<GenericConfiguration, ConfigurationError> {
        let inner: Value = self.inner.extract()?;
        Ok(GenericConfiguration {
            inner,
            lookup_sources: self.lookup_sources,
        })
    }
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
pub struct GenericConfiguration {
    inner: Value,
    lookup_sources: HashSet<LookupSource>,
}

impl GenericConfiguration {
    fn get(&self, key: &str) -> Option<&Value> {
        match self.inner.find_ref(key) {
            Some(value) => Some(value),
            None => {
                // We might have been given a key that uses nested notation -- `foo.bar` -- but is only present in the
                // environment variables. We specifically don't want to use a different separator in environment
                // variables to map to nested key separators, so we simply try again here but with all nested key
                // separators (`.`) replaced with `_`, to match environment variables.
                let key = key.replace('.', "_");
                self.inner.find_ref(&key)
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
        match self.get(key) {
            Some(value) => {
                let deserialized = value.deserialize().map_err(|e| e.with_path(key))?;
                Ok(deserialized)
            }
            None => Err(from_figment_error(
                &self.lookup_sources,
                Kind::MissingField(key.to_string().into()).into(),
            )),
        }
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
        match self.get(key) {
            Some(value) => value.deserialize().unwrap_or_default(),
            None => T::default(),
        }
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
            Some(value) => {
                let deserialized = value
                    .deserialize()
                    .map_err(|e| e.with_path(key))
                    .map_err(|e| from_figment_error(&self.lookup_sources, e))?;
                Ok(Some(deserialized))
            }
            None => Ok(None),
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
            .deserialize()
            .map_err(|e| from_figment_error(&self.lookup_sources, e))
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
