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
/// does not verify its KE server has no security property left to offer.
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
