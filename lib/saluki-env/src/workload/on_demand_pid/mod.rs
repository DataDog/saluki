#[cfg(target_os = "linux")]
use std::sync::Arc;

use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use stringtheory::interning::GenericMapInterner;

use crate::{features::FeatureDetector, workload::EntityId};

#[cfg(target_os = "linux")]
mod linux;

/// A resolver for mapping process IDs to their container IDs based on querying the underlying host.
///
/// # Platform support
///
/// On Linux, PIDs are resolved by querying procfs to find the cgroup of the process, if one exists, the cgroup
/// hierarchy is queried to discover the container ID that owns the process, if possible.
///
/// On all other platforms, resolving a PID is a no-op.
#[derive(Clone)]
pub struct OnDemandPIDResolver {
    #[cfg(target_os = "linux")]
    inner: Arc<linux::ResolverImpl>,
    #[cfg(not(target_os = "linux"))]
    _empty: (),
}

impl OnDemandPIDResolver {
    /// Creates a new `OnDemandPIDResolver` from the given configuration.
    ///
    /// # Errors
    ///
    /// If a cgroups hierarchy can't be found, or the internal cache can't be created, an error is returned.
    pub fn from_configuration(
        config: &GenericConfiguration, feature_detector: FeatureDetector, interner: GenericMapInterner,
    ) -> Result<Self, GenericError> {
        #[cfg(target_os = "linux")]
        {
            let resolver_inner = linux::ResolverImpl::from_configuration(config, feature_detector, interner)?;
            Ok(Self {
                inner: Arc::new(resolver_inner),
            })
        }

        #[cfg(not(target_os = "linux"))]
        {
            // Rebind to make compiler happy.
            let _config = config;
            let _feature_detector = feature_detector;
            let _interner = interner;

            Ok(Self { _empty: () })
        }
    }

    /// Resolves a process ID to the container ID of the container is part of.
    ///
    /// If the process ID isn't part of a container, or can't be found, `None` is returned.
    pub fn resolve(&self, process_id: u32) -> Option<EntityId> {
        #[cfg(target_os = "linux")]
        let resolved = self.inner.resolve(process_id);

        #[cfg(not(target_os = "linux"))]
        let resolved = {
            // Rebind to make compiler happy.
            let _process_id = process_id;
            None
        };

        resolved
    }

    /// Resolves the current process's container entity from local cgroup membership.
    ///
    /// On non-Linux platforms, or when the process is not in a recognizable container cgroup, this returns `None`.
    pub fn resolve_self_container(&self) -> Option<EntityId> {
        #[cfg(target_os = "linux")]
        let resolved = self.inner.resolve_self_container();

        #[cfg(not(target_os = "linux"))]
        let resolved = None;

        resolved
    }
}
