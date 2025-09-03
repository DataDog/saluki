//! Dynamic configuration.

mod diff;
mod event;

pub use self::diff::diff_config;
pub use self::event::{ConfigChangeEvent, ConfigUpdate};
