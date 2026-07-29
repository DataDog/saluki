use std::{collections::HashMap, sync::Arc, time::Duration};

use airlock::driver::ContainerOs;
use rustls::{server::WebPkiClientVerifier, RootCertStore, ServerConfig};
use saluki_tls::test_util::SelfSignedCert;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use super::{Assertion as _, AssertionContext, LogBuffer, TargetCommand, TlsClientCertificateRequiredAssertion};

enum TlsServerMode {
    RequireClientCertificate,
    ReturnHttpResponse,
    ReturnHttpRedirect(String),
}

fn initialize_crypto_provider() {
    let _ = crate::default_crypto_provider().install_default();
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_some(),
        "default crypto provider should be installed"
    );
}

async fn spawn_tls_server(mode: TlsServerMode) -> (u16, tokio::task::JoinHandle<()>) {
    initialize_crypto_provider();

    let server_cert = SelfSignedCert::localhost();
    let mut server_config = match &mode {
        TlsServerMode::RequireClientCertificate => {
            let trusted_client_cert = SelfSignedCert::new(["trusted-client"]);
            let mut client_roots = RootCertStore::empty();
            client_roots
                .add(
                    trusted_client_cert
                        .cert_chain()
                        .into_iter()
                        .next()
                        .expect("client certificate chain should not be empty"),
                )
                .expect("client certificate should be a valid trust anchor");
            let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
                .build()
                .expect("client certificate verifier should build");
            ServerConfig::builder().with_client_cert_verifier(verifier)
        }
        TlsServerMode::ReturnHttpResponse | TlsServerMode::ReturnHttpRedirect(_) => {
            ServerConfig::builder().with_no_client_auth()
        }
    }
    .with_single_cert(server_cert.cert_chain(), server_cert.private_key())
    .expect("server TLS config should build");
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("TLS server should bind");
    let port = listener.local_addr().expect("TLS server should have an address").port();
    let server_task = tokio::spawn(async move {
        let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("TLS server should receive a connection")
            .expect("TLS server should accept a connection");
        let handshake = timeout(
            Duration::from_secs(5),
            TlsAcceptor::from(Arc::new(server_config)).accept(stream),
        )
        .await
        .expect("TLS handshake should finish");

        match mode {
            TlsServerMode::RequireClientCertificate => {
                assert!(handshake.is_err(), "anonymous TLS handshake should be rejected");
            }
            TlsServerMode::ReturnHttpResponse | TlsServerMode::ReturnHttpRedirect(_) => {
                let mut stream = handshake.expect("anonymous TLS handshake should succeed");
                let mut request = [0; 4096];
                let bytes_read = stream
                    .read(&mut request)
                    .await
                    .expect("server should read HTTP request");
                assert!(bytes_read > 0, "server should receive an HTTP request");
                let response = match mode {
                    TlsServerMode::ReturnHttpResponse => {
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                    }
                    TlsServerMode::ReturnHttpRedirect(location) => format!(
                        "HTTP/1.1 302 Found\r\nlocation: {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        location
                    ),
                    TlsServerMode::RequireClientCertificate => unreachable!(),
                };
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("server should write HTTP response");
            }
        }
    });

    (port, server_task)
}

fn assertion_context(mapped_port: u16, target_os: ContainerOs) -> AssertionContext {
    AssertionContext {
        log_buffer: Arc::new(std::sync::RwLock::new(LogBuffer::default())),
        container_exit_token: CancellationToken::new(),
        cancel_token: CancellationToken::new(),
        port_mappings: HashMap::from([("55101/tcp".to_string(), mapped_port)]),
        container_ip: None,
        target_os: Some(target_os),
        container_name: "tls-assertion-test".to_string(),
        is_host_process: false,
        host_process_exit_code: None,
        docker_container_exit_code: None,
        core_agent_auth_token_path: None,
        adp_cli_command: TargetCommand::new(Vec::new()),
        core_agent_cli_command: TargetCommand::new(Vec::new()),
    }
}

#[test]
fn description_redacts_endpoint_secrets() {
    let assertion = TlsClientCertificateRequiredAssertion::new(
        "https://probe-user:probe-password@localhost:55101/private/config?api_key=query-secret#fragment-secret"
            .to_string(),
        Duration::from_secs(3),
    );

    let description = assertion.description();

    assert!(
        description.contains("https://localhost:55101/private/config"),
        "description should preserve safe endpoint diagnostics: {}",
        description
    );
    for secret in [
        "probe-user",
        "probe-password",
        "api_key",
        "query-secret",
        "fragment-secret",
    ] {
        assert!(
            !description.contains(secret),
            "description must redact endpoint secret '{secret}': {description}"
        );
    }
}

#[tokio::test]
async fn malformed_endpoint_uses_safe_diagnostics() {
    initialize_crypto_provider();
    let malformed_endpoint = "https://probe-user:malformed-password@[invalid-host/private?query-secret#fragment-secret";
    let assertion = TlsClientCertificateRequiredAssertion::new(malformed_endpoint.to_string(), Duration::from_secs(1));

    let description = assertion.description();
    let result = assertion.check(&assertion_context(55101, ContainerOs::Linux)).await;

    assert!(description.contains("<invalid endpoint>"));
    assert!(!result.passed);
    assert!(
        result.message.contains("malformed"),
        "failure should diagnose a malformed endpoint safely: {}",
        result.message
    );
    for secret in [
        malformed_endpoint,
        "probe-user",
        "malformed-password",
        "query-secret",
        "fragment-secret",
    ] {
        assert!(
            !description.contains(secret),
            "description leaked '{secret}': {description}"
        );
        assert!(
            !result.message.contains(secret),
            "failure leaked '{secret}': {}",
            result.message
        );
    }
}

#[tokio::test]
async fn anonymous_client_passes_when_server_requires_a_client_certificate() {
    let (mapped_port, server_task) = spawn_tls_server(TlsServerMode::RequireClientCertificate).await;
    let assertion = TlsClientCertificateRequiredAssertion::new(
        "https://localhost:55101/config".to_string(),
        Duration::from_secs(3),
    );

    let result = assertion
        .check(&assertion_context(mapped_port, ContainerOs::Linux))
        .await;

    assert!(result.passed, "assertion should pass: {}", result.message);
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("TLS server should finish")
        .expect("TLS server task should not panic");
}

#[tokio::test]
async fn anonymous_client_fails_when_server_returns_an_http_response() {
    let (mapped_port, server_task) = spawn_tls_server(TlsServerMode::ReturnHttpResponse).await;
    let assertion = TlsClientCertificateRequiredAssertion::new(
        "https://localhost:55101/config".to_string(),
        Duration::from_secs(3),
    );

    let result = assertion
        .check(&assertion_context(mapped_port, ContainerOs::Linux))
        .await;

    assert!(!result.passed, "an HTTP response must not satisfy the assertion");
    assert!(
        result.message.contains("HTTP status 200"),
        "failure should identify the unexpected HTTP response: {}",
        result.message
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("TLS server should finish")
        .expect("TLS server task should not panic");
}

#[tokio::test]
async fn anonymous_client_does_not_follow_a_cross_origin_redirect_to_an_mtls_endpoint() {
    let (redirect_target_port, mut redirect_target_task) =
        spawn_tls_server(TlsServerMode::RequireClientCertificate).await;
    let (mapped_port, redirect_source_task) = spawn_tls_server(TlsServerMode::ReturnHttpRedirect(format!(
        "https://localhost:{}/private",
        redirect_target_port
    )))
    .await;
    let assertion = TlsClientCertificateRequiredAssertion::new(
        "https://localhost:55101/config".to_string(),
        Duration::from_secs(3),
    );

    let result = assertion
        .check(&assertion_context(mapped_port, ContainerOs::Linux))
        .await;

    timeout(Duration::from_secs(5), redirect_source_task)
        .await
        .expect("redirect source should finish")
        .expect("redirect source task should not panic");
    let redirect_target_was_contacted = timeout(Duration::from_millis(500), &mut redirect_target_task)
        .await
        .is_ok();
    if !redirect_target_was_contacted {
        redirect_target_task.abort();
    }

    assert!(!result.passed, "a redirect response must not satisfy the assertion");
    assert!(
        result.message.contains("HTTP status 302"),
        "failure should identify the redirect response: {}",
        result.message
    );
    assert!(
        !redirect_target_was_contacted,
        "the anonymous probe must not follow the redirect to the mTLS endpoint"
    );
}

#[tokio::test]
async fn request_failure_redacts_endpoint_secrets() {
    initialize_crypto_provider();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("temporary listener should bind");
    let unused_port = listener.local_addr().expect("listener should have an address").port();
    drop(listener);
    let assertion = TlsClientCertificateRequiredAssertion::new(
        "https://probe-user:probe-password@localhost:55101/private/config?api_key=query-secret#fragment-secret"
            .to_string(),
        Duration::from_secs(1),
    );

    let result = assertion
        .check(&assertion_context(unused_port, ContainerOs::Linux))
        .await;

    assert!(!result.passed, "connection refusal must fail the assertion");
    assert!(
        result.message.contains("127.0.0.1") && result.message.contains("/private/config"),
        "failure should preserve safe endpoint diagnostics: {}",
        result.message
    );
    for secret in [
        "probe-user",
        "probe-password",
        "api_key",
        "query-secret",
        "fragment-secret",
    ] {
        assert!(
            !result.message.contains(secret),
            "failure must redact endpoint secret '{secret}': {}",
            result.message
        );
    }
}

#[tokio::test]
async fn refused_connection_is_not_mistaken_for_required_client_authentication() {
    initialize_crypto_provider();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("temporary listener should bind");
    let unused_port = listener.local_addr().expect("listener should have an address").port();
    drop(listener);
    let assertion = TlsClientCertificateRequiredAssertion::new(
        "https://localhost:55101/config".to_string(),
        Duration::from_secs(1),
    );

    let result = assertion
        .check(&assertion_context(unused_port, ContainerOs::Linux))
        .await;

    assert!(!result.passed, "connection refusal must not satisfy the assertion");
    assert!(
        result.message.contains("TLS client-certificate authentication"),
        "failure should explain the required rejection: {}",
        result.message
    );
}

#[tokio::test]
async fn windows_target_reports_that_the_host_side_probe_is_unsupported() {
    let assertion = TlsClientCertificateRequiredAssertion::new(
        "https://localhost:55101/config".to_string(),
        Duration::from_secs(1),
    );

    let result = assertion.check(&assertion_context(55101, ContainerOs::Windows)).await;

    assert!(!result.passed);
    assert!(
        result.message.contains("not supported for Windows container targets"),
        "failure should identify the unsupported target mode: {}",
        result.message
    );
}
