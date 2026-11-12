//! Files placed into a container as one archive, and files read back out of it.

use std::io::Read as _;

use bollard::{
    body_full,
    query_parameters::{DownloadFromContainerOptionsBuilder, UploadToContainerOptionsBuilder},
};
use bytes::Bytes;
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::time::Instant;

use crate::{container::Container, removal::bounded};

/// A set of files to place into a container in one request.
pub struct Archive {
    builder: tar::Builder<Vec<u8>>,
}

impl Archive {
    /// Creates an empty archive.
    pub fn new() -> Self {
        Self {
            builder: tar::Builder::new(Vec::new()),
        }
    }

    /// Adds a file with the given permission bits.
    ///
    /// `path` is relative to the root the archive is placed at, uses `/` as its separator, and may name directories
    /// that do not exist yet.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` is absolute or contains `..`.
    pub fn add_file(&mut self, path: &str, contents: &[u8], mode: u32) -> Result<(), GenericError> {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Regular);
        self.builder
            .append_data(&mut header, path, contents)
            .with_error_context(|| format!("Failed to add '{}' to an archive.", path))
    }

    /// Adds a directory with the given permission bits, so the directory has them even if no file of the archive lies
    /// in it.
    ///
    /// `path` is relative to the root the archive is placed at, as in [`Self::add_file`].
    ///
    /// # Errors
    ///
    /// Returns an error if `path` is absolute or contains `..`.
    pub fn add_dir(&mut self, path: &str, mode: u32) -> Result<(), GenericError> {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Directory);
        self.builder
            .append_data(&mut header, path, std::io::empty())
            .with_error_context(|| format!("Failed to add '{}' to an archive.", path))
    }
}

impl Default for Archive {
    fn default() -> Self {
        Self::new()
    }
}

impl Container {
    /// Places every file of `archive` under `root`, such as `/`, in the container's filesystem.
    ///
    /// This works whether or not the container is running, so files can be placed between create and start. A file
    /// whose path lies on a volume the container mounts is written to that volume.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive cannot be finished, if Docker refuses to extract it, or if `deadline` passes
    /// first.
    pub async fn place(&self, root: &str, archive: Archive, deadline: Instant) -> Result<(), GenericError> {
        let tar = archive
            .builder
            .into_inner()
            .error_context("Failed to finish an archive.")?;
        let options = UploadToContainerOptionsBuilder::default().path(root).build();
        let upload = self
            .docker
            .upload_to_container(&self.name, Some(options), body_full(Bytes::from(tar)));
        bounded(Some(deadline), upload).await.map_err(|e| {
            generic_error!(
                "Failed to place files under '{}' in container '{}': {}",
                root,
                &self.name,
                e
            )
        })
    }

    /// Reads the file at `path` in the container's filesystem, or returns `None` if nothing is there.
    ///
    /// # Errors
    ///
    /// Returns an error if the container does not exist, if Docker cannot copy the path out, if the path is not a
    /// regular file, or if `deadline` passes first.
    pub async fn read_file(&self, path: &str, deadline: Instant) -> Result<Option<Vec<u8>>, GenericError> {
        let options = DownloadFromContainerOptionsBuilder::default().path(path).build();
        let download = self
            .docker
            .download_from_container(&self.name, Some(options))
            .map_ok(|chunk| chunk.to_vec())
            .try_concat();
        let tar = match bounded(Some(deadline), download).await {
            Ok(tar) => tar,
            // Docker answers 404 both for a missing path and for a missing container.
            Err(e) if e.is_not_found() => {
                return match bounded(Some(deadline), self.docker.inspect_container(&self.name, None)).await {
                    Ok(_) => Ok(None),
                    Err(e) => Err(generic_error!(
                        "Failed to read '{}': container '{}' cannot be inspected: {}",
                        path,
                        &self.name,
                        e
                    )),
                }
            }
            Err(e) => {
                return Err(generic_error!(
                    "Failed to copy '{}' out of container '{}': {}",
                    path,
                    &self.name,
                    e
                ))
            }
        };

        let mut archive = tar::Archive::new(tar.as_slice());
        let mut entries = archive
            .entries()
            .with_error_context(|| format!("Failed to read the archive of '{}'.", path))?;
        let mut entry = entries
            .next()
            .ok_or_else(|| generic_error!("Docker returned an empty archive for '{}'.", path))?
            .with_error_context(|| format!("Failed to read the archive of '{}'.", path))?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(generic_error!(
                "'{}' in container '{}' is not a regular file.",
                path,
                &self.name
            ));
        }
        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .with_error_context(|| format!("Failed to read '{}' from its archive.", path))?;
        Ok(Some(contents))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_refuses_paths_outside_its_root() {
        let mut archive = Archive::new();

        assert!(archive.add_file("/etc/passwd", b"", 0o644).is_err());
        assert!(archive.add_file("data/../../etc/passwd", b"", 0o644).is_err());
        assert!(archive.add_dir("/tmp", 0o777).is_err());
        archive.add_file("data/greeting", b"hello", 0o644).unwrap();
    }
}
