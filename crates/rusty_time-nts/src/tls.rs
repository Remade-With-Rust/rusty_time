//! The TLS seam: one place that decides which rustls crypto provider we use.
//!
//! rustls's *default* provider is aws-lc-rs, which compiles C — forbidden by
//! the house rule (mission plan §2). Rather than let every call site remember
//! `builder_with_provider`, both config builders live here and no other module
//! constructs a rustls config. If the provider is ever swapped, it changes on
//! one line, and a `builder_with_provider` appearing anywhere else in the
//! workspace is a review finding.

pub use rustls;
pub use rustls_pki_types as pki_types;

use std::sync::Arc;

/// The pure-Rust provider, as a single named decision.
pub fn crypto_provider() -> rustls::crypto::CryptoProvider {
    oxitls_rustcrypto_provider::provider()
}

/// A client config trusting the Mozilla root set, with `alpn` offered.
pub fn client_config(alpn: &[&[u8]]) -> Result<rustls::ClientConfig, rustls::Error> {
    client_config_with_roots(alpn, &[])
}

/// As [`client_config`], plus `extra_roots` (DER certificates) added as trust
/// anchors — a private mesh's own CA, or a pinned self-signed NTS-KE server
/// (chrony calls this `ntstrustedcerts`).
///
/// These are *additional* anchors, never a replacement for verification: there
/// is no "skip validation" path in this crate, deliberately. An NTS client that
/// does not verify its KE server has no security property left to offer. The
/// one relaxation, [`client_config_ignoring_cert_dates`], still verifies all of
/// that; it only judges the validity dates at the chain's own time rather than
/// at a local clock known to be wrong.
pub fn client_config_with_roots(
    alpn: &[&[u8]],
    extra_roots: &[pki_types::CertificateDer<'static>],
) -> Result<rustls::ClientConfig, rustls::Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in extra_roots {
        roots.add(cert.clone())?;
    }

    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(crypto_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

/// A server config presenting `certs`/`key`, with `alpn` advertised.
pub fn server_config(
    certs: Vec<pki_types::CertificateDer<'static>>,
    key: pki_types::PrivateKeyDer<'static>,
    alpn: &[&[u8]],
) -> Result<rustls::ServerConfig, rustls::Error> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(crypto_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

/// As [`client_config_with_roots`], except that certificate validity dates are
/// judged at a moment inside every presented certificate's window instead of
/// at the local clock: chrony's `nocerttimecheck`.
///
/// For one situation only: a machine whose clock is known to be wrong because
/// NTS is about to set it. A router has no battery clock and boots at its
/// image's build date, so months later every KE certificate reads "not yet
/// valid" and an NTS-only router could never set its clock. Everything else is
/// still verified exactly as before: the chain to a trusted root, every
/// signature, the server name, and that the chain's windows overlap at all. A
/// caller must stop using this once the clock has been set; `rtimed sync
/// --nocerttimecheck` uses it only until its first accepted correction.
pub fn client_config_ignoring_cert_dates(
    alpn: &[&[u8]],
    extra_roots: &[pki_types::CertificateDer<'static>],
) -> Result<rustls::ClientConfig, rustls::Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in extra_roots {
        roots.add(cert.clone())?;
    }
    let provider = Arc::new(crypto_provider());
    let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::clone(&provider),
    )
    .build()
    .map_err(|e| rustls::Error::General(e.to_string()))?;

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DatesFromTheChain { inner }))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

/// The standard webpki verifier, asked about the chain at the latest
/// `notBefore` in it rather than at the local clock.
#[derive(Debug)]
struct DatesFromTheChain {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for DatesFromTheChain {
    fn verify_server_cert(
        &self,
        end_entity: &pki_types::CertificateDer<'_>,
        intermediates: &[pki_types::CertificateDer<'_>],
        server_name: &pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // The latest notBefore is a moment every certificate had begun; it is
        // one they all still cover if it precedes the earliest notAfter. If a
        // date cannot be read, or the windows do not overlap, the chain is
        // judged at the local clock as usual and fails or passes on its merits.
        let windows: Option<Vec<(i64, i64)>> = std::iter::once(end_entity)
            .chain(intermediates)
            .map(|c| crate::dates::validity(c.as_ref()))
            .collect();
        let at = windows.and_then(|w| {
            let start = w.iter().map(|&(b, _)| b).max()?;
            let end = w.iter().map(|&(_, a)| a).min()?;
            (start <= end && start >= 0).then_some(start)
        });
        let at = match at {
            Some(secs) => {
                pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(secs as u64))
            }
            None => now,
        };
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, at)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_offers_the_alpn_it_was_given() {
        let c = client_config(&[crate::ALPN]).expect("config");
        assert_eq!(c.alpn_protocols, vec![b"ntske/1".to_vec()]);
    }

    #[test]
    fn provider_supplies_tls13_suites() {
        let p = crypto_provider();
        assert!(
            p.cipher_suites
                .iter()
                .any(|s| s.version().version == rustls::ProtocolVersion::TLSv1_3),
            "provider has no TLS 1.3 suites"
        );
    }
}
