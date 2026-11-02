//! ThinQ1 upstream RTI TLS connection (port of bridge/thinq1connection.ts).
//!
//! One task per bridged session owns the link: it posts TotalDeviceInfoSvc, opens the RTI TLS
//! connection, serves it, and reconnects after [`RECONNECT_PERIOD`] when it drops. Stopping the
//! session's [`StopSignal`] drops that task and with it the socket, so nothing is read, acked
//! or sent afterwards.

use crate::pair::Thinq1DeviceState;
use crate::resolver::Resolver;
use crate::stop::StopSignal;
use crate::{OnLink, RECONNECT_PERIOD};
use rethink_util::length_prefixed_frame::{self, Splitter};
use rethink_util::sync::Mutex;
use rustls::pki_types::{CertificateDer, ServerName};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::TlsConnector;

const ALIVE_PERIOD: Duration = Duration::from_secs(60);

/// Cloneable send handle for local→LG status.
#[derive(Clone)]
pub struct Thinq1Handle {
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    is_live: Arc<AtomicBool>,
    device_id: String,
    last_state: Arc<Mutex<Option<Vec<u8>>>>,
    stop: StopSignal,
}

impl Thinq1Handle {
    pub fn send_from_local(&self, data: &[u8]) {
        if self.stop.is_stopped() {
            return;
        }
        *self.last_state.lock() = Some(data.to_vec());
        eprintln!(
            "[bridge] {} -> {}",
            self.device_id,
            rethink_util::hex::encode(data)
        );
        if !self.is_live.load(Ordering::SeqCst) {
            return;
        }
        let body = format_status_body(&self.device_id, data);
        let frame = length_prefixed_frame::make(body.to_string().as_bytes());
        let _ = self.write_tx.send(frame);
    }

    pub fn stop(&self) {
        self.stop.stop();
    }
}

/// Start the session's connection task; it runs until `stop`. `on_link` hears when the TLS
/// link to LG is up and when it has dropped. LG's names are looked up through `resolver`.
pub fn connect_thinq1(
    state: &Thinq1DeviceState,
    device_id: &str,
    model_name: &str,
    device_type: Option<&str>,
    stop: StopSignal,
    on_link: OnLink,
    resolver: Arc<Resolver>,
) -> anyhow::Result<(Thinq1Handle, mpsc::UnboundedReceiver<serde_json::Value>)> {
    let (host, port) = parse_host_port(&state.rti_server)?;
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let (from_lg_tx, from_lg_rx) = mpsc::unbounded_channel();
    let handle = Thinq1Handle {
        write_tx,
        is_live: Arc::new(AtomicBool::new(false)),
        device_id: device_id.to_string(),
        last_state: Arc::new(Mutex::new(None)),
        stop: stop.clone(),
    };
    let link = Link {
        state: state.clone(),
        host,
        port,
        device_id: device_id.to_string(),
        model_name: model_name.to_string(),
        device_type: device_type.unwrap_or("201").to_string(),
        is_live: handle.is_live.clone(),
        last_state: handle.last_state.clone(),
        from_lg: from_lg_tx,
        stop: stop.clone(),
        on_link,
        resolver,
    };
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = stop.stopped() => {}
            _ = link.run(write_rx) => {}
        }
    });
    Ok((handle, from_lg_rx))
}

struct Link {
    state: Thinq1DeviceState,
    host: String,
    port: u16,
    device_id: String,
    model_name: String,
    device_type: String,
    is_live: Arc<AtomicBool>,
    last_state: Arc<Mutex<Option<Vec<u8>>>>,
    from_lg: mpsc::UnboundedSender<serde_json::Value>,
    stop: StopSignal,
    on_link: OnLink,
    resolver: Arc<Resolver>,
}

impl Link {
    async fn run(self, mut outgoing: mpsc::UnboundedReceiver<Vec<u8>>) {
        loop {
            if let Some(tls) = self.connect().await {
                eprintln!("[bridge] {} connected", self.device_id);
                (self.on_link)(true);
                // Status queued for an earlier link is stale; LG asks again with Start.
                while outgoing.try_recv().is_ok() {}
                if let Err(e) = self.serve(tls, &mut outgoing).await {
                    eprintln!(
                        "[bridge] Error communicating with {}: {e}",
                        self.state.rti_server
                    );
                }
                self.is_live.store(false, Ordering::SeqCst);
                eprintln!("[bridge] {} disconnected", self.device_id);
                (self.on_link)(false);
            }
            tokio::time::sleep(RECONNECT_PERIOD).await;
        }
    }

    /// TotalDeviceInfoSvc, then the RTI TLS handshake. `None` when stopped meanwhile or failed.
    async fn connect(&self) -> Option<TlsStream<TcpStream>> {
        let url = format!(
            "{}/lgehadm/api/Device/TotalDeviceInfoSvc",
            self.state.http_server.trim_end_matches('/')
        );
        if let Err(e) = self.total_device_info(&url).await {
            eprintln!(
                "[bridge] Failed to fetch {url}: {:#}",
                anyhow::Error::from(e)
            );
            return None;
        }
        if self.stop.is_stopped() {
            return None;
        }

        eprintln!(
            "[bridge] {} connecting to {}",
            self.device_id, self.state.rti_server
        );
        match self.handshake().await {
            Ok(tls) if !self.stop.is_stopped() => Some(tls),
            Ok(_) => None,
            Err(e) => {
                eprintln!(
                    "[bridge] Error communicating with {}: {e:#}",
                    self.state.rti_server
                );
                None
            }
        }
    }

    async fn total_device_info(&self, url: &str) -> Result<String, reqwest::Error> {
        self.resolver
            .http_client()
            .danger_accept_invalid_certs(true)
            .build()?
            .post(url)
            .header("Accept", "text/xml")
            .header("content-type", "text/xml;charset=utf-8")
            .header("x-lgedm-userid", "lgehadmUser")
            .header(
                "x-lgedm-password",
                "bxLoLAZ+rp3oJDbEzRuIfAG4YumeqwWM9l6uUH6TupQ=",
            )
            .header("x-lgedm-deviceid", &self.device_id)
            .header("x-lgedm-devicetype", &self.device_type)
            .body(format!(
                "<lgedmRoot><countryCode>WW</countryCode><modelName>{}</modelName>\
                 <itemList><item>THINQ_TIME_SYNC_URI</item>\
                 <elementList><elementCode>pushDetailYn</elementCode>\
                 <elementValue>Y</elementValue></elementList></itemList></lgedmRoot>",
                self.model_name
            ))
            .send()
            .await?
            .text()
            .await
    }

    async fn handshake(&self) -> anyhow::Result<TlsStream<TcpStream>> {
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerifier))
                .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let tcp = self.resolver.connect(&self.host, self.port).await?;
        // The resolved address is only for TCP; TLS still names the host.
        let server_name = ServerName::try_from(self.host.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .to_owned();
        Ok(connector.connect(server_name, tcp).await?)
    }

    /// Until LG closes the link or it fails.
    async fn serve(
        &self,
        mut tls: TlsStream<TcpStream>,
        outgoing: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> std::io::Result<()> {
        let mut splitter = Splitter::new(1_000_000);
        let mut buf = [0u8; 8192];
        // The first tick is immediate: Alive goes out as soon as the link is up.
        let mut alive = tokio::time::interval(ALIVE_PERIOD);
        loop {
            tokio::select! {
                _ = alive.tick() => {
                    let frame = length_prefixed_frame::make(format_alive(&self.device_id).as_bytes());
                    tls.write_all(&frame).await?;
                }
                Some(frame) = outgoing.recv() => tls.write_all(&frame).await?,
                n = tls.read(&mut buf) => {
                    let n = n?;
                    if n == 0 {
                        return Ok(());
                    }
                    let frames = splitter.feed(&buf[..n]).map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                    })?;
                    for payload in frames {
                        let Ok(j) = serde_json::from_slice::<serde_json::Value>(&payload) else {
                            continue;
                        };
                        if let Some(reply) = handle_lg_json(
                            &j,
                            &self.device_id,
                            &self.is_live,
                            &self.last_state,
                            &self.from_lg,
                        ) {
                            tls.write_all(&reply).await?;
                        }
                    }
                }
            }
        }
    }
}

/// Act on one LG frame; returns the frame to answer with, if any.
fn handle_lg_json(
    j: &serde_json::Value,
    device_id: &str,
    is_live: &AtomicBool,
    last_state: &Mutex<Option<Vec<u8>>>,
    from_lg: &mpsc::UnboundedSender<serde_json::Value>,
) -> Option<Vec<u8>> {
    let body = j.get("Body")?;
    if body.get("CmdOpt").and_then(|v| v.as_str()) == Some("Start") {
        is_live.store(true, Ordering::SeqCst);
        return last_state.lock().as_ref().map(|st| {
            let msg = format_status_body(device_id, st);
            length_prefixed_frame::make(msg.to_string().as_bytes())
        });
    }
    if body.get("CmdOpt").and_then(|v| v.as_str()) == Some("Stop") {
        is_live.store(false, Ordering::SeqCst);
        return None;
    }
    eprintln!("[bridge] {device_id} <- {body}");
    let _ = from_lg.send(body.clone());
    if body.get("ReturnCode").is_some() {
        return None;
    }
    let cmd_w_id = body.get("CmdWId")?;
    let ack = serde_json::json!({
        "Header": { "x-lgedm-deviceId": device_id },
        "Body": { "CmdWId": cmd_w_id, "ReturnCode": "0000" }
    });
    Some(length_prefixed_frame::make(ack.to_string().as_bytes()))
}

pub fn format_status_body(device_id: &str, data: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "Header": { "x-lgedm-deviceId": device_id },
        "Body": {
            "CmdWId": format!("n-{device_id}"),
            "ReturnCode": "0000",
            "Format": "B64",
            "Data": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                data
            ),
        }
    })
}

fn format_alive(device_id: &str) -> String {
    serde_json::json!({
        "Header": { "x-lgedm-deviceId": device_id },
        "Body": {
            "CmdWId": uuid::Uuid::new_v4().to_string(),
            "Cmd": "Alive",
        }
    })
    .to_string()
}

fn parse_host_port(s: &str) -> anyhow::Result<(String, u16)> {
    let mut parts = s.split(':');
    let host = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("bad rtiServer"))?
        .to_string();
    let port: u16 = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("bad rtiServer port"))?
        .parse()?;
    Ok((host, port))
}

#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{read_t1_frame, DnsStub, HttpStub, TestPki, TlsStub};
    use rethink_util::length_prefixed_frame;

    #[tokio::test]
    async fn lg_names_resolve_through_the_bridge_servers_and_tls_keeps_the_hostname() {
        let pki = TestPki::new();
        let mut rti = TlsStub::start(pki.server_config(&pki.leaf(&["rti.test"]), false)).await;
        let http = HttpStub::start(false).await;
        let http_port = url::Url::parse(&http.url).unwrap().port().unwrap();
        // Names only the bridge's DNS server knows.
        let dns = DnsStub::start(vec![
            ("rti.test", "127.0.0.1".parse().unwrap(), 60),
            ("http.test", "127.0.0.1".parse().unwrap(), 60),
        ])
        .await;
        let resolver = Arc::new(Resolver::new(&[dns.entry()]).unwrap());
        let state = Thinq1DeviceState {
            rti_server: format!("rti.test:{}", rti.port),
            http_server: format!("http://http.test:{http_port}"),
        };
        let stop = StopSignal::new();
        let (_handle, _from_lg) = connect_thinq1(
            &state,
            "t1-dns",
            "MODEL",
            None,
            stop.clone(),
            Arc::new(|_| {}),
            resolver,
        )
        .unwrap();

        let (mut lg, sni) = rti.next().await;
        assert_eq!(sni.as_deref(), Some("rti.test"));
        assert_eq!(
            read_t1_frame(&mut lg).await.unwrap()["Body"]["Cmd"],
            "Alive"
        );
        assert_eq!(
            http.accepted.load(Ordering::SeqCst),
            1,
            "TotalDeviceInfoSvc"
        );
        assert_eq!(dns.queries(), 4, "A and AAAA for each name");
        stop.stop();
        assert!(read_t1_frame(&mut lg).await.is_none());
    }

    #[test]
    fn status_body_is_b64_and_framed() {
        let body = format_status_body("id-1", &[0xAA, 0xBB]);
        assert_eq!(body["Body"]["Format"], "B64");
        let data = body["Body"]["Data"].as_str().unwrap();
        assert_eq!(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).unwrap(),
            vec![0xAA, 0xBB]
        );
        let frame = length_prefixed_frame::make(body.to_string().as_bytes());
        assert!(frame.len() > 4);
    }
}
