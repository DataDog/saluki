//! The stages of the remote agent startup sequence.
//!
//! [`RemoteAgent`](super::RemoteAgent) takes one of these as its stage parameter, which determines the methods it
//! offers.

use std::time::Instant;

use datadog_agent_remote_config::{RemoteConfigurationClient, RemoteConfigurationWorker};
use saluki_core::{
    accounting::ComponentRegistry,
    health::HealthRegistry,
    runtime::{state::ResourceRegistry, Supervisor},
};

use super::RemoteAgentSpecification;
use crate::{env::RemoteAgentEnvironmentProvider, logging::LoggingTranslator, remote_agent::RemoteAgentBootstrap};

/// A remote agent that hasn't started yet.
pub enum Unstarted {}

/// A remote agent whose configuration authority is selected.
///
/// The binary can inspect its configuration here, and stop before anything else starts.
pub enum Started {}

/// A remote agent with its registries and environment provider in place.
///
/// The binary builds its topology and its control plane supervisor here, and then runs the remote agent.
pub enum Initialized {}

mod sealed {
    pub trait Sealed {}

    impl Sealed for super::Unstarted {}
    impl Sealed for super::Started {}
    impl Sealed for super::Initialized {}
}

/// A stage of the remote agent startup sequence.
///
/// This trait is sealed, and only implemented by the stages in this module.
pub trait Stage: sealed::Sealed {
    /// The state that the remote agent holds in this stage.
    #[doc(hidden)]
    type Data<R: RemoteAgentSpecification>;
}

impl Stage for Unstarted {
    type Data<R: RemoteAgentSpecification> = data::UnstartedData<R>;
}

impl Stage for Started {
    type Data<R: RemoteAgentSpecification> = data::StartedData<R>;
}

impl Stage for Initialized {
    type Data<R: RemoteAgentSpecification> = data::InitializedData<R>;
}

pub(super) use self::data::{InitializedData, StartedData, UnstartedData};

mod data {
    use super::*;

    pub struct UnstartedData<R: RemoteAgentSpecification> {
        pub local: R::LocalConfiguration,
        pub translator: LoggingTranslator,
        pub start_time: Instant,
    }

    pub struct StartedData<R: RemoteAgentSpecification> {
        pub config: R::AuthoritativeConfiguration,
        pub ra_bootstrap: Option<RemoteAgentBootstrap>,
        pub config_updates: Option<R::ConfigurationUpdates>,
        pub bootstrap_supervisor: Supervisor,
        pub start_time: Instant,
    }

    pub struct InitializedData<R: RemoteAgentSpecification> {
        pub started: StartedData<R>,
        pub root_supervisor: Supervisor,
        pub component_registry: ComponentRegistry,
        pub health_registry: HealthRegistry,
        pub resource_registry: ResourceRegistry,
        pub env_provider: RemoteAgentEnvironmentProvider,
        pub env_supervisor: Option<Supervisor>,
        pub remote_config: Option<(RemoteConfigurationClient, RemoteConfigurationWorker)>,
    }
}
