use std::{borrow::Cow, collections::HashSet};

use figment::{error::Kind, providers::Env, value::Value, Figment};

pub use figment::value;
use saluki_error::GenericError;
use serde::Deserialize;
use snafu::Snafu;

#[cfg(any(feature = "json", feature = "yaml"))]
mod provider;

#[cfg(any(feature = "json", feature = "yaml"))]
use self::provider::ResolvedProvider;

#[derive(Debug, Snafu)]
pub enum ConfigurationError {
    #[snafu(display("Missing field '{}' in configuration. {}", field, help_text))]
    MissingField {
        help_text: String,
        field: Cow<'static, str>,
    },

    #[snafu(display(
        "Expected value at path '{}' to be '{}', got '{}' instead.",
        path,
        expected_ty,
        actual_ty
    ))]
    InvalidFieldType {
        path: String,
        expected_ty: String,
        actual_ty: String,
    },

    #[snafu(display("{}", source))]
    Generic { source: GenericError },
}

impl From<figment::Error> for ConfigurationError {
    fn from(e: figment::Error) -> Self {
        match e.kind {
            Kind::InvalidType(actual_ty, expected_ty) => Self::InvalidFieldType {
                path: e.path.join("."),
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

/// A configuration loader that can load configuration from various sources.
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
/// - YAML file (requires the `yaml` feature)
/// - JSON file (requires the `json` feature)
/// - environment variables (must be prefixed; see [`from_environment`][Self::from_environment])
#[derive(Default)]
pub struct ConfigurationLoader {
    inner: Figment,
    lookup_sources: HashSet<LookupSource>,
}

impl ConfigurationLoader {
    #[cfg(feature = "yaml")]
    pub fn from_yaml<P>(mut self, path: P) -> Result<Self, ConfigurationError>
    where
        P: AsRef<std::path::Path>,
    {
        let resolved_provider = ResolvedProvider::from_yaml(&path)?;
        self.inner = self.inner.admerge(resolved_provider);
        Ok(self)
    }

    #[cfg(feature = "yaml")]
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

    #[cfg(feature = "json")]
    pub fn from_json<P>(mut self, path: P) -> Result<Self, ConfigurationError>
    where
        P: AsRef<std::path::Path>,
    {
        let resolved_provider = ResolvedProvider::from_json(&path)?;
        self.inner = self.inner.admerge(resolved_provider);
        Ok(self)
    }

    #[cfg(feature = "json")]
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

    pub fn from_environment(mut self, prefix: &'static str) -> Self {
        let prefix = if prefix.ends_with('_') {
            prefix.to_string()
        } else {
            format!("{}_", prefix)
        };

        self.inner = self.inner.admerge(Env::prefixed(&prefix));
        self.lookup_sources.insert(LookupSource::Environment { prefix });
        self
    }

    pub fn into_typed<'a, T>(self) -> Result<T, ConfigurationError>
    where
        T: Deserialize<'a>,
    {
        self.inner.extract().map_err(Into::into)
    }

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
    /// If the value could not be deserialized into `T`, an error variant will be returned.
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
    /// If the value could not be deserialized into `T`, an error variant will be returned.
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
            path: e.path.join("."),
            expected_ty,
            actual_ty: actual_ty.to_string(),
        },
        _ => ConfigurationError::Generic { source: e.into() },
    }
}
