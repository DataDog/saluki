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
use crate::programmable_metrics::{
    installer::status::{LastDrain, RecipientEntry, RecipientEntryState},
    test_support::{configuration, programmable_metrics, ConfigCell},
};

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

/// An export bundle, written by hand.
fn bundle() -> ExportBundle {
    let artifact: PolicyArtifact = serde_json::from_value(json!({
        "ir_version": 1,
        "nodes": [{"id": "metrics_policy_0", "op": "emit", "params": {"output": "dd_metrics_encode"}}],
        "edges": [{"from": "input.dogstatsd", "to": "metrics_policy_0.in"}]
    }))
    .expect("the artifact is a policy artifact");
    ExportBundle {
        export_version: EXPORT_VERSION,
        artifact,
        ir_versions: IrVersions::supported(),
        catalog: MemoryCatalog::new(),
        labels: BTreeMap::from([(
            "dd_metrics_encode".parse::<OutputId>().unwrap(),
            "Main metrics output".to_string(),
        )]),
        annotations: Default::default(),
        form: Default::default(),
        omitted: Default::default(),
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
    let mut body: Value = serde_json::from_str(&body).unwrap();
    // The boot generation is active, and the default program runs: `dd_out`'s primary recipient is live from boot,
    // and no recipient retired yet.
    assert_eq!(body["active_generation"], 1);
    assert_eq!(body["active_artifact"], Value::Null);
    assert_eq!(body["live_generations"], 1);
    assert_eq!(body["config_states"], json!([]));
    assert_eq!(
        body["recipients"],
        json!([{
            "host": "dd_out",
            "id": "dd_out/primary/0/https://app.datadoghq.com",
            "slot": 0,
            "incarnation": 1,
            "state": "live",
        }])
    );
    // Every other member is the apply states', as a poll reports them.
    body.as_object_mut().unwrap().remove("recipients");
    assert_eq!(body, serde_json::to_value(&status.apply_states).unwrap());
    assert_eq!(
        serde_json::from_value::<ApplyStates>(body).unwrap(),
        status.apply_states
    );
}

#[tokio::test]
async fn the_status_route_lists_each_retired_recipient_incarnation_with_its_drain_report() {
    let mut status = booted_status().await;
    status.recipients = vec![RecipientEntry {
        host: "dd_out".to_string(),
        id: "dd_out/additional/0/http://127.0.0.1:17105".to_string(),
        slot: 3,
        incarnation: 2,
        state: RecipientEntryState::Retired,
        draining_since_ms: None,
        last_drain: Some(LastDrain {
            delivered: 0,
            persisted: 0,
            dropped: 7,
            deadline_reached: true,
            retired_ms: 1_791_726_956_032,
        }),
    }];
    let (code, _, body) = get(status, "/programmable-metrics/status").await;
    assert_eq!(code, StatusCode::OK);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body["recipients"],
        json!([{
            "host": "dd_out",
            "id": "dd_out/additional/0/http://127.0.0.1:17105",
            "slot": 3,
            "incarnation": 2,
            "state": "retired",
            "last_drain": {
                "delivered": 0,
                "persisted": 0,
                "dropped": 7,
                "deadline_reached": true,
                "retired_ms": 1_791_726_956_032u64,
            },
        }])
    );
}

#[tokio::test]
async fn the_resources_route_serves_the_hosts_outputs_and_recipients_with_credentials_by_reference() {
    let status = booted_status().await;
    let (code, content_type, body) = get(status, "/programmable-metrics/resources").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let body: Value = serde_json::from_str(&body).unwrap();
    // Nothing is counted without a verified memory limit.
    assert!(body.get("declared").is_none() && body.get("missing_credentials").is_none());
    assert_eq!(body["budget"], json!({"granted": null, "used": 0}));
    assert_eq!(
        body["hosts"],
        json!([
            {
                "host": "dd_out", "encoder": "dd_metrics_encode",
                // The main output's lease runs from boot; the topology was not built, so no host reports its table.
                "leases": [{"lease": 0, "output": "dd_metrics_encode", "state": "live"}],
                "withdrawn": [], "conflicts": [],
            },
            {
                "host": "mrf_dd_out", "encoder": "mrf_metrics_encode",
                "leases": [], "withdrawn": [], "conflicts": [],
            },
        ])
    );
    let main = body["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["id"] == "dd_metrics_encode")
        .expect("the main output is listed");
    assert_eq!(main["label"], "Main metrics output");
    assert_eq!(main["host"], "dd_out");
    assert_eq!(
        main["recipients"],
        json!(["dd_out/primary/0/https://app.datadoghq.com"])
    );
    assert_eq!(
        body["recipients"],
        json!([{
            "id": "dd_out/primary/0/https://app.datadoghq.com",
            "host": "dd_out",
            "endpoint": "https://app.datadoghq.com",
            "credential": "native:api_key",
            "classes": "all",
            "delivery": "native",
        }])
    );
    // The test configuration's API key appears nowhere.
    assert!(!body.to_string().contains("lowering-primary-key"));
}

#[tokio::test]
async fn the_export_route_serves_the_default_programs_export_in_the_form_the_query_names() {
    let mut status = booted_status().await;
    // The installer exports the default program it lowered, whole: the metrics path after the sources, aggregation
    // included, and no source.
    let exported = Arc::clone(status.export.as_ref().expect("the default program exports"));
    assert!(exported.artifact.nodes.iter().any(|node| node.id.as_str() == "dsd_agg"));
    assert!(!exported.artifact.nodes.iter().any(|node| node.op.as_str() == "source"));
    assert!(exported
        .artifact
        .edges
        .iter()
        .any(|edge| edge.from.to_string() == "input.dogstatsd"));

    // The route serves the published export, detached by default.
    for uri in [
        "/programmable-metrics/export",
        "/programmable-metrics/export?form=detached",
    ] {
        let (code, content_type, body) = get(status.clone(), uri).await;
        assert_eq!((code, content_type), (StatusCode::OK, json_type()), "{}", uri);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body, serde_json::to_value(&*exported).unwrap(), "{}", uri);
        assert_eq!(body["form"], "detached", "{}", uri);
        assert_eq!(body["labels"]["dd_metrics_encode"], "Main metrics output");
    }
    // The status reports the default program's core hash: at boot, the active plan's.
    assert_eq!(
        status.apply_states.status.default_program_hash,
        status.installer.active.as_ref().map(|active| active.plan_hash)
    );
    let bound = Arc::clone(status.bound_export.as_ref().expect("the default program exports"));
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/export?form=bound").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body, serde_json::to_value(&*bound).unwrap());
    assert_eq!(body["form"], "bound");

    // Whatever the status publishes, as it publishes it.
    status.export = Ok(Arc::new(bundle()));
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/export").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body, serde_json::to_value(bundle()).unwrap());
    assert_eq!(body["export_version"], 2);
    assert!(body["artifact"].get("scope").is_none() && body["artifact"].get("base").is_none());

    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/export?form=whole").await;
    assert_eq!(
        (code, content_type, body.as_str()),
        (
            StatusCode::BAD_REQUEST,
            text_type(),
            "The `form` query parameter is `detached`, the default, or `bound`, not `whole`."
        )
    );

    // A default program the installer could not export says why.
    status.export = Err("the plan has no canonical form".to_string());
    let (code, content_type, body) = get(status, "/programmable-metrics/export").await;
    assert_eq!(
        (code, content_type, body.as_str()),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            text_type(),
            "The default program cannot be exported: the plan has no canonical form"
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
    // With the resolved values of any fact or signal it binds: the plan the boot generation runs, hash for hash.
    let record = status.installer.active.as_ref().unwrap();
    assert_eq!(served.hash().unwrap(), record.plan_hash);

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
async fn the_default_program_route_serves_the_plan_artifacts_are_composed_with() {
    let status = booted_status().await;
    let (code, content_type, body) = get(status.clone(), "/programmable-metrics/default-program").await;
    assert_eq!((code, content_type), (StatusCode::OK, json_type()));
    let served = Plan::from_value(serde_json::from_str(&body).unwrap()).expect("the body is a plan");
    assert_eq!(served, *status.default_program);
    // It is the program the export was taken from: exported with the export's catalog and labels, it is the export, so
    // a client can tell that an export and a default program it fetched separately belong together.
    let export = status.export.as_ref().expect("the default program exports");
    let registry = saluki_metric_plan::operators::OperatorRegistry::native();
    let again = saluki_metric_plan::export::export_as(
        &served,
        &export.catalog,
        &export.labels,
        saluki_metric_plan::export::ExportForm::Detached,
        &registry,
    )
    .unwrap();
    assert_eq!(again.artifact, export.artifact);
    // It writes the resolved values of any fact or signal it binds, as composition needs them, and elaborated with the
    // export's catalog it is the core plan, whose hash the status reports: at boot, the plan the active generation
    // runs.
    let core = saluki_metric_plan::elaborate::elaborate_default(&served, &export.catalog, &registry);
    assert_eq!(
        Some(core.hash().unwrap()),
        status.apply_states.status.default_program_hash
    );
    assert_eq!(core.plan.nodes, status.active.as_ref().unwrap().plan.nodes);
    assert_eq!(core.plan.edges, status.active.as_ref().unwrap().plan.edges);

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
    let version = format!("explain_version {}", saluki_metric_plan::explain::EXPLAIN_VERSION);
    assert!(body.starts_with(&version), "{}", body);

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
    assert_eq!(body, serde_json::to_value(StatusDocument::of(&status)).unwrap());

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(PATIENCE, server).await;
}
