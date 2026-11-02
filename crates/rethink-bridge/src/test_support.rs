//! Local stand-ins for LG's servers in the connection tests. Everything listens on 127.0.0.1.

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::{
    BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
};
use openssl::x509::{X509Builder, X509NameBuilder, X509};
use rethink_util::length_prefixed_frame;
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

pub const WAIT: Duration = Duration::from_secs(5);

/// Poll `cond` until it holds or [`WAIT`] runs out.
pub async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A test CA and the leaves it signs, as PEM.
pub struct TestPki {
    pub ca_pem: String,
    ca_cert: X509,
    ca_key: PKey<Private>,
}

pub struct Leaf {
    pub cert_pem: String,
    /// PKCS#8, like the key pairing generates.
    pub key_pem: String,
}

fn ec_key() -> PKey<Private> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap()
}

fn certificate(
    cn: &str,
    key: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    dns_names: &[&str],
) -> X509 {
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", cn).unwrap();
    let name = name.build();
    let mut b = X509Builder::new().unwrap();
    b.set_version(2).unwrap();
    let mut serial = BigNum::new().unwrap();
    serial.rand(64, MsbOption::MAYBE_ZERO, false).unwrap();
    b.set_serial_number(&serial.to_asn1_integer().unwrap())
        .unwrap();
    b.set_subject_name(&name).unwrap();
    match issuer {
        Some((ca, _)) => b.set_issuer_name(ca.subject_name()).unwrap(),
        None => b.set_issuer_name(&name).unwrap(),
    }
    b.set_pubkey(key).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    b.set_not_before(&Asn1Time::from_unix(now - 86_400).unwrap())
        .unwrap();
    b.set_not_after(&Asn1Time::from_unix(now + 30 * 86_400).unwrap())
        .unwrap();
    match issuer {
        None => {
            b.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
                .unwrap();
            b.append_extension(
                KeyUsage::new()
                    .critical()
                    .key_cert_sign()
                    .crl_sign()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        }
        Some((ca, _)) => {
            let mut san = SubjectAlternativeName::new();
            for dns in dns_names {
                san.dns(dns);
            }
            san.ip("127.0.0.1");
            let san = san.build(&b.x509v3_context(Some(ca), None)).unwrap();
            b.append_extension(san).unwrap();
            b.append_extension(
                ExtendedKeyUsage::new()
                    .server_auth()
                    .client_auth()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        }
    }
    b.sign(
        issuer.map(|(_, k)| k).unwrap_or(key),
        MessageDigest::sha256(),
    )
    .unwrap();
    b.build()
}

impl TestPki {
    pub fn new() -> Self {
        let ca_key = ec_key();
        let ca_cert = certificate("rethink test CA", &ca_key, None, &[]);
        Self {
            ca_pem: String::from_utf8(ca_cert.to_pem().unwrap()).unwrap(),
            ca_cert,
            ca_key,
        }
    }

    /// A leaf for `dns_names` and 127.0.0.1, usable by servers and clients.
    pub fn leaf(&self, dns_names: &[&str]) -> Leaf {
        let key = ec_key();
        let cert = certificate(
            dns_names.first().copied().unwrap_or("leaf"),
            &key,
            Some((&self.ca_cert, &self.ca_key)),
            dns_names,
        );
        Leaf {
            cert_pem: String::from_utf8(cert.to_pem().unwrap()).unwrap(),
            key_pem: String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap(),
        }
    }

    /// Server config presenting `leaf`; with `require_client_cert`, clients need a leaf of
    /// this CA.
    pub fn server_config(&self, leaf: &Leaf, require_client_cert: bool) -> Arc<ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut leaf.cert_pem.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap();
        let key = rustls_pemfile::private_key(&mut leaf.key_pem.as_bytes())
            .unwrap()
            .unwrap();
        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = if require_client_cert {
            let mut roots = RootCertStore::empty();
            for cert in rustls_pemfile::certs(&mut self.ca_pem.as_bytes()) {
                roots.add(cert.unwrap()).unwrap();
            }
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .unwrap();
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        Arc::new(builder.with_single_cert(certs, key).unwrap())
    }
}

/// How [`HttpStub`] answers a request.
pub enum Reply {
    /// `200 OK` with this body.
    Ok(String),
    /// Read the request and never answer.
    Hang,
    /// Close the connection without an answer.
    Close,
}

/// HTTP/1.1 stand-in, one request per connection. Counts the connections it accepted and
/// those the client closed.
pub struct HttpStub {
    pub url: String,
    pub accepted: Arc<AtomicUsize>,
    pub closed: Arc<AtomicUsize>,
}

impl HttpStub {
    /// Answers `200 OK` with an empty body, or with `hang` never answers.
    pub async fn start(hang: bool) -> Self {
        Self::serve(move |_| {
            if hang {
                Reply::Hang
            } else {
                Reply::Ok(String::new())
            }
        })
        .await
    }

    /// Answers the `n`th request (from 0) with `reply(n)`.
    pub async fn serve(reply: impl Fn(usize) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let (acc, clo) = (accepted.clone(), closed.clone());
        let reply = Arc::new(reply);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let n = acc.fetch_add(1, Ordering::SeqCst);
                let (clo, reply) = (clo.clone(), reply.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => {
                                clo.fetch_add(1, Ordering::SeqCst);
                                return;
                            }
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    match reply(n) {
                        Reply::Ok(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = sock.write_all(head.as_bytes()).await;
                            let _ = sock.write_all(body.as_bytes()).await;
                        }
                        Reply::Hang => {}
                        Reply::Close => {
                            clo.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                    }
                    while matches!(sock.read(&mut chunk).await, Ok(n) if n > 0) {}
                    clo.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            url,
            accepted,
            closed,
        }
    }
}

/// TLS listener handing each accepted connection, and the SNI it asked for, to the test.
pub struct TlsStub {
    pub port: u16,
    pub accepted: mpsc::UnboundedReceiver<(TlsStream<TcpStream>, Option<String>)>,
}

impl TlsStub {
    pub async fn start(config: Arc<ServerConfig>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, accepted) = mpsc::unbounded_channel();
        let acceptor = TlsAcceptor::from(config);
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(sock).await {
                        let sni = tls.get_ref().1.server_name().map(str::to_string);
                        let _ = tx.send((tls, sni));
                    }
                });
            }
        });
        Self { port, accepted }
    }

    pub async fn next(&mut self) -> (TlsStream<TcpStream>, Option<String>) {
        tokio::time::timeout(WAIT, self.accepted.recv())
            .await
            .expect("timed out waiting for a TLS connection")
            .unwrap()
    }
}

/// One ThinQ1 frame (length-prefixed JSON); `None` once the peer has closed or failed.
pub async fn read_t1_frame<S: AsyncRead + Unpin>(s: &mut S) -> Option<serde_json::Value> {
    let read = async {
        let mut len = [0u8; 4];
        s.read_exact(&mut len).await.ok()?;
        let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
        s.read_exact(&mut payload).await.ok()?;
        serde_json::from_slice(&payload).ok()
    };
    tokio::time::timeout(WAIT, read)
        .await
        .expect("timed out waiting for a ThinQ1 frame or the end of the link")
}

/// Read and discard until the peer closes; panics if it does not within [`WAIT`].
pub async fn expect_closed<S: AsyncRead + Unpin>(s: &mut S, what: &str) {
    let mut buf = [0u8; 4096];
    let drain = async { while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {} };
    tokio::time::timeout(WAIT, drain)
        .await
        .unwrap_or_else(|_| panic!("{what}: the connection stayed open"));
}

pub async fn write_t1_frame<S: AsyncWrite + Unpin>(s: &mut S, json: &serde_json::Value) {
    let _ = s
        .write_all(&length_prefixed_frame::make(json.to_string().as_bytes()))
        .await;
    let _ = s.flush().await;
}
