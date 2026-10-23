//! Workers that serve a remote agent's internal APIs and keep its configuration current.
//!
//! Each remote agent builds its own control plane supervisor from these workers, the ones in `saluki-core` and
//! `saluki-app`, and the remote agent workers that [`RemoteAgentBootstrap`][crate::remote_agent::RemoteAgentBootstrap]
//! creates.

mod config_runtime;
pub use self::config_runtime::{ConfigRuntimeWorker, RuntimeConfigSnapshot};

mod config_updates;
pub use self::config_updates::{ConfigUpdatesRunner, ConfigUpdatesWorker};
