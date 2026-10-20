//! Exercises certificate distrust on direct connections, HTTPS proxies, and CONNECT destinations.

use std::{env, sync::Arc, time::Duration};

use bytes::Bytes;
use http::{Request, Uri};
use http_body_util::Empty;
use hyper_http_proxy::{Intercept, Proxy};
use rustls::{
    pki_types::{pem::PemObject as _, CertificateDer},
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
const DEADLINE: Duration = Duration::from_secs(5);

fn server_config(cert: &SelfSignedCert, blocked_extra: bool) -> Arc<ServerConfig> {
    let mut certificates = cert.cert_chain();
    if blocked_extra {
        let intermediate = CertificateDer::pem_slice_iter(AFFECTED_CHAIN).nth(1).unwrap().unwrap();
        certificates.push(intermediate);
    }
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, cert.private_key())
            .unwrap(),
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
    cert.write_cert_pem(&root_path);
    env::set_var("SSL_CERT_FILE", root_path);
    env::remove_var("SSL_CERT_DIR");
    load_platform_root_certificates().unwrap();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // A blocked extra certificate makes otherwise valid handshakes fail without needing a compromised key.
            for (proxy_scheme, blocked_origin, blocked_proxy) in [
                (None, false, false),
                (None, true, false),
                (Some("http"), false, false),
                (Some("http"), true, false),
                (Some("https"), false, false),
                (Some("https"), true, false),
                (Some("https"), false, true),
            ] {
                let (origin, origin_task) = serve_origin(server_config(&cert, blocked_origin)).await;
                let mut builder = HttpClient::builder().with_http_protocol(HttpProtocol::Http1);
                let mut proxy_task = None;
                if let Some(scheme) = proxy_scheme {
                    let config = (scheme == "https").then(|| server_config(&cert, blocked_proxy));
                    let (proxy, task) = serve_proxy(origin.port_u16().unwrap(), config).await;
                    builder = builder.with_proxies(vec![Proxy::new(Intercept::All, proxy)]);
                    proxy_task = Some(task);
                }
                let mut client = builder.build().unwrap();
                let request = Request::get(origin).body(Empty::<Bytes>::new()).unwrap();
                let result = timeout(DEADLINE, client.send(request))
                    .await
                    .expect("request completed");
                if blocked_origin || blocked_proxy {
                    let error = result.expect_err("blocked certificate must fail TLS");
                    assert!(
                        format!("{error:#}").contains("ApplicationVerificationFailure"),
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
