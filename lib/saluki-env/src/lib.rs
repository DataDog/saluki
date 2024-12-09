//! Helpers for querying environment-specific data.
//!
//! In many cases, a Saluki service will need to interact with the environment in which it is running. This may include
//! interacting with the operating system, introspecting workloads running on the host, and so on.
//!
//! This crate provides implementations of various "providers" -- specific facets of the environment that can be queried
//! -- for a number of different environments, such as bare metal hosts, Kubernetes clusters, and so on.
#![deny(warnings)]
#![deny(missing_docs)]

pub mod features;
#[allow(missing_docs)]
pub mod helpers;
pub mod host;
mod prelude;
pub mod time;
pub mod workload;

pub use self::host::HostProvider;
pub use self::workload::WorkloadProvider;

/// Provides information about the environment in which the process is running.
pub trait EnvironmentProvider {
    /// Type of the host provider.
    type Host: self::host::HostProvider;

    /// Type of the workload provider.
    type Workload: self::workload::WorkloadProvider;

    /// Gets a reference to the host provider for this environment.
    fn host(&self) -> &Self::Host;

    /// Gets a reference to workload provider for this environment.
    fn workload(&self) -> &Self::Workload;
}
