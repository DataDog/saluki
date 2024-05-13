use figment::{providers::Env, value::Value, Figment};

pub use figment::{value, Error};
use serde::Deserialize;

#[cfg(any(feature = "json", feature = "yaml"))]
mod provider;

#[cfg(any(feature = "json", feature = "yaml"))]
use self::provider::ResolvedProvider;

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
}

impl ConfigurationLoader {
    #[cfg(feature = "yaml")]
    pub fn from_yaml<P>(mut self, path: P) -> Result<Self, Error>
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
    pub fn from_json<P>(mut self, path: P) -> Result<Self, Error>
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

    pub fn from_environment(mut self, prefix: &str) -> Self {
        let env = if prefix.ends_with('_') {
            Env::prefixed(prefix)
        } else {
            let with_underscore = format!("{}_", prefix);
            Env::prefixed(&with_underscore)
        };

        self.inner = self.inner.admerge(env);
        self
    }

    pub fn into_typed<'a, T>(self) -> Result<T, Error>
    where
        T: Deserialize<'a>,
    {
        self.inner.extract()
    }

    pub fn into_generic(self) -> Result<GenericConfiguration, Error> {
        let inner: Value = self.inner.extract()?;
        Ok(GenericConfiguration { inner })
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
}

impl GenericConfiguration {
    pub fn get(&self, key: &str) -> Option<&Value> {
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

    pub fn get_typed<'a, T>(&self, key: &str) -> Result<Option<T>, Error>
    where
        T: Deserialize<'a>,
    {
        match self.get(key) {
            Some(value) => Ok(Some(value.deserialize().map_err(|e| e.with_path(key))?)),
            None => Ok(None),
        }
    }

    pub fn get_typed_or_default<'a, T>(&self, key: &str) -> T
    where
        T: Default + Deserialize<'a>,
    {
        match self.get(key) {
            Some(value) => value.deserialize().unwrap_or_default(),
            None => T::default(),
        }
    }

    pub fn as_typed<'a, T>(&self) -> Result<T, Error>
    where
        T: Deserialize<'a>,
    {
        self.inner.deserialize()
    }
}
