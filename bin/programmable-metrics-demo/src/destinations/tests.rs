use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::{adp_api::Surface, settings::LaneFlags};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn settings(run_dir: PathBuf) -> Arc<RunSettings> {
    Arc::new(RunSettings {
        ports: DemoPorts {
            primary: free_port(),
            team_b: free_port(),
            team_c: free_port(),
            mrf: free_port(),
            ..DemoPorts::default()
        },
        run_dir,
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 1,
        counter_expiry_secs: 0,
        traffic_rate: 20.0,
        traffic_paused: true,
        contexts: 0,
        lanes: LaneFlags::default(),
    })
}

/// A status of ADP's that lists the recipient of the endpoint at `url` in `state`.
fn listing(url: &str, state: &str) -> Surface<serde_json::Value> {
    Surface::Available(json!({
        "recipients": [{"host": "dd_out", "id": format!("dd_out/additional/0/{}", url), "incarnation": 1,
                        "state": state}]
    }))
}

#[tokio::test]
async fn a_removed_destination_answers_until_adp_retires_its_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let settings = settings(dir.path().join("run"));
    launcher::prepare_run_dir(&settings).unwrap();
    let control = ControlSurface::new();
    let destinations = Destinations::start(Arc::clone(&settings), control.clone()).unwrap();
    assert_eq!(
        destinations
            .labels()
            .iter()
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>(),
        ["Primary", "Team B", "Team C", "Failover (MRF)"]
    );

    let added = destinations.add().unwrap();
    assert_eq!((added.name.as_str(), added.added), ("dest-d", true));
    control.record_status(listing(&added.url(), "live"), unix_millis());

    destinations.remove("dest-d").unwrap();
    assert_eq!(destinations.remove("dest-d").unwrap_err(), RemoveError::Removing);
    let (_, removing) = destinations
        .intakes_with_removal()
        .into_iter()
        .find(|(intake, _)| intake.recipient().name == "dest-d")
        .unwrap();
    assert!(removing.is_some());

    // While ADP lists the recipient live, then draining, the intake keeps listening.
    tokio::time::sleep(REMOVAL_POLL * 3).await;
    control.record_status(listing(&added.url(), "draining"), unix_millis());
    tokio::time::sleep(REMOVAL_POLL * 2).await;
    assert!(destinations.find("dest-d").is_some());
    std::net::TcpStream::connect(DemoPorts::loopback(added.port)).unwrap();

    // Once ADP retires it, the intake stops, and the demo forgets it.
    control.record_status(listing(&added.url(), "retired"), unix_millis());
    let deadline = Instant::now() + Duration::from_secs(10);
    while destinations.find("dest-d").is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(destinations.find("dest-d").is_none());
    assert!(std::net::TcpStream::connect(DemoPorts::loopback(added.port)).is_err());

    // The next destination takes the next label; the boot ones are removable additional endpoints, but the primary
    // and failover endpoints are not.
    assert_eq!(destinations.add().unwrap().label, "Destination E");
    assert_eq!(destinations.remove("primary").unwrap_err(), RemoveError::NotRemovable);
    assert_eq!(destinations.remove("mrf").unwrap_err(), RemoveError::NotRemovable);
    assert_eq!(destinations.remove("nowhere").unwrap_err(), RemoveError::NotFound);
    destinations.shutdown().await;
    assert!(destinations.intakes().is_empty());
}

#[tokio::test]
async fn at_most_a_few_destinations_are_added_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let settings = settings(dir.path().join("run"));
    launcher::prepare_run_dir(&settings).unwrap();
    let destinations = Destinations::start(Arc::clone(&settings), ControlSurface::new()).unwrap();
    for _ in 0..MAX_ADDED {
        destinations.add().unwrap();
    }
    let error = destinations.add().unwrap_err().to_string();
    assert!(error.contains("remove one first"), "{}", error);
    let written = std::fs::read_to_string(settings.adp_config_path()).unwrap();
    assert_eq!(written.matches("- demo-dest-").count(), MAX_ADDED);
    destinations.shutdown().await;
}
