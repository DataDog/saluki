//! Shared state carried by each HTTP router.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::capture;
use crate::sut_config::SutConfig;

/// Per-router state: the shared recorder handle plus the lane this router writes to.
#[derive(Clone, Debug)]
pub struct AppState {
    pub(crate) recorder: capture::State,
    pub(crate) target: capture::Target,
    /// First non-empty host resolved on this lane, set once. Pyld17 requires every series across all
    /// inbound traffic on the lane to resolve to this same host.
    pub(crate) established_host: Arc<OnceLock<String>>,
    /// Directory holding the timeline's sampled `datadog.yaml`.
    agent_config_dir: Arc<Path>,
    /// The sampled config, read on the first request that finds the file written.
    sut_config: Arc<OnceLock<SutConfig>>,
}

impl AppState {
    /// Creates router state for Datadog Agent intake.
    #[must_use]
    pub fn agent(recorder: &capture::State, agent_config_dir: &Path) -> Self {
        Self::new(recorder, capture::Target::Agent, agent_config_dir)
    }

    /// Creates router state for ADP intake.
    #[must_use]
    pub fn adp(recorder: &capture::State, agent_config_dir: &Path) -> Self {
        Self::new(recorder, capture::Target::Adp, agent_config_dir)
    }

    fn new(recorder: &capture::State, target: capture::Target, agent_config_dir: &Path) -> Self {
        Self {
            recorder: recorder.clone(),
            target,
            established_host: Arc::default(),
            agent_config_dir: Arc::from(agent_config_dir),
            sut_config: Arc::default(),
        }
    }

    /// The sampled config, or `None` while the file is still absent or unparseable. Retried per
    /// request until it reads, since the intake binds before `first_sample_config` runs.
    pub(crate) fn sut_config(&self) -> Option<&SutConfig> {
        if let Some(config) = self.sut_config.get() {
            return Some(config);
        }
        let config = SutConfig::load(&self.agent_config_dir)?;
        Some(self.sut_config.get_or_init(|| config))
    }
}
