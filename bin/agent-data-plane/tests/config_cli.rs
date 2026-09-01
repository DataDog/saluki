use std::{convert::Infallible, fs, process::Output, sync::Mutex, time::Duration};

use async_trait::async_trait;
use datadog_agent_commons::ipc::tls::build_ipc_server_tls_config;
use http::Response;
use http_body_util::Full;
use hyper::body::Bytes;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use saluki_api::{extract::Request, routing::Router};
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{
    state::{DataspaceRegistry, DataspaceUpdate, Identifier, IdentifierFilter},
    InitializationError, Supervisable, Supervisor, SupervisorFuture,
};
use saluki_error::generic_error;
use saluki_io::net::{server::http::HttpServer, BoundListenAddress, ListenAddress};
use tokio::{process::Command, sync::oneshot, time::timeout};
use tower::util::service_fn;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_TIMEOUT: Duration = Duration::from_secs(5);

/// Server identifier for the stand-in privileged API server.
///
/// `HttpServer` asserts its bound listen address under `http-server-<server ID>`, which is how this test finds out
/// which ephemeral port the server landed on.
const SERVER_ID: &str = "privileged-api";

/// Hands the supervision tree's dataspace registry back to the test.
///
/// The registry only exists inside a running supervision tree, and reading the server's bound address out of it is the
/// only way to learn the address, since the server binds during its own initialization.
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
        let dataspace_tx = self
            .dataspace_tx
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| generic_error!("DataspaceCapture can only be initialized once."))?;
        let _ = dataspace_tx.send(dataspace);

        Ok(Box::pin(async move {
            process_shutdown.await;
            Ok(())
        }))
    }
}

async fn run_config_request(extra_args: &[&str], response_body: &'static str) -> (Output, String) {
    let _ = saluki_tls::initialize_default_crypto_provider();
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_owned()])
        .expect("self-signed localhost certificate should be generated");
    let temp_dir = tempfile::tempdir().expect("temporary test directory should be created");
    let cert_path = temp_dir.path().join("ipc-cert.pem");
    fs::write(&cert_path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
        .expect("certificate and private key should be written");

    let server_tls_config = build_ipc_server_tls_config(&cert_path)
        .await
        .expect("production IPC server TLS config should build");
    let (request_tx, mut request_rx) = tokio::sync::mpsc::channel(1);
    let service = service_fn(move |request: Request| {
        let request_tx = request_tx.clone();
        async move {
            request_tx
                .send(request.uri().path().to_string())
                .await
                .expect("test should receive the request path");
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(response_body.as_bytes()))))
        }
    });

    // Run the server under a supervisor of its own, since that is the only way it can run, and capture the dataspace
    // registry alongside it so that we can find out which port it bound to.
    let (dataspace_tx, dataspace_rx) = oneshot::channel();
    let mut supervisor = Supervisor::new("config-cli-test").expect("test supervisor name should be valid");
    supervisor.add_worker(
        HttpServer::from_listen_address(ListenAddress::tcp_loopback(0))
            .with_routes(Router::new().fallback_service(service))
            .with_tls_config(server_tls_config)
            .with_server_id(SERVER_ID),
    );
    supervisor.add_worker(DataspaceCapture {
        dataspace_tx: Mutex::new(Some(dataspace_tx)),
    });

    let (server_shutdown, server_shutdown_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(async move { supervisor.run_with_shutdown(server_shutdown_rx).await });

    let dataspace = timeout(SERVER_TIMEOUT, dataspace_rx)
        .await
        .expect("dataspace registry should be captured before timeout")
        .expect("dataspace capture worker should send the registry");
    let mut bound_addrs = dataspace.subscribe::<BoundListenAddress>(IdentifierFilter::exact(Identifier::from(
        format!("http-server-{SERVER_ID}"),
    )));
    let listen_addr = match timeout(SERVER_TIMEOUT, bound_addrs.recv()).await {
        Ok(Some(DataspaceUpdate::Asserted(_, addr @ BoundListenAddress::Tcp(_)))) => addr,
        update => panic!("expected a bound TCP address for the privileged API server, got {update:?}"),
    };

    let config_path = temp_dir.path().join("datadog.yaml");
    let config = serde_json::json!({
        "disable_file_logging": true,
        "ipc_cert_file_path": cert_path,
        "data_plane": { "secure_api_listen_address": listen_addr.to_string() },
    });
    fs::write(
        &config_path,
        serde_json::to_vec(&config).expect("test config should serialize"),
    )
    .expect("test config should be written");

    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-data-plane"));
    command
        .arg("-c")
        .arg(config_path)
        .arg("config")
        .arg("--json")
        .args(extra_args)
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("DD_") {
            command.env_remove(name);
        }
    }
    let output = timeout(PROCESS_TIMEOUT, command.output())
        .await
        .expect("agent-data-plane config command should exit before timeout")
        .expect("agent-data-plane config command should launch");
    let request_path = timeout(SERVER_TIMEOUT, request_rx.recv())
        .await
        .expect("config request should arrive before timeout")
        .expect("server should report the config request");

    let _ = server_shutdown.send(());
    timeout(SERVER_TIMEOUT, server_task)
        .await
        .expect("privileged API server should shut down before timeout")
        .expect("privileged API server task should not panic")
        .expect("privileged API server should stop cleanly");
    assert!(
        output.status.success(),
        "config command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    (output, request_path)
}

#[tokio::test]
async fn source_json_config_emits_compact_scrubbed_stdout_over_production_mtls() {
    let (output, request_path) = run_config_request(&[], r#"{"view":"source","password":"source-secret"}"#).await;

    assert_eq!(request_path, "/config");
    assert_eq!(output.stdout, b"{\"password\":\"********\",\"view\":\"source\"}\n");
    assert!(output.stderr.is_empty(), "stderr was not empty");
}

#[tokio::test]
async fn runtime_json_config_selects_the_runtime_route_and_response() {
    let (output, request_path) = run_config_request(&["--runtime"], r#"{"view":"runtime"}"#).await;

    assert_eq!(request_path, "/config/runtime");
    assert_eq!(output.stdout, b"{\"view\":\"runtime\"}\n");
}
