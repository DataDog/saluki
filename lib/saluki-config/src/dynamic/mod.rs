//! Dynamic configuration.

pub mod event;
pub use event::ConfigChangeEvent;

/// Shared configuration.
pub mod shared_config;
pub use shared_config::SharedConfig;

mod provider;
pub use provider::Provider;
