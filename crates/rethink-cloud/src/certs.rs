//! Device-facing TLS acceptor (legacy CBC-SHA) plus re-exports of in-process PKI.

use crate::pki::{self, Certificate};
use crate::sni::CertificateIssuer;
use anyhow::{Context, Result};
use openssl::pkey::PKey;
use openssl::ssl::SslAcceptor;
use openssl::x509::X509;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::sync::Arc;

pub use pki::{load_custom_root, load_or_create, root_certificate_pem, sign_device_csr};

#[derive(Clone)]
pub struct Ca {
    pub key_pem: String,
    pub cert_pem: String,
}

impl From<Certificate> for Ca {
    fn from(c: Certificate) -> Self {
        Self {
            key_pem: c.key_pem,
            cert_pem: c.cert_pem,
        }
    }
}

/// Build a rustls ServerConfig from the CA PEMs (server = CA for rethink).
///
/// Prefer [`device_ssl_acceptor`] for appliance-facing HTTPS/MQTTS — many CLIP
/// modules (RTK_RTL8711am) only offer legacy CBC-SHA suites that rustls rejects.
#[allow(dead_code)] // kept for non-device TLS uses / tests
pub fn server_config(ca: &Ca) -> Result<Arc<ServerConfig>> {
    let mut cert_reader = std::io::Cursor::new(ca.cert_pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .context("parse cert pem")?;
    let mut key_reader = std::io::Cursor::new(ca.key_pem.as_bytes());
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("parse key pem")?
        .context("no private key in pem")?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build server config")?;
    Ok(Arc::new(config))
}

/// OpenSSL acceptor matching Node `deviceTlsOptions` (PR #131 / `util/device_tls.ts`):
/// - min TLS 1.0
/// - `DEFAULT:@SECLEVEL=0` so ECDHE-RSA-AES128-SHA / AES128-SHA256 etc. work
/// - honor server cipher order (modern suites preferred when the client offers them)
///
/// Use for **all** listeners that speak to appliance Wi‑Fi modules (ThinQ2 HTTPS
/// `/route`, MQTTS, ThinQ1 TLS). Without this, RTK_RTL8711am ClientHellos fail
/// with handshake_failure before any HTTP is logged.
pub fn device_ssl_acceptor(issuer: &Arc<CertificateIssuer>) -> Result<Arc<openssl::ssl::SslAcceptor>> {
    use openssl::ssl::{NameType, SslMethod, SslOptions, SslVerifyMode, SslVersion};

    let mut builder =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).context("SslAcceptor")?;

    // mozilla_intermediate disables TLS1.0/1.1 and old ciphers — reverse that for devices.
    builder.clear_options(SslOptions::NO_TLSV1 | SslOptions::NO_TLSV1_1);
    builder
        .set_min_proto_version(Some(SslVersion::TLS1))
        .context("set min TLS1")?;
    // Allow SHA1-CBC suites rejected at OpenSSL 3 default security level.
    builder.set_security_level(0);
    builder
        .set_cipher_list("DEFAULT:@SECLEVEL=0")
        .context("set_cipher_list DEFAULT:@SECLEVEL=0")?;
    // Prefer modern suites when both sides support them (Node honorCipherOrder).
    builder.set_options(SslOptions::CIPHER_SERVER_PREFERENCE);

    let (key_pem, cert_pem) = issuer.select(issuer.default_hostname());
    install_pem(&mut builder, &key_pem, &cert_pem)?;

    let issuer = issuer.clone();
    builder.set_servername_callback(move |ssl, _alert| {
        let name = ssl.servername(NameType::HOST_NAME).unwrap_or("");
        let (key_pem, cert_pem) = issuer.select(name);
        if let (Ok(cert), Ok(key)) = (
            X509::from_pem(cert_pem.as_bytes()),
            PKey::private_key_from_pem(key_pem.as_bytes()),
        ) {
            let _ = ssl.set_certificate(&cert);
            let _ = ssl.set_private_key(&key);
        }
        Ok(())
    });

    builder.set_verify(SslVerifyMode::NONE);
    Ok(Arc::new(builder.build()))
}

fn install_pem(
    builder: &mut openssl::ssl::SslAcceptorBuilder,
    key_pem: &str,
    cert_pem: &str,
) -> Result<()> {
    let cert = X509::from_pem(cert_pem.as_bytes()).context("default certificate")?;
    let key = PKey::private_key_from_pem(key_pem.as_bytes()).context("default key")?;
    builder.set_certificate(&cert).context("set certificate")?;
    builder.set_private_key(&key).context("set private key")?;
    builder.check_private_key().context("check private key")?;
    Ok(())
}

/// Accept a TCP stream with the device legacy SSL profile.
pub async fn accept_device_tls(
    acceptor: &openssl::ssl::SslAcceptor,
    stream: tokio::net::TcpStream,
) -> Result<tokio_openssl::SslStream<tokio::net::TcpStream>> {
    use openssl::ssl::Ssl;
    use std::pin::Pin;

    let ssl = Ssl::new(acceptor.context()).context("Ssl::new")?;
    let mut tls = tokio_openssl::SslStream::new(ssl, stream).context("SslStream::new")?;
    Pin::new(&mut tls).accept().await.context("TLS accept")?;
    Ok(tls)
}

/// Sign a device CSR with the CA. Serial is `0x64`, matching upstream.
pub fn sign_csr(ca: &Ca, csr_pem: &str) -> Result<String> {
    let issuer = pki::load_issuer(&pki::Certificate {
        key_pem: ca.key_pem.clone(),
        cert_pem: ca.cert_pem.clone(),
    })?;
    pki::sign_device_csr(csr_pem, &issuer)
}

#[allow(dead_code)] // helper for rustls TLS wiring / tests
pub fn load_private_key_der(ca: &Ca) -> Result<PrivateKeyDer<'static>> {
    let mut key_reader = std::io::Cursor::new(ca.key_pem.as_bytes());
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .context("no private key")?;
    Ok(key)
}

#[allow(dead_code)] // helper for rustls TLS wiring / tests
pub fn load_cert_der(ca: &Ca) -> Result<Vec<CertificateDer<'static>>> {
    let mut cert_reader = std::io::Cursor::new(ca.cert_pem.as_bytes());
    Ok(rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?)
}

// Silence unused import if PrivatePkcs8KeyDer not used
#[allow(dead_code)]
fn _pkcs8(key: PrivatePkcs8KeyDer<'static>) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(key)
}
