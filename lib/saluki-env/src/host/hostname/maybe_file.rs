use std::path::PathBuf;

use async_trait::async_trait;
use tokio::fs;
use tracing::debug;

use super::{validate_hostname, HostnameProvider};

/// A hostname provider that reads the hostname from a file.
///
/// This is a convenience provider that allows setting a fixed hostname based on a file, where the file may or may not
/// exist, so the file path can be provided via `Option<PathBuf>` to avoid having to add that logic at the point of
/// specifying this provider, making it ease to provider a fluent builder.
pub struct MaybeFileHostnameProvider {
    file_path: Option<PathBuf>,
}

impl MaybeFileHostnameProvider {
    /// Create a new `MaybeFileHostnameProvider` with the given file path.
    pub fn new(file_path: Option<PathBuf>) -> Self {
        Self { file_path }
    }
}

#[async_trait]
impl HostnameProvider for MaybeFileHostnameProvider {
    async fn get_hostname(&self) -> Option<String> {
        let file_path = match self.file_path.as_ref() {
            Some(file_path) => file_path,
            None => {
                debug!("No file path provided for hostname file.");
                return None;
            }
        };

        let hostname = match fs::read_to_string(file_path).await {
            Ok(hostname) => hostname.trim().to_string(),
            Err(e) => {
                debug!(error = %e, path = ?self.file_path, "Failed to read hostname file.");
                return None;
            }
        };

        if let Err(e) = validate_hostname(&hostname) {
            debug!(error = %e, path = ?self.file_path, "Invalid hostname in file.");
            return None;
        }

        Some(hostname)
    }
}
