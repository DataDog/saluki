//! Dynamic configuration.

mod diff;
mod event;
mod watcher;

pub use self::diff::diff_config;
pub use self::event::{settings_to_state, ConfigChangeEvent, ConfigSetting, ConfigUpdate, Provenance};
pub use self::watcher::FieldUpdateWatcher;
