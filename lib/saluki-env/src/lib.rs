pub mod host;
pub mod tagger;
pub mod time;
pub mod workload;

pub use self::host::HostProvider;
pub use self::tagger::TaggerProvider;
pub use self::workload::WorkloadProvider;

pub trait EnvironmentProvider {
    type Host: self::host::HostProvider;
    type Workload: self::workload::WorkloadProvider;
    type Tagger: self::tagger::TaggerProvider;

    fn host(&self) -> &Self::Host;
    fn workload(&self) -> &Self::Workload;
    fn tagger(&self) -> &Self::Tagger;
}
