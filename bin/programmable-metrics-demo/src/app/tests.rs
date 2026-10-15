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
    // The intakes listen, on ports nothing else holds, so that destinations can be added and removed.
    let ports = DemoPorts {
        api: closed_port(),
        secure_api: closed_port(),
        primary: closed_port(),
        team_b: closed_port(),
        team_c: closed_port(),
        mrf: closed_port(),
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
        contexts: 0,
        lanes: Default::default(),
    });
    launcher::prepare_run_dir(&settings).unwrap();
    let api = AdpApi::new(settings.ports.api, settings.ports.secure_api, &settings.ipc_cert_path())
        .await
        .unwrap();
    let control = ControlSurface::new();
    let destinations = Destinations::start(Arc::clone(&settings), control.clone()).unwrap();
    let app = App::new(Components {
        destinations,
        traffic: TrafficGenerator::new(SocketAddr::from(([127, 0, 0, 1], closed_port())), 20.0, false, 0),
        telemetry: TelemetryScraper::new(),
        channel: StubChannel::new(ArtifactStore::open(&settings.store_dir()).unwrap()),
        control,
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

/// The demo export's program as the editor prints it, with its base stated, so that it compiles where the export is
/// not at hand to give the base back.
fn export_source() -> String {
    let export = ExportBundle::from_value(serde_json::from_str(DEMO_EXPORT).unwrap()).unwrap();
    let base = format!("policy metrics {{\n  base \"{}\"\n\n", export.program_hash());
    default_program_source(&export, &RecipientLabels::default()).replacen("policy metrics {\n", &base, 1)
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
    assert_eq!(diagnostic["range"]["line"], 46);
    assert!(diagnostic["message"]
        .as_str()
        .unwrap()
        .contains("metrics_routing_encode_9"));

    let (_, body) = call(
        app,
        "POST",
        "/api/compile",
        Some(json!({ "source": "policy metrics {\n  nod" })),
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
        .map(|export| default_program_source(&export, &RecipientLabels::default()))
        .unwrap()
        .replacen("\n    -> metrics_policy\n", "\n    -> drop_debug\n", 1)
        .replacen(
            "\n  node metrics_policy = ",
            "\n  node drop_debug = split(name starts_with \"demo.debug.\") {\n    true  -> drop\n    false -> \
             metrics_policy\n  }\n\n  node metrics_policy = ",
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
    assert_eq!(split["change"], json!("added"));

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
    // Teams B and C are additional endpoints, which can be removed; without an export or an active plan, the demo
    // cannot say whether ADP declares or addresses them.
    assert_eq!(
        (&intakes[1]["kind"], &intakes[1]["removable"], &intakes[0]["removable"]),
        (&json!("additional"), &json!(true), &json!(false))
    );
    assert_eq!(
        (&intakes[1]["in_directory"], &intakes[1]["addressed"]),
        (&Value::Null, &Value::Null)
    );
    assert_eq!(
        state["native"],
        json!({"metric_filterlist": [], "failover_metrics": false})
    );
    assert_eq!(state["channel"]["next_revision"], 1);
    assert_eq!(state["recipients"]["reported"], false);
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
    assert_eq!(state["legend"]["outputs"][0]["intake"], "primary");
    assert_eq!(state["legend"]["bindings"][1]["value"], json!(["demo.checkout."]));
    // The export's directory names teams B and C (as the fixture writes their ports); this harness's intakes listen
    // elsewhere, so ADP's directory does not name them.
    assert_eq!(state["intakes"][1]["in_directory"], false);

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

    // Pausing an intake shows in the state, and resuming it too.
    let (status, body) = call(app, "POST", "/api/intakes/team-c/pause", Some(json!({"paused": true}))).await;
    assert_eq!((status, &body["paused"]), (StatusCode::OK, &json!(true)));
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["intakes"][2]["paused"], true);
    call(app, "POST", "/api/intakes/team-c/pause", Some(json!({"paused": false}))).await;
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["intakes"][2]["paused"], false);
    let (status, _) = call(app, "POST", "/api/intakes/nowhere/pause", Some(json!({"paused": true}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn destinations_are_added_and_removed_in_adps_configuration_file_and_native_settings_written_there() {
    let harness = harness().await;
    let app = &harness.app;
    let config = || -> Value {
        serde_yaml::from_str(&std::fs::read_to_string(app.settings.adp_config_path()).unwrap()).unwrap()
    };
    let team_b = DemoPorts::http_url(app.settings.ports.team_b);

    // Added: an intake on a free port, with a key and a label of its own, in the file and the state.
    let (status, added) = call(app, "POST", "/api/destinations", None).await;
    assert_eq!(status, StatusCode::OK, "{}", added);
    assert_eq!(
        (&added["name"], &added["label"]),
        (&json!("dest-d"), &json!("Destination D"))
    );
    let url = added["url"].as_str().unwrap().to_string();
    assert_eq!(config()["additional_endpoints"][&url], json!(["demo-dest-d"]));
    assert!(config()["additional_endpoints"][&team_b].is_array());
    let (_, state) = call(app, "GET", "/api/state", None).await;
    let names = state["intakes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|intake| intake["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["primary", "team-b", "team-c", "mrf", "dest-d"]);
    // Its intake listens.
    std::net::TcpStream::connect(added["url"].as_str().unwrap().trim_start_matches("http://")).unwrap();
    let (status, second) = call(app, "POST", "/api/destinations", None).await;
    assert_eq!((status, &second["label"]), (StatusCode::OK, &json!("Destination E")));

    // Removed: out of the file at once; the intake answers until ADP retires the recipient, which, with no ADP
    // listing it, it already has.
    let (status, removed) = call(app, "DELETE", "/api/destinations/dest-d", None).await;
    assert_eq!((status, &removed["removing"]), (StatusCode::OK, &json!(true)));
    assert!(config()["additional_endpoints"][&url].is_null());
    let (status, again) = call(app, "DELETE", "/api/destinations/dest-d", None).await;
    assert!(
        status == StatusCode::CONFLICT || status == StatusCode::NOT_FOUND,
        "{} {}",
        status,
        again
    );
    // A status ADP sent once it applied the file lists no recipient for it.
    app.control
        .record_status(Surface::Available(json!({ "recipients": [] })), unix_millis() + 2_000);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while app.destinations.find("dest-d").is_some() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(app.destinations.find("dest-d").is_none());
    let (status, body) = call(app, "DELETE", "/api/destinations/primary", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("Only additional endpoints"),
        "{}",
        body
    );
    let (status, _) = call(app, "DELETE", "/api/destinations/nowhere", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Native settings: the filterlist and the failover signal, written as given.
    let (status, native) = call(
        app,
        "POST",
        "/api/native",
        Some(json!({"metric_filterlist": [" demo.catalog.views ", ""], "failover_metrics": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        native,
        json!({"metric_filterlist": ["demo.catalog.views"], "failover_metrics": true})
    );
    assert_eq!(config()["metric_filterlist"], json!(["demo.catalog.views"]));
    assert_eq!(config()["multi_region_failover"]["failover_metrics"], true);
    let (_, native) = call(app, "POST", "/api/native", Some(json!({"metric_filterlist": []}))).await;
    assert_eq!(native["failover_metrics"], true);
    assert!(config()["metric_filterlist"].is_null());
    // The destination added before is still there.
    assert!(config()["additional_endpoints"][second["url"].as_str().unwrap()].is_array());
}

/// Returns the source of the default program with both of its windows set to `window`, with its base stated, since
/// the app these tests compile through has no export to give it back.
fn width_source(window: &str) -> String {
    let (_, export) = crate::preview::tests::fixture();
    let base = format!("policy metrics {{\n  base \"{}\"\n\n", export.program_hash());
    default_program_source(&export, &RecipientLabels::default())
        .replacen("policy metrics {\n", &base, 1)
        .replace(
            "window: config(\"dogstatsd.aggregation.window_duration_seconds\")",
            &format!("window: {}", window),
        )
}

/// Returns the default program with both of its windows set to `window`, compiled.
fn width_artifact(window: &str) -> Value {
    use crate::{compile::compile, preview::CompileContext};

    let (plan, export) = crate::preview::tests::fixture();
    let source = width_source(window);
    let context = CompileContext {
        export: &export,
        default_program: Some(&plan),
        active: None,
        others: &[],
        revision: 1,
        labels: &RecipientLabels::default(),
        in_effect: None,
    };
    let (_, compiled) = compile(&source, Some(&context));
    compiled.unwrap().document
}

#[tokio::test]
async fn the_canary_check_aligns_to_the_widths_an_artifact_serves_and_reports_its_range_by_bucket() {
    let harness = harness().await;
    let app = &harness.app;
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["canary_width_secs"], 1);
    assert_eq!(state["lanes"]["on"], false);

    // An artifact with 30-second windows, served by the stub.
    app.channel
        .publish(width_artifact("30s"), None, None, unix_millis())
        .unwrap();
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["canary_width_secs"], 30);
    let (from_s, to_s) = (
        state["canary"]["from_s"].as_u64().unwrap(),
        state["canary"]["to_s"].as_u64().unwrap(),
    );
    assert_eq!((from_s % 30, to_s - from_s), (0, 30));

    // Revoked through the API, its buckets stay in the history the check reads: the alignment stays, at every later
    // look, until the history no longer holds them.
    let (status, body) = call(app, "POST", "/api/revoke", None).await;
    assert_eq!((status, &body["revoked"]), (StatusCode::OK, &json!(true)));
    let revoked_ms = unix_millis();
    for _ in 0..2 {
        let (_, state) = call(app, "GET", "/api/state", None).await;
        assert_eq!(state["canary_width_secs"], 30);
    }

    // Served and revoked with no look at the state in between, an artifact's width still counts from its revoke on.
    app.channel
        .publish(width_artifact("20s"), None, None, unix_millis())
        .unwrap();
    let (status, _) = call(app, "POST", "/api/revoke", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["canary_width_secs"], 60);
    assert_eq!(
        app.canary_width(revoked_ms + CANARY_HISTORY_SECS * 1000 - 1_000),
        Some(60)
    );
    assert_eq!(
        app.canary_width(revoked_ms + CANARY_HISTORY_SECS * 1000 + 1_000),
        Some(1)
    );

    // A range ending seven seconds ago is answered, with what was sent each second and received for each bucket.
    let now_s = unix_millis() / 1000;
    let path = format!("/api/canary?from_s={}&to_s={}", now_s - 20, now_s - 7);
    let (status, range) = call(app, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{}", range);
    assert_eq!(range["sent_by_second"], json!([]));
    assert_eq!(range["received_by_timestamp"], json!([]));
    assert_eq!(range["sent"], 0);
    let path = format!("/api/canary?from_s={}&to_s={}", now_s - 20, now_s - 5);
    let (status, _) = call(app, "GET", &path, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn artifacts_that_cannot_set_the_canarys_width_leave_its_alignment_alone() {
    let harness = harness().await;
    let app = &harness.app;

    // An artifact of a scope ADP does not declare, with literal windows: ADP refuses it, so its windows never take
    // effect.
    let mut other = width_artifact("10s");
    other["scope"] = json!("other_scope");
    let served = app.channel.publish(other, None, None, unix_millis()).unwrap();
    assert_eq!(app.channel.configs()[0].scope, "other_scope");
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["canary_width_secs"], 1);
    assert!(app.channel.revoke(None, unix_millis()).unwrap());

    // An artifact ADP rejected: its width counts only until the rejection is known.
    let rejected = app
        .channel
        .publish(width_artifact("30s"), None, None, unix_millis())
        .unwrap();
    assert!(rejected.revision > served.revision);
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["canary_width_secs"], 30);
    let status = json!({"config_states": [
        {"product": "policy", "id": "demo", "revision": rejected.revision,
         "state": "rejected", "error": "the artifact does not validate"}
    ]});
    app.control.record_status(Surface::Available(status), unix_millis());
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["canary_width_secs"], 1);
    assert!(app.channel.revoke(None, unix_millis()).unwrap());

    // Windows of 7 and 60 seconds share a bucket boundary every 420 seconds, beyond the history: the check waits, and
    // says why, until the older width leaves. Published through the API, one replacing the other with no look at the
    // state in between, each width counts.
    for window in ["7s", "60s"] {
        let source = width_source(window);
        let (status, compiled) = call(app, "POST", "/api/compile", Some(json!({ "source": source }))).await;
        assert_eq!(
            (status, &compiled["ok"]),
            (StatusCode::OK, &json!(true)),
            "{}",
            compiled
        );
        let hash = compiled["artifact_hash"].clone();
        let (status, published) = call(app, "POST", "/api/publish", Some(json!({ "artifact_hash": hash }))).await;
        assert_eq!(status, StatusCode::OK, "{}", published);
    }
    let replaced_ms = unix_millis();
    let (_, state) = call(app, "GET", "/api/state", None).await;
    assert_eq!(state["canary_width_secs"], Value::Null);
    assert_eq!(state["canary"]["state"], "waiting");
    assert!(state["canary"]["reason"]
        .as_str()
        .unwrap()
        .contains("7 and 60 s with the configured 1 s"));
    assert_eq!(
        app.canary_width(replaced_ms + CANARY_HISTORY_SECS * 1000 + 1_000),
        Some(60)
    );
}

#[test]
fn adp_the_demo_launched_is_up_from_ready_while_its_process_runs_whatever_the_scrapes() {
    let launch = |phase: Phase, ready_ms: Option<u64>| LaunchView {
        phase,
        pid: None,
        started_ms: Some(500),
        ready_ms,
        exit: None,
        command: String::new(),
        log_path: String::new(),
        console_log_path: String::new(),
    };
    let telemetry = |up_since_ms: Option<u64>| TelemetrySnapshot {
        state: if up_since_ms.is_some() { "ok" } else { "unreachable" },
        last_scrape_ms: None,
        last_success_ms: None,
        up_since_ms,
        error: None,
        skipped_lines: 0,
        plan: Vec::new(),
        nodes: Vec::new(),
        components: Vec::new(),
    };
    // Launched and ready: up from ready, through a failed scrape and after it.
    assert_eq!(
        up_since_ms(&launch(Phase::Ready, Some(1_000)), &telemetry(None)),
        Some(1_000)
    );
    assert_eq!(
        up_since_ms(&launch(Phase::Ready, Some(1_000)), &telemetry(Some(9_000))),
        Some(1_000)
    );
    // Started separately: up while it answers, since the scrapes began succeeding.
    assert_eq!(
        up_since_ms(&launch(Phase::External, None), &telemetry(Some(9_000))),
        Some(9_000)
    );
    assert_eq!(up_since_ms(&launch(Phase::External, None), &telemetry(None)), None);
    // Not ready yet, or exited: not up.
    assert_eq!(
        up_since_ms(&launch(Phase::Starting, None), &telemetry(Some(9_000))),
        None
    );
    assert_eq!(
        up_since_ms(&launch(Phase::Exited, Some(1_000)), &telemetry(Some(9_000))),
        None
    );
}
