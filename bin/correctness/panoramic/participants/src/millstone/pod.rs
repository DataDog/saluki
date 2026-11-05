//! Paired senders in one pod container, exiting after sending.

use panoramic_core::{
    participant::RenderedConfig,
    recipe::{ContainerRecipe, PodRecipe, Recipe},
};

use super::{Millstone, CONFIG_DIR, CONFIG_FILE, STATE_DIR, WRAPPER, WRAPPER_PATH};
use crate::TOOLS_LINUX_IMAGE;

pub(super) fn recipe(config: &RenderedConfig) -> Result<Recipe, String> {
    let mut container = ContainerRecipe::new(TOOLS_LINUX_IMAGE)
        .with_entrypoint(["/bin/sh", WRAPPER_PATH])
        .with_command(["/usr/local/bin/millstone", CONFIG_DIR, STATE_DIR])
        .with_env(config.env().clone())
        .with_file(WRAPPER_PATH, 0o755, WRAPPER);
    for (path, contents) in Millstone::sender_files(config)? {
        container = container.with_file(path, 0o644, contents);
    }
    // DogStatsD external_data identifies this container by name.
    Ok(Recipe::Pod(
        PodRecipe::new("millstone", container)
            .with_late_file(format!("{CONFIG_DIR}/subject.yaml"), CONFIG_FILE)
            .with_late_file(format!("{CONFIG_DIR}/baseline.yaml"), CONFIG_FILE),
    ))
}
