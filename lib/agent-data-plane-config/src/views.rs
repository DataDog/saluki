//! Typed `/config` view models.
//!
//! These types define the shape of the `/config` views; they do not produce the views. The views
//! are produced live-on-request by config-system, which holds the retained source snapshot and the
//! current [`SalukiConfiguration`](crate::SalukiConfiguration). config-system constructs these view
//! types from an already-scrubbed serialized snapshot.
//!
//! Each view exposes only print/serialize accessors. There is deliberately no typed lookup, no
//! update subscription, and no raw-map type: `/config` is a compatibility view, not a reason to
//! expose configuration as a queryable map to ADP runtime code.

/// The bundle of `/config` views.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigViews {
    /// The source-shaped effective config view, served at `/config` and `/config/raw`.
    pub raw: SourceConfigView,

    /// The ADP-native Saluki-shaped view, served at `/config/internal`.
    pub internal: InternalConfigView,
}

impl ConfigViews {
    /// Creates a new `ConfigViews` from already-scrubbed serialized view snapshots.
    pub fn new(raw: SourceConfigView, internal: InternalConfigView) -> Self {
        Self { raw, internal }
    }
}

/// The source-shaped effective Datadog configuration view.
///
/// Holds a scrubbed, serialized snapshot of the retained Datadog source config. Served at `/config`
/// and `/config/raw`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceConfigView {
    serialized: String,
}

impl SourceConfigView {
    /// Creates a new `SourceConfigView` from an already-scrubbed serialized snapshot.
    pub fn new(serialized: String) -> Self {
        Self { serialized }
    }

    /// Returns the scrubbed, serialized snapshot.
    pub fn as_str(&self) -> &str {
        &self.serialized
    }
}

impl std::fmt::Display for SourceConfigView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.serialized)
    }
}

/// The ADP-native Saluki-shaped configuration view.
///
/// Holds a scrubbed, serialized snapshot of the current
/// [`SalukiConfiguration`](crate::SalukiConfiguration). Served at `/config/internal`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InternalConfigView {
    serialized: String,
}

impl InternalConfigView {
    /// Creates a new `InternalConfigView` from an already-scrubbed serialized snapshot.
    pub fn new(serialized: String) -> Self {
        Self { serialized }
    }

    /// Returns the scrubbed, serialized snapshot.
    pub fn as_str(&self) -> &str {
        &self.serialized
    }
}

impl std::fmt::Display for InternalConfigView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.serialized)
    }
}
