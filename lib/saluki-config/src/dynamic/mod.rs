//! Dynamic configuration.
//!
//! A configuration producer publishes [`ConfigSetting`]s over a stream of [`ConfigUpdate`]s. Two
//! things consume that stream. A caller wanting a live by-key view hands the receiver to
//! [`ConfigurationLoader::with_dynamic_configuration`][crate::ConfigurationLoader::with_dynamic_configuration],
//! which keeps a [`GenericConfiguration`][crate::GenericConfiguration] current and reports each change
//! through [`ConfigChangeEvent`] or a [`FieldUpdateWatcher`]. A caller wanting its own representation
//! takes [`ConfigSetting`] and [`ConfigUpdate`] alone and folds them itself.

// The by-key view has no consumer in this repository: `agent-data-plane` folds the stream into its own
// typed model and takes only `ConfigSetting`, `ConfigUpdate` and `Provenance` from this module. The
// view is kept because this crate is general-purpose infrastructure and the by-key path is the
// cheapest way for another process to consume a configuration stream. Until it has a consumer, the
// unit tests in this module and in `crate` are the only thing exercising it, so do not thin them out.

mod diff;
mod event;
mod watcher;

pub use self::diff::diff_config;
pub use self::event::{settings_to_state, ConfigChangeEvent, ConfigSetting, ConfigUpdate, Provenance};
pub use self::watcher::FieldUpdateWatcher;
