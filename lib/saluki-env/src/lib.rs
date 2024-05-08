pub mod features;
pub mod host;
pub mod time;
pub mod workload;

pub use self::host::HostProvider;
pub use self::workload::WorkloadProvider;

pub trait EnvironmentProvider {
    type Host: self::host::HostProvider;
    type Workload: self::workload::WorkloadProvider;

    fn host(&self) -> &Self::Host;
    fn workload(&self) -> &Self::Workload;
}
