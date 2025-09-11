//! Dynamic configuration.

mod diff;
mod event;
mod watcher;

pub use self::diff::diff_config;
pub use self::event::{ConfigChangeEvent, ConfigUpdate};
pub use self::watcher::FieldUpdateWatcher;
