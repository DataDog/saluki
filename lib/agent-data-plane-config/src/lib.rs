//! ADP-native configuration model: the typed target of configuration translation.
//!
//! This crate owns the domain-shaped model types that translation produces and that ADP runtime
//! code consumes: `SalukiConfiguration { control, shared, domains }`, `ControlConfiguration`,
//! `SharedConfiguration`, `DomainConfiguration` and its per-domain structs.
//!
//! It does not embed component config structs (those stay in `saluki-components`, built from this
//! model). It depends on neither the raw configuration map nor the Datadog source model, so a
//! consumer can depend on it without inheriting either.
//!
//! Every field is plain, source-agnostic data. There are no source key names in identifiers and no
//! source serde (these structs are serialized for the `/config/internal` view but never
//! deserialized from a source language; that is the source adapter's job).

use std::fmt;

use serde::Serialize;

pub mod control;
pub mod defaults;
pub mod domains;
pub mod live;
pub mod shared;

pub use control::{ControlConfiguration, ListenAddress, Logging};
pub use domains::DomainConfiguration;
pub use live::Live;
pub use shared::SharedConfiguration;

/// The complete ADP-native runtime configuration after translation.
///
/// Two writers fill it: the Datadog witness `drive` (schema fields) and `seed` (Saluki-only
/// fields). They write disjoint fields. It is read by the orchestration layer (`control`) and by
/// components at topology assembly (`shared` and `domains`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SalukiConfiguration {
    /// Read first: decides which pipelines/topology to build. Orchestration layer only.
    pub control: ControlConfiguration,
    /// Cross-cutting values consumed by more than one domain, each with a single home.
    pub shared: SharedConfiguration,
    /// Per-domain resolved config, grouped by ownership domain.
    pub domains: DomainConfiguration,
}

/// An error produced while translating a `DatadogConfiguration` or `SalukiOnly` value into a
/// `SalukiConfiguration` value.
#[derive(Debug)]
pub struct Error {
    context: String,
    error: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

/// A boxed source error that can cross thread boundaries.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

impl Error {
    /// Create an error that wraps an underlying error.
    ///
    /// Displays `{context}: {error}`.
    pub fn new<S, E>(context: S, error: E) -> Self
    where
        S: Into<String>,
        E: Into<BoxError>,
    {
        Self {
            context: context.into(),
            error: Some(error.into()),
        }
    }

    /// Create an error without an underlying source error.
    pub fn new_without_source<S>(context: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            context: context.into(),
            error: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.context)?;
        if let Some(error) = &self.error {
            write!(f, ": {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.as_deref().map(|s| s as &(dyn std::error::Error + 'static))
    }
}
