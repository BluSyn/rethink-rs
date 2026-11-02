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
use rethink_util::sync::Mutex;
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
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

/// A response to `query` in the shape real servers send: a CNAME to "cdns.<name>" followed by
/// `answers` (type, ttl, rdata) for it, with compression pointers. Port of the `response`
/// helper in upstream's tests/bridge/resolver.test.ts.
pub fn dns_response(query: &[u8], answers: &[(u16, u32, Vec<u8>)], rcode: u16) -> Vec<u8> {
    let rr = |rtype: u16, ttl: u32, data: &[u8], name: &[u8]| {
        let mut out = name.to_vec();
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&ttl.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
        out
    };
    let mut msg = query[..12].to_vec();
    msg[2..4].copy_from_slice(&(0x8180 | rcode).to_be_bytes());
    msg[6..8].copy_from_slice(&(answers.len() as u16 + 1).to_be_bytes());
    msg.extend_from_slice(&query[12..]);
    // "cdns." + a pointer to the question name
    let cname_target = [0x04, b'c', b'd', b'n', b's', 0xc0, 0x0c];
    let cname_rdata_offset = query.len() + 2 + 10;
    let pointer_to_cname = [
        0xc0 | (cname_rdata_offset >> 8) as u8,
        (cname_rdata_offset & 0xff) as u8,
    ];
    msg.extend(rr(5, 300, &cname_target, &[0xc0, 0x0c]));
    for (rtype, ttl, data) in answers {
        msg.extend(rr(*rtype, *ttl, data, &pointer_to_cname));
    }
    msg
}

/// The names a DNS stand-in knows: `(name, address, ttl)`.
pub type DnsRecords = Vec<(&'static str, IpAddr, u32)>;

/// Name and type of a query's question.
pub fn dns_question(query: &[u8]) -> (String, u16) {
    let mut labels = Vec::new();
    let mut i = 12;
    while query[i] != 0 {
        let n = query[i] as usize;
        labels.push(String::from_utf8_lossy(&query[i + 1..i + 1 + n]).into_owned());
        i += n + 1;
    }
    (
        labels.join("."),
        u16::from_be_bytes([query[i + 1], query[i + 2]]),
    )
}

/// The records of `records` asked for, or NXDOMAIN for a name it does not know.
pub fn dns_answer(query: &[u8], records: &[(&str, IpAddr, u32)]) -> Vec<u8> {
    let (name, qtype) = dns_question(query);
    let known: Vec<_> = records.iter().filter(|(n, ..)| *n == name).collect();
    let answers: Vec<(u16, u32, Vec<u8>)> = known
        .iter()
        .filter_map(|(_, ip, ttl)| match (ip, qtype) {
            (IpAddr::V4(v4), 1) => Some((1, *ttl, v4.octets().to_vec())),
            (IpAddr::V6(v6), 28) => Some((28, *ttl, v6.octets().to_vec())),
            _ => None,
        })
        .collect();
    dns_response(query, &answers, if known.is_empty() { 3 } else { 0 })
}

/// Plain DNS stand-in, UDP and TCP on the same port, answering like [`dns_answer`].
pub struct DnsStub {
    pub addr: SocketAddr,
    udp_queries: Arc<AtomicUsize>,
    tcp_queries: Arc<AtomicUsize>,
}

impl DnsStub {
    pub async fn start(records: DnsRecords) -> Self {
        Self::start_with(records, Duration::ZERO, false).await
    }

    /// Answers after `delay`; with `truncate_udp`, UDP answers only say "truncated".
    pub async fn start_with(records: DnsRecords, delay: Duration, truncate_udp: bool) -> Self {
        let (udp, tcp) = loop {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            if let Ok(tcp) = TcpListener::bind(udp.local_addr().unwrap()).await {
                break (Arc::new(udp), tcp);
            }
        };
        let addr = udp.local_addr().unwrap();
        let records = Arc::new(records);
        let udp_queries = Arc::new(AtomicUsize::new(0));
        let tcp_queries = Arc::new(AtomicUsize::new(0));
        let (count, answers) = (udp_queries.clone(), records.clone());
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, peer)) = udp.recv_from(&mut buf).await {
                count.fetch_add(1, Ordering::SeqCst);
                let (query, udp, answers) = (buf[..n].to_vec(), udp.clone(), answers.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let mut answer = dns_answer(&query, &answers);
                    if truncate_udp {
                        answer.truncate(12);
                        answer[2] |= 0x02;
                        answer[4..].fill(0);
                    }
                    let _ = udp.send_to(&answer, peer).await;
                });
            }
        });
        let (count, answers) = (tcp_queries.clone(), records);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = tcp.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let answers = answers.clone();
                tokio::spawn(async move {
                    let mut len = [0u8; 2];
                    if sock.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let mut query = vec![0u8; u16::from_be_bytes(len) as usize];
                    if sock.read_exact(&mut query).await.is_err() {
                        return;
                    }
                    let answer = dns_answer(&query, &answers);
                    let mut framed = (answer.len() as u16).to_be_bytes().to_vec();
                    framed.extend(answer);
                    let _ = sock.write_all(&framed).await;
                });
            }
        });
        Self {
            addr,
            udp_queries,
            tcp_queries,
        }
    }

    /// As written in `bridge.dns`.
    pub fn entry(&self) -> String {
        self.addr.to_string()
    }

    pub fn queries(&self) -> usize {
        self.udp_queries.load(Ordering::SeqCst)
    }

    pub fn tcp_queries(&self) -> usize {
        self.tcp_queries.load(Ordering::SeqCst)
    }
}

/// RFC 8484 stand-in over plain HTTP: answers `GET …?dns=<base64url query>` like
/// [`dns_answer`], or with `status` alone when that is not 200. Keeps each request's target
/// and accept header.
pub struct DohStub {
    pub url: String,
    pub requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl DohStub {
    pub async fn start(records: DnsRecords, status: u16) -> Self {
        use base64::Engine;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/dns-query", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (log, records) = (requests.clone(), Arc::new(records));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, records) = (log.clone(), records.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(n) if n > 0 => head.extend_from_slice(&chunk[..n]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let target = head.split(' ').nth(1).unwrap_or("").to_string();
                    let accept = head
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case("accept"))
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default();
                    log.lock().push((target.clone(), accept));
                    let answer = url::Url::parse(&format!("http://doh{target}"))
                        .ok()
                        .and_then(|u| {
                            u.query_pairs()
                                .find(|(k, _)| k == "dns")
                                .map(|(_, v)| v.into_owned())
                        })
                        .and_then(|q| {
                            base64::engine::general_purpose::URL_SAFE_NO_PAD
                                .decode(q)
                                .ok()
                        })
                        .map(|query| dns_answer(&query, &records));
                    let resp = match answer {
                        Some(answer) if status == 200 => {
                            let mut resp = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/dns-message\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n",
                                answer.len()
                            )
                            .into_bytes();
                            resp.extend(answer);
                            resp
                        }
                        _ => format!(
                            "HTTP/1.1 {status} Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .into_bytes(),
                    };
                    let _ = sock.write_all(&resp).await;
                });
            }
        });
        Self { url, requests }
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
