use std::{
    collections::HashMap,
    future::Future,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use airlock::{
    docker::{connect, image_id, Docker},
    driver::{Driver, DriverConfig, ExitStatus},
    group::IsolationGroup,
};
use bollard::query_parameters::{ListContainersOptionsBuilder, ListNetworksOptionsBuilder, ListVolumesOptionsBuilder};
use futures::future::try_join_all;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

fn test_image() -> String {
    std::env::var("AIRLOCK_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".to_string())
}

async fn daemon() -> Docker {
    let docker = connect().expect("failed to connect to Docker; check DOCKER_HOST");
    docker
        .ping()
        .await
        .expect("Docker daemon is unavailable; check DOCKER_HOST");
    let image = test_image();
    assert!(
        image_id(&docker, &image)
            .await
            .expect("image inspection failed; check DOCKER_HOST")
            .is_some(),
        "AIRLOCK_TEST_IMAGE '{image}' is not on the daemon; load it first, these tests never pull"
    );
    docker
}

fn unique_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let mut id = format!("t{}x{nanos:x}", std::process::id());
    id.truncate(63);
    id
}

async fn in_group(id: String, scenario: impl Future<Output = ()> + Send + 'static) {
    let outcome = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(60), scenario)
            .await
            .expect("the daemon scenario should finish within a minute");
    })
    .await;
    let mut problems = Vec::new();
    if let Err(error) = Driver::clean_related_resources(id.clone()).await {
        problems.push(format!("group cleanup failed: {error:#}"));
    }
    let resources = match connect() {
        Ok(docker) => group_resources(&docker, &id).await,
        Err(error) => Err(error),
    };
    match resources {
        Ok(resources) => {
            for (kind, remaining) in [
                ("containers", resources.containers),
                ("volumes", resources.volumes),
                ("networks", resources.networks),
            ] {
                if !remaining.is_empty() {
                    problems.push(format!("remaining {kind}: {remaining:?}"));
                }
            }
        }
        Err(error) => problems.push(format!("cleanup verification failed: {error:#}")),
    }
    let problems = problems.join("\n");
    if let Err(error) = outcome {
        if !problems.is_empty() {
            eprintln!("Cleanup problems for {id}:\n{problems}");
        }
        std::panic::resume_unwind(error.into_panic());
    }
    assert!(problems.is_empty(), "Cleanup problems for {id}:\n{problems}");
}

#[derive(Debug)]
struct GroupResources {
    containers: Vec<String>,
    volumes: Vec<String>,
    networks: Vec<String>,
}

async fn group_resources(docker: &Docker, id: &str) -> Result<GroupResources, GenericError> {
    let filters: HashMap<_, _> = [("label", vec![format!("airlock-isolation-group={id}")])]
        .into_iter()
        .collect();
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .error_context("Failed to list containers")?
        .into_iter()
        .flat_map(|container| container.names.unwrap_or_default())
        .map(|name| name.trim_start_matches('/').to_string())
        .collect();
    let volumes = docker
        .list_volumes(Some(ListVolumesOptionsBuilder::default().filters(&filters).build()))
        .await
        .error_context("Failed to list volumes")?
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|volume| volume.name)
        .collect();
    let networks = docker
        .list_networks(Some(ListNetworksOptionsBuilder::default().filters(&filters).build()))
        .await
        .error_context("Failed to list networks")?
        .into_iter()
        .map(|network| network.name.ok_or_else(|| generic_error!("Network has no name")))
        .collect::<Result<_, _>>()?;
    Ok(GroupResources {
        containers,
        volumes,
        networks,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_group_creates_its_participants_concurrently_on_one_network_and_volume() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let participants = ["alpha", "bravo", "charlie"];
        let image = test_image();
        let group = IsolationGroup::prepare(docker.clone(), &id, &participants, &image)
            .await
            .unwrap();
        let drivers: Vec<_> = participants
            .iter()
            .map(|participant| {
                let script = format!(
                    "for p in alpha bravo charlie; do for d in exit log shared; do \
                 [ \"$(stat -c %a /airlock/$p/$d)\" = 777 ] || exit 1; done; done; \
                 echo ok > /airlock/{participant}/shared/ok"
                );
                group
                    .driver(
                        DriverConfig::from_image(*participant, image.clone()).with_entrypoint(vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            script,
                        ]),
                    )
                    .unwrap()
            })
            .collect();

        try_join_all(drivers.iter().map(Driver::create)).await.unwrap();

        let mut resources = group_resources(&docker, &id).await.unwrap();
        let name = format!("airlock-{id}");
        assert_eq!(resources.networks, vec![name.clone()]);
        assert_eq!(resources.volumes, vec![name.clone()]);
        resources.containers.sort();
        assert_eq!(
            resources.containers,
            participants.map(|participant| format!("{name}-{participant}"))
        );

        for driver in &drivers {
            let details = driver.start_container().await.unwrap();
            let status = driver.wait_for_container_exit().await.unwrap();
            assert!(
                matches!(status, ExitStatus::Success),
                "{}: {status}",
                details.container_name()
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_participant_reaches_another_by_its_participant_name() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let names: Vec<String> = ["server", "client"].into_iter().map(String::from).collect();
        let image = test_image();
        let group = IsolationGroup::prepare(docker.clone(), &id, &names, &image)
            .await
            .unwrap();
        let server = group
            .driver(
                DriverConfig::from_image(names[0].clone(), image.clone()).with_entrypoint(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "nc -l -p 9000 -e echo reached".to_string(),
                ]),
            )
            .unwrap();
        let client = group
            .driver(DriverConfig::from_image(names[1].clone(), image).with_entrypoint(vec![
                "sh".to_string(),
                "-c".to_string(),
                r#"[ "$(hostname)" = client ] || exit 2
for i in $(seq 50); do
    out=$(nc -w 2 server 9000 2>/dev/null) && [ "$out" = reached ] && exit 0
    sleep 0.1
done
exit 1"#
                    .to_string(),
            ]))
            .unwrap();

        tokio::try_join!(server.create(), client.create()).unwrap();
        tokio::try_join!(server.start_container(), client.start_container()).unwrap();
        let (server_status, client_status) =
            tokio::try_join!(server.wait_for_container_exit(), client.wait_for_container_exit()).unwrap();
        assert!(matches!(server_status, ExitStatus::Success), "server: {server_status}");
        assert!(matches!(client_status, ExitStatus::Success), "client: {client_status}");

        let name = format!("airlock-{id}");
        let inspected = docker.inspect_container(&format!("{name}-server"), None).await.unwrap();
        assert_eq!(inspected.config.unwrap().hostname.as_deref(), Some("server"));
        let networks = inspected.network_settings.unwrap().networks.unwrap();
        let aliases = networks[&name].aliases.as_ref().unwrap();
        assert!(aliases.iter().any(|alias| alias == "server"), "{aliases:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn preparing_a_group_whose_identity_is_in_use_fails() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let image = test_image();
        let _group = IsolationGroup::prepare(docker.clone(), &id, &["alpha"], &image)
            .await
            .unwrap();
        let error = IsolationGroup::prepare(docker.clone(), &id, &["alpha"], &image)
            .await
            .err()
            .expect("an identity in use should be rejected")
            .to_string();
        assert_eq!(
            error,
            format!("Isolation group '{id}' already has network 'airlock-{id}'")
        );
        let resources = group_resources(&docker, &id).await.unwrap();
        let name = format!("airlock-{id}");
        assert_eq!(resources.networks, vec![name.clone()]);
        assert_eq!(resources.volumes, vec![name]);
        assert!(resources.containers.is_empty(), "{resources:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_from_an_absent_image_fails_with_no_such_image() {
    let docker = daemon().await;
    let id = unique_id();
    in_group(id.clone(), async move {
        let group = IsolationGroup::prepare(docker.clone(), &id, &["alpha"], &test_image())
            .await
            .unwrap();
        let absent = format!("airlock.invalid/absent:{id}");
        assert_eq!(image_id(&docker, &absent).await.unwrap(), None);
        let driver = group.driver(DriverConfig::from_image("alpha", absent)).unwrap();
        let error = driver.create().await.expect_err("an absent image should fail creation");
        assert!(
            matches!(
                error.downcast_ref::<bollard::errors::Error>(),
                Some(bollard::errors::Error::DockerResponseServerError { status_code: 404, message })
                    if message.contains("No such image")
            ),
            "{error}"
        );
        assert!(group_resources(&docker, &id).await.unwrap().containers.is_empty());
    })
    .await;
}
