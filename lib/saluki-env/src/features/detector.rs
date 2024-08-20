use saluki_config::GenericConfiguration;
use tracing::info;

use super::{
    has_host_mapped_cgroupfs, has_host_mapped_procfs, is_running_inside_container, ContainerdDetector, Feature,
};

/// Detects workload features.
///
/// The feature detector is used to determine which features are present in the workload, such as if the application is
/// running in a Kubernetes environment or directly on Docker. This information is used to drive the collection of
/// workload metadata that powers entity tagging.
///
/// `FeatureDetector` can be used in an automatic detection mode, where it will attempt to detect all features that are
/// present, or it can be used to assert that a specific feature is present for scenarios where the feature is required,
/// and the workload is already known upfront.
#[derive(Clone)]
pub struct FeatureDetector {
    detected_features: Feature,
}

impl FeatureDetector {
    /// Creates a new `FeatureDetector` that checks for all possible features.
    pub fn automatic(config: &GenericConfiguration) -> Self {
        Self::for_feature(config, Feature::all_bits())
    }

    /// Creates a new `FeatureDetector` that checks for the given feature(s).
    ///
    /// Multiple features can be checked for by combining the feature variants in a bitwise OR fashion.
    pub fn for_feature(config: &GenericConfiguration, feature_mask: Feature) -> Self {
        let detected_features = Self::detect_features(config, feature_mask);

        Self { detected_features }
    }

    /// Checks if the given feature was detected and is available.
    pub fn is_feature_available(&self, feature: Feature) -> bool {
        self.detected_features.contains(feature)
    }

    fn detect_features(config: &GenericConfiguration, feature_mask: Feature) -> Feature {
        let mut detected_features = Feature::none();

        if feature_mask.contains(Feature::Containerized) && is_running_inside_container() {
            info!("Detected presence of containerized workload.");
            detected_features |= Feature::Containerized;
        }

        if feature_mask.contains(Feature::HostMappedProcfs) && has_host_mapped_procfs() {
            info!("Detected presence of host-mapped procfs.");
            detected_features |= Feature::HostMappedProcfs;
        }

        if feature_mask.contains(Feature::HostMappedCgroupfs) && has_host_mapped_cgroupfs() {
            info!("Detected presence of host-mapped cgroupfs.");
            detected_features |= Feature::HostMappedCgroupfs;
        }

        if feature_mask.contains(Feature::Containerd) && ContainerdDetector::detect_grpc_socket_path(config).is_some() {
            info!("Detected presence of containerd.");
            detected_features |= Feature::Containerd;
        }

        detected_features
    }
}
