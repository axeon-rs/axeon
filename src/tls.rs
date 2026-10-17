//! HTTPS support (the default `tls` feature).

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::fs::File;
use std::io::BufReader as StdBufReader;
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
        let cert_file = File::open(&self.cert_file)?;
        let mut reader = StdBufReader::new(cert_file);
        let certs = rustls_pemfile::certs(&mut reader)
            .filter_map(|result| result.ok())
            .collect();
        Ok(certs)
    }

    fn load_key(&self) -> Result<PrivateKeyDer<'static>, BoxError> {
        let key_file = File::open(&self.key_file)?;
        let mut reader = StdBufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut reader)?
            .ok_or("No private key found")?;
        Ok(key)
    }

    pub(crate) fn acceptor(&self) -> Result<TlsAcceptor, BoxError> {
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(self.load_certs()?, self.load_key()?)?;
        // Offer HTTP/2, falling back to HTTP/1.1.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}
