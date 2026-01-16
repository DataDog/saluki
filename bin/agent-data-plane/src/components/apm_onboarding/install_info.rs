use std::io::ErrorKind;

use saluki_common::time::get_unix_timestamp;
use saluki_error::{ErrorContext as _, GenericError};
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;
use uuid::Uuid;

use crate::internal::platform::PlatformSettings;

static INSTALL_TYPE_DEFAULT: MetaString = MetaString::from_static("manual");
static INSTALL_TYPE_DOCKER_DEFAULT: MetaString = MetaString::from_static("docker_manual");
static INSTALL_TYPE_DOCKER_SINGLE_STEP_DEFAULT: MetaString = MetaString::from_static("docker_single_step");

/// Installation information about this Agent.
#[derive(Deserialize, Serialize)]
pub struct InstallInfo {
    pub install_id: MetaString,
    pub install_type: MetaString,
    pub install_time: u64,
}

impl InstallInfo {
    fn from_environment() -> Self {
        Self {
            install_id: Uuid::new_v4().hyphenated().to_string().into(),
            install_type: infer_install_type_from_environment(),
            install_time: get_unix_timestamp(),
        }
    }

    /// Loads the existing installation info from disk, or creates it if it doesn't exist.
    ///
    /// When the file doesn't exist, the newly created installation info will be written to disk before returning.
    ///
    /// # Errors
    ///
    /// If the default installation info path cannot be read or written to, or if there is an error during serialization
    /// or deserialization of the installation info, an error will be returned.
    pub async fn load_or_create() -> Result<Self, GenericError> {
        let path = PlatformSettings::get_config_dir_path().join("install.json");

        // See if the file exists, and load it if so.
        let (install_info, should_write) = match tokio::fs::read(&path).await {
            Ok(data) => {
                // Try and decode the installation info.
                //
                // If we fail, we don't try to update it.
                let install_info = serde_json::from_slice(&data).with_error_context(|| {
                    format!(
                        "Failed to decode installation info file '{}'.",
                        path.as_path().display()
                    )
                })?;

                (install_info, false)
            }

            Err(e) => match e.kind() {
                // If the file doesn't exist, then _we'll_ try and create it.
                ErrorKind::NotFound => (Self::from_environment(), true),

                // There was a legitimate error so we bail out.
                _ => {
                    return Err(e).with_error_context(|| {
                        format!("Failed to read installation info file '{}'.", path.as_path().display())
                    })
                }
            },
        };

        // Write it out if we were the ones to create it.
        //
        // If we fail to write it out, then we also just bail out.
        if should_write {
            let install_info_json =
                serde_json::to_vec(&install_info).error_context("Failed to serialize installation info to JSON.")?;

            tokio::fs::write(&path, install_info_json)
                .await
                .with_error_context(|| {
                    format!("Failed to write installation info to '{}'.", path.as_path().display())
                })?;
        }

        Ok(install_info)
    }
}

fn infer_install_type_from_environment() -> MetaString {
    let is_containerized = std::env::var("DOCKER_DD_AGENT").is_ok_and(|s| !s.is_empty());
    let apm_enabled = std::env::var("DD_APM_ENABLED").is_ok_and(|s| !s.is_empty());
    match (is_containerized, apm_enabled) {
        (true, true) => INSTALL_TYPE_DOCKER_SINGLE_STEP_DEFAULT.clone(),
        (true, false) => INSTALL_TYPE_DOCKER_DEFAULT.clone(),
        _ => INSTALL_TYPE_DEFAULT.clone(),
    }
}
