//! Checks that direct connections, HTTPS proxies, and CONNECT destinations all verify server certificates through the
//! `saluki-tls` client configuration, which rejects distrusted certificate authorities (RUSTSEC-2026-0382).
//!
//! A server that presents `dv-g3b-emudhra.pem` must fail the handshake with `CertDistrusted`. The server doesn't have
//! the private key of that leaf and signs with an unrelated key, but the client checks the certificate chain first, so
//! the chain decides the result.

use std::{env, sync::Arc, time::Duration};

use bytes::Bytes;
use http::{Request, Uri};
use http_body_util::Empty;
use hyper_http_proxy::{Intercept, Proxy};
use rustls::{
    crypto::CryptoProvider,
    pki_types::{pem::PemObject as _, CertificateDer},
    sign::{CertifiedKey, SingleCertAndKey},
    ServerConfig,
};
use saluki_io::net::client::http::{HttpClient, HttpProtocol};
use saluki_tls::{initialize_default_crypto_provider, load_platform_root_certificates, test_util::SelfSignedCert};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;

const AFFECTED_CHAIN: &[u8] = include_bytes!("../../saluki-tls/tests/data/distrusted/dv-g3b-emudhra.pem");
const AFFECTED_ROOTS: &str = include_str!("../../saluki-tls/tests/data/distrusted/roots.pem");
const DEADLINE: Duration = Duration::from_secs(5);

/// Builds a server configuration that presents either the self-signed certificate or the affected chain. Both sign
/// with the private key of the self-signed certificate.
fn server_config(cert: &SelfSignedCert, affected: bool) -> Arc<ServerConfig> {
    let certificates = if affected {
        CertificateDer::pem_slice_iter(AFFECTED_CHAIN)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    } else {
        cert.cert_chain()
    };
    // `with_single_cert` rejects a key that doesn't match the leaf, so build the certified key directly.
    let signing_key = CryptoProvider::get_default()
        .expect("crypto provider is installed")
        .key_provider
        .load_private_key(cert.private_key())
        .unwrap();
    let certified_key = CertifiedKey::new(certificates, signing_key);
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key))),
    )
}

async fn read_headers(stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        headers.push(stream.read_u8().await.expect("complete HTTP headers"));
        assert!(headers.len() < 8192);
    }
    headers
}

async fn serve_origin(config: Arc<ServerConfig>) -> (Uri, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        if let Ok(mut tls) = TlsAcceptor::from(config).accept(stream).await {
            read_headers(&mut tls).await;
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        }
    });
    (format!("https://localhost:{port}/").parse().unwrap(), task)
}

async fn relay(mut stream: impl AsyncRead + AsyncWrite + Unpin, origin_port: u16) {
    let headers = read_headers(&mut stream).await;
    assert!(headers.starts_with(b"CONNECT "));
    let mut origin = TcpStream::connect(("127.0.0.1", origin_port)).await.unwrap();
    stream
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .unwrap();
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut origin).await;
}

async fn serve_proxy(origin_port: u16, config: Option<Arc<ServerConfig>>) -> (Uri, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let scheme = if config.is_some() { "https" } else { "http" };
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        if let Some(config) = config {
            if let Ok(tls) = TlsAcceptor::from(config).accept(stream).await {
                relay(tls, origin_port).await;
            }
        } else {
            relay(stream, origin_port).await;
        }
    });
    (format!("{scheme}://localhost:{port}").parse().unwrap(), task)
}

#[test]
fn all_http_tls_paths_enforce_distrust() {
    // This integration-test process contains one test. Configure roots before starting any async tasks.
    initialize_default_crypto_provider().unwrap();
    let cert = SelfSignedCert::localhost();
    let directory = tempfile::tempdir().unwrap();
    let root_path = directory.path().join("roots.pem");
    // Trust the self-signed certificate for the control cases, and the root that issues the affected chain.
    std::fs::write(&root_path, format!("{}\n{AFFECTED_ROOTS}", cert.cert_pem())).unwrap();
    env::set_var("SSL_CERT_FILE", root_path);
    env::remove_var("SSL_CERT_DIR");
    load_platform_root_certificates().unwrap();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for (proxy_scheme, affected_origin, affected_proxy) in [
                (None, false, false),
                (None, true, false),
                (Some("http"), false, false),
                (Some("http"), true, false),
                (Some("https"), false, false),
                (Some("https"), true, false),
                (Some("https"), false, true),
            ] {
                let (origin, origin_task) = serve_origin(server_config(&cert, affected_origin)).await;
                let mut builder = HttpClient::builder().with_http_protocol(HttpProtocol::Http1);
                let mut proxy_task = None;
                if let Some(scheme) = proxy_scheme {
                    let config = (scheme == "https").then(|| server_config(&cert, affected_proxy));
                    let (proxy, task) = serve_proxy(origin.port_u16().unwrap(), config).await;
                    builder = builder.with_proxies(vec![Proxy::new(Intercept::All, proxy)]);
                    proxy_task = Some(task);
                }
                let mut client = builder.build().unwrap();
                let request = Request::get(origin).body(Empty::<Bytes>::new()).unwrap();
                let result = timeout(DEADLINE, client.send(request))
                    .await
                    .expect("request completed");
                if affected_origin || affected_proxy {
                    let error = result.expect_err("distrusted certificate must fail TLS");
                    assert!(
                        format!("{error:#}").contains("CertDistrusted"),
                        "expected explicit certificate distrust through {proxy_scheme:?}, got {error:#}"
                    );
                } else {
                    assert_eq!(result.expect("control handshake succeeds").status(), 200);
                }
                origin_task.abort();
                if let Some(task) = proxy_task {
                    task.abort();
                }
            }
        });
}
