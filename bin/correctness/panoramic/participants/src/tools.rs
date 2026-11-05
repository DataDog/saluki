//! Artifacts for the intake and load-generator tools.

use panoramic_core::{
    artifact::{ArtifactKind, ArtifactRole},
    layout::Os,
};

/// Linux image providing the intake and load-generator tools.
pub const TOOLS_LINUX_IMAGE: ArtifactRole = ArtifactRole {
    name: "tools-linux-image",
    kind: ArtifactKind::Image { os: Os::Linux },
    local_default: "saluki-images/correctness-tools:latest",
};

/// Windows image providing the intake executable.
pub(crate) const TOOLS_WINDOWS_IMAGE: ArtifactRole = ArtifactRole {
    name: "tools-windows-image",
    kind: ArtifactKind::Image { os: Os::Windows },
    local_default: "saluki-images/correctness-tools:testing-windows",
};

/// Intake executable for macOS host processes.
pub(crate) const INTAKE_HOST_BINARY: ArtifactRole = ArtifactRole {
    name: "intake-host-binary",
    kind: ArtifactKind::Binary,
    local_default: "target/release/datadog-intake",
};
