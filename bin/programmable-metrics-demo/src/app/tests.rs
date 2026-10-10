use std::{net::SocketAddr, time::Duration};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use super::*;
use crate::{
    adp_api::SurfaceState,
    control::NOT_EXPOSED_MESSAGE,
    export::{tests::DEMO_EXPORT, ExportBundle},
    launcher::{self, Phase},
    stub::{
        protocol::{ClientInfo, Envelope, PollRequest, PollResponse},
        store::ArtifactStore,
    },
};

struct Harness {
    app: App,
    _dir: tempfile::TempDir,
}

/// Returns a port nothing listens on: bound once, then released.
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn harness() -> Harness {
    let _ = saluki_tls::initialize_default_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let ports = DemoPorts {
        api: closed_port(),
        secure_api: closed_port(),
        ..DemoPorts::default()
    };
    let settings = Arc::new(RunSettings {
        ports,
        run_dir: dir.path().join("run"),
        poll_interval: Duration::from_secs(1),
        aggregate_window_secs: 1,
        counter_expiry_secs: 0,
        traffic_rate: 20.0,
        traffic_paused: true,
    });
    launcher::prepare_run_dir(&settings).unwrap();
    let api = AdpApi::new(settings.ports.api, settings.ports.secure_api, &settings.ipc_cert_path())
        .await
        .unwrap();
    let intakes = settings
        .ports
        .recipients()
        .into_iter()
        .map(IntakeRecorder::new)
        .collect();
    let app = App::new(Components {
        intakes,
        traffic: TrafficGenerator::new(SocketAddr::from(([127, 0, 0, 1], closed_port())), 20.0, false),
        telemetry: TelemetryScraper::new(),
        channel: StubChannel::new(ArtifactStore::open(&settings.store_dir()).unwrap()),
        control: ControlSurface::new(),
        launch: Launch::new(Phase::External, std::path::Path::new("agent-data-plane"), &settings),
        api,
        settings,
    });
    Harness { app, _dir: dir }
}

async fn call(app: &App, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    let request = match body {
        Some(body) => request.body(Body::from(serde_json::to_vec(&body).unwrap())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = router(app.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

fn export_source() -> String {
    let export = ExportBundle::from_value(serde_json::from_str(DEMO_EXPORT).unwrap()).unwrap();
    default_program_source(&export)
}

/// Records a poll from ADP whose status carries the given active plan hash and revision high-water mark.
fn adp_polls(app: &App, active_plan_hash: &str, revision_high_water: u64) {
    let request = PollRequest {
        protocol_version: Some(1),
        client: ClientInfo {
            id: "adp-1".to_string(),
            version: "1.8.0".to_string(),
        },
        products: vec!["policy".to_string()],
        config_states: Vec::new(),
        status: Some(ChannelStatus {
            active_plan_hash: Some(active_plan_hash.to_string()),
            revision_high_water: Some(revision_high_water),
            ..ChannelStatus::default()
        }),
    };
    app.channel.poll(request, Vec::new(), unix_millis()).unwrap();
}

#[tokio::test]
async fn publishing_approves_exactly_the_last_compiled_hash() {
    let harness = harness().await;
    let app = &harness.app;

    let (status, body) = call(app, "POST", "/api/publish", Some(json!({"artifact_hash": "sha256:00"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("no compiled artifact"));

    let active_plan_hash = format!("sha256:{}", "ab".repeat(32));
    adp_polls(app, &active_plan_hash, 6);
    let (status, compiled) = call(app, "POST", "/api/compile", Some(json!({"source": export_source()}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(compiled["ok"], true);
    // Without the export bundle, the source is parsed but not checked.
    assert_eq!(compiled["checked_against"]["available"], false);
    let hash = compiled["artifact_hash"].as_str().unwrap().to_string();

    let wrong = format!("sha256:{}", "cd".repeat(32));
    let (status, body) = call(app, "POST", "/api/publish", Some(json!({"artifact_hash": wrong}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("is not the hash of the last compiled artifact"));
    assert!(app.channel.view(10, None).current.is_none());

    // Compiling a source that does not parse withdraws the approval target.
    let (_, failed) = call(app, "POST", "/api/compile", Some(json!({"source": "policy {"}))).await;
    assert_eq!(failed["ok"], false);
    let (status, _) = call(app, "POST", "/api/publish", Some(json!({"artifact_hash": hash}))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (_, compiled) = call(app, "POST", "/api/compile", Some(json!({"source": export_source()}))).await;
    assert_eq!(compiled["artifact_hash"], hash.as_str());
    let (status, body) = call(app, "POST", "/api/publish", Some(json!({"artifact_hash": hash}))).await;
    assert_eq!(status, StatusCode::OK, "{}", body);
    // ADP reported revision 6 as its high-water mark.
    assert_eq!(body["revision"], 7);
    assert_eq!(body["artifact_hash"], hash.as_str());

    // The approval is used up: approving the same compile again publishes nothing, and says why.
    let (status, body) = call(app, "POST", "/api/publish", Some(json!({"artifact_hash": hash}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("already used"), "{}", body);
    assert!(body["error"].as_str().unwrap().contains("revision 7"), "{}", body);
    assert_eq!(app.channel.view(10, None).revision_counter, 7);

    // ADP's next poll receives an envelope approved against the plan that was active when the artifact was compiled.
    let response: PollResponse = {
        let request = PollRequest {
            protocol_version: Some(1),
            ..PollRequest::default()
        };
        app.channel.poll(request, Vec::new(), unix_millis()).unwrap()
    };
    let envelope: Envelope = serde_json::from_str(&response.products["policy"][0].payload).unwrap();
    assert_eq!(envelope.revision, 7);
    assert_eq!(envelope.artifact_hash, hash);
    assert_eq!(envelope.approved_against.unwrap().active_plan_hash, active_plan_hash);

    let (status, body) = call(app, "POST", "/api/revoke", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revoked"], true);
    assert!(app.channel.view(10, None).current.is_none());
    // Revoking an empty snapshot changes nothing.
    let (status, body) = call(app, "POST", "/api/revoke", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revoked"], false);
    assert_eq!(app.channel.view(10, None).history.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_approvals_of_one_compile_publish_it_once() {
    let harness = harness().await;
    let app = &harness.app;
    let (_, compiled) = call(app, "POST", "/api/compile", Some(json!({"source": export_source()}))).await;
    let hash = compiled["artifact_hash"].as_str().unwrap().to_string();

    // A double-click, or two tabs: the same approval arrives several times at once.
    let mut publishes = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let (app, hash) = (app.clone(), hash.clone());
        publishes
            .spawn(async move { call(&app, "POST", "/api/publish", Some(json!({ "artifact_hash": hash }))).await });
    }
    let statuses = publishes.join_all().await;
    let published = statuses.iter().filter(|(status, _)| *status == StatusCode::OK).count();
    let refused = statuses
        .iter()
        .filter(|(status, _)| *status == StatusCode::CONFLICT)
        .count();
    assert_eq!((published, refused), (1, 3), "{:?}", statuses);
    assert_eq!(app.channel.view(10, None).revision_counter, 1);
}

#[tokio::test]
async fn the_compile_api_returns_placed_diagnostics_and_checks_against_the_export_in_hand() {
    let harness = harness().await;
    let app = &harness.app;
    app.control.record_export(
        Surface::Available(serde_json::from_str(DEMO_EXPORT).unwrap()),
        unix_millis(),
    );

    let source = export_source().replacen("emit metrics_routing_encode_1", "emit metrics_routing_encode_9", 1);
    let (status, body) = call(app, "POST", "/api/compile", Some(json!({ "source": source }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["checked_against"]["available"], true);
    assert_eq!(body["errors"], 1);
    let diagnostic = &body["diagnostics"][0];
    assert_eq!(diagnostic["severity"], "error");
    assert_eq!(diagnostic["origin"], "check");
    assert_eq!(diagnostic["range"]["line"], 37);
    assert!(diagnostic["message"]
        .as_str()
        .unwrap()
        .contains("metrics_routing_encode_9"));

    let (_, body) = call(
        app,
        "POST",
        "/api/compile",
        Some(json!({ "source": "policy post_enrich {\n  nod" })),
    )
    .await;
    assert_eq!(body["ok"], false);
    assert_eq!(body["diagnostics"][0]["origin"], "syntax");
    assert_eq!(body["diagnostics"][0]["range"]["line"], 2);
}

#[tokio::test]
async fn the_compile_api_composes_into_the_default_program_adp_serves_as_the_revision_a_publish_would_get() {
    let harness = harness().await;
    let app = &harness.app;
    let (plan, export) = crate::preview::tests::fixture_documents();
    let active_plan_hash = plan
        .hash(saluki_metric_plan::canonical::ResolvedValues::Include)
        .unwrap()
        .to_string();
    // ADP's high-water mark is 4, so a publish would be revision 5; the default program is active, in generation 2.
    app.control.record_status(
        Surface::Available(json!({
            "active_plan_hash": active_plan_hash,
            "active_generation": 2,
            "revision_high_water": 4,
        })),
        unix_millis(),
    );
    app.control.record_export(Surface::Available(export), unix_millis());
    assert_eq!(
        app.control
            .record_default_program(Surface::Available(serde_json::to_value(&plan).unwrap())),
        SurfaceState::Available
    );
    app.control
        .record_active_plan(Surface::Available(serde_json::to_value(&plan).unwrap()));

    let source = app
        .control
        .export()
        .map(|export| default_program_source(&export))
        .unwrap()
        .replacen(
            "  input.metrics -> metrics_policy\n",
            "  input.metrics -> drop_debug\n\n  node drop_debug = split(name starts_with \"demo.debug.\") {\n    true  \
             -> drop\n    false -> metrics_policy\n  }\n",
            1,
        );
    let (status, body) = call(app, "POST", "/api/compile", Some(json!({ "source": source }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["checked_against"]["composed"], true);
    assert_eq!(body["diagnostics"], json!([]));
    let preview = &body["preview"];
    assert_eq!((&preview["available"], &preview["valid"]), (&json!(true), &json!(true)));
    assert_eq!(
        preview["against"],
        json!({
            "kind": "active",
            "plan_hash": active_plan_hash,
            "generation": 2,
            "artifact_revision": null,
        })
    );
    let split = preview["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["node"] == "policy/drop_debug")
        .expect("the new split is in the diff");
    assert_eq!(
        (&split["change"], &split["provenance"]),
        (&json!("added"), &json!("revision 5"))
    );

    // A default program that is not the one the export was taken from (the configuration changed between the two
    // requests) is refused, and compiling falls back to the structural checks.
    let mut other = plan.clone();
    let gateway = other
        .nodes
        .iter_mut()
        .find(|node| node.id.to_string() == "af_metrics_gateway")
        .unwrap();
    gateway
        .params
        .insert("predicate".parse().unwrap(), json!({ "const": true }));
    let state = app
        .control
        .record_default_program(Surface::Available(serde_json::to_value(&other).unwrap()));
    assert_eq!(state, SurfaceState::Failed);
    assert!(app
        .control
        .view()
        .default_program_error
        .unwrap()
        .contains("fetched again"));
    let (_, body) = call(app, "POST", "/api/compile", Some(json!({ "source": source }))).await;
    assert_eq!(body["checked_against"]["composed"], false);
    assert_eq!(body["preview"]["available"], false);
}

#[tokio::test]
async fn the_state_says_plainly_when_adp_lacks_the_control_surface() {
    let harness = harness().await;
    let app = &harness.app;
    app.control.record_status(Surface::NotExposed, unix_millis());

    let (status, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["control"]["status_state"], "not_exposed");
    assert_eq!(state["control"]["message"], NOT_EXPOSED_MESSAGE);
    assert_eq!(state["adp"]["phase"], "external");
    let intakes = state["intakes"].as_array().unwrap();
    let names = intakes
        .iter()
        .map(|intake| intake["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["primary", "team-b", "team-c", "mrf"]);
    assert_eq!(intakes[1]["expected_api_key"], "demo-team-b");
    assert_eq!(state["channel"]["next_revision"], 1);
    assert_eq!(state["telemetry"]["state"], "waiting");
    assert_eq!(state["traffic"]["running"], false);
    assert_eq!(state["compiled"], Value::Null);
    assert_eq!(state["legend"], Value::Null);
    // ADP is not answering, so the canary check waits rather than call anything missing.
    assert_eq!(state["canary"]["state"], "waiting");
    assert!(state["canary"]["reason"].as_str().unwrap().contains("not answering"));

    // With the export in hand, the state carries its legend.
    app.control.record_export(
        Surface::Available(serde_json::from_str(DEMO_EXPORT).unwrap()),
        unix_millis(),
    );
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["legend"]["outputs"][1]["intake"], "team-b");
    assert_eq!(state["legend"]["bindings"][1]["value"], json!(["demo.checkout."]));

    // ADP is not listening at all: the default program cannot be loaded, and the reason says why.
    let (status, body) = call(app, "GET", "/api/default-program", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], false);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("could not be loaded from ADP"));
    let (status, body) = call(app, "GET", "/api/adp/status", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().is_some());
}

#[tokio::test]
async fn the_ui_traffic_and_intake_routes_answer() {
    let harness = harness().await;
    let app = &harness.app;

    let (status, page) = call(app, "GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.as_str().unwrap().contains("<title>"));
    for asset in ["/app.js", "/app.css"] {
        let (status, body) = call(app, "GET", asset, None).await;
        assert_eq!(status, StatusCode::OK, "{}", asset);
        assert!(!body.as_str().unwrap().is_empty(), "{}", asset);
    }

    let (status, traffic) = call(app, "POST", "/api/traffic", Some(json!({"rate": 5.0}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(traffic["rate"], 5.0);
    assert_eq!(traffic["running"], false);

    let (status, recent) = call(app, "GET", "/api/intakes/team-c/recent", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recent["series"], json!([]));
    let (status, _) = call(app, "GET", "/api/intakes/nowhere/recent", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(app, "GET", "/api/adp/secrets", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
