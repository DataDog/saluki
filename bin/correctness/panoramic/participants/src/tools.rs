//! Artifacts shared by intake and load-generator tools.

use panoramic_core::artifact::{ArtifactKind, ArtifactRole};

/// Linux image providing the intake and load-generator tools.
pub const TOOLS_LINUX_IMAGE: ArtifactRole = ArtifactRole {
    name: "tools-linux-image",
    kind: ArtifactKind::Image,
    local_default: "saluki-images/correctness-tools:latest",
};
