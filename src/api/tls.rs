//! TLS policy for Proxmox connections.
//!
//! Proxmox nodes ship self-signed certificates by default, so besides normal
//! CA verification we support pinning the server certificate's SHA-256
//! fingerprint (trust-on-first-use) and, for labs only, accepting anything.

use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Debug)]
pub struct FingerprintVerifier {
    expected: Option<Vec<u8>>,
    captured: Arc<Mutex<Option<Vec<u8>>>>,
    provider: Arc<CryptoProvider>,
}

impl FingerprintVerifier {
    /// `expected = None` accepts any certificate (insecure / capture mode).
    pub fn new(expected: Option<Vec<u8>>) -> Self {
        Self { expected, captured: Arc::new(Mutex::new(None)), provider: Arc::new(rustls::crypto::ring::default_provider()) }
    }

    pub fn captured(&self) -> Arc<Mutex<Option<Vec<u8>>>> {
        self.captured.clone()
    }
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = Sha256::digest(end_entity.as_ref()).to_vec();
        *self.captured.lock() = Some(fp.clone());
        match &self.expected {
            Some(exp) if *exp != fp => {
                Err(rustls::Error::General(format!("server certificate fingerprint mismatch (got {})", format_fingerprint(&fp))))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn client_config(verifier: Arc<FingerprintVerifier>) -> anyhow::Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(cfg)
}

/// Parses `AA:BB:..`, `aabb..` or `sha256 Fingerprint=AA:BB:..` (openssl output).
pub fn parse_fingerprint(s: &str) -> Option<Vec<u8>> {
    let s = s.rsplit('=').next().unwrap_or(s);
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 64 {
        return None;
    }
    hex::decode(hex).ok()
}

pub fn format_fingerprint(fp: &[u8]) -> String {
    fp.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_formats() {
        let fp = vec![0xABu8; 32];
        let s = format_fingerprint(&fp);
        assert_eq!(parse_fingerprint(&s), Some(fp.clone()));
        assert_eq!(parse_fingerprint(&format!("sha256 Fingerprint={s}")), Some(fp.clone()));
        assert_eq!(parse_fingerprint(&hex::encode(&fp)), Some(fp));
        assert_eq!(parse_fingerprint("AB:CD"), None);
    }
}
