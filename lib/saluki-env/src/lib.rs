//! Helpers for querying environment-specific data.
//!
//! In many cases, a Saluki service will need to interact with the environment in which it is running. This may include
//! interacting with the operating system, introspecting workloads running on the host, and so on.
//!
//! This crate provides implementations of various "providers" -- specific facets of the environment that can be queried
//! -- for a number of different environments, such as bare metal hosts, Kubernetes clusters, and so on.
#![deny(warnings)]
#![deny(missing_docs)]

pub mod autodiscovery;
pub mod configstream;
pub mod features;
pub mod helpers;
pub mod host;
pub mod workload;

use std::sync::Arc;

pub use self::autodiscovery::AutodiscoveryProvider;
pub use self::host::HostProvider;
pub use self::workload::WorkloadProvider;

/// Provides information about the environment in which the process is running.
pub trait EnvironmentProvider {
    /// Type of the host provider.
    type Host: self::host::HostProvider;

    /// Type of the workload provider.
    type Workload: self::workload::WorkloadProvider;

    /// Type of the autodiscovery provider.
    type AutodiscoveryProvider: self::autodiscovery::AutodiscoveryProvider;

    /// Gets a reference to the host provider for this environment.
    fn host(&self) -> &Self::Host;

    /// Gets a reference to workload provider for this environment.
    fn workload(&self) -> &Self::Workload;

    /// Gets a reference to autodiscovery provider for this environment.
    fn autodiscovery(&self) -> &Self::AutodiscoveryProvider;
}

impl<E> EnvironmentProvider for Arc<E>
where
    E: EnvironmentProvider,
{
    type Host = E::Host;
    type Workload = E::Workload;
    type AutodiscoveryProvider = E::AutodiscoveryProvider;

    fn host(&self) -> &Self::Host {
        (**self).host()
    }

    fn workload(&self) -> &Self::Workload {
        (**self).workload()
    }

    fn autodiscovery(&self) -> &Self::AutodiscoveryProvider {
        (**self).autodiscovery()
    }
}
