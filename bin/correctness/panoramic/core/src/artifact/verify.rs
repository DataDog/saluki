//! Checks bound artifacts and computes their identities: image IDs from the Docker daemon,
//! SHA-256 digests of binaries and install main binaries.

use std::{fs::File, io::Seek, path::Path};

use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::{
    elf::{self, Architecture},
    ArtifactBinding, ArtifactKind,
};
use crate::backend::docker::{image_id_within_deadline, DockerDaemon};

/// One binding's verification or pull outcome.
#[derive(Debug)]
pub struct ArtifactVerification {
    /// The role, reference and source checked or pulled.
    pub binding: ArtifactBinding,
    /// The artifact's identity, or why verification or pulling failed.
    pub identity: Result<String, String>,
}

/// An artifact a run verified, as `run.json` records it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerifiedArtifact {
    pub(crate) role: &'static str,
    /// The binding's reference as written.
    pub(crate) reference: String,
    pub(crate) identity: String,
    #[serde(serialize_with = "serialize_kind")]
    pub(crate) kind: ArtifactKind,
}

fn serialize_kind<S>(kind: &ArtifactKind, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(kind.name())
}

/// Returns the verified artifacts in input order.
///
/// # Errors
///
/// Returns the number of failed results if any binding failed verification.
pub fn verified_artifacts(results: &[ArtifactVerification]) -> Result<Vec<VerifiedArtifact>, usize> {
    let mut verified = Vec::with_capacity(results.len());
    let mut failed = 0;
    for result in results {
        match &result.identity {
            Ok(identity) => verified.push(VerifiedArtifact {
                role: result.binding.role.name,
                reference: result.binding.reference.clone(),
                identity: identity.clone(),
                kind: result.binding.role.kind,
            }),
            Err(_) => failed += 1,
        }
    }
    if failed == 0 {
        Ok(verified)
    } else {
        Err(failed)
    }
}

/// Verifies each binding, resolving relative paths against `base`, in input order.
/// Images are inspected on the command's Docker daemon, which must run the image role's OS.
/// Container binaries must be static executables for the daemon's OS and architecture.
/// Files are identified by their SHA-256 digest.
/// A daemon timeout fails the remaining image bindings; file checks continue.
pub(crate) async fn verify(
    docker: &DockerDaemon, bindings: &[ArtifactBinding], base: &Path,
) -> Vec<ArtifactVerification> {
    let mut timeout: Option<String> = None;
    let mut results = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let identity = match binding.role.kind {
            _ if binding.reference.is_empty() => Err(format!(
                "role '{}' requires a binding in an artifact manifest",
                binding.role.name
            )),
            ArtifactKind::Image { os } => match &timeout {
                Some(error) => Err(error.clone()),
                None => match docker.for_os(os).await {
                    Ok(images) => match image_id_within_deadline(images, &binding.reference).await {
                        Ok(result) => result.and_then(|id| {
                            id.ok_or_else(|| {
                                format!("image '{}' was not found on the Docker daemon", binding.reference)
                            })
                        }),
                        Err(error) => {
                            timeout = Some(error.clone());
                            Err(error)
                        }
                    },
                    Err(error) => Err(error),
                },
            },
            ArtifactKind::ContainerBinary { os } => {
                let path = base.join(&binding.reference);
                let display = path.display().to_string();
                let local = tokio::task::spawn_blocking(move || {
                    let mut file = open_binary(&path)?;
                    let identity = hash_binary(&mut file, &path)?;
                    let machine = elf::static_machine(&mut file, &path)?;
                    Ok((identity, machine))
                })
                .await
                .unwrap_or_else(|error| Err(format!("cannot verify '{display}': {error}")));
                match local {
                    Err(error) => Err(error),
                    Ok((identity, machine)) => match docker.architecture(os).await {
                        Err(error) => Err(error),
                        Ok(architecture) if architecture.elf_machine() == machine => Ok(identity),
                        Ok(architecture) => {
                            let built_for = Architecture::from_elf_machine(machine).map_or_else(
                                || format!("ELF machine {machine}"),
                                |architecture| architecture.name().to_string(),
                            );
                            Err(format!(
                                "'{display}' is built for {built_for}, but the Docker daemon runs on {}",
                                architecture.name()
                            ))
                        }
                    },
                }
            }
            kind => {
                let path = base.join(&binding.reference);
                let display = path.display().to_string();
                tokio::task::spawn_blocking(move || {
                    let binary = match kind {
                        ArtifactKind::Install { main_binary } => {
                            let metadata = std::fs::metadata(&path)
                                .map_err(|error| format!("cannot inspect '{}': {error}", path.display()))?;
                            if !metadata.is_dir() {
                                return Err(format!("'{}' is not a directory", path.display()));
                            }
                            path.join(main_binary)
                        }
                        _ => path,
                    };
                    let mut file = open_binary(&binary)?;
                    hash_binary(&mut file, &binary)
                })
                .await
                .unwrap_or_else(|error| Err(format!("cannot verify '{display}': {error}")))
            }
        };
        results.push(ArtifactVerification {
            binding: binding.clone(),
            identity,
        });
    }
    results
}

/// Opens a regular file, requiring execute permission on Unix.
fn open_binary(path: &Path) -> Result<File, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("cannot inspect '{}': {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("'{}' is not a file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("'{}' is not executable", path.display()));
        }
    }
    File::open(path).map_err(|error| format!("cannot open '{}': {error}", path.display()))
}

/// Hashes the open file from the start; `path` labels I/O errors.
fn hash_binary(file: &mut File, path: &Path) -> Result<String, String> {
    file.rewind()
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    let mut hash = Sha256::new();
    std::io::copy(file, &mut hash).map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    Ok(format!("sha256:{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use airlock::docker::DOCKER_DAEMON_TIMEOUT;
    use async_trait::async_trait;

    use super::*;
    use crate::{
        artifact::{
            test_util::{write_elf, write_executable},
            ArtifactRole, BindingSource,
        },
        backend::docker::{
            test_util::{recording_daemon, FakeDocker},
            DaemonInfo, DockerApi,
        },
        layout::Os,
    };

    async fn verify_with(
        bindings: &[ArtifactBinding], base: &Path,
        connect: impl Fn() -> Result<Box<dyn DockerApi>, String> + Send + Sync + 'static,
    ) -> Vec<ArtifactVerification> {
        verify(&DockerDaemon::with_connect(connect), bindings, base).await
    }

    const TOOL_DIGEST: &str = "sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d";
    const CONTAINER_BINARY: ArtifactKind = ArtifactKind::ContainerBinary { os: Os::Linux };
    const INSTALL: ArtifactKind = ArtifactKind::Install {
        main_binary: "bin/tool",
    };

    fn binding(name: &'static str, kind: ArtifactKind, reference: impl Into<String>) -> ArtifactBinding {
        ArtifactBinding {
            role: ArtifactRole {
                name,
                kind,
                local_default: "unused",
            },
            reference: reference.into(),
            source: BindingSource::Manifest,
        }
    }

    #[tokio::test]
    async fn missing_artifact_references_require_manifest_bindings_without_io() {
        let bindings = [
            ArtifactKind::Image { os: Os::Linux },
            ArtifactKind::Binary,
            CONTAINER_BINARY,
            INSTALL,
        ]
        .map(|kind| ArtifactBinding {
            role: ArtifactRole {
                name: "required",
                kind,
                local_default: "",
            },
            reference: String::new(),
            source: BindingSource::LocalDefault,
        });
        let results = verify_with(&bindings, Path::new("missing"), || {
            panic!("unbound roles must not connect")
        })
        .await;
        assert_eq!(results.len(), bindings.len());
        for (result, binding) in results.iter().zip(&bindings) {
            assert_eq!(&result.binding, binding);
            assert_eq!(
                result.identity.as_deref().map_err(String::as_str),
                Err("role 'required' requires a binding in an artifact manifest")
            );
        }
    }

    #[tokio::test]
    async fn binaries_and_installs_hash_the_main_binary_and_resolve_relative_references_against_base() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("bin/tool");
        write_executable(&binary);
        let bindings = [
            binding("absolute", ArtifactKind::Binary, binary.to_str().unwrap()),
            binding("relative", ArtifactKind::Binary, "bin/tool"),
            binding("install", INSTALL, dir.path().to_str().unwrap()),
            binding("relative-install", INSTALL, "."),
        ];
        let results = verify_with(&bindings, dir.path(), || panic!("file roles must not connect")).await;
        assert_eq!(results.len(), bindings.len());
        for (result, expected) in results.iter().zip(&bindings) {
            assert_eq!(&result.binding, expected);
            assert_eq!(result.identity.as_deref().map_err(String::as_str), Ok(TOOL_DIGEST));
        }
    }

    #[test]
    fn hashing_and_elf_checks_use_the_open_file_when_its_path_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        write_elf(&path, 2, 1, 3, 62, false);
        let expected_digest = format!("sha256:{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        let mut file = open_binary(&path).unwrap();
        assert_eq!(hash_binary(&mut file, &path), Ok(expected_digest.clone()));
        std::fs::rename(&path, dir.path().join("original")).unwrap();
        write_elf(&path, 2, 1, 2, 183, false);
        assert_eq!(elf::static_machine(&mut file, &path), Ok(62));
        assert_eq!(hash_binary(&mut file, &path), Ok(expected_digest));
        assert_eq!(elf::static_machine(&mut open_binary(&path).unwrap(), &path), Ok(183));
    }

    #[tokio::test]
    async fn a_container_binary_must_be_a_static_executable_for_the_daemons_architecture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        for (machine, daemon, expected) in [
            (62, "x86_64", None),
            (183, "aarch64", None),
            (
                62,
                "aarch64",
                Some("is built for x86_64, but the Docker daemon runs on aarch64"),
            ),
            (
                183,
                "x86_64",
                Some("is built for aarch64, but the Docker daemon runs on x86_64"),
            ),
            (
                243,
                "aarch64",
                Some("is built for ELF machine 243, but the Docker daemon runs on aarch64"),
            ),
        ] {
            write_elf(&path, 2, 1, 3, machine, false);
            let mut api = FakeDocker::new(Some("linux"));
            api.info.as_mut().unwrap().architecture = Some(daemon.into());
            let bindings = [binding("tool", CONTAINER_BINARY, "tool")];
            let results = verify(&api.daemon(), &bindings, dir.path()).await;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].binding, bindings[0]);
            let expected = match expected {
                Some(problem) => Err(format!("'{}' {problem}", path.display())),
                None => Ok(format!("sha256:{:x}", Sha256::digest(std::fs::read(&path).unwrap()))),
            };
            assert_eq!(results[0].identity, expected);
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
        }
        for (elf, expected) in [
            (true, "is dynamically linked; containers need a static executable"),
            (false, "is not an ELF executable"),
        ] {
            if elf {
                write_elf(&path, 2, 1, 3, 62, true);
            } else {
                write_executable(&path);
            }
            let results = verify(
                &FakeDocker::new(Some("linux")).daemon(),
                &[binding("tool", CONTAINER_BINARY, "tool")],
                dir.path(),
            )
            .await;
            assert_eq!(results[0].identity, Err(format!("'{}' {expected}", path.display())));
        }
    }

    #[tokio::test]
    async fn container_binary_file_problems_fail_without_contacting_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        write_elf(&dir.path().join("dynamic"), 2, 1, 3, 62, true);
        write_executable(&dir.path().join("non-elf"));
        std::fs::create_dir(dir.path().join("directory")).unwrap();
        let cases = [
            ("missing", "cannot inspect"),
            ("directory", "is not a file"),
            ("dynamic", "is dynamically linked; containers need a static executable"),
            ("non-elf", "is not an ELF executable"),
        ];
        #[cfg(unix)]
        let cases = {
            use std::os::unix::fs::PermissionsExt as _;
            let path = dir.path().join("not-executable");
            write_elf(&path, 2, 1, 2, 62, false);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let mut cases = cases.to_vec();
            cases.push(("not-executable", "is not executable"));
            cases
        };
        let bindings: Vec<_> = cases
            .iter()
            .map(|(name, _)| binding(name, CONTAINER_BINARY, *name))
            .collect();
        let results = verify_with(&bindings, dir.path(), || panic!("invalid files must not connect")).await;
        assert_eq!(results.len(), cases.len());
        for (result, (name, problem)) in results.iter().zip(cases) {
            let message = result.identity.as_ref().unwrap_err();
            assert!(message.contains(dir.path().join(name).to_str().unwrap()), "{message}");
            assert!(message.contains(problem), "{message}");
        }
    }

    #[tokio::test]
    async fn a_container_binary_on_a_daemon_of_another_os_or_unreachable_daemon_reports_the_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        write_elf(&dir.path().join("tool"), 2, 1, 2, 62, false);
        let bindings = [binding("tool", CONTAINER_BINARY, "tool")];
        let mut missing_architecture = FakeDocker::new(Some("linux"));
        missing_architecture.info.as_mut().unwrap().architecture = None;
        for (daemon, expected) in [
            (
                FakeDocker::new(Some("windows")).daemon(),
                "the Docker daemon runs windows containers, not linux",
            ),
            (
                DockerDaemon::with_connect(|| Err("unavailable".into())),
                "cannot connect to a Docker daemon: unavailable",
            ),
            (
                missing_architecture.daemon(),
                "the Docker daemon did not report its architecture",
            ),
        ] {
            let results = verify(&daemon, &bindings, dir.path()).await;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].identity, Err(expected.into()));
        }
    }

    #[tokio::test]
    async fn container_binaries_and_images_share_one_info_query() {
        let dir = tempfile::tempdir().unwrap();
        write_elf(&dir.path().join("tool"), 2, 1, 3, 62, false);
        let expected_digest = format!(
            "sha256:{:x}",
            Sha256::digest(std::fs::read(dir.path().join("tool")).unwrap())
        );
        for binary_first in [true, false] {
            let mut bindings = [
                binding("tool", CONTAINER_BINARY, "tool"),
                binding("image", ArtifactKind::Image { os: Os::Linux }, "tool:latest"),
            ];
            if !binary_first {
                bindings.reverse();
            }
            let api = FakeDocker::new(Some("linux"));
            let results = verify(&api.daemon(), &bindings, dir.path()).await;
            assert_eq!(results.len(), 2);
            for (result, binding) in results.iter().zip(bindings) {
                assert_eq!(result.binding, binding);
                let expected = if binding.role.kind == CONTAINER_BINARY {
                    &expected_digest
                } else {
                    "sha256:tool:latest"
                };
                assert_eq!(result.identity.as_deref().map_err(String::as_str), Ok(expected));
            }
            assert_eq!(*api.calls.lock().unwrap(), ["connect", "info", "inspect:tool:latest"]);
        }
    }

    #[tokio::test]
    async fn verified_artifacts_preserve_each_bindings_role_reference_identity_and_kind_in_input_order() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("bin/tool"));
        let bindings = [
            binding("tool", ArtifactKind::Binary, "./bin/tool"),
            binding("install", INSTALL, "."),
        ];
        let results = verify_with(&bindings, dir.path(), || panic!("file roles must not connect")).await;
        assert_eq!(
            verified_artifacts(&results),
            Ok(vec![
                VerifiedArtifact {
                    role: "tool",
                    reference: "./bin/tool".into(),
                    identity: TOOL_DIGEST.into(),
                    kind: ArtifactKind::Binary,
                },
                VerifiedArtifact {
                    role: "install",
                    reference: ".".into(),
                    identity: TOOL_DIGEST.into(),
                    kind: INSTALL,
                },
            ])
        );
    }

    #[tokio::test]
    async fn verified_artifacts_count_failures_instead_of_listing_identities() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("bin/tool"));
        let bindings = [
            binding("missing", ArtifactKind::Binary, "missing"),
            binding("tool", ArtifactKind::Binary, "bin/tool"),
            binding("missing-install", INSTALL, "missing-install"),
            binding("install", INSTALL, "."),
        ];
        let results = verify_with(&bindings, dir.path(), || panic!("file roles must not connect")).await;
        assert_eq!(verified_artifacts(&results), Err(2));
    }

    #[test]
    fn verified_artifacts_serialize_role_reference_identity_and_kind_in_order_with_kind_as_its_name() {
        for (kind, name) in [
            (ArtifactKind::Image { os: Os::Linux }, "image"),
            (ArtifactKind::Binary, "binary"),
            (INSTALL, "install"),
        ] {
            let artifact = VerifiedArtifact {
                role: "tool",
                reference: " reference ".into(),
                identity: TOOL_DIGEST.into(),
                kind,
            };
            assert_eq!(
                serde_json::to_value(&artifact).unwrap(),
                serde_json::json!({
                    "role": "tool",
                    "reference": " reference ",
                    "identity": TOOL_DIGEST,
                    "kind": name,
                })
            );
            assert_eq!(
                serde_json::to_string(&artifact).unwrap(),
                format!(
                    "{{\"role\":\"tool\",\"reference\":\" reference \",\"identity\":\"{TOOL_DIGEST}\",\"kind\":\"{name}\"}}"
                )
            );
        }
    }

    #[tokio::test]
    async fn invalid_binary_and_install_paths_report_every_failure_in_input_order() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("tool"));
        std::fs::create_dir(dir.path().join("install")).unwrap();
        let cases = [
            (
                binding("missing", ArtifactKind::Binary, "missing"),
                dir.path().join("missing"),
                "cannot inspect",
            ),
            (
                binding("directory", ArtifactKind::Binary, "install"),
                dir.path().join("install"),
                "is not a file",
            ),
            (
                binding("incomplete", INSTALL, "install"),
                // Joined the way verification joins an install with its main binary.
                dir.path().join("install").join("bin/tool"),
                "cannot inspect",
            ),
            (
                binding("file", INSTALL, "tool"),
                dir.path().join("tool"),
                "is not a directory",
            ),
            (
                binding("absent-install", INSTALL, "absent"),
                dir.path().join("absent"),
                "cannot inspect",
            ),
        ];
        let bindings: Vec<_> = cases.iter().map(|(binding, _, _)| binding.clone()).collect();
        let results = verify_with(&bindings, dir.path(), || panic!("files must not connect")).await;
        assert_eq!(results.len(), cases.len());
        for (result, (binding, path, problem)) in results.iter().zip(&cases) {
            assert_eq!(&result.binding, binding);
            let message = result.identity.as_ref().unwrap_err();
            assert!(message.contains(path.to_str().unwrap()), "{message}");
            assert!(message.contains(problem), "{message}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binaries_and_install_main_binaries_must_be_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("bin/tool");
        write_executable(&binary);
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).unwrap();
        let results = verify(
            &DockerDaemon::with_connect(|| panic!("files must not connect")),
            &[
                binding("tool", ArtifactKind::Binary, "bin/tool"),
                binding("install", INSTALL, dir.path().to_str().unwrap()),
            ],
            dir.path(),
        )
        .await;
        assert_eq!(results.len(), 2);
        for result in results {
            assert_eq!(
                result.identity,
                Err(format!("'{}' is not executable", binary.display()))
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binaries_and_install_main_binaries_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("tool"));
        std::os::unix::fs::symlink("tool", dir.path().join("link")).unwrap();
        std::fs::create_dir_all(dir.path().join("install/bin")).unwrap();
        std::os::unix::fs::symlink("../../tool", dir.path().join("install/bin/tool")).unwrap();
        let results = verify(
            &DockerDaemon::with_connect(|| panic!("files must not connect")),
            &[
                binding("tool", ArtifactKind::Binary, "link"),
                binding("install", INSTALL, "install"),
            ],
            dir.path(),
        )
        .await;
        assert_eq!(results.len(), 2);
        for result in results {
            assert_eq!(result.identity.as_deref().map_err(String::as_str), Ok(TOOL_DIGEST));
        }
    }

    #[derive(Clone, Default)]
    struct FakeImages {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DockerApi for FakeImages {
        async fn info(&self) -> Result<DaemonInfo, String> {
            self.calls.lock().unwrap().push("info".into());
            Ok(DaemonInfo {
                os_type: Some("linux".into()),
                architecture: Some("x86_64".into()),
            })
        }

        async fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
            self.calls.lock().unwrap().push(format!("inspect:{reference}"));
            match reference {
                "tool:present" => Ok(Some("sha256:123".into())),
                "tool:missing" => Ok(None),
                "tool:error" => Err("inspection failed".into()),
                _ => panic!("unexpected reference {reference}"),
            }
        }

        async fn pull(&self, _reference: &str) -> Result<(), String> {
            panic!("verification must not pull")
        }

        fn client(&self) -> Option<airlock::docker::Docker> {
            None
        }
    }

    #[tokio::test]
    async fn images_report_the_identity_missing_reference_or_error_with_one_connection_and_one_os_query() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("tool"));
        let bindings = [
            binding("image", ArtifactKind::Image { os: Os::Linux }, "tool:present"),
            binding("missing", ArtifactKind::Image { os: Os::Linux }, "tool:missing"),
            binding("binary", ArtifactKind::Binary, "tool"),
            binding("error", ArtifactKind::Image { os: Os::Linux }, "tool:error"),
            binding("last", ArtifactKind::Image { os: Os::Linux }, "tool:present"),
        ];
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let results = verify(&recording_daemon(images, calls.clone()), &bindings, dir.path()).await;
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "connect",
                "info",
                "inspect:tool:present",
                "inspect:tool:missing",
                "inspect:tool:error",
                "inspect:tool:present"
            ]
        );
        assert_eq!(results.len(), bindings.len());
        assert_eq!(
            results.iter().map(|result| &result.binding).collect::<Vec<_>>(),
            bindings.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            results
                .iter()
                .map(|result| result.identity.as_deref().map_err(String::as_str))
                .collect::<Vec<_>>(),
            [
                Ok("sha256:123"),
                Err("image 'tool:missing' was not found on the Docker daemon"),
                Ok(TOOL_DIGEST),
                Err("inspection failed"),
                Ok("sha256:123"),
            ]
        );
    }

    struct UnresponsiveImages(Arc<AtomicUsize>);

    #[async_trait]
    impl DockerApi for UnresponsiveImages {
        async fn info(&self) -> Result<DaemonInfo, String> {
            Ok(DaemonInfo {
                os_type: Some("linux".into()),
                architecture: Some("x86_64".into()),
            })
        }

        async fn image_id(&self, _reference: &str) -> Result<Option<String>, String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            std::future::pending().await
        }

        async fn pull(&self, _reference: &str) -> Result<(), String> {
            panic!("verification must not pull")
        }

        fn client(&self) -> Option<airlock::docker::Docker> {
            None
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_image_inspection_timeout_fails_remaining_images_without_waiting_again_and_verifies_files() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("bin/tool"));
        write_elf(&dir.path().join("courier"), 2, 1, 3, 62, false);
        let courier_digest = format!(
            "sha256:{:x}",
            Sha256::digest(std::fs::read(dir.path().join("courier")).unwrap())
        );
        let bindings = [
            binding("one", ArtifactKind::Image { os: Os::Linux }, "one:latest"),
            binding("tool", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image { os: Os::Linux }, "two:latest"),
            binding("install", INSTALL, "."),
            binding("courier", CONTAINER_BINARY, "courier"),
            binding("three", ArtifactKind::Image { os: Os::Linux }, "three:latest"),
        ];
        let inspections = Arc::new(AtomicUsize::new(0));
        let start = tokio::time::Instant::now();
        let results = verify_with(&bindings, dir.path(), {
            let inspections = inspections.clone();
            move || Ok(Box::new(UnresponsiveImages(inspections.clone())))
        })
        .await;
        assert_eq!(start.elapsed(), DOCKER_DAEMON_TIMEOUT);
        assert_eq!(inspections.load(Ordering::Relaxed), 1);
        assert_eq!(results.len(), bindings.len());
        let timeout = format!(
            "the Docker daemon did not answer within {}s inspecting image 'one:latest'",
            DOCKER_DAEMON_TIMEOUT.as_secs()
        );
        for (result, binding) in results.iter().zip(&bindings) {
            assert_eq!(&result.binding, binding);
            let expected = match binding.role.kind {
                ArtifactKind::Image { .. } => Err(timeout.as_str()),
                ArtifactKind::ContainerBinary { .. } => Ok(courier_digest.as_str()),
                _ => Ok(TOOL_DIGEST),
            };
            assert_eq!(result.identity.as_deref().map_err(String::as_str), expected);
        }
    }

    #[tokio::test]
    async fn connection_failure_fails_every_image_and_no_file_binding() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("bin/tool"));
        let bindings = [
            binding("one", ArtifactKind::Image { os: Os::Linux }, "one:latest"),
            binding("tool", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image { os: Os::Linux }, "two:latest"),
            binding("install", INSTALL, "."),
        ];
        let results = verify_with(&bindings, dir.path(), || Err("unavailable".into())).await;
        assert_eq!(results.len(), bindings.len());
        for (result, binding) in results.iter().zip(&bindings) {
            assert_eq!(&result.binding, binding);
            let expected = if matches!(binding.role.kind, ArtifactKind::Image { .. }) {
                Err("cannot connect to a Docker daemon: unavailable")
            } else {
                Ok(TOOL_DIGEST)
            };
            assert_eq!(result.identity.as_deref().map_err(String::as_str), expected);
        }
    }

    #[tokio::test]
    async fn no_bindings_need_no_daemon() {
        let results = verify_with(&[], Path::new("."), || panic!("no bindings must not connect")).await;
        assert!(results.is_empty());
        assert_eq!(verified_artifacts(&results), Ok(Vec::new()));
    }

    #[tokio::test]
    async fn an_image_for_another_os_fails_without_inspection_and_other_images_are_verified() {
        let images = FakeImages::default();
        let calls = images.calls.clone();
        let bindings = [
            binding("windows", ArtifactKind::Image { os: Os::Windows }, "tool:windows"),
            binding("linux", ArtifactKind::Image { os: Os::Linux }, "tool:present"),
        ];
        let results = verify(&recording_daemon(images, calls.clone()), &bindings, Path::new(".")).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].binding, bindings[0]);
        assert_eq!(
            results[0].identity,
            Err("the Docker daemon runs linux containers, not windows".into())
        );
        assert_eq!(results[1].binding, bindings[1]);
        assert_eq!(results[1].identity, Ok("sha256:123".into()));
        assert_eq!(*calls.lock().unwrap(), ["connect", "info", "inspect:tool:present"]);
    }
}
