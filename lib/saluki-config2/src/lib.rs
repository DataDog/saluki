use std::sync::Arc;

use facet::Facet;
use saluki_error::GenericError;

mod deser;

mod value;
use self::value::Values;

/// Configuration data provider.
pub trait DataProvider {
    /// Gets the value from the data provider.
    fn get_value(&self) -> Result<Values, GenericError>;
}

/// Configuration builder.
pub struct Builder {
    providers: Vec<Arc<dyn DataProvider>>,
}

impl Builder {
    /// Creates a new, emtpy configuration builder.
    pub const fn new() -> Self {
        Self { providers: Vec::new() }
    }

    /// Adds a data provider.
    ///
    /// When materializing a configuration, data providers are used in the order they were added. This means that data
    /// providers added later have higher precedence and their values will override values retrieved from earlier data
    /// providers.
    pub fn add_provider<P>(&mut self, provider: P)
    where
        P: DataProvider + 'static,
    {
        self.providers.push(Arc::new(provider));
    }

    /// Builds the configuration.
    ///
    /// The configuration will be materialized by querying all data providers that were added.
    ///
    /// # Errors
    ///
    /// If any of the data providers fail to provide a value, an error will be returned.
    pub fn build(self) -> Result<GenericConfiguration, GenericError> {
        let providers = self.providers;

        // Collect the values from all data providers, and then merge them together to get the initial set of
        // configuration values.
        let mut provider_values = Vec::with_capacity(providers.len());
        for provider in &providers {
            provider_values.push(provider.get_value()?);
        }

        let data = provider_values
            .into_iter()
            .try_fold(Values::default(), |acc, value| acc.merge(value))?;

        Ok(GenericConfiguration { data })
    }
}

/// A generic configuration object.
pub struct GenericConfiguration {
    data: Values,
}

impl GenericConfiguration {
    /// Gets the configuration value associated with the given key, converting it to the specified type.
    ///
    /// The key supports a dot-separated path to a nested value, where each segment is the key within the object at the
    /// given level.
    ///
    /// If the key does not exist, either in terms of an intermediate segment or the final key, `Ok(None)` is returned.
    ///
    /// # Errors
    ///
    /// If the key exists but cannot be converted to the specified type, an error is returned.
    pub fn get<T: Facet<'static>>(&self, key: &str) -> Result<Option<T>, GenericError> {
        todo!()
    }
}
