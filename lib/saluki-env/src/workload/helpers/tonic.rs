use std::{path::Path, str::FromStr as _, sync::Arc};

use hyper::client::HttpConnector;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use snafu::{ResultExt as _, Snafu};
use tokio_rustls::rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    version::TLS13,
    ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
};
use tonic::{
    metadata::{Ascii, MetadataValue},
    service::Interceptor,
    Request, Status,
};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum InterceptorError {
    #[snafu(display("Failed to read token from file '{}': {}", file_path, source))]
    FailedToRead { file_path: String, source: std::io::Error },

    #[snafu(display("Invalid bearer token: must only container visible ASCII characters (32-127)"))]
    InvalidToken,
}

#[derive(Clone)]
pub struct BearerAuthInterceptor {
    bearer_token: MetadataValue<Ascii>,
}

impl BearerAuthInterceptor {
    pub async fn from_file<P>(file_path: P) -> Result<Self, InterceptorError>
    where
        P: AsRef<Path>,
    {
        let file_path = file_path.as_ref();
        let raw_bearer_token = tokio::fs::read_to_string(file_path).await.context(FailedToRead {
            file_path: file_path.to_string_lossy().to_string(),
        })?;

        let raw_bearer_token = format!("bearer {}", raw_bearer_token);
        match MetadataValue::<Ascii>::from_str(&raw_bearer_token) {
            Ok(bearer_token) => Ok(Self { bearer_token }),
            Err(_) => Err(InterceptorError::InvalidToken),
        }
    }
}

impl Interceptor for BearerAuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.bearer_token.clone());
        Ok(request)
    }
}

pub fn build_self_signed_https_connector() -> HttpsConnector<HttpConnector> {
    let tls_client_config = ClientConfig::builder_with_protocol_versions(&[&TLS13])
        .dangerous()
        .with_custom_certificate_verifier(NoopServerCertificateVerifier::new())
        .with_no_client_auth();

    let mut http_connector = HttpConnector::new();
    http_connector.enforce_http(false);

    HttpsConnectorBuilder::new()
        .with_tls_config(tls_client_config)
        .https_only()
        .enable_http2()
        .wrap_connector(http_connector)
}

#[derive(Debug)]
struct NoopServerCertificateVerifier;

impl NoopServerCertificateVerifier {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl ServerCertVerifier for NoopServerCertificateVerifier {
    fn verify_server_cert(
        &self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}
