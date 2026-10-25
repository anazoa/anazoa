pub mod calls;
pub mod integrity;
pub mod login;
pub mod oneme;
pub mod signaling;

pub use anazoa_config::{FingerprintConfig, ServiceEndpoints};
pub use login::run_login_with_endpoints;

use serde::Deserialize;
use std::sync::OnceLock;

/// Unified ICE/TURN server credentials used for both incoming and outgoing calls.
#[derive(Clone, Debug, Deserialize)]
pub struct TurnServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

/// Russian Trusted Root CA (Минцифры), extracted from the Max APK's
/// `res/raw/rootca_ssl_rsa2022.cer`. Max trusts it alongside the system
/// roots, and some okcdn.ru endpoints (notably the `wte` WebTransport
/// signaling) present chains that only validate against it — it is not in
/// Mozilla's list, so without it those connections fail as UnknownIssuer.
const RUSSIAN_TRUSTED_ROOT_CA: &[u8] = include_bytes!("../certs/russian_trusted_root_ca.der");

/// Root store matching Max's trust set: Mozilla's roots (webpki-roots, since
/// Android has no OS store rustls can read) plus the bundled Russian root.
pub(crate) fn root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
        .add(rustls::pki_types::CertificateDer::from(RUSSIAN_TRUSTED_ROOT_CA))
        .expect("bundled Russian Trusted Root CA parses");
    roots
}

#[derive(Debug)]
pub(crate) struct SkipTlsVerifier;

impl rustls::client::danger::ServerCertVerifier for SkipTlsVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

pub fn ensure_rustls_provider() {
    static RUSTLS_PROVIDER: OnceLock<()> = OnceLock::new();
    let _ = RUSTLS_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn root_store_includes_russian_trusted_root() {
        let roots = super::root_store();
        assert_eq!(roots.len(), webpki_roots::TLS_SERVER_ROOTS.len() + 1);
    }
}
