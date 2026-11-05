//! Container and isolation-group removal through a caller-owned Docker connection.

use std::{collections::HashMap, time::Duration};

use bollard::{
    query_parameters::{ListContainersOptionsBuilder, RemoveContainerOptionsBuilder, StopContainerOptionsBuilder},
    Docker,
};
use saluki_error::{generic_error, GenericError};
use tokio::time::{timeout_at, Instant};
use tracing::{debug, warn};

use crate::{
    docker::redacted_docker_error,
    driver::{group_resource_name, is_not_found, with_transient_error_retry},
};

/// Time kept before a removal deadline for the forced removal after SIGKILL.
const REMOVAL_RESERVE: Duration = Duration::from_secs(2);

/// Whole seconds of SIGTERM grace that leave `REMOVAL_RESERVE` before the deadline.
fn stop_seconds(grace: Duration, remaining: Duration) -> i32 {
    grace
        .min(remaining.saturating_sub(REMOVAL_RESERVE))
        .as_secs()
        .min(i32::MAX as u64) as i32
}

pub(crate) async fn stop_and_remove(
    docker: &Docker, container: &str, grace: Duration, deadline: Instant,
) -> Result<(), GenericError> {
    let seconds = stop_seconds(grace, deadline.saturating_duration_since(Instant::now()));
    if seconds > 0 {
        let options = StopContainerOptionsBuilder::default().t(seconds).build();
        match timeout_at(
            deadline - REMOVAL_RESERVE,
            docker.stop_container(container, Some(options)),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) if is_not_found(&error) => {}
            Ok(Err(error)) => warn!(%error, container, "Failed to stop container; force-removing it."),
            Err(_) => debug!(container, "Stop grace expired; force-removing container."),
        }
    }

    let options = RemoveContainerOptionsBuilder::default().force(true).v(true).build();
    match docker.remove_container(container, Some(options)).await {
        Ok(()) => Ok(()),
        Err(error) if is_not_found(&error) => Ok(()),
        Err(error) => Err(redacted_docker_error(
            format!("Failed to remove container '{container}'"),
            "no such container",
            error,
        )),
    }
}

pub(crate) async fn remove_group(docker: &Docker, isolation_group_id: &str) -> Result<(), GenericError> {
    let isolation_group_name = group_resource_name(isolation_group_id);
    let isolation_group_label = format!("airlock-isolation-group={}", isolation_group_id);
    let mut failures = Vec::new();

    let list_filters: HashMap<&str, Vec<&str>> =
        [("label", vec!["created_by=airlock", isolation_group_label.as_str()])]
            .into_iter()
            .collect();
    let list_options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&list_filters)
        .build();
    match docker.list_containers(Some(list_options)).await {
        Ok(containers) => {
            for container_id in containers.into_iter().filter_map(|container| container.id) {
                let remove_options = RemoveContainerOptionsBuilder::default().force(true).v(true).build();
                match docker.remove_container(&container_id, Some(remove_options)).await {
                    Err(e) if !is_not_found(&e) => failures.push(format!("container '{}': {}", container_id, e)),
                    _ => debug!("Removed container '{}'.", container_id),
                }
            }
        }
        Err(e) => failures.push(format!("container list: {}", e)),
    }

    match docker
        .remove_volume(
            &isolation_group_name,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
    {
        Err(e) if !is_not_found(&e) => failures.push(format!("volume '{}': {}", isolation_group_name, e)),
        _ => debug!("Removed shared volume '{}'.", isolation_group_name),
    }

    // Windows NAT removals can collide with other Host Network Service calls.
    let network_removal = with_transient_error_retry("network removal", isolation_group_id, || async {
        docker
            .remove_network(&isolation_group_name)
            .await
            .map_err(GenericError::from)
    })
    .await;
    match network_removal {
        Err(e) if !e.downcast_ref().is_some_and(is_not_found) => {
            failures.push(format!("network '{}': {}", isolation_group_name, e))
        }
        _ => debug!("Removed shared network '{}'.", isolation_group_name),
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(generic_error!(
            "Failed to remove isolation group '{}': {}",
            isolation_group_id,
            failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    use super::*;
    use crate::{
        driver::{Driver, DriverConfig},
        fake_daemon::FakeDaemon,
    };

    const GROUP: &str = "ab12cd34";
    const BOUND: Duration = Duration::from_secs(5);

    impl FakeDaemon {
        /// Runs isolation-group cleanup and returns its result and recorded requests.
        async fn remove_isolation_group(&self) -> (Result<(), GenericError>, Vec<String>) {
            let removed = tokio::time::timeout(BOUND, remove_group(&self.docker, GROUP))
                .await
                .expect("removal should finish within the bound");
            (removed, self.requests.lock().unwrap().clone())
        }
    }

    #[test]
    fn the_stop_grace_is_whole_seconds_that_leave_the_removal_reserve() {
        for (grace, remaining, expected) in [
            (Duration::from_secs(10), Duration::from_secs(30), 10),
            (Duration::from_secs(60), Duration::from_secs(5), 3),
            (Duration::from_millis(1900), Duration::from_secs(30), 1),
            (Duration::from_secs(10), Duration::from_millis(2500), 0),
            (Duration::from_secs(10), Duration::ZERO, 0),
            (Duration::MAX, Duration::from_secs(30), 28),
            (Duration::MAX, Duration::MAX, i32::MAX),
        ] {
            assert_eq!(stop_seconds(grace, remaining), expected, "{grace:?}, {remaining:?}");
        }
    }

    #[tokio::test]
    async fn stopping_sends_the_grace_then_force_removes_the_container_and_its_volumes() {
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("POST", "/containers/airlock-ab12cd34-subject/stop")
            | ("DELETE", "/containers/airlock-ab12cd34-subject") => (204, ""),
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            GROUP.to_string(),
            DriverConfig::from_image("subject", "local:latest".to_string()),
        );
        timeout_at(
            Instant::now() + BOUND,
            driver.stop_and_remove(Duration::from_secs(10), Instant::now() + Duration::from_secs(30)),
        )
        .await
        .unwrap()
        .unwrap();

        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[0], "POST /containers/airlock-ab12cd34-subject/stop?t=10");
        assert!(requests[1].starts_with("DELETE /containers/airlock-ab12cd34-subject?"));
        assert!(requests[1].contains("force=true"), "{requests:?}");
        assert!(requests[1].contains("v=true"), "{requests:?}");
    }

    #[tokio::test]
    async fn a_deadline_too_close_for_a_grace_skips_the_stop() {
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("DELETE", "/containers/subject") => (204, ""),
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        for remaining in [Duration::from_millis(2500), Duration::ZERO] {
            timeout_at(
                Instant::now() + BOUND,
                stop_and_remove(
                    &daemon.docker,
                    "subject",
                    Duration::from_secs(10),
                    Instant::now() + remaining,
                ),
            )
            .await
            .unwrap()
            .unwrap();
        }
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "{requests:?}");
        for request in requests.iter() {
            assert!(request.starts_with("DELETE /containers/subject?"), "{request}");
            assert!(
                request.contains("force=true") && request.contains("v=true"),
                "{request}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_container_or_a_failed_stop_still_ends_in_a_forced_removal() {
        let daemon = FakeDaemon::start(|method, path| match (method, path.split('?').next().unwrap()) {
            ("POST", "/containers/missing/stop") | ("DELETE", "/containers/missing") => {
                (404, r#"{"message":"secret response body"}"#)
            }
            ("POST", "/containers/failed-stop/stop") | ("DELETE", "/containers/failed-remove") => {
                (500, r#"{"message":"secret response body"}"#)
            }
            ("POST", "/containers/failed-remove/stop")
            | ("DELETE", "/containers/failed-stop" | "/containers/exited") => (204, ""),
            ("POST", "/containers/exited/stop") => (304, ""),
            _ => (500, r#"{"message":"unexpected request"}"#),
        })
        .await;
        for (container, fails) in [
            ("missing", false),
            ("failed-stop", false),
            ("failed-remove", true),
            ("exited", false),
        ] {
            let result = timeout_at(
                Instant::now() + BOUND,
                stop_and_remove(
                    &daemon.docker,
                    container,
                    Duration::from_secs(10),
                    Instant::now() + Duration::from_secs(30),
                ),
            )
            .await
            .unwrap();
            if fails {
                let error = result.unwrap_err();
                let diagnostic = format!("{error:?}");
                assert!(
                    diagnostic.contains(&format!("Failed to remove container '{container}'")),
                    "{diagnostic}"
                );
                assert!(diagnostic.contains("500"), "{diagnostic}");
                assert!(!diagnostic.contains("secret response body"), "{diagnostic}");
            } else {
                result.unwrap();
            }
            let mut requests = daemon.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "{requests:?}");
            assert!(
                requests[0].starts_with(&format!("POST /containers/{container}/stop?")),
                "{requests:?}"
            );
            assert!(
                requests[1].starts_with(&format!("DELETE /containers/{container}?")),
                "{requests:?}"
            );
            assert!(
                requests[1].contains("force=true") && requests[1].contains("v=true"),
                "{requests:?}"
            );
            requests.clear();
        }
    }

    #[tokio::test]
    async fn a_stop_the_daemon_never_answers_is_abandoned_before_the_deadline_and_removal_is_attempted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let docker = Docker::connect_with_http(
            &format!("http://{}", listener.local_addr().unwrap()),
            30,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let removal = timeout_at(
            deadline,
            stop_and_remove(&docker, "subject", Duration::from_secs(10), deadline),
        );
        tokio::pin!(removal);
        let mut connections = Vec::new();
        let result = loop {
            tokio::select! {
                result = &mut removal => break result,
                accepted = listener.accept() => {
                    let (stream, _) = accepted.unwrap();
                    connections.push((Instant::now(), stream));
                }
            }
        };
        assert!(result.is_err(), "the caller's deadline should expire: {result:?}");
        assert_eq!(connections.len(), 2);
        assert!(connections[1].0 - connections[0].0 >= Duration::from_millis(1500));
        let mut requests = Vec::new();
        for (_, stream) in connections {
            let mut request = String::new();
            timeout_at(Instant::now() + BOUND, BufReader::new(stream).read_line(&mut request))
                .await
                .expect("the request line should arrive within the bound")
                .unwrap();
            assert!(request.ends_with("\r\n"), "{request:?}");
            requests.push(request);
        }
        assert!(requests[0].starts_with("POST /"), "{requests:?}");
        assert!(requests[0].contains("/containers/subject/stop?t="), "{requests:?}");
        assert!(requests[1].starts_with("DELETE /"), "{requests:?}");
        assert!(requests[1].contains("/containers/subject?"), "{requests:?}");
        assert!(requests[1].contains("force=true"), "{requests:?}");
        assert!(requests[1].contains("v=true"), "{requests:?}");
    }

    #[tokio::test]
    async fn removing_an_isolation_group_attempts_everything_and_reports_every_failure() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("GET", "/containers/json") => (200, r#"[{"Id": "c1"}, {"Id": "c2"}]"#),
                ("POST", stop) if stop.ends_with("/stop") => (500, r#"{"message": "stop failed"}"#),
                ("DELETE", "/containers/c1") if path.contains("force=true") => (500, r#"{"message": "boom"}"#),
                ("DELETE", "/containers/c1") => (409, r#"{"message": "container is running"}"#),
                ("DELETE", "/containers/c2") => (204, ""),
                ("DELETE", "/volumes/airlock-ab12cd34") => (409, r#"{"message": "in use"}"#),
                ("DELETE", "/networks/airlock-ab12cd34") => (204, ""),
                _ => (500, r#"{"message": "unexpected request"}"#),
            },
        )
        .await;

        let (removed, requests) = daemon.remove_isolation_group().await;

        let error = removed.expect_err("the failed removals should be reported").to_string();
        assert!(error.contains("container 'c1'") && error.contains("boom"), "{error}");
        assert!(
            error.contains("volume 'airlock-ab12cd34'") && error.contains("in use"),
            "{error}"
        );
        assert!(!error.contains("c2") && !error.contains("network"), "{error}");
        let paths: Vec<&str> = requests
            .iter()
            .map(|request| request.split('?').next().unwrap_or_default())
            .collect();
        assert_eq!(
            paths,
            [
                "GET /containers/json",
                "DELETE /containers/c1",
                "DELETE /containers/c2",
                "DELETE /volumes/airlock-ab12cd34",
                "DELETE /networks/airlock-ab12cd34",
            ]
        );
        for container_removal in &requests[1..3] {
            assert!(container_removal.contains("force=true"), "{container_removal}");
            assert!(container_removal.contains("v=true"), "{container_removal}");
        }
    }

    #[tokio::test]
    async fn isolation_group_resources_that_are_already_gone_count_as_removed() {
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("GET", "/containers/json") => (200, r#"[{"Id":"gone"}]"#),
                ("DELETE", "/containers/gone") => (404, r#"{"message": "no such container"}"#),
                ("DELETE", "/volumes/airlock-ab12cd34") => (404, r#"{"message": "no such volume"}"#),
                ("DELETE", "/networks/airlock-ab12cd34") => (404, r#"{"message": "no such network"}"#),
                _ => (500, r#"{"message": "unexpected request"}"#),
            },
        )
        .await;

        let (removed, requests) = daemon.remove_isolation_group().await;

        removed.expect("missing resources should count as removed");
        assert_eq!(requests.len(), 4, "{requests:?}");
        assert!(
            requests[1].contains("force=true") && requests[1].contains("v=true"),
            "{requests:?}"
        );
    }

    #[tokio::test]
    async fn a_network_removal_that_collides_in_the_host_network_service_is_retried() {
        static REMOVALS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let daemon = FakeDaemon::start(
            |method, path| match (method, path.split('?').next().unwrap_or_default()) {
                ("GET", "/containers/json") => (200, "[]"),
                ("DELETE", "/volumes/airlock-ab12cd34") => (404, r#"{"message": "no such volume"}"#),
                ("DELETE", "/networks/airlock-ab12cd34") => {
                    if REMOVALS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        (
                            500,
                            r#"{"message": "hnsCall failed in Win32: The object already exists. (0x1392)"}"#,
                        )
                    } else {
                        (204, "")
                    }
                }
                _ => (500, r#"{"message": "unexpected request"}"#),
            },
        )
        .await;

        let (removed, requests) = daemon.remove_isolation_group().await;

        removed.expect("the retried removal should succeed");
        let network_removals = requests
            .iter()
            .filter(|request| request.starts_with("DELETE /networks/"))
            .count();
        assert_eq!(network_removals, 2, "{requests:?}");
    }
}
