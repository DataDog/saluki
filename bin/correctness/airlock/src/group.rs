//! Shared networks, volumes, and participant directories for Linux containers, and shared networks for Windows
//! containers.

use std::collections::HashSet;

use bollard::{
    models::{ContainerCreateBody, HostConfig, Ipam, NetworkCreateRequest, VolumeCreateRequest},
    query_parameters::{CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder},
    Docker,
};
use saluki_error::{generic_error, GenericError};
use tracing::warn;

use crate::{
    driver::{
        get_default_airlock_labels, group_resource_name, is_not_found, with_transient_error_retry, ContainerOs, Driver,
        DriverConfig,
    },
    observe, removal,
};

/// The path where every Linux container in a group mounts the group volume.
///
/// Participant directories are `<SHARED_ROOT>/<participant>/{exit,log,shared}`.
pub const SHARED_ROOT: &str = "/airlock";

/// A prepared isolation group.
///
/// Linux groups prepare a network, shared volume, and participant directories. Windows groups prepare a NAT network.
pub struct IsolationGroup {
    docker: Docker,
    id: String,
    participants: Vec<String>,
    container_os: ContainerOs,
}

impl IsolationGroup {
    /// Prepares shared resources and writable `exit`, `log`, and `shared` directories for every participant.
    ///
    /// IDs and participant names must be 1–63 lowercase ASCII letters, digits, or hyphens, with a letter or digit
    /// at each end.
    ///
    /// `fixup_image` must already be on the daemon and provide `sh`, `mkdir`, and `chmod`. Register the supplied ID
    /// for cleanup before calling: errors or cancellation can leave partial resources. Remove them with
    /// [`IsolationGroup::remove`].
    ///
    /// # Errors
    ///
    /// Returns an error for invalid DNS labels, duplicate participants, an empty participant list, an identity
    /// already in use, Docker failures, or a nonzero fix-up exit status.
    pub async fn prepare(
        docker: Docker, id: impl Into<String>, participants: &[impl AsRef<str>], fixup_image: &str,
    ) -> Result<Self, GenericError> {
        let id = id.into();
        let participants = validate_names(&id, participants)?;
        let name = group_resource_name(&id);

        match docker.inspect_network(&name, None).await {
            Ok(_) => return Err(generic_error!("Isolation group '{id}' already has network '{name}'")),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error.into()),
        }
        match docker.inspect_volume(&name).await {
            Ok(_) => return Err(generic_error!("Isolation group '{id}' already has volume '{name}'")),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error.into()),
        }

        docker
            .create_network(NetworkCreateRequest {
                name: name.clone(),
                driver: Some("bridge".to_string()),
                ipam: Some(Ipam::default()),
                enable_ipv6: Some(false),
                labels: Some(get_default_airlock_labels(&id)),
                ..Default::default()
            })
            .await?;
        docker
            .create_volume(VolumeCreateRequest {
                name: Some(name),
                driver: Some("local".to_string()),
                labels: Some(get_default_airlock_labels(&id)),
                ..Default::default()
            })
            .await?;

        let group = Self {
            docker,
            id,
            participants,
            container_os: ContainerOs::Linux,
        };
        group.prepare_directories(fixup_image).await?;
        Ok(group)
    }

    /// Prepares a labeled `nat` network for a group of Windows containers.
    ///
    /// Each Windows container keeps its files in its own filesystem, placed with [`Driver::place_files`].
    /// Collisions in the Windows Host Network Service while creating the network are retried.
    /// IDs and participant names follow the rules of [`Self::prepare`].
    ///
    /// Register the supplied ID for cleanup before calling: errors or cancellation can leave the network. Remove it
    /// with [`IsolationGroup::remove`].
    ///
    /// # Errors
    ///
    /// Returns an error for invalid DNS labels, duplicate participants, an empty participant list, a network
    /// already in use, or Docker failures.
    pub async fn prepare_windows(
        docker: Docker, id: impl Into<String>, participants: &[impl AsRef<str>],
    ) -> Result<Self, GenericError> {
        let id = id.into();
        let participants = validate_names(&id, participants)?;
        let name = group_resource_name(&id);

        match docker.inspect_network(&name, None).await {
            Ok(_) => return Err(generic_error!("Isolation group '{id}' already has network '{name}'")),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error.into()),
        }

        with_transient_error_retry("network creation", &id, || async {
            match docker.inspect_network(&name, None).await {
                Ok(_) => return Ok(()),
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error.into()),
            }
            docker
                .create_network(NetworkCreateRequest {
                    name: name.clone(),
                    driver: Some("nat".to_string()),
                    ipam: Some(Ipam::default()),
                    enable_ipv6: Some(false),
                    labels: Some(get_default_airlock_labels(&id)),
                    ..Default::default()
                })
                .await
                .map(|_| ())
                .map_err(GenericError::from)
        })
        .await?;

        Ok(Self {
            docker,
            id,
            participants,
            container_os: ContainerOs::Windows,
        })
    }

    /// Force-removes a group's containers and their anonymous volumes, then its shared volume and network.
    ///
    /// Attempts every removal despite failures and treats missing resources as removed. The group may be partly
    /// prepared or never prepared. This future is cancel-safe: dropping it leaves completed removals in place,
    /// and calling it again removes remaining resources. Use `tokio::time::timeout_at` to bound the operation.
    ///
    /// # Errors
    ///
    /// Returns a combined error listing all resource-listing and removal failures.
    pub async fn remove(docker: &Docker, id: &str) -> Result<(), GenericError> {
        removal::remove_group(docker, id).await
    }

    /// Builds a container driver for one of this group's prepared participants.
    ///
    /// The container is named `airlock-<id>-<participant>`, with the participant name as its hostname and DNS alias
    /// on the group network. Linux containers mount the group volume by default; Windows containers use explicitly
    /// configured mounts. Each keeps its own PID namespace unless [`DriverConfig::with_host_pid_namespace`] is set
    /// to `true`, which Windows containers ignore. Published ports require an explicit host port, which Windows containers
    /// reject when created.
    ///
    /// # Errors
    ///
    /// Returns an error for a participant outside this group, a [`DriverConfig::with_container_os`] that differs
    /// from the group's OS.
    pub fn driver(&self, config: DriverConfig) -> Result<Driver, GenericError> {
        if !self.participants.contains(&config.driver_id) {
            return Err(generic_error!(
                "Participant '{}' was not prepared in isolation group '{}'",
                config.driver_id,
                self.id
            ));
        }
        if config.container_os != self.container_os {
            return Err(generic_error!(
                "Participant '{}' selects {:?} containers; isolation group '{}' requires {:?}",
                config.driver_id,
                config.container_os,
                self.id,
                self.container_os
            ));
        }
        Ok(Driver::in_group(self.docker.clone(), self.id.clone(), config))
    }

    /// Runs a temporary container to create writable per-participant directories.
    async fn prepare_directories(&self, image: &str) -> Result<(), GenericError> {
        let name = group_resource_name(&self.id);
        let container_name = format!("{name}_fix-up");
        let mut entrypoint = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("mkdir -p \"$@\" && chmod -R 777 {SHARED_ROOT}"),
            "fix-up".to_string(),
        ];
        for participant in &self.participants {
            for directory in ["exit", "log", "shared"] {
                entrypoint.push(format!("{SHARED_ROOT}/{participant}/{directory}"));
            }
        }
        self.docker
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(&container_name).build()),
                ContainerCreateBody {
                    image: Some(image.to_string()),
                    entrypoint: Some(entrypoint),
                    host_config: Some(HostConfig {
                        binds: Some(vec![format!("{name}:{SHARED_ROOT}:z")]),
                        network_mode: Some("none".to_string()),
                        ..Default::default()
                    }),
                    labels: Some(get_default_airlock_labels(&self.id)),
                    ..Default::default()
                },
            )
            .await?;

        let result: Result<(), GenericError> = async {
            self.docker.start_container(&container_name, None).await?;
            let exit = observe::wait(&self.docker, &container_name).await?;
            if exit.code == 0 {
                Ok(())
            } else {
                Err(generic_error!(
                    "Isolation group '{}' fix-up exited with code {}",
                    self.id,
                    exit.code
                ))
            }
        }
        .await;
        let removed = self
            .docker
            .remove_container(
                &container_name,
                Some(RemoveContainerOptionsBuilder::default().force(true).v(true).build()),
            )
            .await;
        if let Err(error) = &removed {
            warn!(error = %error, container = %container_name, "Failed to remove fix-up container.");
        }
        result?;
        removed?;
        Ok(())
    }
}

/// Validates DNS labels and requires at least one participant, each with a unique name.
fn validate_names(id: &str, participants: &[impl AsRef<str>]) -> Result<Vec<String>, GenericError> {
    if !is_dns_label(id) {
        return Err(generic_error!("Isolation group ID '{id}' must be a DNS label"));
    }
    if participants.is_empty() {
        return Err(generic_error!("Isolation group '{id}' has no participants"));
    }
    let mut seen = HashSet::new();
    let mut names = Vec::with_capacity(participants.len());
    for participant in participants {
        let participant = participant.as_ref();
        if !is_dns_label(participant) {
            return Err(generic_error!("Participant '{participant}' must be a DNS label"));
        }
        if !seen.insert(participant) {
            return Err(generic_error!(
                "Duplicate participant '{participant}' in isolation group '{id}'"
            ));
        }
        names.push(participant.to_string());
    }
    Ok(names)
}

/// Accepts lowercase DNS labels safe for path components and container names.
fn is_dns_label(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_daemon::FakeDaemon;

    #[test]
    fn group_identities_and_participant_names_must_be_dns_labels() {
        for invalid in ["", "-a", "a-", "A", "a/b", "..", "a_b", &"a".repeat(64), "é"] {
            let error = validate_names(invalid, &["alpha"]).unwrap_err().to_string();
            assert!(error.contains(&format!("'{invalid}'")), "{error}");
            let error = validate_names("group", &[invalid]).unwrap_err().to_string();
            assert!(error.contains(&format!("'{invalid}'")), "{error}");
        }
        for valid in [
            "a",
            "adp",
            "intake-adp",
            "abcdefgh-0003",
            "ab12cd34",
            "0",
            &"a".repeat(63),
        ] {
            assert_eq!(validate_names(valid, &[valid]).unwrap(), vec![valid.to_string()]);
        }
        let error = validate_names("group", &["alpha", "alpha"]).unwrap_err().to_string();
        assert!(error.contains("Duplicate participant 'alpha'"), "{error}");
        let error = validate_names("group", &[] as &[&str]).unwrap_err().to_string();
        assert_eq!(error, "Isolation group 'group' has no participants");
    }

    #[tokio::test]
    async fn a_driver_for_an_unprepared_participant_or_a_windows_container_is_rejected() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"unexpected request"}"#)).await;
        let group = IsolationGroup {
            docker: daemon.docker.clone(),
            id: "group".to_string(),
            participants: vec!["alpha".to_string()],
            container_os: ContainerOs::Linux,
        };
        let error = group
            .driver(DriverConfig::from_image("bravo", "local:latest".to_string()))
            .err()
            .expect("an unprepared participant must be rejected")
            .to_string();
        assert!(error.contains("'bravo'"), "{error}");
        let error = group
            .driver(
                DriverConfig::from_image("alpha", "local:latest".to_string()).with_container_os(ContainerOs::Windows),
            )
            .err()
            .expect("Windows containers must be rejected")
            .to_string();
        assert!(error.contains("'alpha'") && error.contains("Windows"), "{error}");
        group
            .driver(DriverConfig::from_image("alpha", "local:latest".to_string()))
            .unwrap();
        assert!(daemon.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_windows_isolation_group_accepts_only_windows_drivers_for_its_participants() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"unexpected request"}"#)).await;
        let group = IsolationGroup {
            docker: daemon.docker.clone(),
            id: "group".to_string(),
            participants: vec!["alpha".to_string()],
            container_os: ContainerOs::Windows,
        };
        let windows =
            DriverConfig::from_image("alpha", "local:latest".to_string()).with_container_os(ContainerOs::Windows);
        let error = group
            .driver(DriverConfig::from_image("alpha", "local:latest".to_string()))
            .err()
            .expect("Linux containers must be rejected")
            .to_string();
        assert!(
            error.contains("'alpha'") && error.contains("Linux") && error.contains("Windows"),
            "{error}"
        );
        let error = group
            .driver(
                DriverConfig::from_image("bravo", "local:latest".to_string()).with_container_os(ContainerOs::Windows),
            )
            .err()
            .expect("an unprepared participant must be rejected")
            .to_string();
        assert!(error.contains("'bravo'"), "{error}");
        group.driver(windows).unwrap();
        assert!(daemon.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn preparing_a_windows_group_creates_only_a_labeled_nat_network() {
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("GET", "/networks/airlock-group") => (404, r#"{"message":"missing"}"#),
            ("POST", "/networks/create") => (201, r#"{"Id":"network","Warning":""}"#),
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        let group = IsolationGroup::prepare_windows(daemon.docker.clone(), "group", &["alpha", "bravo"])
            .await
            .unwrap();
        assert_eq!(group.container_os, ContainerOs::Windows);
        assert_eq!(group.participants, ["alpha", "bravo"]);
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(
            requests.as_slice(),
            [
                "GET /networks/airlock-group",
                "GET /networks/airlock-group",
                "POST /networks/create"
            ]
        );
        let bodies = daemon.bodies.lock().unwrap();
        let body = &bodies[2];
        assert!(body.contains(r#""Name":"airlock-group""#), "{body}");
        assert!(body.contains(r#""Driver":"nat""#), "{body}");
        assert!(body.contains(r#""created_by":"airlock""#), "{body}");
        assert!(body.contains(r#""airlock-isolation-group":"group""#), "{body}");
    }

    #[tokio::test]
    async fn a_windows_network_stored_before_an_hns_error_is_reused_on_retry() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // A failed HNS response can still leave the network on the daemon.
        static CREATED: AtomicBool = AtomicBool::new(false);
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("GET", "/networks/airlock-group") if CREATED.load(Ordering::SeqCst) => {
                (200, r#"{"Name":"airlock-group"}"#)
            }
            ("GET", "/networks/airlock-group") => (404, r#"{"message":"missing"}"#),
            ("POST", "/networks/create") => {
                CREATED.store(true, Ordering::SeqCst);
                (
                    500,
                    r#"{"message":"hnsCall failed in Win32: The object already exists. (0x1392)"}"#,
                )
            }
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        let group = IsolationGroup::prepare_windows(daemon.docker.clone(), "group", &["alpha"])
            .await
            .unwrap();
        assert_eq!(group.container_os, ContainerOs::Windows);
        assert_eq!(
            daemon.requests.lock().unwrap().as_slice(),
            [
                "GET /networks/airlock-group",
                "GET /networks/airlock-group",
                "POST /networks/create",
                "GET /networks/airlock-group"
            ]
        );
    }

    #[tokio::test]
    async fn preparing_a_windows_group_validates_names_and_refuses_an_existing_network() {
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("GET", "/networks/airlock-group") => (200, r#"{"Name":"airlock-group"}"#),
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        for (id, participants) in [
            ("Group", vec!["alpha"]),
            ("group", vec![]),
            ("group", vec!["alpha", "alpha"]),
        ] {
            assert!(
                IsolationGroup::prepare_windows(daemon.docker.clone(), id, &participants)
                    .await
                    .is_err()
            );
        }
        assert!(daemon.requests.lock().unwrap().is_empty());
        let error = IsolationGroup::prepare_windows(daemon.docker.clone(), "group", &["alpha"])
            .await
            .err()
            .expect("an existing network must be rejected")
            .to_string();
        assert!(error.contains("'group'") && error.contains("network"), "{error}");
        assert_eq!(daemon.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_fix_up_that_exits_nonzero_fails_preparation_and_is_still_removed() {
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("GET", "/networks/airlock-group" | "/volumes/airlock-group") => (404, r#"{"message":"missing"}"#),
            ("POST", "/networks/create") => (201, r#"{"Id":"network","Warning":""}"#),
            ("POST", "/volumes/create") => (
                201,
                r#"{"Name":"airlock-group","Driver":"local","Mountpoint":"/x","Labels":{},
                    "Options":{},"Scope":"local"}"#,
            ),
            ("POST", "/containers/create") => (201, r#"{"Id":"fix-up","Warnings":[]}"#),
            ("POST", "/containers/airlock-group_fix-up/start") => (204, ""),
            ("GET", "/containers/airlock-group_fix-up/json") => (200, r#"{"State":{"Status":"exited"}}"#),
            ("POST", "/containers/airlock-group_fix-up/wait") => (200, r#"{"StatusCode":1}"#),
            ("DELETE", "/containers/airlock-group_fix-up") => (204, ""),
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        let error = IsolationGroup::prepare(daemon.docker.clone(), "group", &["alpha"], "local:latest")
            .await
            .err()
            .expect("a failed fix-up must fail preparation")
            .to_string();
        assert!(error.contains("'group'") && error.contains("code 1"), "{error}");
        let requests = daemon.requests.lock().unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|request| request.split('?').next().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "GET /networks/airlock-group",
                "GET /volumes/airlock-group",
                "POST /networks/create",
                "POST /volumes/create",
                "POST /containers/create",
                "POST /containers/airlock-group_fix-up/start",
                "GET /containers/airlock-group_fix-up/json",
                "POST /containers/airlock-group_fix-up/wait",
                "DELETE /containers/airlock-group_fix-up",
            ]
        );
        assert!(requests[4].contains("name=airlock-group_fix-up"), "{requests:?}");
        assert!(requests[7].contains("condition=not-running"), "{requests:?}");
        assert!(requests[8].contains("force=true"), "{requests:?}");
        assert!(requests[8].contains("v=true"), "{requests:?}");
    }

    #[tokio::test]
    async fn preparing_a_group_refuses_an_existing_network_or_volume() {
        for existing in ["network", "volume"] {
            let respond = if existing == "network" {
                |method: &str, path: &str| match (method, path.split('?').next().unwrap()) {
                    ("GET", "/networks/airlock-group") => (200, r#"{"Name":"airlock-group"}"#),
                    _ => (500, r#"{"message":"unexpected request"}"#),
                }
            } else {
                |method: &str, path: &str| match (method, path.split('?').next().unwrap()) {
                    ("GET", "/networks/airlock-group") => (404, r#"{"message":"missing"}"#),
                    ("GET", "/volumes/airlock-group") => (
                        200,
                        r#"{"Name":"airlock-group","Driver":"local","Mountpoint":"/x","Labels":{},
                            "Options":{},"Scope":"local"}"#,
                    ),
                    _ => (500, r#"{"message":"unexpected request"}"#),
                }
            };
            let daemon = FakeDaemon::start(respond).await;
            let error = IsolationGroup::prepare(daemon.docker.clone(), "group", &["alpha"], "local:latest")
                .await
                .err()
                .expect("an existing identity must be rejected")
                .to_string();
            assert!(error.contains("'group'") && error.contains(existing), "{error}");
            let requests = daemon.requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                if existing == "network" { 1 } else { 2 },
                "{requests:?}"
            );
            assert!(
                requests.iter().all(|request| request.starts_with("GET ")),
                "{requests:?}"
            );
        }
    }
}
