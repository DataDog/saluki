//! The control surface's routes: through its handler, over a real installer's published status, and over the
//! privileged API with mutual TLS.

use std::{collections::BTreeMap, sync::Mutex, time::Duration};

use datadog_agent_commons::ipc::tls::{build_ipc_client_ipc_tls_config, build_ipc_server_tls_config};
use http::Request;
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Bytes;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use saluki_app::api::APIBuilder;
use saluki_core::{
    accounting::ComponentRegistry,
    runtime::{
        state::{DataspaceUpdate, Identifier, IdentifierFilter},
        Supervisor,
    },
};
use saluki_io::net::{client::http::HttpClient, BoundListenAddress, ListenAddress};
use saluki_metric_plan::{
    channel::ApplyStates,
    export::{ExportBundle, IrVersions, EXPORT_VERSION},
    ir::{OutputId, Plan, PolicyArtifact},
    validate::MemoryCatalog,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tower::ServiceExt as _;

use super::*;
use crate::programmable_metrics::test_support::{configuration, programmable_metrics, ConfigCell};

/// The longest a test waits for the privileged API.
const PATIENCE: Duration = Duration::from_secs(10);

/// Returns the status the installer of the base test configuration publishes before it runs.
async fn booted_status() -> PlanStatus {
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let metrics = programmable_metrics(&config, cell.live(), &ComponentRegistry::default())
        .expect("a metrics pipeline is required");
    PlanStatus::clone(&metrics.status().load())
}

/// An export bundle of the `post_enrich` scope, written by hand.
fn bundle() -> ExportBundle {
    let artifact: PolicyArtifact = serde_json::from_value(json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": format!("sha256:{}", "ab".repeat(32))},
        "nodes": [{"id": "metrics_policy_0", "op": "emit", "params": {"output": "dd_metrics_encode"}}],
        "edges": [{"from": "input.metrics", "to": "metrics_policy_0.in"}]
    }))
    .expect("the artifact is a policy artifact");
    ExportBundle {
        export_version: EXPORT_VERSION,
        scope: "post_enrich".parse().unwrap(),
        artifact,
        ir_versions: IrVersions::supported(),
        catalog: MemoryCatalog::new(),
        labels: BTreeMap::from([(
            "dd_metrics_encode".parse::<OutputId>().unwrap(),
            "Main metrics output".to_string(),
        )]),
    }
}

/// Sends `GET uri` to the control surface's routes over `status`, and returns the status, content type, and body.
async fn get(status: PlanStatus, uri: &str) -> (StatusCode, Option<String>, String) {
    let handler = ControlSurfaceAPIHandler::new(Arc::new(ArcSwap::from_pointee(status)));
    let request = Request::builder().uri(uri).body(Empty::<Bytes>::new()).unwrap();
    let response = handler
        .generate_routes()
        .with_state(handler.generate_initial_state())
        .oneshot(request)
        .await
        .unwrap();
    let code = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .map(|value| value.to_str().unwrap().to_string());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (code, content_type, String::from_utf8(body.to_vec()).unwrap())
}

fn json_type() -> Option<String> {
    Some("application/json".to_string())
}

fn text_type() -> Option<String> {
    Some("text/plain; charset=utf-8".to_string())
}

#[tokio::test]
async fn the_status_route_serves_the_status_summary_and_acknowledgments_as_the_stub_protocol_writes_them() {
    let status = booted_status().await;
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/status").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body, serde_json::to_value(&status.apply_states).unwrap());
    // The boot generation is active, and the default program runs.
    assert_eq!(body["active_generation"], 1);
    assert_eq!(body["active_artifact"], Value::Null);
    assert_eq!(body["live_generations"], 1);
    assert_eq!(body["config_states"], json!([]));
    assert_eq!(
        serde_json::from_value::<ApplyStates>(body).unwrap(),
        status.apply_states
    );
}

#[tokio::test]
async fn the_export_route_serves_the_export_of_the_scope_named_by_the_query() {
    let mut status = booted_status().await;
    let post_enrich: ScopeName = "post_enrich".parse().unwrap();
    // The installer exports every scope of the default program it lowered: `post_enrich`.
    assert_eq!(status.exports.keys().collect::<Vec<_>>(), [&post_enrich]);

    // The route serves the published export, whose artifact records the default program's base program hash.
    let exported = Arc::clone(status.exports[&post_enrich].as_ref().expect("the scope exports"));
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/export?scope=post_enrich").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body, serde_json::to_value(&*exported).unwrap());
    assert_eq!(
        status.apply_states.status.default_program_hash,
        Some(exported.artifact.base.program_hash)
    );
    assert_eq!(body["artifact"]["scope"], "post_enrich");
    assert_eq!(body["labels"]["dd_metrics_encode"], "Main metrics output");

    // Whatever the status publishes, as it publishes it.
    status.exports.insert(post_enrich.clone(), Ok(Arc::new(bundle())));

    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/export?scope=post_enrich").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body, serde_json::to_value(bundle()).unwrap());
    assert_eq!(body["export_version"], 1);
    assert_eq!(body["labels"]["dd_metrics_encode"], "Main metrics output");

    let refused = [
        (
            "/programmable-metrics/export",
            StatusCode::BAD_REQUEST,
            "The `scope` query parameter is required: name the scope of the default program to export \
             (`post_enrich`).",
        ),
        (
            "/programmable-metrics/export?scope=pre_aggregation",
            StatusCode::NOT_FOUND,
            "The default program declares no scope `pre_aggregation`. The scopes it declares are `post_enrich`.",
        ),
        (
            "/programmable-metrics/export?scope=Not%20A%20Scope",
            StatusCode::NOT_FOUND,
            "The default program declares no scope `Not A Scope`. The scopes it declares are `post_enrich`.",
        ),
    ];
    for (uri, expected_code, expected_body) in refused {
        let (code, content_type, body) = get(status.clone(), uri).await;
        assert_eq!(
            (code, content_type, body.as_str()),
            (expected_code, text_type(), expected_body),
            "{}",
            uri
        );
    }

    // A scope the installer could not export says why.
    status
        .exports
        .insert(post_enrich, Err("the plan has no canonical form".to_string()));
    let (code, content_type, body) = get(status, "/programmable-metrics/export?scope=post_enrich").await;
    assert_eq!(
        (code, content_type, body.as_str()),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            text_type(),
            "Scope `post_enrich` of the default program cannot be exported: the plan has no canonical form"
        )
    );
}

#[tokio::test]
async fn the_plan_route_serves_the_active_composed_plan_with_its_resolved_values() {
    let status = booted_status().await;
    let active = status.active.clone().expect("the boot generation is active");
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/plan").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let served = Plan::from_value(serde_json::from_str(&body).unwrap()).expect("the body is a plan");
    assert_eq!(served, *active.plan);
    // Resolved values are included: the plan the boot generation runs, hash for hash.
    let record = status.installer.active.as_ref().unwrap();
    assert_eq!(
        served
            .hash(saluki_metric_plan::canonical::ResolvedValues::Include)
            .unwrap(),
        record.plan_hash
    );
    assert!(body.contains("\"resolved\""), "{}", body);

    let mut idle = status;
    idle.active = None;
    let (code, content_type, body) = get(idle, "/programmable-metrics/plan").await;
    assert_eq!(
        (code, content_type, body.as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            text_type(),
            "No generation of the metrics plan is active yet."
        )
    );
}

#[tokio::test]
async fn the_default_program_route_serves_the_plan_artifacts_are_composed_into() {
    let status = booted_status().await;
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/default-program").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let served = Plan::from_value(serde_json::from_str(&body).unwrap()).expect("the body is a plan");
    assert_eq!(served, *status.default_program);
    // It is the program the export was taken from: its base is the default program hash the status reports, so a
    // client can tell that an export and a default program it fetched separately belong together.
    let post_enrich: ScopeName = "post_enrich".parse().unwrap();
    assert_eq!(
        Some(
            saluki_metric_plan::export::base(&served, &post_enrich)
                .unwrap()
                .program_hash
        ),
        status.apply_states.status.default_program_hash
    );
    // With resolved values, as composition needs them: at boot, it is the plan the active generation runs.
    assert!(body.contains("\"resolved\""), "{}", body);
    assert_eq!(served, *status.active.as_ref().unwrap().plan);

    // It does not depend on a generation being active, or on what the active generation runs.
    let mut idle = status.clone();
    idle.active = None;
    let (code, _, body) = get(idle, "/programmable-metrics/default-program").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        Plan::from_value(serde_json::from_str(&body).unwrap()).unwrap(),
        *status.default_program
    );
}

#[tokio::test]
async fn the_explain_route_serves_the_active_plans_explain_document_as_json_or_text() {
    let status = booted_status().await;
    let active = status.active.clone().expect("the boot generation is active");
    // The document describes the active plan as it runs in the frame: it has the physical sections, and the rewrites
    // compiling the active generation considered, as the core recorded them.
    assert!(active.explain.physical.is_some());
    assert!(active.explain.rewrites.is_some());
    assert_eq!(
        active.explain.plan.hash,
        Some(status.installer.active.as_ref().unwrap().plan_hash)
    );

    for uri in [
        "/programmable-metrics/explain",
        "/programmable-metrics/explain?format=json",
    ] {
        let (code, content_type, body) = get(status.clone(), uri).await;
        assert_eq!((code, content_type), (StatusCode::OK, json_type()), "{}", uri);
        assert_eq!(body, active.explain.to_json().unwrap(), "{}", uri);
    }
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/explain?format=text").await;
    assert_eq!((code, content_type), (StatusCode::OK, text_type()));
    assert_eq!(body, active.explain.to_text());
    assert!(body.starts_with("explain_version 1"), "{}", body);

    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/explain?format=yaml").await;
    assert_eq!(
        (code, content_type, body.as_str()),
        (
            StatusCode::BAD_REQUEST,
            text_type(),
            "Unknown explain format `yaml`: the formats are `json`, the default, and `text`."
        )
    );

    let mut idle = status;
    idle.active = None;
    let (code, _, _) = get(idle, "/programmable-metrics/explain?format=text").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
}

/// Hands the supervision tree's dataspace back to the test, which learns from it where the API listens.
struct DataspaceCapture {
    dataspace_tx: Mutex<Option<oneshot::Sender<DataspaceRegistry>>>,
}

#[async_trait]
impl Supervisable for DataspaceCapture {
    fn name(&self) -> &str {
        "dataspace-capture"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let dataspace = DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;
        if let Some(dataspace_tx) = self.dataspace_tx.lock().unwrap().take() {
            let _ = dataspace_tx.send(dataspace);
        }
        Ok(Box::pin(async move {
            process_shutdown.await;
            Ok(())
        }))
    }
}

#[tokio::test]
async fn the_routes_are_served_on_the_privileged_api_to_a_client_with_the_ipc_certificate() {
    let _ = saluki_tls::initialize_default_crypto_provider();
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_owned()]).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cert_path = directory.path().join("ipc_cert.pem");
    std::fs::write(&cert_path, format!("{}{}", cert.pem(), signing_key.serialize_pem())).unwrap();

    let status = booted_status().await;
    let worker = ControlSurfaceWorker::new(Arc::new(ArcSwap::from_pointee(status.clone())));
    assert_eq!(worker.name(), "programmable-metrics-api");
    let (dataspace_tx, dataspace_rx) = oneshot::channel();
    let mut supervisor = Supervisor::new("control-surface-test").unwrap();
    supervisor.add_worker(
        APIBuilder::new(EndpointType::Privileged, ListenAddress::tcp_loopback(0))
            .with_tls_config(build_ipc_server_tls_config(&cert_path).await.unwrap())
            .into_supervisor(),
    );
    supervisor.add_worker(worker);
    supervisor.add_worker(DataspaceCapture {
        dataspace_tx: Mutex::new(Some(dataspace_tx)),
    });
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

    let dataspace = tokio::time::timeout(PATIENCE, dataspace_rx).await.unwrap().unwrap();
    let mut bound = dataspace
        .subscribe::<BoundListenAddress>(IdentifierFilter::exact(Identifier::named("http-server-privileged-api")));
    let address = match tokio::time::timeout(PATIENCE, bound.recv()).await.unwrap() {
        Some(DataspaceUpdate::Asserted(_, BoundListenAddress::Tcp(address))) => address,
        other => panic!("the privileged API did not bind a TCP address: {:?}", other),
    };

    let mut client = HttpClient::builder()
        .with_client_tls_config(build_ipc_client_ipc_tls_config(&cert_path).await.unwrap())
        .build()
        .unwrap();
    // The route is asserted once the worker runs: wait for it to be served.
    let uri = format!("https://{}/programmable-metrics/status", address);
    let body = tokio::time::timeout(PATIENCE, async {
        loop {
            let request = Request::get(uri.as_str()).body(Empty::<Bytes>::new()).unwrap();
            if let Ok(response) = client.send(request).await {
                if response.status() == StatusCode::OK {
                    return response.into_body().collect().await.unwrap().to_bytes();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the privileged API serves the status route");
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, serde_json::to_value(&status.apply_states).unwrap());

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(PATIENCE, server).await;
}
