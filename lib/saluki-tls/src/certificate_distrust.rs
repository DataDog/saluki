//! Rejects compromised public keys in presented server certificates and configured trust anchors.
//!
//! This check is deliberately conservative: a blocked certificate anywhere in the presented chain rejects
//! the connection, even if path building could ignore it or find an alternative valid path.

use std::sync::Arc;

use rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        WebPkiServerVerifier,
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, TrustAnchor, UnixTime},
    server::ParsedCertificate,
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, RootCertStore, SignatureScheme,
};
use saluki_error::{generic_error, GenericError};

// These fixed, complete DER SPKIs have a two-byte SEQUENCE header followed by 118 bytes of contents.
// See data/distrusted/README.md for provenance and encoding.
const DISTRUSTED_SPKIS: &[&[u8; 120]] = &[
    include_bytes!("data/distrusted/EMDVTLSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMDVTLSCAG3C.spki.der"),
    include_bytes!("data/distrusted/EMEVTLSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMEVTLSCAG3C.spki.der"),
    include_bytes!("data/distrusted/EMOVTLSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMOVTLSCAG3C.spki.der"),
    include_bytes!("data/distrusted/EMOVCSCAG3B.spki.der"),
    include_bytes!("data/distrusted/EMOVCSCAG3C.spki.der"),
];

#[derive(Debug)]
pub(super) struct DistrustServerCertVerifier {
    inner: Arc<WebPkiServerVerifier>,
}

impl DistrustServerCertVerifier {
    pub(super) fn new(mut roots: Arc<RootCertStore>, provider: Arc<CryptoProvider>) -> Result<Self, GenericError> {
        let is_allowed = |anchor: &TrustAnchor<'_>| {
            !DISTRUSTED_SPKIS
                .iter()
                .any(|spki| anchor.subject_public_key_info.as_ref() == &spki[2..])
        };
        if roots.roots.iter().any(|anchor| !is_allowed(anchor)) {
            // A server need not send its trust anchor, so inspecting the presented chain alone is insufficient.
            Arc::make_mut(&mut roots).roots.retain(is_allowed);
        }

        let inner = WebPkiServerVerifier::builder_with_provider(roots, provider)
            .build()
            .map_err(|error| {
                generic_error!("Failed to build TLS verifier after applying certificate distrust: {error}")
            })?;
        Ok(Self { inner })
    }
}

impl ServerCertVerifier for DistrustServerCertVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], server_name: &ServerName<'_>,
        ocsp_response: &[u8], now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        for cert in std::iter::once(end_entity).chain(intermediates) {
            let spki = ParsedCertificate::try_from(cert)?.subject_public_key_info();
            if DISTRUSTED_SPKIS
                .iter()
                .any(|blocked| spki.as_ref() == blocked.as_slice())
            {
                return Err(Error::InvalidCertificate(
                    CertificateError::ApplicationVerificationFailure,
                ));
            }
        }

        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}
