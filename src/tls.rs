//! HTTPS support (the default `tls` feature).

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::crypto::CryptoProvider;
use rustls::ServerConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// TLS configuration for HTTPS support
pub(crate) struct TlsConfig {
    cert_file: PathBuf,
    key_file: PathBuf,
}

impl TlsConfig {
    pub fn new<P: AsRef<Path>>(cert_file: P, key_file: P) -> Self {
        Self {
            cert_file: cert_file.as_ref().to_path_buf(),
            key_file: key_file.as_ref().to_path_buf(),
        }
    }

    fn load_certs(&self) -> Result<Vec<CertificateDer<'static>>, BoxError> {
        let certs = CertificateDer::pem_file_iter(&self.cert_file)?.collect::<Result<Vec<_>, _>>()?;
        if certs.is_empty() {
            return Err(format!("no certificates found in {}", self.cert_file.display()).into());
        }
        Ok(certs)
    }

    fn load_key(&self) -> Result<PrivateKeyDer<'static>, BoxError> {
        Ok(PrivateKeyDer::from_pem_file(&self.key_file)?)
    }

    pub(crate) fn acceptor(&self) -> Result<TlsAcceptor, BoxError> {
        let mut config = ServerConfig::builder_with_provider(crypto_provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(self.load_certs()?, self.load_key()?)?;
        // Offer HTTP/2, falling back to HTTP/1.1.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}

/// The process-wide rustls crypto provider, installing aws-lc-rs if the app
/// hasn't picked one.
///
/// `ServerConfig::builder()` panics when more than one provider is compiled
/// in (for example aws-lc-rs from Axeon and ring from a database driver) and
/// none is installed. Installing aws-lc-rs as the default here also covers
/// other crates in the process that call `builder()`, such as Redis clients.
pub(crate) fn crypto_provider() -> Arc<CryptoProvider> {
    if let Some(provider) = CryptoProvider::get_default() {
        return provider.clone();
    }
    // Losing a race with another thread installing one is fine: use theirs.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}
