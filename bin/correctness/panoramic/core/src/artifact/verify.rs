//! Checks bound artifacts and computes their identities: image IDs from the Docker daemon,
//! SHA-256 digests of binaries and install main binaries.

use std::{fs::File, path::Path};

use sha2::{Digest, Sha256};

use super::{
    image_store::{connect, image_id_within_deadline, ImageStore},
    ArtifactBinding, ArtifactKind,
};

/// One binding's verification or pull outcome.
#[derive(Debug)]
pub struct ArtifactVerification {
    /// The role, reference and source checked or pulled.
    pub binding: ArtifactBinding,
    /// The artifact's identity, or why verification or pulling failed.
    pub identity: Result<String, String>,
}

/// Verifies each binding, resolving relative paths against `base`, in input order.
/// Images use the same Docker daemon as backends; files are identified by their SHA-256 digest.
/// A daemon timeout fails the remaining image bindings; file checks continue.
pub async fn verify(bindings: &[ArtifactBinding], base: &Path) -> Vec<ArtifactVerification> {
    verify_with(bindings, base, connect).await
}

async fn verify_with(
    bindings: &[ArtifactBinding], base: &Path, connect: impl FnOnce() -> Result<Box<dyn ImageStore>, String>,
) -> Vec<ArtifactVerification> {
    let mut store = bindings
        .iter()
        .any(|binding| binding.role.kind == ArtifactKind::Image)
        .then(|| connect().map_err(|error| format!("cannot connect to a Docker daemon: {error}")));
    let mut results = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let identity = match binding.role.kind {
            ArtifactKind::Image => match store.as_ref().expect("image bindings initialize the store") {
                Ok(images) => match image_id_within_deadline(images.as_ref(), &binding.reference).await {
                    Ok(result) => result.and_then(|id| {
                        id.ok_or_else(|| format!("image '{}' was not found on the Docker daemon", binding.reference))
                    }),
                    Err(error) => {
                        store = Some(Err(error.clone()));
                        Err(error)
                    }
                },
                Err(error) => Err(error.clone()),
            },
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
                    hash_binary(&binary)
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

fn hash_binary(path: &Path) -> Result<String, String> {
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
    let mut file = File::open(path).map_err(|error| format!("cannot open '{}': {error}", path.display()))?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
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
    use crate::artifact::{test_util::write_executable, ArtifactRole, BindingSource};

    const TOOL_DIGEST: &str = "sha256:7c9bbe5ec9b3fb774e8fa0f54247e93c34ddf8e5d16fe3073420de0ae81a262d";
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

    #[tokio::test]
    async fn invalid_binary_and_install_paths_report_every_failure_in_input_order() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("tool"));
        std::fs::create_dir(dir.path().join("install")).unwrap();
        let cases = [
            (
                binding("missing", ArtifactKind::Binary, "missing"),
                "missing",
                "cannot inspect",
            ),
            (
                binding("directory", ArtifactKind::Binary, "install"),
                "install",
                "is not a file",
            ),
            (
                binding("incomplete", INSTALL, "install"),
                "install/bin/tool",
                "cannot inspect",
            ),
            (binding("file", INSTALL, "tool"), "tool", "is not a directory"),
            (binding("absent-install", INSTALL, "absent"), "absent", "cannot inspect"),
        ];
        let bindings: Vec<_> = cases.iter().map(|(binding, _, _)| binding.clone()).collect();
        let results = verify(&bindings, dir.path()).await;
        assert_eq!(results.len(), cases.len());
        for (result, (binding, path, problem)) in results.iter().zip(&cases) {
            assert_eq!(&result.binding, binding);
            let message = result.identity.as_ref().unwrap_err();
            assert!(message.contains(dir.path().join(path).to_str().unwrap()), "{message}");
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

    struct FakeImages;

    #[async_trait]
    impl ImageStore for FakeImages {
        async fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
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
    }

    #[tokio::test]
    async fn images_report_the_store_identity_missing_reference_or_error_with_one_connection() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("tool"));
        let bindings = [
            binding("image", ArtifactKind::Image, "tool:present"),
            binding("missing", ArtifactKind::Image, "tool:missing"),
            binding("binary", ArtifactKind::Binary, "tool"),
            binding("error", ArtifactKind::Image, "tool:error"),
            binding("last", ArtifactKind::Image, "tool:present"),
        ];
        let connections = Mutex::new(0);
        let results = verify_with(&bindings, dir.path(), || {
            *connections.lock().unwrap() += 1;
            Ok(Box::new(FakeImages))
        })
        .await;
        assert_eq!(*connections.lock().unwrap(), 1);
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
    impl ImageStore for UnresponsiveImages {
        async fn image_id(&self, _reference: &str) -> Result<Option<String>, String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            std::future::pending().await
        }

        async fn pull(&self, _reference: &str) -> Result<(), String> {
            panic!("verification must not pull")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_image_inspection_timeout_fails_remaining_images_without_waiting_again_and_verifies_files() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("bin/tool"));
        let bindings = [
            binding("one", ArtifactKind::Image, "one:latest"),
            binding("tool", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image, "two:latest"),
            binding("install", INSTALL, "."),
            binding("three", ArtifactKind::Image, "three:latest"),
        ];
        let inspections = Arc::new(AtomicUsize::new(0));
        let start = tokio::time::Instant::now();
        let results = verify_with(&bindings, dir.path(), || {
            Ok(Box::new(UnresponsiveImages(inspections.clone())))
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
            let expected = if binding.role.kind == ArtifactKind::Image {
                Err(timeout.as_str())
            } else {
                Ok(TOOL_DIGEST)
            };
            assert_eq!(result.identity.as_deref().map_err(String::as_str), expected);
        }
    }

    #[tokio::test]
    async fn connection_failure_fails_every_image_and_no_file_binding() {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("bin/tool"));
        let bindings = [
            binding("one", ArtifactKind::Image, "one:latest"),
            binding("tool", ArtifactKind::Binary, "bin/tool"),
            binding("two", ArtifactKind::Image, "two:latest"),
            binding("install", INSTALL, "."),
        ];
        let results = verify_with(&bindings, dir.path(), || Err("unavailable".into())).await;
        assert_eq!(results.len(), bindings.len());
        for (result, binding) in results.iter().zip(&bindings) {
            assert_eq!(&result.binding, binding);
            let expected = if binding.role.kind == ArtifactKind::Image {
                Err("cannot connect to a Docker daemon: unavailable")
            } else {
                Ok(TOOL_DIGEST)
            };
            assert_eq!(result.identity.as_deref().map_err(String::as_str), expected);
        }
    }

    #[tokio::test]
    async fn no_bindings_need_no_image_store() {
        assert!(
            verify_with(&[], Path::new("."), || panic!("no bindings must not connect"))
                .await
                .is_empty()
        );
    }
}
