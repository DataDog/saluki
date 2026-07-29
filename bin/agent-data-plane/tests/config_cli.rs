use std::{convert::Infallible, fs, process::Output, time::Duration};

use datadog_agent_commons::ipc::tls::build_ipc_server_tls_config;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::{body::Bytes, service::service_fn};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use saluki_io::net::{listener::ConnectionOrientedListener, server::http::HttpServer, ListenAddress};
use tokio::{process::Command, time::timeout};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_TIMEOUT: Duration = Duration::from_secs(5);

async fn run_config_command(config_path: &std::path::Path, extra_args: &[&str]) -> Output {
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

    timeout(PROCESS_TIMEOUT, command.output())
        .await
        .expect("agent-data-plane config command should exit before timeout")
        .expect("agent-data-plane config command should launch")
}

#[tokio::test]
async fn json_config_views_use_the_expected_mtls_routes_and_emit_only_compact_scrubbed_json() {
    let _ = saluki_tls::initialize_default_crypto_provider();
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(["localhost".to_owned()])
        .expect("self-signed localhost certificate should be generated");
    let temp_dir = tempfile::tempdir().expect("temporary test directory should be created");
    let cert_path = temp_dir.path().join("ipc-cert.pem");
    fs::write(&cert_path, format!("{}{}", cert.pem(), signing_key.serialize_pem()))
        .expect("certificate and private key should be written");

    let listener = ConnectionOrientedListener::from_listen_address(
        ListenAddress::try_from("tcp://127.0.0.1:0").expect("ephemeral TCP address should parse"),
    )
    .await
    .expect("privileged API listener should bind");
    let listen_addr = listener.local_addr().expect("listener should have a local address");
    let server_tls_config = build_ipc_server_tls_config(&cert_path)
        .await
        .expect("production IPC server TLS config should build");
    let (request_tx, mut request_rx) = tokio::sync::mpsc::channel(2);
    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
        let request_tx = request_tx.clone();
        async move {
            let path = request.uri().path().to_string();
            request_tx
                .send(path.clone())
                .await
                .expect("test should receive the request path");
            let (status, body) = match path.as_str() {
                "/config" => (StatusCode::OK, r#"{"view":"source","password":"source-secret"}"#),
                "/config/internal" => (StatusCode::OK, r#"{"view":"runtime","password":"runtime-secret"}"#),
                _ => (StatusCode::NOT_FOUND, r#"{"error":"unexpected path"}"#),
            };
            Ok::<_, Infallible>(
                Response::builder()
                    .status(status)
                    .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
                    .expect("test response should build"),
            )
        }
    });
    let (server_shutdown, error_handle) = HttpServer::from_listener(listener, service)
        .with_tls_config(server_tls_config)
        .listen();

    let config_path = temp_dir.path().join("datadog.yaml");
    let config = serde_json::json!({
        "disable_file_logging": true,
        "ipc_cert_file_path": cert_path,
        "data_plane": {
            "secure_api_listen_address": format!("tcp://{listen_addr}"),
        },
    });
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("test config should serialize"),
    )
    .expect("test config should be written");

    let source_output = run_config_command(&config_path, &[]).await;
    assert!(
        source_output.status.success(),
        "source config command failed: {}",
        String::from_utf8_lossy(&source_output.stderr)
    );
    assert_eq!(
        source_output.stdout,
        b"{\"password\":\"********\",\"view\":\"source\"}\n"
    );
    assert!(source_output.stderr.is_empty(), "source stderr was not empty");
    assert_eq!(
        timeout(SERVER_TIMEOUT, request_rx.recv())
            .await
            .expect("source request should arrive before timeout")
            .expect("server should report source request"),
        "/config"
    );

    let runtime_output = run_config_command(&config_path, &["--runtime"]).await;
    assert!(
        runtime_output.status.success(),
        "runtime config command failed: {}",
        String::from_utf8_lossy(&runtime_output.stderr)
    );
    assert_eq!(
        runtime_output.stdout,
        b"{\"password\":\"********\",\"view\":\"runtime\"}\n"
    );
    assert!(runtime_output.stderr.is_empty(), "runtime stderr was not empty");
    assert_eq!(
        timeout(SERVER_TIMEOUT, request_rx.recv())
            .await
            .expect("runtime request should arrive before timeout")
            .expect("server should report runtime request"),
        "/config/internal"
    );

    timeout(SERVER_TIMEOUT, server_shutdown.shutdown_and_wait())
        .await
        .expect("privileged API server should shut down before timeout");
    let server_error = timeout(SERVER_TIMEOUT, error_handle)
        .await
        .expect("privileged API error handle should resolve before timeout");
    assert!(server_error.is_none(), "privileged API server failed: {server_error:?}");
}
