//! DNS for bridge-initiated connections (port of bridge/resolver.ts).
//!
//! The appliances find rethink through a DNS redirect of the ThinQ hostnames, and the host
//! running rethink usually sits behind the same resolver. Left to the system resolver, the
//! bridge would connect back to rethink instead of the LG cloud, so its upstream connections
//! can resolve names through servers of their own: DNS-over-HTTPS URLs (RFC 8484) or plain DNS
//! server addresses, tried in order until one answers. With none configured, the system
//! resolver is used and connections are made exactly as without this module.

use crate::stop::StopSignal;
use base64::Engine;
use rethink_util::sync::{Mutex, RwLock};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::OnceCell;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Attempts per plain DNS server, like upstream's `Resolver({ tries: 2 })`.
const TRIES: usize = 2;
const MIN_TTL: u32 = 60;
const MAX_TTL: u32 = 3600;
/// How long a proxy tunnel may still carry its client's goodbye once the proxy is stopped.
#[cfg(not(test))]
const TUNNEL_GRACE: Duration = Duration::from_secs(2);
#[cfg(test)]
const TUNNEL_GRACE: Duration = Duration::from_millis(200);

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub address: IpAddr,
    pub ttl: u32,
}

/// Which addresses a lookup wants, like the `family` option of upstream's `lookup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Any,
    V4,
    V6,
}

impl Family {
    fn matches(self, ip: &IpAddr) -> bool {
        match self {
            Self::Any => true,
            Self::V4 => ip.is_ipv4(),
            Self::V6 => ip.is_ipv6(),
        }
    }
}

pub(crate) enum Server {
    Doh {
        url: url::Url,
        client: reqwest::Client,
    },
    Plain(SocketAddr),
}

impl Server {
    /// Any http(s) URL; configuration only takes `https://` ones.
    pub(crate) fn doh(url: url::Url) -> Result<Self, String> {
        // Plain reqwest: the DoH server itself is found through the system resolver.
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self::Doh { url, client })
    }

    /// A DoH URL or a DNS server address (`1.2.3.4`, `1.2.3.4:53`, `::1`, `[::1]:53`).
    fn parse(entry: &str) -> Result<Self, String> {
        if entry.starts_with("https://") {
            let url = url::Url::parse(entry)
                .map_err(|e| format!("Invalid DNS-over-HTTPS URL {entry}: {e}"))?;
            return Self::doh(url);
        }
        dns_server_address(entry).map(Self::Plain).ok_or_else(|| {
            format!("{entry:?} is neither a DNS-over-HTTPS URL nor a DNS server address")
        })
    }

    /// IPv4 first: an IPv6 address is of no use to a host without IPv6 connectivity, and the
    /// callers mostly want a single address.
    async fn query(&self, hostname: &str) -> Result<Vec<Record>, String> {
        let (v4, v6) = tokio::join!(
            self.query_type(hostname, TYPE_A),
            self.query_type(hostname, TYPE_AAAA)
        );
        match (v4, v6) {
            (Err(e), Err(_)) => Err(e),
            (v4, v6) => Ok(v4
                .unwrap_or_default()
                .into_iter()
                .chain(v6.unwrap_or_default())
                .collect()),
        }
    }

    async fn query_type(&self, hostname: &str, qtype: u16) -> Result<Vec<Record>, String> {
        match self {
            Self::Doh { url, client } => doh_query(client, url, hostname, qtype).await,
            Self::Plain(server) => udp_query(*server, hostname, qtype).await,
        }
    }
}

fn dns_server_address(entry: &str) -> Option<SocketAddr> {
    let addr = match entry.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, 53),
        Err(_) => match entry.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(_) => {
                let v6 = entry.strip_prefix('[')?.strip_suffix(']')?;
                SocketAddr::new(v6.parse::<Ipv6Addr>().ok()?.into(), 53)
            }
        },
    };
    match addr.port() {
        0 => Some(SocketAddr::new(addr.ip(), 53)),
        _ => Some(addr),
    }
}

struct NamedServer {
    name: String,
    server: Server,
}

type Pending = Arc<OnceCell<Result<Vec<IpAddr>, String>>>;

pub struct Resolver {
    servers: Vec<NamedServer>,
    cache: Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Resolver {
    /// Servers in the order given. Empty means the system resolver. Rejects entries that are
    /// neither a DoH URL nor a DNS server address, like upstream's `setServers`.
    pub fn new(entries: &[String]) -> Result<Self, String> {
        let servers = entries
            .iter()
            .map(|entry| {
                Ok(NamedServer {
                    name: entry.clone(),
                    server: Server::parse(entry)?,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Self::with_servers(servers))
    }

    fn with_servers(servers: Vec<NamedServer>) -> Self {
        Self {
            servers,
            cache: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_servers(servers: Vec<(&str, Server)>) -> Self {
        Self::with_servers(
            servers
                .into_iter()
                .map(|(name, server)| NamedServer {
                    name: name.to_string(),
                    server,
                })
                .collect(),
        )
    }

    pub fn uses_system_resolver(&self) -> bool {
        self.servers.is_empty()
    }

    /// Addresses of `host` of `family`, IPv4 first when the servers answer. IP literals come
    /// back as they are, without a query.
    pub async fn lookup(&self, host: &str, family: Family) -> io::Result<Vec<IpAddr>> {
        let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
        if let Ok(ip) = literal.unwrap_or(host).parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        let all: Vec<IpAddr> = if self.servers.is_empty() {
            tokio::net::lookup_host((host, 0))
                .await?
                .map(|addr| addr.ip())
                .collect()
        } else {
            self.resolve(host)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?
        };
        let matching: Vec<IpAddr> = all.into_iter().filter(|ip| family.matches(ip)).collect();
        if matching.is_empty() {
            let wanted = match family {
                Family::Any => "",
                Family::V4 => "IPv4 ",
                Family::V6 => "IPv6 ",
            };
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("No {wanted}address for {host}"),
            ));
        }
        Ok(matching)
    }

    /// A TCP connection to `host:port`, trying its addresses in order. Callers that speak TLS
    /// keep `host` as the server name.
    pub async fn connect(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        if self.servers.is_empty() {
            return TcpStream::connect((host, port)).await;
        }
        let mut last_error = None;
        for ip in self.lookup(host, Family::Any).await? {
            match TcpStream::connect((ip, port)).await {
                Ok(stream) => return Ok(stream),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("No address for {host}"))
        }))
    }

    /// A reqwest client builder whose lookups go through this resolver (reqwest's own when it
    /// has no servers).
    pub fn http_client(self: &Arc<Self>) -> reqwest::ClientBuilder {
        let builder = reqwest::Client::builder();
        if self.servers.is_empty() {
            return builder;
        }
        builder.dns_resolver(Arc::new(ReqwestDns(self.clone())))
    }

    async fn resolve(&self, hostname: &str) -> Result<Vec<IpAddr>, String> {
        if let Some((addresses, expires)) = self.cache.lock().get(hostname) {
            if *expires > Instant::now() {
                return Ok(addresses.clone());
            }
        }
        // Connections made together (fetch + MQTT at bridge startup) share one query.
        let query = self
            .pending
            .lock()
            .entry(hostname.to_string())
            .or_default()
            .clone();
        let result = query
            .get_or_init(|| self.query_servers(hostname))
            .await
            .clone();
        let mut pending = self.pending.lock();
        if pending
            .get(hostname)
            .is_some_and(|current| Arc::ptr_eq(current, &query))
        {
            pending.remove(hostname);
        }
        result
    }

    async fn query_servers(&self, hostname: &str) -> Result<Vec<IpAddr>, String> {
        let mut last_error = None;
        for NamedServer { name, server } in &self.servers {
            match server.query(hostname).await {
                // An answer without addresses (NXDOMAIN, no records) is final.
                Ok(records) if records.is_empty() => break,
                Ok(records) => {
                    let addresses: Vec<IpAddr> = records.iter().map(|r| r.address).collect();
                    let expires = Instant::now() + Duration::from_secs(cache_ttl(&records).into());
                    self.cache
                        .lock()
                        .insert(hostname.to_string(), (addresses.clone(), expires));
                    let shown: Vec<String> = addresses.iter().map(|a| a.to_string()).collect();
                    eprintln!(
                        "[bridge] Resolved {hostname} via {name}: {}",
                        shown.join(", ")
                    );
                    return Ok(addresses);
                }
                Err(e) => {
                    eprintln!("[bridge] Resolving {hostname} via {name} failed: {e}");
                    last_error = Some(e);
                }
            }
        }
        Err(match last_error {
            Some(e) => format!("Can't resolve {hostname}: {e}"),
            None => format!("Can't resolve {hostname}"),
        })
    }
}

/// The shortest TTL of an answer, kept between [`MIN_TTL`] and [`MAX_TTL`].
fn cache_ttl(records: &[Record]) -> u32 {
    records
        .iter()
        .map(|r| r.ttl)
        .min()
        .unwrap_or(MIN_TTL)
        .clamp(MIN_TTL, MAX_TTL)
}

static GLOBAL: RwLock<Option<Arc<Resolver>>> = RwLock::new(None);

/// Install the bridge's servers (`bridge.dns`). Empty means the system resolver. An entry
/// that is neither a DoH URL nor a DNS server address is an error, and nothing changes.
pub fn set_servers(entries: &[String]) -> Result<(), String> {
    let resolver = Arc::new(Resolver::new(entries)?);
    *GLOBAL.write() = Some(resolver);
    Ok(())
}

/// The resolver for bridge-initiated connections.
pub fn global() -> Arc<Resolver> {
    if let Some(resolver) = GLOBAL.read().as_ref() {
        return resolver.clone();
    }
    GLOBAL
        .write()
        .get_or_insert_with(|| Arc::new(Resolver::with_servers(Vec::new())))
        .clone()
}

/// A reqwest client builder for a bridge-initiated request: lookups go through [`global`].
pub fn http_client() -> reqwest::ClientBuilder {
    global().http_client()
}

struct ReqwestDns(Arc<Resolver>);

impl reqwest::dns::Resolve for ReqwestDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let resolver = self.0.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = resolver.lookup(&host, Family::Any).await?;
            // reqwest puts the URL's port in.
            let addrs: reqwest::dns::Addrs =
                Box::new(addresses.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// An HTTP CONNECT proxy on 127.0.0.1 for a client that resolves names itself (rumqttc). It
/// tunnels `host:port` only, resolving `host` through `resolver`; TLS stays end to end between
/// the client and the server, so the client's SNI and certificate checks are untouched.
/// Returns the proxy's port. On `stop` it stops accepting, and open tunnels get a moment to
/// carry the client's goodbye before they are dropped.
pub fn connect_proxy(
    resolver: Arc<Resolver>,
    host: &str,
    port: u16,
    stop: StopSignal,
) -> io::Result<u16> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let proxy_port = listener.local_addr()?.port();
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let host = host.to_string();
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                biased;
                _ = stop.stopped() => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((client, _)) = accepted else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let (resolver, host, stop) = (resolver.clone(), host.clone(), stop.clone());
            tokio::spawn(async move {
                let tunnel = tunnel(client, &resolver, &host, port);
                tokio::pin!(tunnel);
                tokio::select! {
                    _ = &mut tunnel => return,
                    _ = stop.stopped() => {}
                }
                let _ = tokio::time::timeout(TUNNEL_GRACE, tunnel).await;
            });
        }
    });
    Ok(proxy_port)
}

async fn tunnel(
    mut client: TcpStream,
    resolver: &Resolver,
    host: &str,
    port: u16,
) -> io::Result<()> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() > 8192 {
            return Ok(());
        }
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..n]);
    };
    if !head.starts_with(format!("CONNECT {host}:{port} ").as_bytes()) {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await?;
        return Ok(());
    }
    let mut upstream = match resolver.connect(host, port).await {
        Ok(upstream) => upstream,
        Err(e) => {
            eprintln!("[bridge] Can't connect to {host}:{port}: {e}");
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await?;
            return Ok(());
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    upstream.write_all(&head[end..]).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn doh_query(
    client: &reqwest::Client,
    url: &url::Url,
    hostname: &str,
    qtype: u16,
) -> Result<Vec<Record>, String> {
    let query = encode_query(hostname, qtype)?;
    let mut target = url.clone();
    let others: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "dns")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    target
        .query_pairs_mut()
        .clear()
        .extend_pairs(others)
        .append_pair(
            "dns",
            &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&query),
        );
    let resp = client
        .get(target)
        .header("accept", "application/dns-message")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    parse_response(&body, qtype)
}

async fn udp_query(server: SocketAddr, hostname: &str, qtype: u16) -> Result<Vec<Record>, String> {
    let mut query = encode_query(hostname, qtype)?;
    let id: u16 = rand::random();
    query[..2].copy_from_slice(&id.to_be_bytes());
    let local: SocketAddr = match server {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(local).await.map_err(|e| e.to_string())?;
    socket.connect(server).await.map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 4096];
    for _ in 0..TRIES {
        socket.send(&query).await.map_err(|e| e.to_string())?;
        let answer = tokio::time::timeout(TIMEOUT, async {
            loop {
                let n = socket.recv(&mut buf).await?;
                // An answer (QR) to this query; anything else is not for us.
                if n >= 12 && buf[..2] == id.to_be_bytes() && buf[2] & 0x80 != 0 {
                    return io::Result::Ok(n);
                }
            }
        })
        .await;
        match answer {
            // Truncated (TC): ask again over TCP.
            Ok(Ok(_)) if buf[2] & 0x02 != 0 => return tcp_query(server, &query, qtype).await,
            Ok(Ok(n)) => return parse_response(&buf[..n], qtype),
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => continue,
        }
    }
    Err("timeout".into())
}

/// For an answer truncated over UDP.
async fn tcp_query(server: SocketAddr, query: &[u8], qtype: u16) -> Result<Vec<Record>, String> {
    let exchange = async {
        let mut stream = TcpStream::connect(server).await?;
        let mut framed = (query.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(query);
        stream.write_all(&framed).await?;
        let mut len = [0u8; 2];
        stream.read_exact(&mut len).await?;
        let mut answer = vec![0u8; u16::from_be_bytes(len) as usize];
        stream.read_exact(&mut answer).await?;
        io::Result::Ok(answer)
    };
    let answer = tokio::time::timeout(TIMEOUT, exchange)
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;
    if answer.len() < 12 || answer[..2] != query[..2] {
        return Err("Mismatched DNS answer".into());
    }
    parse_response(&answer, qtype)
}

/// DNS query message. ID stays 0 and RD is set, as RFC 8484 recommends for DoH.
pub fn encode_query(hostname: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let mut msg = vec![0u8; 12];
    msg[2] = 0x01; // recursion desired
    msg[5] = 1; // one question
    let host = hostname.trim_end_matches('.');
    for label in host.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err(format!("Invalid hostname {hostname}"));
        }
        msg.push(bytes.len() as u8);
        msg.extend_from_slice(bytes);
    }
    msg.push(0);
    msg.extend_from_slice(&qtype.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes()); // class IN
    Ok(msg)
}

fn skip_name(msg: &[u8], mut offset: usize) -> Result<usize, String> {
    loop {
        let length = *msg.get(offset).ok_or("Truncated DNS message")?;
        if length == 0 {
            return Ok(offset + 1);
        }
        if length & 0xc0 == 0xc0 {
            return Ok(offset + 2);
        }
        offset += length as usize + 1;
    }
}

/// Addresses of `qtype` in the answer section. CNAME records the server followed on the way
/// come along in the answer section and are skipped.
pub fn parse_response(msg: &[u8], qtype: u16) -> Result<Vec<Record>, String> {
    if msg.len() < 12 {
        return Err("Truncated DNS message".into());
    }
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    let rcode = flags & 0x0f;
    if rcode == 3 {
        return Ok(Vec::new());
    }
    if rcode != 0 {
        return Err(format!("DNS error code {rcode}"));
    }
    let questions = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let answers = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    let mut offset = 12;
    for _ in 0..questions {
        offset = skip_name(msg, offset)? + 4;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        offset = skip_name(msg, offset)?;
        if offset + 10 > msg.len() {
            return Err("Truncated DNS message".into());
        }
        let rtype = u16::from_be_bytes([msg[offset], msg[offset + 1]]);
        let ttl = u32::from_be_bytes([
            msg[offset + 4],
            msg[offset + 5],
            msg[offset + 6],
            msg[offset + 7],
        ]);
        let length = u16::from_be_bytes([msg[offset + 8], msg[offset + 9]]) as usize;
        let start = offset + 10;
        let end = start + length;
        if end > msg.len() {
            return Err("Truncated DNS message".into());
        }
        let data = &msg[start..end];
        offset = end;
        if rtype != qtype {
            continue;
        }
        let address = match data.len() {
            4 if qtype == TYPE_A => IpAddr::from(<[u8; 4]>::try_from(data).unwrap()),
            16 if qtype == TYPE_AAAA => IpAddr::from(<[u8; 16]>::try_from(data).unwrap()),
            _ => continue,
        };
        records.push(Record { address, ttl });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        dns_response, eventually, expect_closed, DnsRecords, DnsStub, DohStub, HttpStub, Reply,
    };
    use tokio::net::TcpListener;

    // ── Upstream tests/bridge/resolver.test.ts ─────────────────────────────────

    #[test]
    fn dns_query_encodes_the_hostname_as_labels_with_type_and_class() {
        let query = encode_query("common.lgthinq.com", 1).unwrap();
        assert_eq!(u16::from_be_bytes([query[0], query[1]]), 0); // ID
        assert_eq!(u16::from_be_bytes([query[2], query[3]]), 0x0100); // RD
        assert_eq!(u16::from_be_bytes([query[4], query[5]]), 1); // QDCOUNT
        let expected = rethink_util::hex::decode(
            &["06636f6d6d6f6e076c677468696e7103636f6d00", "0001", "0001"].concat(),
        )
        .unwrap();
        assert_eq!(&query[12..], &expected[..]);
        assert!(encode_query("bad..name", 1).is_err());
    }

    #[test]
    fn dns_response_parsing_follows_compression_and_skips_cnames() {
        let a = encode_query("common.lgthinq.com", 1).unwrap();
        let answers = [(1, 120, vec![52, 1, 2, 3]), (1, 90, vec![52, 1, 2, 4])];
        assert_eq!(
            parse_response(&dns_response(&a, &answers, 0), 1).unwrap(),
            [
                Record {
                    address: "52.1.2.3".parse().unwrap(),
                    ttl: 120
                },
                Record {
                    address: "52.1.2.4".parse().unwrap(),
                    ttl: 90
                },
            ]
        );

        let aaaa = encode_query("common.lgthinq.com", 28).unwrap();
        let data = rethink_util::hex::decode("20010db8000000000000000000000001").unwrap();
        let records = parse_response(&dns_response(&aaaa, &[(28, 60, data)], 0), 28).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].address.to_string(), "2001:db8::1");
        assert_eq!(records[0].ttl, 60);
    }

    #[test]
    fn dns_response_parsing_handles_nxdomain_and_errors() {
        let q = encode_query("nope.example", 1).unwrap();
        assert_eq!(parse_response(&dns_response(&q, &[], 3), 1).unwrap(), []);
        let err = parse_response(&dns_response(&q, &[], 2), 1).unwrap_err();
        assert!(err.contains("code 2"), "{err}");
    }

    #[tokio::test]
    async fn lookup_passes_ip_literals_through_without_querying() {
        // Unreachable, must not be used.
        let resolver = Resolver::new(&["https://192.0.2.1/dns-query".into()]).unwrap();
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(
            resolver.lookup("127.0.0.1", Family::Any).await.unwrap(),
            [v4]
        );
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(resolver.lookup("::1", Family::Any).await.unwrap(), [v6]);
        assert_eq!(resolver.lookup("[::1]", Family::Any).await.unwrap(), [v6]);
    }

    #[test]
    fn set_servers_rejects_entries_that_are_not_servers() {
        for entry in ["not a server", "system"] {
            let err = set_servers(&[entry.into()]).unwrap_err();
            assert!(err.contains(entry), "{err}");
        }
        assert!(Resolver::new(&["https://".into()]).is_err());
        assert!(Resolver::new(&["8.8.8.8:dns".into()]).is_err());
    }

    #[tokio::test]
    async fn lookup_falls_back_to_the_system_resolver_without_servers() {
        let resolver = Resolver::new(&[]).unwrap();
        assert!(resolver.uses_system_resolver());
        let addresses = resolver.lookup("localhost", Family::V4).await.unwrap();
        assert!(
            addresses.contains(&"127.0.0.1".parse().unwrap()),
            "{addresses:?}"
        );
    }

    #[test]
    fn the_example_configuration_ships_upstreams_doh_servers() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.jsonc");
        let config = rethink_core::config::load_config(std::path::Path::new(path)).unwrap();
        let dns = config.bridge.unwrap().dns.unwrap();
        assert_eq!(
            dns,
            ["https://1.1.1.1/dns-query", "https://8.8.8.8/dns-query"]
        );
        assert!(!Resolver::new(&dns).unwrap().uses_system_resolver());
    }

    // ── The servers themselves, against local stand-ins ────────────────────────

    fn lg_records() -> DnsRecords {
        vec![
            ("lg.test", "127.0.0.1".parse().unwrap(), 300),
            ("lg.test", "::1".parse().unwrap(), 300),
            ("v4only.test", "127.0.0.2".parse().unwrap(), 5),
        ]
    }

    #[test]
    fn dns_server_addresses_take_the_forms_upstream_takes() {
        for (entry, addr) in [
            ("1.1.1.1", "1.1.1.1:53"),
            ("1.1.1.1:5353", "1.1.1.1:5353"),
            ("::1", "[::1]:53"),
            ("[::1]", "[::1]:53"),
            ("[::1]:5353", "[::1]:5353"),
            ("1.1.1.1:0", "1.1.1.1:53"),
        ] {
            assert_eq!(dns_server_address(entry), addr.parse().ok(), "{entry}");
        }
        let resolver =
            Resolver::new(&["https://1.1.1.1/dns-query".into(), "8.8.8.8".into()]).unwrap();
        assert!(!resolver.uses_system_resolver());
    }

    #[tokio::test]
    async fn plain_dns_answers_ipv4_first_and_the_answer_is_cached() {
        let dns = DnsStub::start(lg_records()).await;
        let resolver = Resolver::new(&[dns.entry()]).unwrap();
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(
            resolver.lookup("lg.test", Family::Any).await.unwrap(),
            [v4, v6]
        );
        assert_eq!(dns.queries(), 2, "one A and one AAAA query");
        assert_eq!(resolver.lookup("lg.test", Family::V6).await.unwrap(), [v6]);
        assert_eq!(resolver.lookup("lg.test", Family::V4).await.unwrap(), [v4]);
        assert_eq!(dns.queries(), 2, "answered from the cache");

        let err = resolver
            .lookup("v4only.test", Family::V6)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "No IPv6 address for v4only.test");
    }

    #[test]
    fn answers_are_cached_between_one_minute_and_one_hour() {
        let ttl = |ttls: &[u32]| {
            let records: Vec<Record> = ttls
                .iter()
                .map(|&ttl| Record {
                    address: "127.0.0.1".parse().unwrap(),
                    ttl,
                })
                .collect();
            cache_ttl(&records)
        };
        assert_eq!(ttl(&[5]), MIN_TTL);
        assert_eq!(ttl(&[300, 90]), 90);
        assert_eq!(ttl(&[86_400]), MAX_TTL);
    }

    #[tokio::test]
    async fn lookups_made_together_share_one_query() {
        let dns = DnsStub::start_with(lg_records(), Duration::from_millis(100), false).await;
        let resolver = Resolver::new(&[dns.entry()]).unwrap();
        let (all, v4) = tokio::join!(
            resolver.lookup("lg.test", Family::Any),
            resolver.lookup("lg.test", Family::V4)
        );
        assert_eq!(all.unwrap().len(), 2);
        assert_eq!(v4.unwrap(), ["127.0.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(dns.queries(), 2, "one A and one AAAA query for both");
        assert!(resolver.pending.lock().is_empty());
    }

    #[tokio::test]
    async fn servers_are_tried_in_order_until_one_answers() {
        let broken = DohStub::start(lg_records(), 500).await;
        let dns = DnsStub::start(lg_records()).await;
        let doh = Server::doh(url::Url::parse(&broken.url).unwrap()).unwrap();
        let resolver = Resolver::from_servers(vec![("doh", doh), ("dns", Server::Plain(dns.addr))]);
        let addresses = resolver.lookup("lg.test", Family::V4).await.unwrap();
        assert_eq!(addresses, ["127.0.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(
            broken.requests.lock().len(),
            2,
            "the first server was asked first"
        );
        assert_eq!(dns.queries(), 2);
    }

    #[tokio::test]
    async fn a_name_a_server_does_not_know_is_not_asked_elsewhere() {
        let empty = DnsStub::start(Vec::new()).await;
        let dns = DnsStub::start(lg_records()).await;
        let resolver = Resolver::new(&[empty.entry(), dns.entry()]).unwrap();
        let err = resolver.lookup("lg.test", Family::Any).await.unwrap_err();
        assert_eq!(err.to_string(), "Can't resolve lg.test");
        assert_eq!(dns.queries(), 0, "NXDOMAIN is an answer");
    }

    #[tokio::test]
    async fn a_failed_lookup_says_why_and_is_not_cached() {
        let closed = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let resolver = Resolver::new(&[closed.to_string()]).unwrap();
        for _ in 0..2 {
            let err = resolver.lookup("lg.test", Family::Any).await.unwrap_err();
            assert!(
                err.to_string().starts_with("Can't resolve lg.test: "),
                "{err}"
            );
        }
    }

    #[tokio::test]
    async fn doh_queries_follow_rfc_8484() {
        let doh = DohStub::start(lg_records(), 200).await;
        let url = url::Url::parse(&format!("{}?ct=1&dns=stale", doh.url)).unwrap();
        let resolver = Resolver::from_servers(vec![("doh", Server::doh(url).unwrap())]);
        assert_eq!(
            resolver.lookup("lg.test", Family::Any).await.unwrap().len(),
            2
        );

        let mut types = Vec::new();
        for (target, accept) in doh.requests.lock().iter() {
            assert_eq!(accept, "application/dns-message");
            let url = url::Url::parse(&format!("http://doh{target}")).unwrap();
            assert_eq!(url.path(), "/dns-query");
            let params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
            assert_eq!(
                params[0],
                ("ct".into(), "1".into()),
                "other parameters stay"
            );
            assert_eq!(params.len(), 2, "one dns parameter: {params:?}");
            let query = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&params[1].1)
                .unwrap();
            let qtype = u16::from_be_bytes([query[query.len() - 4], query[query.len() - 3]]);
            assert_eq!(query, encode_query("lg.test", qtype).unwrap(), "ID 0, RD");
            types.push(qtype);
        }
        types.sort();
        assert_eq!(types, [TYPE_A, TYPE_AAAA]);
    }

    #[tokio::test]
    async fn a_truncated_answer_is_asked_again_over_tcp() {
        let dns = DnsStub::start_with(lg_records(), Duration::ZERO, true).await;
        let resolver = Resolver::new(&[dns.entry()]).unwrap();
        assert_eq!(
            resolver.lookup("lg.test", Family::Any).await.unwrap().len(),
            2
        );
        assert_eq!(dns.tcp_queries(), 2);
    }

    #[tokio::test]
    async fn http_clients_and_tcp_connections_resolve_through_it() {
        let dns = DnsStub::start(vec![("svc.test", "127.0.0.1".parse().unwrap(), 60)]).await;
        let resolver = Arc::new(Resolver::new(&[dns.entry()]).unwrap());
        let http = HttpStub::serve(|_| Reply::Ok("{}".into())).await;
        let port = url::Url::parse(&http.url).unwrap().port().unwrap();
        let body = resolver
            .http_client()
            .build()
            .unwrap()
            .get(format!("http://svc.test:{port}/x"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "{}");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (connected, accepted) =
            tokio::join!(resolver.connect("svc.test", port), listener.accept());
        connected.unwrap();
        accepted.unwrap();
        assert_eq!(dns.queries(), 2, "looked up once, then cached");
    }

    async fn read_head(s: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            head.push(byte[0]);
        }
        String::from_utf8(head).unwrap()
    }

    #[tokio::test]
    async fn the_connect_proxy_tunnels_its_one_target_until_stopped() {
        let dns = DnsStub::start(vec![("broker.test", "127.0.0.1".parse().unwrap(), 60)]).await;
        let resolver = Arc::new(Resolver::new(&[dns.entry()]).unwrap());
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = target.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = sock.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let stop = StopSignal::new();
        let proxy = connect_proxy(resolver, "broker.test", port, stop.clone()).unwrap();

        let mut tunnel = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy))
            .await
            .unwrap();
        let connect =
            format!("CONNECT broker.test:{port} HTTP/1.1\r\nHost: broker.test:{port}\r\n\r\n");
        tunnel.write_all(connect.as_bytes()).await.unwrap();
        assert_eq!(
            read_head(&mut tunnel).await,
            "HTTP/1.1 200 Connection established\r\n\r\n"
        );
        tunnel.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        tunnel.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"ping");

        let mut other = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy))
            .await
            .unwrap();
        other
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(
            read_head(&mut other).await,
            "HTTP/1.1 403 Forbidden\r\n\r\n"
        );

        stop.stop();
        eventually("the proxy stops listening", || {
            std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, proxy)).is_err()
        })
        .await;
        // A tunnel whose client stays quiet is dropped once the grace period is over.
        expect_closed(&mut tunnel, "the tunnel after stop").await;
    }
}
