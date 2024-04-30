mod detector;

pub use self::detector::FeatureDetector;

use bitmask_enum::bitmask;

/// Features are distinct markers or indicators of a particular technology or platform that is present.
///
/// In general, features represent the type of environment that the application is running in, such as the Kubernetes
/// feature being present indicating that the application is running in Kubernetes, and so on.
#[bitmask(u16)]
#[bitmask_config(vec_debug)]
pub enum Feature {
    /// Cloud Foundry.
    CloudFoundry,

    /// Containerd.
    Containerd,

    /// CRI.
    CRI,

    /// Docker.
    Docker,

    /// ECS on EC2.
    ECS,

    /// ECS on Fargate.
    ECSFargate,

    /// EKS on Fargate.
    EKSFargate,

    /// Kubernetes.
    Kubernetes,

    /// Podman.
    Podman,
}
