//! HTTP requests from the runner host.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{body::Incoming, client::conn::http1, Request, Response};
use hyper_util::rt::TokioIo;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::TlsConnector;
use tokio_util::task::AbortOnDropHandle;

use crate::{endpoint::ResolvedAddress, handle::TlsVerification};

/// Distinguishes invalid HTTP requests from connection failures.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HttpError {
    /// The request cannot be built, so retrying cannot help.
    Request(String),
    /// Connecting, the handshake or the exchange failed.
    Connection(String),
}

/// Returns the status of a plain HTTP GET as soon as the response headers arrive.
pub(crate) async fn get_status(address: &ResolvedAddress, path: &str) -> Result<u16, HttpError> {
    let (response, _connection) = request(address, path, TlsVerification::Verify).await?;
    Ok(response.status().as_u16())
}

/// Returns the status and complete body of an HTTP GET.
pub(crate) async fn get(
    address: &ResolvedAddress, path: &str, verification: TlsVerification,
) -> Result<(u16, Bytes), HttpError> {
    let (response, _connection) = request(address, path, verification).await?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|error| HttpError::Connection(error.to_string()))?
        .to_bytes();
    Ok((status, body))
}

async fn request(
    address: &ResolvedAddress, path: &str, verification: TlsVerification,
) -> Result<(Response<Incoming>, AbortOnDropHandle<Result<(), hyper::Error>>), HttpError> {
    let tls = address.url.starts_with("https://");
    match (tls, verification) {
        (true, TlsVerification::Verify) => {
            return Err(HttpError::Request(
                "verifying an HTTPS endpoint's certificate is not supported".into(),
            ));
        }
        (false, TlsVerification::Skip) => {
            return Err(HttpError::Request(
                "skipping certificate verification requires an HTTPS endpoint".into(),
            ));
        }
        _ => {}
    }
    let host = address
        .host
        .as_deref()
        .ok_or_else(|| HttpError::Request("HTTP address has no host".into()))?;
    let port = address
        .port
        .ok_or_else(|| HttpError::Request("HTTP address has no port".into()))?;
    let request = Request::get(path)
        .header("Host", format!("{host}:{port}"))
        .body(Empty::<Bytes>::new())
        .map_err(|error| HttpError::Request(error.to_string()))?;
    if tls {
        let provider = CryptoProvider::get_default()
            .cloned()
            .ok_or_else(|| HttpError::Request("no default TLS crypto provider is installed".into()))?;
        let verifier = SkipVerification {
            schemes: provider.signature_verification_algorithms.supported_schemes(),
        };
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| HttpError::Request(error.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let server_name =
            ServerName::try_from(host.to_owned()).map_err(|error| HttpError::Request(error.to_string()))?;
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|error| HttpError::Connection(error.to_string()))?;
        let stream = TlsConnector::from(Arc::new(config))
            .connect(server_name, stream)
            .await
            .map_err(|error| HttpError::Connection(error.to_string()))?;
        exchange(stream, request).await
    } else {
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|error| HttpError::Connection(error.to_string()))?;
        exchange(stream, request).await
    }
}

async fn exchange(
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static, request: Request<Empty<Bytes>>,
) -> Result<(Response<Incoming>, AbortOnDropHandle<Result<(), hyper::Error>>), HttpError> {
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| HttpError::Connection(error.to_string()))?;
    let connection = AbortOnDropHandle::new(tokio::spawn(connection));
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| HttpError::Connection(error.to_string()))?;
    Ok((response, connection))
}

#[derive(Debug)]
struct SkipVerification {
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for SkipVerification {
    fn verify_server_cert(
        &self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _server_name: &ServerName<'_>,
        _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };

    use super::*;
    use crate::endpoint::EndpointKind;

    fn address(port: u16) -> ResolvedAddress {
        ResolvedAddress {
            kind: EndpointKind::Http,
            url: format!("http://127.0.0.1:{port}"),
            host: Some("127.0.0.1".into()),
            port: Some(port),
            path: None,
        }
    }

    #[tokio::test]
    async fn an_https_get_skipping_verification_returns_the_status() {
        #[cfg(not(windows))]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        #[cfg(windows)]
        let _ = rustls_cng_crypto::default_provider().install_default();
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for host in ["127.0.0.1", "localhost"] {
                let certificate = rcgen::generate_simple_self_signed(["other.invalid".into()]).unwrap();
                let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der());
                let mut config =
                    rustls::ServerConfig::builder_with_provider(CryptoProvider::get_default().unwrap().clone())
                        .with_protocol_versions(&[version])
                        .unwrap()
                        .with_no_client_auth()
                        .with_single_cert(vec![certificate.cert.der().clone()], key.into())
                        .unwrap();
                config.alpn_protocols = vec![b"http/1.1".to_vec()];
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let address = ResolvedAddress {
                    url: format!("https://{host}:{port}"),
                    host: Some(host.into()),
                    ..address(port)
                };
                let responder = async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut stream = acceptor.accept(stream).await.unwrap();
                    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"http/1.1".as_slice()));
                    assert_eq!(stream.get_ref().1.server_name(), (host == "localhost").then_some(host));
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                    }
                    assert_eq!(
                        request,
                        format!("GET /metrics HTTP/1.1\r\nhost: {host}:{port}\r\n\r\n").as_bytes(),
                    );
                    stream
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 3\r\n\r\n\x00\xff\n")
                        .await
                        .unwrap();
                    stream.shutdown().await.unwrap();
                };
                timeout(Duration::from_secs(5), async {
                    let (_, result) = tokio::join!(responder, get(&address, "/metrics", TlsVerification::Skip));
                    assert_eq!(result, Ok((405, Bytes::from_static(b"\x00\xff\n"))));
                })
                .await
                .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn an_https_get_without_skipping_verification_is_a_request_error() {
        let address = ResolvedAddress {
            url: "https://127.0.0.1:1".into(),
            ..address(1)
        };
        assert_eq!(
            get(&address, "/metrics", TlsVerification::Verify).await,
            Err(HttpError::Request(
                "verifying an HTTPS endpoint's certificate is not supported".into(),
            )),
        );
    }

    #[tokio::test]
    async fn skipping_verification_on_plain_http_is_a_request_error() {
        assert_eq!(
            get(&address(1), "/metrics", TlsVerification::Skip).await,
            Err(HttpError::Request(
                "skipping certificate verification requires an HTTPS endpoint".into(),
            )),
        );
    }

    #[tokio::test]
    async fn an_http_check_returns_the_status_of_a_get_to_the_path() {
        for status in [200, 503] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    request.push(stream.read_u8().await.unwrap());
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                assert_eq!(
                    String::from_utf8(request).unwrap(),
                    format!("GET /ready?probe=1 HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n")
                );
                // The status check completes while the server withholds the body.
                stream
                    .write_all(format!("HTTP/1.1 {status} Status\r\nContent-Length: 100\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            });
            timeout(Duration::from_secs(5), async {
                assert_eq!(get_status(&address(port), "/ready?probe=1").await, Ok(status));
                server.await.unwrap();
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn an_incomplete_http_body_is_a_connection_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = address(listener.local_addr().unwrap().port());
        let responder = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\none")
                .await
                .unwrap();
        };
        timeout(Duration::from_secs(5), async {
            let (_, result) = tokio::join!(responder, get(&address, "/dump", TlsVerification::Verify));
            assert!(matches!(result, Err(HttpError::Connection(_))), "{result:?}");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_http_check_of_a_closed_port_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = timeout(Duration::from_secs(5), get_status(&address(port), "/ready"))
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error, HttpError::Connection(ref message) if message.to_lowercase().contains("refused")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_invalid_http_request_is_distinct_from_a_connection_failure() {
        for path in ["/a b", "/a\tb"] {
            assert_eq!(
                get_status(&address(1), path).await,
                Err(HttpError::Request("invalid uri character".into()))
            );
        }
        for (address, message) in [
            (
                ResolvedAddress {
                    host: None,
                    ..address(1)
                },
                "HTTP address has no host",
            ),
            (
                ResolvedAddress {
                    port: None,
                    ..address(1)
                },
                "HTTP address has no port",
            ),
        ] {
            assert_eq!(
                get_status(&address, "/ready").await,
                Err(HttpError::Request(message.into()))
            );
        }
    }
}
