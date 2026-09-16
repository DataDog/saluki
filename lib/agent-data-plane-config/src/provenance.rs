//! [`ConfigValue`]: a configuration value paired with the reason it holds that value.

use serde::{Serialize, Serializer};

/// Whether a configuration value was set explicitly, or merely defaulted.
///
/// A configuration source generally supplies every setting it knows about, including settings nobody
/// configured, so a value on its own cannot say whether anything set it. A setting whose meaning
/// depends on that question needs the distinction; a setting that only needs an effective value can
/// ignore it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Provenance {
    /// Nothing set the setting, so it holds a default.
    ///
    /// This is the provenance of a value the model defaulted itself and of a value a source supplied
    /// from its own defaults.
    #[default]
    Default,

    /// Some input set this value: a configuration file, an environment variable, remote
    /// configuration, and so on.
    Explicit,
}

/// A configuration value together with the reason it holds that value.
///
/// Most model fields are plain values, because their effective value is all a consumer needs. Use
/// `ConfigValue<T>` for a setting whose behavior depends on whether the value was set explicitly,
/// rather than on the value alone. The primary intake URL is the canonical example: the Core Agent
/// supplies `dd_url` at its schema default even when the operator configured only `site`, so the URL
/// alone cannot say whether it should override `site`.
///
/// The value and its provenance are independent, and both are always available. A defaulted setting
/// holds its default value with [`Provenance::Default`], rather than holding no value, so a consumer
/// never has to restate a default the configuration layer already resolved:
///
/// ```ignore
/// if endpoints.dd_url.is_explicit() {
///     resolve_verbatim(&endpoints.dd_url.value)
/// } else {
///     resolve_from_site(&endpoints.site.value)
/// }
/// ```
///
/// Equality covers both fields: a value that keeps its contents but becomes explicit is a change, and
/// a [`Live`](crate::Live) view of it wakes.
///
/// Serialization emits the value alone, so the shape of a serialized
/// [`SalukiConfiguration`](crate::SalukiConfiguration) does not depend on which fields track
/// provenance.
///
/// Prefer `ConfigValue<T>` over `ConfigValue<Option<T>>`. Reaching for the inner `Option` usually
/// means it is duplicating the provenance: a `T` with [`Provenance::Default`] already expresses "no
/// one configured this," and the nested form leaves two different ways to say so.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfigValue<T> {
    /// The effective value.
    pub value: T,

    /// Whether an input set [`value`](Self::value), or it is a default.
    pub provenance: Provenance,
}

impl<T> ConfigValue<T> {
    /// Creates a value with the given provenance.
    pub fn new(value: T, provenance: Provenance) -> Self {
        Self { value, provenance }
    }

    /// Creates a value that an input set explicitly.
    pub fn explicit(value: T) -> Self {
        Self::new(value, Provenance::Explicit)
    }

    /// Creates a default value that nothing set.
    pub fn defaulted(value: T) -> Self {
        Self::new(value, Provenance::Default)
    }

    /// Returns whether an input set this value explicitly.
    ///
    /// A setting that acts as an override is in force only when this is true: a defaulted value
    /// expresses no intent to override anything.
    pub fn is_explicit(&self) -> bool {
        self.provenance == Provenance::Explicit
    }
}

impl<T: Serialize> Serialize for ConfigValue<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigValue, Provenance};

    #[test]
    fn a_defaulted_value_keeps_its_effective_value() {
        let value = ConfigValue::defaulted("https://app.datadoghq.com".to_string());

        assert!(!value.is_explicit());
        // The effective value survives, so a consumer need not restate the default.
        assert_eq!(value.value, "https://app.datadoghq.com");
    }

    #[test]
    fn an_explicit_value_reports_itself_as_explicit() {
        let value = ConfigValue::explicit("https://custom.example.com".to_string());

        assert!(value.is_explicit());
        assert_eq!(value.value, "https://custom.example.com");
    }

    #[test]
    fn provenance_participates_in_equality() {
        // A `Live` view projecting a `ConfigValue` must wake when a value becomes explicit even though
        // its contents are unchanged, so provenance cannot be excluded from equality.
        assert_ne!(ConfigValue::defaulted(0u64), ConfigValue::explicit(0u64));
    }

    #[test]
    fn the_model_default_is_a_defaulted_value() {
        assert_eq!(ConfigValue::<u64>::default(), ConfigValue::defaulted(0));
        assert_eq!(Provenance::default(), Provenance::Default);
    }
}
