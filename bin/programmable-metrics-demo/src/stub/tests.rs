use axum::{body::Body, http::Request};
use http_body_util::BodyExt as _;
use saluki_metric_plan::ir::ReceivedArtifact;
use tower::ServiceExt as _;

use super::{
    protocol::{transport_hash, ActiveArtifact, ClientInfo, ConfigEntry, Envelope, DEMO_CONFIG_ID},
    *,
};
use crate::export::tests::DEMO_EXPORT;

/// The default program's policy scope as an artifact document, which the demo publishes unchanged in these tests.
fn artifact() -> Value {
    serde_json::from_str::<Value>(DEMO_EXPORT).unwrap()["artifact"].clone()
}

/// A second, different artifact: the default program with its team C route removed.
fn other_artifact() -> Value {
    let mut artifact = artifact();
    artifact["nodes"][0]["params"]["ports"] = 3.into();
    let nodes = artifact["nodes"].as_array_mut().unwrap();
    nodes.retain(|node| !node["id"].as_str().unwrap().starts_with("metrics_routing_filter_1"));
    let edges = artifact["edges"].as_array_mut().unwrap();
    edges.retain(|edge| {
        !edge["from"].as_str().unwrap().starts_with("metrics_routing_filter_1")
            && edge["to"].as_str().unwrap() != "metrics_routing_filter_1.in"
    });
    for edge in edges.iter_mut() {
        if edge["from"] == "metrics_policy.3" {
            edge["from"] = "metrics_policy.2".into();
        }
    }
    artifact
}

/// A fake of ADP's side of the stub protocol: it polls, checks what it receives as the protocol requires, installs,
/// and acknowledges on its next poll.
struct FakeAdp {
    client_id: String,
    revision_high_water: u64,
    generation: u64,
    active: Option<(u64, String)>,
    config_states: Vec<ConfigState>,
}

impl FakeAdp {
    fn new(client_id: &str) -> Self {
        Self {
            client_id: client_id.to_string(),
            revision_high_water: 0,
            generation: 1,
            active: None,
            config_states: Vec::new(),
        }
    }

    fn poll_request(&self) -> PollRequest {
        PollRequest {
            protocol_version: Some(PROTOCOL_VERSION),
            client: ClientInfo {
                id: self.client_id.clone(),
                version: "1.8.0".to_string(),
            },
            products: vec![POLICY_PRODUCT.to_string()],
            config_states: self.config_states.clone(),
            status: Some(ChannelStatus {
                default_program_hash: Some(format!("sha256:{}", "0f".repeat(32))),
                revision_high_water: Some(self.revision_high_water),
                active_generation: Some(self.generation),
                active_plan_hash: Some(format!("sha256:{}", "ab".repeat(32))),
                active_artifact: self.active.as_ref().map(|(revision, hash)| ActiveArtifact {
                    id: DEMO_CONFIG_ID.to_string(),
                    revision: Some(*revision),
                    artifact_hash: Some(hash.clone()),
                }),
                stale_base: Some(false),
                stale_review: Some(false),
                degraded: None,
                live_generations: Some(1),
                scopes: Default::default(),
            }),
        }
    }

    /// Applies a snapshot as the protocol describes ADP's side.
    fn apply(&mut self, response: &PollResponse) {
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);
        let configs = response.products.get(POLICY_PRODUCT).cloned().unwrap_or_default();
        match configs.as_slice() {
            [] => {
                // An empty snapshot revokes: the default program runs.
                if self.active.take().is_some() {
                    self.generation += 1;
                }
                self.config_states.clear();
            }
            [config] => self.apply_config(config),
            _ => panic!("the demo publishes at most one policy config"),
        }
    }

    fn apply_config(&mut self, config: &ConfigEntry) {
        let state = |state: &str, revision, hash: Option<String>, generation, error: Option<&str>| ConfigState {
            product: POLICY_PRODUCT.to_string(),
            id: config.id.clone(),
            version: Some(config.version),
            revision,
            artifact_hash: hash,
            state: state.to_string(),
            generation,
            cut_at: None,
            error: error.map(str::to_string),
        };
        // The transport hash covers the payload's exact bytes.
        if transport_hash(&config.payload) != config.sha256 {
            self.config_states = vec![state("rejected", None, None, None, Some("transport hash mismatch"))];
            return;
        }
        let envelope: Envelope = serde_json::from_str(&config.payload).expect("the payload is an envelope");
        let received = ReceivedArtifact::from_value(envelope.artifact.clone()).expect("the artifact reads");
        if received.hash.to_string() != envelope.artifact_hash {
            self.config_states = vec![state(
                "rejected",
                Some(envelope.revision),
                Some(envelope.artifact_hash),
                None,
                Some("artifact hash mismatch"),
            )];
            return;
        }
        // Idempotent: the active artifact acknowledges the current state.
        if self.active.as_ref().map(|(_, hash)| hash) == Some(&envelope.artifact_hash) {
            return;
        }
        if envelope.revision <= self.revision_high_water {
            self.config_states = vec![state(
                "rejected",
                Some(envelope.revision),
                Some(envelope.artifact_hash),
                None,
                Some("stale revision"),
            )];
            return;
        }
        self.revision_high_water = envelope.revision;
        self.generation += 1;
        self.active = Some((envelope.revision, envelope.artifact_hash.clone()));
        self.config_states = vec![state(
            "active",
            Some(envelope.revision),
            Some(envelope.artifact_hash),
            Some(self.generation),
            None,
        )];
    }
}

/// Sends a poll through the stub's router and returns the status and the raw body.
async fn post_poll(channel: &StubChannel, body: Vec<u8>) -> (StatusCode, bytes::Bytes) {
    let request = Request::post(POLL_PATH)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = router(channel.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    (status, response.into_body().collect().await.unwrap().to_bytes())
}

/// Polls as the fake ADP, applies the answer, and returns the raw answer.
async fn poll(channel: &StubChannel, adp: &mut FakeAdp) -> bytes::Bytes {
    let (status, body) = post_poll(channel, serde_json::to_vec(&adp.poll_request()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let response: PollResponse = serde_json::from_slice(&body).unwrap();
    adp.apply(&response);
    body
}

fn channel_in(dir: &std::path::Path) -> StubChannel {
    StubChannel::new(ArtifactStore::open(dir).unwrap())
}

#[tokio::test]
async fn an_empty_store_serves_an_empty_policy_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    let mut adp = FakeAdp::new("adp-1");
    let body = poll(&channel, &mut adp).await;
    let response: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(response, json!({"protocol_version": 1, "products": {"policy": []}}));
    assert!(adp.active.is_none());

    let view = channel.view(10, None);
    assert_eq!(view.polls_total, 1);
    assert!(view.current.is_none());
    assert_eq!(view.next_revision, 1);
    assert_eq!(view.last_poll.unwrap().client_id, "adp-1");
}

#[tokio::test]
async fn a_published_artifact_reaches_adp_with_verifiable_hashes_and_repeats_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    let mut adp = FakeAdp::new("adp-1");
    poll(&channel, &mut adp).await;

    let approved = ApprovedAgainst {
        active_plan_hash: format!("sha256:{}", "ab".repeat(32)),
    };
    let published = channel
        .publish(artifact(), Some(approved.clone()), None, 1_000)
        .unwrap();
    assert_eq!(published.revision, 1);
    assert_eq!(published.version, 1);
    assert_eq!(published.id, "demo");
    let expected_hash = ReceivedArtifact::from_value(artifact()).unwrap().hash.to_string();
    assert_eq!(published.artifact_hash, expected_hash);

    let first = poll(&channel, &mut adp).await;
    let response: PollResponse = serde_json::from_slice(&first).unwrap();
    let config = &response.products[POLICY_PRODUCT][0];
    // The transport hash is the SHA-256 of the payload's bytes, and the payload is the envelope as a JSON string.
    assert_eq!(config.sha256, transport_hash(&config.payload));
    assert_eq!(config.sha256.len(), 64);
    let envelope: Envelope = serde_json::from_str(&config.payload).unwrap();
    assert_eq!(envelope.revision, 1);
    assert_eq!(envelope.artifact_hash, expected_hash);
    assert_eq!(envelope.approved_against, Some(approved));
    assert_eq!(envelope.artifact, artifact());
    assert_eq!(adp.active, Some((1, expected_hash.clone())));
    assert_eq!(adp.config_states[0].state, "active");

    // Every later poll gets the same bytes: delivery repeats, and ADP acknowledges the current state.
    let second = poll(&channel, &mut adp).await;
    let third = poll(&channel, &mut adp).await;
    assert_eq!(first, second);
    assert_eq!(second, third);
    assert_eq!(adp.generation, 2);
    assert_eq!(adp.config_states[0].state, "active");

    let view = channel.view(10, None);
    let current = view.current.unwrap();
    assert_eq!(current.revision, 1);
    assert_eq!(current.sha256, config.sha256);
    assert_eq!(view.revision_counter, 1);
}

#[tokio::test]
async fn revoking_empties_the_snapshot_and_adp_returns_to_the_default_program() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    let mut adp = FakeAdp::new("adp-1");
    channel.publish(artifact(), None, None, 1_000).unwrap();
    poll(&channel, &mut adp).await;
    assert!(adp.active.is_some());

    assert!(channel.revoke(None, 2_000).unwrap());
    let body = poll(&channel, &mut adp).await;
    let response: PollResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.products.get(POLICY_PRODUCT), Some(&Vec::new()));
    assert!(adp.active.is_none());

    // Revoking does not consume a revision; the next publish continues the sequence.
    let view = channel.view(10, None);
    assert!(view.current.is_none());
    assert_eq!(view.revision_counter, 1);
    let history = view
        .history
        .iter()
        .map(|entry| (entry.action, entry.revision))
        .collect::<Vec<_>>();
    assert_eq!(
        history,
        [
            (store::HistoryAction::Revoke, None),
            (store::HistoryAction::Publish, Some(1))
        ]
    );
    assert_eq!(
        channel.publish(other_artifact(), None, None, 3_000).unwrap().revision,
        2
    );

    // Revoking an empty snapshot changes nothing and records nothing.
    assert!(channel.revoke(None, 4_000).unwrap());
    assert!(!channel.revoke(None, 5_000).unwrap());
    let actions = channel
        .view(10, None)
        .history
        .iter()
        .map(|entry| entry.action)
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            store::HistoryAction::Revoke,
            store::HistoryAction::Publish,
            store::HistoryAction::Revoke,
            store::HistoryAction::Publish
        ]
    );
}

#[tokio::test]
async fn the_next_revision_is_above_both_the_local_counter_and_adps_high_water_mark() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());

    // ADP has seen revision 41 (from an earlier store, say): the next revision is 42, and ADP accepts it.
    let mut adp = FakeAdp::new("adp-1");
    adp.revision_high_water = 41;
    poll(&channel, &mut adp).await;
    assert_eq!(channel.view(10, None).next_revision, 42);
    assert_eq!(channel.publish(artifact(), None, None, 1_000).unwrap().revision, 42);
    poll(&channel, &mut adp).await;
    assert_eq!(adp.config_states[0].state, "active");
    assert_eq!(adp.config_states[0].revision, Some(42));

    // A high-water mark reported elsewhere (the privileged status route) counts too.
    assert_eq!(
        channel
            .publish(other_artifact(), None, Some(50), 2_000)
            .unwrap()
            .revision,
        51
    );

    // After ADP restarts, its high-water mark is back to zero; the local counter keeps revisions increasing.
    let mut restarted = FakeAdp::new("adp-2");
    poll(&channel, &mut restarted).await;
    assert_eq!(restarted.config_states[0].revision, Some(51));
    assert_eq!(channel.publish(artifact(), None, None, 3_000).unwrap().revision, 52);
}

#[tokio::test]
async fn the_store_serves_the_same_snapshot_after_the_demo_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let first = {
        let channel = channel_in(dir.path());
        channel.publish(artifact(), None, None, 1_000).unwrap();
        poll(&channel, &mut FakeAdp::new("adp-1")).await
    };

    let reopened = channel_in(dir.path());
    let mut adp = FakeAdp::new("adp-1");
    let second = poll(&reopened, &mut adp).await;
    assert_eq!(first, second);
    assert_eq!(adp.active.as_ref().map(|(revision, _)| *revision), Some(1));
    assert_eq!(reopened.view(10, None).revision_counter, 1);
    assert_eq!(reopened.view(10, None).history.len(), 1);
    assert_eq!(
        reopened.publish(other_artifact(), None, None, 2_000).unwrap().revision,
        2
    );
}

#[tokio::test]
async fn polls_in_another_protocol_version_or_not_json_are_refused_and_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    channel.publish(artifact(), None, None, 1_000).unwrap();

    let mut request = FakeAdp::new("adp-1").poll_request();
    request.protocol_version = Some(2);
    let (status, body) = post_poll(&channel, serde_json::to_vec(&request).unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert!(error["error"]
        .as_str()
        .unwrap()
        .contains("Unsupported stub protocol version 2"));
    let last = channel.view(10, None).last_poll.unwrap();
    assert!(last.error.unwrap().contains("version 2"));
    // A refused poll's status does not count as ADP's status.
    assert!(channel.latest_status().is_none());

    let (status, body) = post_poll(&channel, br#"{"protocol_version": "1"}"#.to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert!(error["error"]
        .as_str()
        .unwrap()
        .contains("no readable protocol_version"));

    let (status, body) = post_poll(&channel, b"{not json".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert!(error["error"].as_str().unwrap().contains("not JSON"));

    // Every refused poll is recorded, so the timeline shows the problem.
    let view = channel.view(10, None);
    assert_eq!(view.polls_total, 3);
    let errors = view
        .timeline
        .iter()
        .map(|entry| entry.error.clone().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 3);
    assert!(errors[0].contains("not JSON"), "{}", errors[0]);
    assert!(errors[1].contains("no readable protocol_version"), "{}", errors[1]);
    assert!(errors[2].contains("version 2"), "{}", errors[2]);
}

#[tokio::test]
async fn a_poll_with_mistyped_acknowledgments_still_gets_the_snapshot_and_is_recorded_with_notes() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    let published = channel.publish(artifact(), None, None, 1_000).unwrap();

    // ADP's side gets a type wrong: `generation` as a string, `degraded` as an object, `state` as null.
    let poll = json!({
        "protocol_version": 1,
        "client": {"id": "adp-1", "version": "1.8.0"},
        "products": ["policy"],
        "config_states": [{"product": "policy", "id": "demo", "revision": 1, "artifact_hash": published.artifact_hash,
                           "state": null, "generation": "2"}],
        "status": {"revision_high_water": 1, "active_generation": 2, "degraded": {"reason": "slow exit"}}
    });
    let (status, body) = post_poll(&channel, serde_json::to_vec(&poll).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let response: PollResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.products[POLICY_PRODUCT][0].sha256, published.sha256);

    let view = channel.view(10, None);
    assert_eq!(view.polls_total, 1);
    let entry = &view.timeline[0];
    assert_eq!(entry.error, None);
    assert_eq!(entry.config_states[0].revision, Some(1));
    assert_eq!(entry.status.as_ref().unwrap().active_generation, Some(2));
    assert_eq!(entry.notes.len(), 3, "{:?}", entry.notes);
    assert!(
        entry.notes[0].starts_with("config_states[0].state:"),
        "{:?}",
        entry.notes
    );
    assert!(
        entry.notes[1].starts_with("config_states[0].generation:"),
        "{:?}",
        entry.notes
    );
    assert!(entry.notes[2].starts_with("status.degraded:"), "{:?}", entry.notes);
    // The readable parts of its status count: the high-water mark is ADP's.
    assert_eq!(channel.latest_status().unwrap().1.revision_high_water, Some(1));
    assert_eq!(view.next_revision, 2);
}

#[tokio::test]
async fn the_next_revision_in_the_view_counts_the_privileged_high_water_mark_as_publishing_does() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    let mut adp = FakeAdp::new("adp-1");
    adp.revision_high_water = 4;
    poll(&channel, &mut adp).await;
    assert_eq!(channel.view(10, None).next_revision, 5);
    assert_eq!(channel.view(10, Some(9)).next_revision, 10);
    assert_eq!(channel.view(10, Some(2)).next_revision, 5);
    assert_eq!(channel.publish(artifact(), None, Some(9), 1_000).unwrap().revision, 10);
}

#[test]
fn a_failed_store_write_changes_nothing_and_a_failed_history_line_does_not_undo_the_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::open(dir.path()).unwrap();
    let first = store.publish(artifact(), None, None, 1_000).unwrap();

    // The store file cannot be written: the publish fails and the store still serves revision 1.
    std::fs::create_dir(dir.path().join("store.json.tmp")).unwrap();
    assert!(store.publish(other_artifact(), None, None, 2_000).is_err());
    assert!(store.revoke(None, 2_000).is_err());
    assert_eq!(store.current(), Some(&first));
    assert_eq!(store.revision_counter(), 1);
    assert_eq!(store.snapshot(), vec![first.entry()]);
    std::fs::remove_dir(dir.path().join("store.json.tmp")).unwrap();

    // The history cannot be appended to: the publish stands, in memory and on disk.
    std::fs::remove_file(dir.path().join("history.jsonl")).unwrap();
    std::fs::create_dir(dir.path().join("history.jsonl")).unwrap();
    let second = store.publish(other_artifact(), None, None, 3_000).unwrap();
    assert_eq!(second.revision, 2);
    assert_eq!(store.current(), Some(&second));
    assert_eq!(store.history().last().unwrap().revision, Some(2));
    let reopened = ArtifactStore::open(dir.path()).unwrap();
    assert_eq!(reopened.current(), Some(&second));
    assert_eq!(reopened.revision_counter(), 2);
}

#[test]
fn without_a_store_file_the_revision_counter_continues_from_the_history() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = ArtifactStore::open(dir.path()).unwrap();
        for at_ms in 1..=3 {
            store.publish(artifact(), None, None, at_ms).unwrap();
        }
    }
    // The store file is moved aside, as its error message suggests when it cannot be read.
    std::fs::rename(dir.path().join("store.json"), dir.path().join("store.json.aside")).unwrap();
    let mut store = ArtifactStore::open(dir.path()).unwrap();
    assert_eq!(store.current(), None);
    assert_eq!(store.revision_counter(), 3);
    assert_eq!(store.publish(artifact(), None, None, 4).unwrap().revision, 4);
}

#[tokio::test]
async fn the_acknowledgment_timeline_folds_polls_that_report_the_same_state() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    let mut adp = FakeAdp::new("adp-1");
    for _ in 0..3 {
        poll(&channel, &mut adp).await;
    }
    channel.publish(artifact(), None, None, 1_000).unwrap();
    // The poll that receives the artifact still reports the old state; the next ones report it active.
    for _ in 0..4 {
        poll(&channel, &mut adp).await;
    }

    let view = channel.view(10, None);
    assert_eq!(view.polls_total, 7);
    let timeline = view
        .timeline
        .iter()
        .map(|entry| {
            (
                entry.polls,
                entry.config_states.first().map(|state| state.state.clone()),
                entry.status.as_ref().and_then(|status| status.active_generation),
            )
        })
        .collect::<Vec<_>>();
    // Most recent first.
    assert_eq!(timeline, [(3, Some("active".to_string()), Some(2)), (4, None, Some(1))]);
    assert!(view.timeline[0].first_ms <= view.timeline[0].last_ms);
}

/// An artifact of the pre-aggregation scope.
fn pre_aggregation_artifact(prefix: &str) -> Value {
    json!({
        "ir_version": 1,
        "scope": "dogstatsd_pre_aggregation",
        "base": {"program_hash": format!("sha256:{}", "0".repeat(64))},
        "nodes": [{"id": "keep", "op": "split", "params": {"predicate": {"name": {"prefix": prefix}}}}],
        "edges": [{"from": "input.metrics", "to": "keep.in"}]
    })
}

/// Returns the config IDs and revisions of the snapshot a poll gets.
async fn served(channel: &StubChannel) -> Vec<(String, u64)> {
    let (status, body) = post_poll(
        channel,
        serde_json::to_vec(&FakeAdp::new("adp-1").poll_request()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response: PollResponse = serde_json::from_slice(&body).unwrap();
    response.products[POLICY_PRODUCT]
        .iter()
        .map(|config| {
            let envelope: Envelope = serde_json::from_str(&config.payload).unwrap();
            (config.id.clone(), envelope.revision)
        })
        .collect()
}

#[tokio::test]
async fn each_scope_has_one_config_under_one_revision_counter() {
    let dir = tempfile::tempdir().unwrap();
    let channel = channel_in(dir.path());
    channel.publish(artifact(), None, None, 1_000).unwrap();
    let pre = channel
        .publish(pre_aggregation_artifact("a."), None, None, 2_000)
        .unwrap();
    assert_eq!(
        (pre.id.as_str(), pre.scope.as_str(), pre.revision),
        ("demo-dogstatsd_pre_aggregation", "dogstatsd_pre_aggregation", 2)
    );
    // The snapshot holds one config per scope, in scope order.
    assert_eq!(
        served(&channel).await,
        [
            ("demo-dogstatsd_pre_aggregation".to_string(), 2),
            ("demo".to_string(), 1)
        ]
    );
    // A publish replaces its own scope's config, under the next revision of the one counter.
    channel.publish(other_artifact(), None, None, 3_000).unwrap();
    assert_eq!(
        served(&channel).await,
        [
            ("demo-dogstatsd_pre_aggregation".to_string(), 2),
            ("demo".to_string(), 3)
        ]
    );
    let view = channel.view(10, None);
    assert_eq!(view.current.as_ref().map(|current| current.revision), Some(3));
    let configs = view
        .configs
        .iter()
        .map(|config| (config.scope.as_str(), config.revision))
        .collect::<Vec<_>>();
    assert_eq!(configs, [("dogstatsd_pre_aggregation", 2), ("post_enrich", 3)]);

    // Revoking one scope removes its config alone; revoking it again changes nothing.
    assert!(channel.revoke(Some("dogstatsd_pre_aggregation"), 4_000).unwrap());
    assert!(!channel.revoke(Some("dogstatsd_pre_aggregation"), 5_000).unwrap());
    assert_eq!(served(&channel).await, [("demo".to_string(), 3)]);
    // Revoking every scope empties the snapshot.
    assert!(channel.revoke(None, 6_000).unwrap());
    assert!(served(&channel).await.is_empty());
    let history = channel
        .view(10, None)
        .history
        .iter()
        .map(|entry| (entry.action, entry.scope.clone(), entry.revision))
        .collect::<Vec<_>>();
    let scope = |scope: &str| Some(scope.to_string());
    assert_eq!(
        history,
        [
            (store::HistoryAction::Revoke, None, None),
            (store::HistoryAction::Revoke, scope("dogstatsd_pre_aggregation"), None),
            (store::HistoryAction::Publish, scope("post_enrich"), Some(3)),
            (
                store::HistoryAction::Publish,
                scope("dogstatsd_pre_aggregation"),
                Some(2)
            ),
            (store::HistoryAction::Publish, scope("post_enrich"), Some(1)),
        ]
    );

    // The store keeps every scope's config across a restart.
    channel
        .publish(pre_aggregation_artifact("b."), None, None, 7_000)
        .unwrap();
    let reopened = channel_in(dir.path());
    assert_eq!(
        served(&reopened).await,
        [("demo-dogstatsd_pre_aggregation".to_string(), 4)]
    );
}

#[test]
fn a_store_the_first_version_of_the_demo_wrote_still_reads() {
    let dir = tempfile::tempdir().unwrap();
    let config = {
        let mut store = ArtifactStore::open(dir.path()).unwrap();
        store.publish(artifact(), None, None, 1_000).unwrap()
    };
    // The first version wrote one config, for the policy scope, without its scope.
    let mut one_scope = serde_json::to_value(&config).unwrap();
    one_scope.as_object_mut().unwrap().remove("scope");
    let file = json!({"format": 1, "revision_counter": 5, "current": one_scope});
    std::fs::write(dir.path().join("store.json"), file.to_string()).unwrap();
    let store = ArtifactStore::open(dir.path()).unwrap();
    assert_eq!(store.current(), Some(&config));
    assert_eq!(store.revision_counter(), 5);
    assert_eq!(store.snapshot(), vec![config.entry()]);

    // Another format is refused, with what to do.
    std::fs::write(dir.path().join("store.json"), json!({"format": 9}).to_string()).unwrap();
    let error = ArtifactStore::open(dir.path()).err().unwrap().to_string();
    assert!(
        error.contains("it has format 9, but this demo reads formats 1 and 2. Use a new run directory."),
        "{}",
        error
    );
}
