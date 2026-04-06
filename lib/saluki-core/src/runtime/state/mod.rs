//! Runtime state management utilities.
//!
//! This module provides utilities for managing shared state across processes in the runtime system.

use stringtheory::MetaString;

use crate::runtime::process::Id;

mod dataspace;
pub(crate) use self::dataspace::CURRENT_DATASPACE;
pub use self::dataspace::{AssertionUpdate, DataspaceRegistry, Subscription};

/// An identifier used to key values in a [`DataspaceRegistry`].
///
/// Identifiers come in two flavors:
/// - **Named**: a string-based identifier, using [`MetaString`] for efficient storage.
/// - **Numeric**: a simple numeric identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Identifier {
    /// A string-based identifier.
    Named(MetaString),

    /// A numeric identifier.
    Numeric(usize),
}

impl Identifier {
    /// Creates a named identifier.
    pub fn named(name: impl Into<MetaString>) -> Self {
        Self::Named(name.into())
    }

    /// Creates a numeric identifier.
    pub fn numeric(id: usize) -> Self {
        Self::Numeric(id)
    }
}

impl From<&str> for Identifier {
    fn from(s: &str) -> Self {
        Self::Named(MetaString::from(s))
    }
}

impl From<MetaString> for Identifier {
    fn from(s: MetaString) -> Self {
        Self::Named(s)
    }
}

impl From<usize> for Identifier {
    fn from(id: usize) -> Self {
        Self::Numeric(id)
    }
}

impl From<Id> for Identifier {
    fn from(id: Id) -> Self {
        Self::Numeric(id.as_usize())
    }
}

/// A filter used to match identifiers when subscribing to a [`DataspaceRegistry`].
///
/// Filters control which assertions and retractions a subscription receives:
/// - [`All`](IdentifierFilter::All): receives updates for every identifier.
/// - [`Exact`](IdentifierFilter::Exact): receives updates only for a specific identifier.
/// - [`Prefix`](IdentifierFilter::Prefix): receives updates for named identifiers that start with a given prefix.
#[derive(Clone, Debug)]
pub enum IdentifierFilter {
    /// Matches all identifiers.
    All,

    /// Matches a single exact identifier.
    Exact(Identifier),

    /// Matches named identifiers that start with the given prefix.
    ///
    /// Never matches numeric identifiers.
    Prefix(MetaString),
}

impl IdentifierFilter {
    /// Creates a filter that matches all identifiers.
    pub fn all() -> Self {
        Self::All
    }

    /// Creates a filter that matches a single exact identifier.
    pub fn exact(id: impl Into<Identifier>) -> Self {
        Self::Exact(id.into())
    }

    /// Creates a filter that matches named identifiers with the given prefix.
    pub fn prefix(prefix: impl Into<MetaString>) -> Self {
        Self::Prefix(prefix.into())
    }

    /// Returns `true` if this filter matches the given identifier.
    pub fn matches(&self, id: &Identifier) -> bool {
        match self {
            Self::All => true,
            Self::Exact(expected) => id == expected,
            Self::Prefix(prefix) => match id {
                Identifier::Named(name) => name.as_ref().starts_with(prefix.as_ref()),
                Identifier::Numeric(_) => false,
            },
        }
    }
}
