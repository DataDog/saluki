use figment::{providers::Env, value::Value, Error, Figment};

pub use figment::value;
use serde::Deserialize;

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
    pub fn from_yaml<P>(mut self, path: P) -> Self
    where
        P: AsRef<std::path::Path>,
    {
        self.inner = self
            .inner
            .admerge(figment::providers::Data::<figment::providers::Yaml>::file(path));
        self
    }

    #[cfg(feature = "json")]
    pub fn from_json<P>(mut self, path: P) -> Self
    where
        P: AsRef<std::path::Path>,
    {
        self.inner = self
            .inner
            .admerge(figment::providers::Data::<figment::providers::Json>::file(path));
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
        self.inner.find_ref(key)
    }

    pub fn get_typed<'a, T>(&self, key: &str) -> Result<Option<T>, Error>
    where
        T: Deserialize<'a>,
    {
        match self.inner.find_ref(key) {
            Some(value) => Ok(Some(value.deserialize()?)),
            None => Ok(None),
        }
    }
}
