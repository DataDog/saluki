//! Artifacts shared by intake and load-generator tools.

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
