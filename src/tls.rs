//! One TLS client setup for every upstream connection.
//!
//! HTTP clients and native WebSockets trust the same roots: the bundled Mozilla set
//! plus the operating system's store, so a locally installed CA works on both paths.
//! The roots are read once; every client shares them and their session cache.

use std::sync::{Arc, LazyLock};

use rustls::{ClientConfig, RootCertStore};

static ROOTS: LazyLock<Arc<RootCertStore>> = LazyLock::new(|| {
    let mut roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let native = rustls_native_certs::load_native_certs();
    let (added, _) = roots.add_parsable_certificates(native.certs);
    if !native.errors.is_empty() {
        tracing::debug!(added, errors = native.errors.len(), "some system certificates could not be loaded");
    }
    Arc::new(roots)
});

/// Shared configs, so TLS sessions resume across clients for the same host.
static HTTP: LazyLock<ClientConfig> = LazyLock::new(|| config(&[b"h2", b"http/1.1"]));
static HTTP1: LazyLock<ClientConfig> = LazyLock::new(|| config(&[b"http/1.1"]));
static WEBSOCKET: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| Arc::new(config(&[b"http/1.1"])));

fn config(alpn: &[&[u8]]) -> ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(ROOTS.clone())
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

/// Load the system roots ahead of the first request (they are read from disk).
pub fn warm() {
    LazyLock::force(&ROOTS);
}

/// For reqwest's `use_preconfigured_tls`, which uses the ALPN list as given.
pub fn http(http1_only: bool) -> ClientConfig {
    if http1_only { HTTP1.clone() } else { HTTP.clone() }
}

/// WebSockets are upgraded over HTTP/1.1, so the server must not pick h2.
pub fn websocket() -> Arc<ClientConfig> {
    WEBSOCKET.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_share_one_root_store_and_negotiate_the_right_protocols() {
        assert!(ROOTS.len() >= webpki_roots::TLS_SERVER_ROOTS.len());
        assert_eq!(http(false).alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
        assert_eq!(http(true).alpn_protocols, [b"http/1.1".to_vec()]);
        assert_eq!(websocket().alpn_protocols, [b"http/1.1".to_vec()]);
    }

    #[test]
    fn reqwest_accepts_the_shared_config() {
        for http1 in [false, true] {
            reqwest::Client::builder().use_preconfigured_tls(http(http1)).build().expect("preconfigured rustls client");
        }
    }
}
