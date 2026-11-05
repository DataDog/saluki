//! File placement, status, and reads through the Docker archive API for Linux and Windows containers.

use std::{
    collections::HashSet,
    io::Read as _,
    time::{SystemTime, UNIX_EPOCH},
};

use bollard::{
    body_full,
    container::PathStatResponse,
    errors::Error,
    query_parameters::{
        ContainerArchiveInfoOptionsBuilder, DownloadFromContainerOptionsBuilder, UploadToContainerOptionsBuilder,
    },
    Docker,
};
use bytes::Bytes;
use futures::StreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tar::{Archive, Builder, EntryType, Header};

use crate::driver::ContainerOs;

/// The only drive Windows container paths may name.
const WINDOWS_ROOT: &str = "C:\\";

/// Characters, besides `/` and control characters, that Windows forbids in file names.
const WINDOWS_RESERVED_CHARACTERS: &str = "<>:\"|?*";

/// A container path's kind, permission bits, and size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileStat {
    /// The kind of path Docker reports.
    pub kind: FileKind,
    /// Unix permission bits, including `setuid`, `setgid`, and sticky bits (`0000`–`7777`).
    pub mode: u32,
    /// The size in bytes.
    pub size: u64,
}

/// The kind of a container path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink {
        /// The target as Docker resolves it: an absolute container path.
        target: String,
    },
    /// A Unix socket.
    Socket,
    /// A named pipe (FIFO).
    NamedPipe,
    /// A device or another unrecognized kind.
    Other,
}

/// A file or directory's absolute container path, permission bits, and contents.
///
/// Linux paths must name an entry below `/`, with no empty, `.` or `..` components or NUL bytes. Windows paths
/// must name an entry below `C:\`, with backslash separators, no empty, `.` or `..` components, no `/`, and none
/// of `<>:"|?*` or control characters in a component. Validation happens when placing files. Clones share the
/// contents' storage.
///
/// `Debug` output includes the contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerFile {
    path: String,
    mode: u32,
    contents: Option<Bytes>,
}

impl ContainerFile {
    /// Creates a file with mode `0644`.
    pub fn new(path: impl Into<String>, contents: impl Into<Bytes>) -> Self {
        Self {
            path: path.into(),
            mode: 0o644,
            contents: Some(contents.into()),
        }
    }

    /// Creates a directory with mode `0755`.
    ///
    /// Placement creates missing parents, and an existing directory keeps its contents.
    pub fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mode: 0o755,
            contents: None,
        }
    }

    /// Sets the permission bits, including `setuid`, `setgid`, and sticky bits (`0000`–`7777`).
    ///
    /// Windows containers ignore the bits, but airlock still sends and validates them.
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
}

pub(crate) async fn place(
    docker: &Docker, os: ContainerOs, container: &str, files: &[ContainerFile],
) -> Result<(), GenericError> {
    if files.is_empty() {
        return Ok(());
    }
    validate_files(os, files).with_error_context(|| format!("Invalid files for container '{container}'"))?;
    let archive = build_archive(os, files)
        .with_error_context(|| format!("Failed to build file archive for container '{container}'"))?;
    docker
        .upload_to_container(
            container,
            Some(
                UploadToContainerOptionsBuilder::default()
                    .path(upload_root(os))
                    .no_overwrite_dir_non_dir("true")
                    .build(),
            ),
            body_full(archive.into()),
        )
        .await
        .with_error_context(|| format!("Failed to place files in container '{container}'"))
}

pub(crate) async fn read(
    docker: &Docker, os: ContainerOs, container: &str, path: &str,
) -> Result<Option<Vec<u8>>, GenericError> {
    entry_name(os, path)?;
    let mut stream = docker.download_from_container(
        container,
        Some(DownloadFromContainerOptionsBuilder::default().path(path).build()),
    );
    let mut archive = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => archive.extend_from_slice(&bytes),
            Err(Error::DockerResponseServerError { status_code: 404, .. }) => {
                ensure_container_exists(docker, container, path, "reading").await?;
                return Ok(None);
            }
            Err(error) => {
                return Err(
                    generic_error!(error).context(format!("Failed to read '{path}' in container '{container}'"))
                );
            }
        }
    }
    read_archive(&archive, container, path).map(Some)
}

pub(crate) async fn stat(
    docker: &Docker, os: ContainerOs, container: &str, path: &str,
) -> Result<Option<FileStat>, GenericError> {
    entry_name(os, path)?;
    match docker
        .get_container_archive_info(
            container,
            Some(ContainerArchiveInfoOptionsBuilder::default().path(path).build()),
        )
        .await
    {
        Ok(response) => file_stat(path, response)
            .map(Some)
            .with_error_context(|| format!("Failed to stat '{path}' in container '{container}'")),
        Err(Error::DockerResponseServerError { status_code: 404, .. }) => {
            ensure_container_exists(docker, container, path, "checking").await?;
            Ok(None)
        }
        Err(error) => Err(generic_error!(error).context(format!("Failed to stat '{path}' in container '{container}'"))),
    }
}

async fn ensure_container_exists(
    docker: &Docker, container: &str, path: &str, operation: &str,
) -> Result<(), GenericError> {
    // Docker uses 404 for both a missing path and a missing container.
    match docker.inspect_container(container, None).await {
        Ok(_) => Ok(()),
        Err(Error::DockerResponseServerError { status_code: 404, .. }) => {
            Err(generic_error!("no such container '{container}'"))
        }
        Err(error) => Err(generic_error!(error).context(format!(
            "Failed to inspect container '{container}' after {operation} '{path}'"
        ))),
    }
}

fn file_stat(path: &str, response: PathStatResponse) -> Result<FileStat, GenericError> {
    // Docker encodes mode using Go's io/fs.FileMode bit layout.
    const MODE_DIR: u32 = 1 << 31;
    const MODE_SYMLINK: u32 = 1 << 27;
    const MODE_DEVICE: u32 = 1 << 26;
    const MODE_NAMED_PIPE: u32 = 1 << 25;
    const MODE_SOCKET: u32 = 1 << 24;
    const MODE_SETUID: u32 = 1 << 23;
    const MODE_SETGID: u32 = 1 << 22;
    const MODE_CHAR_DEVICE: u32 = 1 << 21;
    const MODE_STICKY: u32 = 1 << 20;
    const MODE_IRREGULAR: u32 = 1 << 19;

    let mode = response.file_mode;
    let kind = if mode & MODE_DIR != 0 {
        FileKind::Directory
    } else if mode & MODE_SYMLINK != 0 {
        FileKind::Symlink {
            target: response.link_target,
        }
    } else if mode & MODE_SOCKET != 0 {
        FileKind::Socket
    } else if mode & MODE_NAMED_PIPE != 0 {
        FileKind::NamedPipe
    } else if mode & (MODE_DEVICE | MODE_CHAR_DEVICE | MODE_IRREGULAR) != 0 {
        FileKind::Other
    } else {
        FileKind::File
    };
    let mut permissions = mode & 0o777;
    for (go_bit, unix_bit) in [(MODE_SETUID, 0o4000), (MODE_SETGID, 0o2000), (MODE_STICKY, 0o1000)] {
        if mode & go_bit != 0 {
            permissions |= unix_bit;
        }
    }
    let size = u64::try_from(response.size).with_error_context(|| format!("Negative file size for '{path}'"))?;
    Ok(FileStat {
        kind,
        mode: permissions,
        size,
    })
}

/// The archive API path that placed entries are relative to.
fn upload_root(os: ContainerOs) -> &'static str {
    match os {
        ContainerOs::Linux => "/",
        ContainerOs::Windows => WINDOWS_ROOT,
    }
}

/// Validates an absolute container path and returns its archive entry name: the components joined by `/`.
fn entry_name(os: ContainerOs, path: &str) -> Result<String, GenericError> {
    let name = match os {
        ContainerOs::Linux => path
            .strip_prefix('/')
            .filter(|relative| {
                !relative.contains('\0') && relative.split('/').all(|part| !matches!(part, "" | "." | ".."))
            })
            .map(str::to_string),
        ContainerOs::Windows => path
            .strip_prefix(WINDOWS_ROOT)
            .filter(|relative| relative.split('\\').all(is_windows_component))
            .map(|relative| relative.replace('\\', "/")),
    };
    name.ok_or_else(|| match os {
        ContainerOs::Linux => generic_error!(
            "Invalid container file path '{path}': expected an absolute file path with no empty, '.' or '..' components or NUL bytes"
        ),
        ContainerOs::Windows => generic_error!(
            "Invalid container file path '{path}': expected a path below C:\\ with backslash separators, no empty, '.' or '..' components, and no '/', control characters or any of {WINDOWS_RESERVED_CHARACTERS}"
        ),
    })
}

fn is_windows_component(component: &str) -> bool {
    !matches!(component, "" | "." | "..")
        && !component
            .chars()
            .any(|c| c == '/' || c.is_ascii_control() || WINDOWS_RESERVED_CHARACTERS.contains(c))
}

fn validate_files(os: ContainerOs, files: &[ContainerFile]) -> Result<(), GenericError> {
    let mut paths = HashSet::new();
    for file in files {
        let mut name = entry_name(os, &file.path)?;
        if file.mode & !0o7777 != 0 {
            return Err(generic_error!(
                "Invalid mode {:#o} for '{}': only permission bits 0000–7777 are allowed",
                file.mode,
                file.path
            ));
        }
        // Windows paths are case-insensitive.
        if os == ContainerOs::Windows {
            name.make_ascii_lowercase();
        }
        if !paths.insert(name) {
            return Err(generic_error!("Duplicate container file path '{}'", file.path));
        }
    }
    Ok(())
}

fn build_archive(os: ContainerOs, files: &[ContainerFile]) -> Result<Vec<u8>, GenericError> {
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .error_context("Failed to get file modification time")?
        .as_secs();
    let mut archive = Builder::new(Vec::new());
    for file in files {
        let mut name = entry_name(os, &file.path)?;
        let mut header = Header::new_gnu();
        header.set_mode(file.mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(mtime);
        // Archives that hold only files preserve existing directory modes and ownership.
        let contents = match &file.contents {
            Some(contents) => {
                header.set_entry_type(EntryType::Regular);
                contents.as_ref()
            }
            None => {
                header.set_entry_type(EntryType::Directory);
                name.push('/');
                &[]
            }
        };
        header.set_size(contents.len() as u64);
        archive
            .append_data(&mut header, name, contents)
            .with_error_context(|| format!("Failed to archive '{}'", file.path))?;
    }
    archive.into_inner().error_context("Failed to finish file archive")
}

fn read_archive(bytes: &[u8], container: &str, path: &str) -> Result<Vec<u8>, GenericError> {
    // Malformed headers can put arbitrary archive bytes in tar's diagnostics.
    let invalid = || generic_error!("Invalid file archive for '{path}' in container '{container}'");
    let mut archive = Archive::new(bytes);
    let mut entries = archive.entries().map_err(|_| invalid())?;
    let mut entry = entries
        .next()
        .ok_or_else(|| generic_error!("Empty file archive for '{path}' in container '{container}'"))?
        .map_err(|_| invalid())?;
    let kind = entry.header().entry_type();
    if kind.is_dir() {
        return Err(generic_error!("'{path}' in container '{container}' is a directory"));
    }
    if kind.is_symlink() {
        let target = entry.link_name().map_err(|_| invalid())?.ok_or_else(invalid)?;
        return Err(generic_error!(
            "'{path}' in container '{container}' is a symbolic link to '{}'",
            target.display()
        ));
    }
    if !kind.is_file() {
        return Err(generic_error!(
            "'{path}' in container '{container}' is not a regular file"
        ));
    }
    let mut contents = Vec::new();
    entry.read_to_end(&mut contents).map_err(|_| invalid())?;
    if contents.len() as u64 != entry.size() {
        return Err(invalid());
    }
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon::FakeDaemon;

    #[test]
    fn container_paths_must_be_absolute_and_normalized() {
        for path in ["", "etc/x", "/", "/a/", "/a//b", "/a/./b", "/a/../b", "/a\0b"] {
            assert!(entry_name(ContainerOs::Linux, path).is_err(), "{path:?}");
        }
        let long = format!("/opt/{}/file", "a".repeat(210));
        for path in ["/a", "/etc/services.d/data-plane/finish", &long] {
            assert_eq!(entry_name(ContainerOs::Linux, path).unwrap(), &path[1..]);
        }
    }

    #[test]
    fn file_modes_beyond_permission_bits_are_rejected() {
        for (mode, valid) in [
            (0o10000, false),
            (0o100644, false),
            (0o7777, true),
            (0o755, true),
            (0, true),
        ] {
            let result = validate_files(
                ContainerOs::Linux,
                &[ContainerFile::new("/file", "secret").with_mode(mode)],
            );
            assert_eq!(result.is_ok(), valid, "{mode:o}");
            if let Err(error) = result {
                assert!(
                    error.to_string().contains(&format!("Invalid mode {mode:#o}")),
                    "{error}"
                );
                assert!(!format!("{error:?}").contains("secret"));
            }
        }
    }

    #[test]
    fn a_placement_with_a_duplicate_path_is_rejected() {
        let error = validate_files(
            ContainerOs::Linux,
            &[
                ContainerFile::new("/file", "secret"),
                ContainerFile::new("/file", "other secret"),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("Duplicate container file path '/file'"));
        assert!(!format!("{error:?}").contains("secret"));
    }

    #[test]
    fn placed_files_become_root_owned_regular_entries_relative_to_the_root() {
        let long = format!("/opt/{}/file", "a".repeat(210));
        let files = [
            ContainerFile::new("/etc/config", "configuration"),
            ContainerFile::new(long, Bytes::from_static(b"\0\xffbinary")).with_mode(0o755),
        ];
        let bytes = build_archive(ContainerOs::Linux, &files).unwrap();
        let mut archive = Archive::new(bytes.as_slice());
        let mut entries = archive.entries().unwrap();
        for file in files {
            let mut entry = entries.next().unwrap().unwrap();
            assert_eq!(entry.path_bytes(), &file.path.as_bytes()[1..]);
            assert_eq!(entry.header().mode().unwrap(), file.mode);
            assert_eq!(entry.header().uid().unwrap(), 0);
            assert_eq!(entry.header().gid().unwrap(), 0);
            assert_eq!(entry.header().entry_type(), EntryType::Regular);
            assert!(entry.header().mtime().unwrap() > 0);
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).unwrap();
            assert_eq!(Some(contents.into()), file.contents);
        }
        assert!(entries.next().is_none());
    }

    #[test]
    fn an_archive_of_one_regular_file_yields_its_contents() {
        let contents = b"\0\xfffile bytes";
        let bytes = build_archive(
            ContainerOs::Linux,
            &[ContainerFile::new("/file", Bytes::from_static(contents))],
        )
        .unwrap();
        assert_eq!(read_archive(&bytes, "container", "/file").unwrap(), contents);

        let long = format!("/opt/{}/file", "a".repeat(210));
        let mut archive = Builder::new(Vec::new());
        archive
            .append_pax_extensions([("path", &long.as_bytes()[1..])])
            .unwrap();
        let mut header = Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_entry_type(EntryType::Regular);
        archive.append_data(&mut header, "file", &contents[..]).unwrap();
        let bytes = archive.into_inner().unwrap();
        assert_eq!(read_archive(&bytes, "container", &long).unwrap(), contents);
    }

    #[test]
    fn directories_links_and_empty_or_malformed_archives_are_not_file_reads() {
        for (kind, reason) in [
            (EntryType::Directory, "is a directory"),
            (EntryType::Symlink, "is a symbolic link to 'target'"),
            (EntryType::Link, "is not a regular file"),
        ] {
            let mut archive = Builder::new(Vec::new());
            let mut header = Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_link_name("target").unwrap();
            archive.append_data(&mut header, "file", &[][..]).unwrap();
            let bytes = archive.into_inner().unwrap();
            let error = read_archive(&bytes, "container", "/file").unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
        }
        let empty = Builder::new(Vec::new()).into_inner().unwrap();
        let file = build_archive(ContainerOs::Linux, &[ContainerFile::new("/file", "secret".repeat(200))]).unwrap();
        for bytes in [empty.as_slice(), &[], &b"secret".repeat(100), &file[..1024]] {
            let error = read_archive(bytes, "container", "/file").unwrap_err();
            assert!(error.to_string().contains("'/file' in container 'container'"));
            assert!(!format!("{error:?}").contains("secret"));
            assert!(!format!("{error:#}").contains("secret"));
        }
    }

    #[tokio::test]
    async fn placing_no_files_sends_no_request() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"unexpected request"}"#)).await;
        place(&daemon.docker, ContainerOs::Linux, "container", &[])
            .await
            .unwrap();
        assert!(daemon.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_files_and_read_paths_send_no_request() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"unexpected request"}"#)).await;
        for file in [
            ContainerFile::new("relative", "secret"),
            ContainerFile::new("/bad-mode", "secret").with_mode(0o10000),
            ContainerFile::new("/file", "secret"),
        ] {
            let error = place(
                &daemon.docker,
                ContainerOs::Linux,
                "container",
                &[ContainerFile::new("/file", "secret"), file],
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), "Invalid files for container 'container'");
            assert!(!format!("{error:?}").contains("secret"));
        }
        assert!(read(&daemon.docker, ContainerOs::Linux, "container", "/a/../b")
            .await
            .is_err());
        assert!(stat(&daemon.docker, ContainerOs::Linux, "container", "/a/../b")
            .await
            .is_err());
        assert!(daemon.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_placement_uses_one_root_archive_request_and_preserves_docker_errors() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"cannot overwrite directory"}"#)).await;
        let error = place(
            &daemon.docker,
            ContainerOs::Linux,
            "container",
            &[
                ContainerFile::new("/etc", Bytes::from_static(b"\0\xffsecret")),
                ContainerFile::new("/file", "secret"),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Failed to place files in container 'container'");
        assert!(error.downcast_ref::<Error>().is_some());
        assert!(format!("{error:#}").contains("cannot overwrite directory"));
        assert!(!format!("{error:?}").contains("secret"));
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("PUT /containers/container/archive?"),
            "{requests:?}"
        );
        assert!(requests[0].contains("path=%2F"), "{requests:?}");
        assert!(requests[0].contains("noOverwriteDirNonDir=true"), "{requests:?}");
    }

    #[tokio::test]
    async fn a_missing_path_reads_as_none_only_when_the_container_exists() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                (
                    "GET",
                    "/containers/present/archive" | "/containers/missing/archive" | "/containers/uninspectable/archive",
                ) => (404, r#"{"message":"unavailable"}"#),
                ("GET", "/containers/present/json") => (200, "{}"),
                ("GET", "/containers/missing/json") => (404, r#"{"message":"unavailable"}"#),
                _ => (500, r#"{"message":"failed"}"#),
            },
        )
        .await;
        assert_eq!(
            read(&daemon.docker, ContainerOs::Linux, "present", "/file")
                .await
                .unwrap(),
            None
        );
        let error = read(&daemon.docker, ContainerOs::Linux, "missing", "/file")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "no such container 'missing'");
        assert!(read(&daemon.docker, ContainerOs::Linux, "uninspectable", "/file")
            .await
            .is_err());
        assert!(read(&daemon.docker, ContainerOs::Linux, "broken", "/file")
            .await
            .is_err());
        let requests = daemon.requests.lock().unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.split('?').next().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "GET /containers/present/archive",
                "GET /containers/present/json",
                "GET /containers/missing/archive",
                "GET /containers/missing/json",
                "GET /containers/uninspectable/archive",
                "GET /containers/uninspectable/json",
                "GET /containers/broken/archive",
            ]
        );
    }

    #[tokio::test]
    async fn a_missing_path_stats_as_none_only_when_the_container_exists() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                (
                    "HEAD",
                    "/containers/present/archive" | "/containers/missing/archive" | "/containers/uninspectable/archive",
                ) => (404, r#"{"message":"unavailable"}"#),
                ("GET", "/containers/present/json") => (200, "{}"),
                ("GET", "/containers/missing/json") => (404, r#"{"message":"unavailable"}"#),
                _ => (500, r#"{"message":"failed"}"#),
            },
        )
        .await;
        assert_eq!(
            stat(&daemon.docker, ContainerOs::Linux, "present", "/file")
                .await
                .unwrap(),
            None
        );
        let error = stat(&daemon.docker, ContainerOs::Linux, "missing", "/file")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "no such container 'missing'");
        let error = stat(&daemon.docker, ContainerOs::Linux, "uninspectable", "/file")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Failed to inspect container 'uninspectable' after checking '/file'"
        );
        assert!(error.downcast_ref::<Error>().is_some());
        let error = stat(&daemon.docker, ContainerOs::Linux, "broken", "/file")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Failed to stat '/file' in container 'broken'");
        assert!(error.downcast_ref::<Error>().is_some());
        let requests = daemon.requests.lock().unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.split('?').next().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "HEAD /containers/present/archive",
                "GET /containers/present/json",
                "HEAD /containers/missing/archive",
                "GET /containers/missing/json",
                "HEAD /containers/uninspectable/archive",
                "GET /containers/uninspectable/json",
                "HEAD /containers/broken/archive",
            ]
        );
    }

    #[tokio::test]
    async fn a_stat_without_a_path_stat_header_is_an_error() {
        let daemon = FakeDaemon::start(|_, _| (200, "")).await;
        let error = stat(&daemon.docker, ContainerOs::Linux, "container", "/file")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Failed to stat '/file' in container 'container'");
        assert!(error.downcast_ref::<Error>().is_some());
        assert_eq!(
            *daemon.requests.lock().unwrap(),
            ["HEAD /containers/container/archive?path=%2Ffile"]
        );
    }

    #[test]
    fn docker_file_modes_map_to_kinds_and_permission_bits() {
        let link = FileKind::Symlink {
            target: "/airlock/x/f".to_string(),
        };
        for (file_mode, kind, mode) in [
            (416, FileKind::File, 0o640),
            (2147484141, FileKind::Directory, 0o755),
            (33554852, FileKind::NamedPipe, 0o644),
            (134218239, link.clone(), 0o777),
            (16777654, FileKind::Socket, 0o666),
            (1 << 23 | 0o755, FileKind::File, 0o4755),
            (1 << 31 | 1 << 20 | 0o777, FileKind::Directory, 0o1777),
            (1 << 22 | 0o2755, FileKind::File, 0o2755),
            (1 << 26 | 1 << 21 | 0o666, FileKind::Other, 0o666),
            (1 << 26 | 0o660, FileKind::Other, 0o660),
            (1 << 21, FileKind::Other, 0),
            (1 << 19, FileKind::Other, 0),
            (1 << 31 | 1 << 27, FileKind::Directory, 0),
            (1 << 27 | 1 << 24, link, 0),
            (1 << 24 | 1 << 25, FileKind::Socket, 0),
            (1 << 25 | 1 << 26, FileKind::NamedPipe, 0),
        ] {
            let response = PathStatResponse {
                name: "path".to_string(),
                size: 11,
                file_mode,
                modification_time: Default::default(),
                link_target: "/airlock/x/f".to_string(),
            };
            assert_eq!(
                file_stat("/path", response).unwrap(),
                FileStat { kind, mode, size: 11 },
                "{file_mode:#x}"
            );
        }
        let error = file_stat(
            "/negative",
            PathStatResponse {
                name: "negative".to_string(),
                size: -1,
                file_mode: 0o644,
                modification_time: Default::default(),
                link_target: String::new(),
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Negative file size for '/negative'");
    }

    #[test]
    fn windows_container_paths_must_be_drive_rooted_with_backslashes_and_no_traversal() {
        let invalid = [
            "",
            "C:",
            "C:\\",
            "c:\\file",
            "D:\\file",
            "\\file",
            "/file",
            "C:/file",
            "file",
            "C:\\a\\",
            "C:\\a\\\\b",
            "C:\\a\\.\\b",
            "C:\\a\\..\\b",
            "C:\\a/b",
            "C:\\a\\b:stream",
            "C:\\a\\b<",
            "C:\\a\\b>",
            "C:\\a\\\"b",
            "C:\\a\\b|",
            "C:\\a\\b?",
            "C:\\a\\b*",
            "C:\\a\\b\0",
            "C:\\a\\b\n",
        ];
        for path in invalid {
            let error = entry_name(ContainerOs::Windows, path).unwrap_err().to_string();
            assert!(error.contains(&format!("'{path}'")), "{path:?}: {error}");
        }
        for (path, name) in [
            ("C:\\a", "a"),
            ("C:\\airlock\\p\\exit", "airlock/p/exit"),
            ("C:\\Program Files\\x.exe", "Program Files/x.exe"),
            ("C:\\a\\b.c\\d-e_f", "a/b.c/d-e_f"),
        ] {
            assert_eq!(entry_name(ContainerOs::Windows, path).unwrap(), name);
        }
        // Each OS accepts only its own paths.
        assert!(entry_name(ContainerOs::Linux, "C:\\a").is_err());
        assert!(entry_name(ContainerOs::Windows, "/a").is_err());
    }

    #[test]
    fn windows_duplicate_paths_ignore_case() {
        let error = validate_files(
            ContainerOs::Windows,
            &[
                ContainerFile::new("C:\\a\\File", "x"),
                ContainerFile::directory("C:\\A\\file"),
            ],
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Duplicate container file path 'C:\\A\\file'"));
        validate_files(
            ContainerOs::Linux,
            &[ContainerFile::new("/a/File", "x"), ContainerFile::new("/a/file", "x")],
        )
        .unwrap();
    }

    #[test]
    fn windows_files_and_directories_become_one_archive_uploaded_at_the_drive_root() {
        let files = [
            ContainerFile::directory("C:\\airlock\\p\\exit"),
            ContainerFile::directory("C:\\airlock\\p\\log").with_mode(0o700),
            ContainerFile::new("C:\\panoramic\\courier.exe", Bytes::from_static(b"MZ\0\xff")).with_mode(0o755),
        ];
        assert_eq!(upload_root(ContainerOs::Windows), "C:\\");
        assert_eq!(upload_root(ContainerOs::Linux), "/");
        validate_files(ContainerOs::Windows, &files).unwrap();
        let bytes = build_archive(ContainerOs::Windows, &files).unwrap();
        let mut archive = Archive::new(bytes.as_slice());
        let mut entries = archive.entries().unwrap();
        for (name, kind, mode) in [
            ("airlock/p/exit/", EntryType::Directory, 0o755),
            ("airlock/p/log/", EntryType::Directory, 0o700),
            ("panoramic/courier.exe", EntryType::Regular, 0o755),
        ] {
            let mut entry = entries.next().unwrap().unwrap();
            assert_eq!(entry.path_bytes(), name.as_bytes());
            assert_eq!(entry.header().entry_type(), kind);
            assert_eq!(entry.header().mode().unwrap(), mode);
            assert_eq!(entry.header().uid().unwrap(), 0);
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).unwrap();
            assert_eq!(contents, if kind.is_dir() { &b""[..] } else { b"MZ\0\xff" });
        }
        assert!(entries.next().is_none());
    }

    #[test]
    fn linux_directories_are_archived_as_directory_entries() {
        let bytes = build_archive(ContainerOs::Linux, &[ContainerFile::directory("/airlock/p")]).unwrap();
        let mut archive = Archive::new(bytes.as_slice());
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path_bytes(), b"airlock/p/".as_slice());
        assert_eq!(entry.header().entry_type(), EntryType::Directory);
        assert_eq!(entry.header().mode().unwrap(), 0o755);
    }

    #[tokio::test]
    async fn a_windows_placement_uploads_to_the_drive_root_and_invalid_paths_send_nothing() {
        let daemon = FakeDaemon::start(|_, _| (200, "")).await;
        place(
            &daemon.docker,
            ContainerOs::Windows,
            "container",
            &[ContainerFile::directory("C:\\airlock\\p\\exit")],
        )
        .await
        .unwrap();
        {
            let requests = daemon.requests.lock().unwrap();
            assert_eq!(requests.len(), 1, "{requests:?}");
            assert!(
                requests[0].starts_with("PUT /containers/container/archive?"),
                "{requests:?}"
            );
            assert!(
                requests[0].contains("path=C%3A%5C&") || requests[0].ends_with("path=C%3A%5C"),
                "{requests:?}"
            );
            let bodies = daemon.bodies.lock().unwrap();
            assert!(bodies[0].contains("airlock/p/exit/"), "{bodies:?}");
        }
        let error = place(
            &daemon.docker,
            ContainerOs::Windows,
            "container",
            &[ContainerFile::new("/airlock/file", "secret")],
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Invalid files for container 'container'");
        assert!(read(&daemon.docker, ContainerOs::Windows, "container", "C:/file")
            .await
            .is_err());
        assert!(stat(&daemon.docker, ContainerOs::Windows, "container", "C:\\a\\..\\b")
            .await
            .is_err());
        assert_eq!(daemon.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn windows_reads_and_stats_pass_the_path_to_docker_as_written() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("GET" | "HEAD", "/containers/present/archive") => (404, r#"{"message":"unavailable"}"#),
                ("GET", "/containers/present/json") => (200, "{}"),
                _ => (500, r#"{"message":"failed"}"#),
            },
        )
        .await;
        let path = "C:\\airlock\\p\\exit\\adp";
        assert_eq!(
            read(&daemon.docker, ContainerOs::Windows, "present", path)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            stat(&daemon.docker, ContainerOs::Windows, "present", path)
                .await
                .unwrap(),
            None
        );
        let requests = daemon.requests.lock().unwrap();
        let encoded = "path=C%3A%5Cairlock%5Cp%5Cexit%5Cadp";
        assert!(
            requests[0].starts_with("GET /containers/present/archive?") && requests[0].contains(encoded),
            "{requests:?}"
        );
        assert!(
            requests[2].starts_with("HEAD /containers/present/archive?") && requests[2].contains(encoded),
            "{requests:?}"
        );
    }
}
