//! ThinQ2 upstream MQTT connection to LG cloud (port of bridge/thinq2connection.ts).

use crate::pair::{
    classify_lg_downlink, format_device_packet, upstream_pre_deploy, LgDownlink, Thinq2DeviceState,
};
use crate::resolver::{self, Resolver};
use crate::stop::StopSignal;
use crate::OnLink;
use rumqttc::{
    AsyncClient, Event, EventLoop, Incoming, MqttOptions, Outgoing, Proxy, ProxyAuth, ProxyType,
    QoS, TlsConfiguration, Transport,
};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Cloneable handle: local→LG send + stop.
#[derive(Clone)]
pub struct Thinq2Handle {
    client: AsyncClient,
    mid: Arc<AtomicU32>,
    device_id: String,
    model_name: String,
    pub_topic: String,
    stop: StopSignal,
}

impl Thinq2Handle {
    pub async fn send_from_local(&self, data: &[u8]) -> anyhow::Result<()> {
        if self.stop.is_stopped() {
            return Ok(());
        }
        let hex_data = rethink_util::hex::encode_upper(data);
        eprintln!("[bridge] {} -> {hex_data}", self.device_id);
        let m = self.mid.fetch_add(1, Ordering::SeqCst) + 1;
        let payload = format_device_packet(m, &self.device_id, &self.model_name, &hex_data);
        self.client
            .publish(&self.pub_topic, QoS::AtMostOnce, false, payload)
            .await?;
        Ok(())
    }

    pub fn stop(&self) {
        self.stop.stop();
    }
}

/// LG → appliance message after the connection has answered provisioning itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LgToDevice {
    Packet(Vec<u8>),
    Ack(Vec<u8>),
}

/// Open LG MQTT; returns handle + channel of LG→local packets and delivery acks. The event
/// loop runs until `stop`, then disconnects and drops the connection. `on_link` hears when the
/// MQTT session is up and when it has dropped. The broker's name is looked up through
/// `resolver`.
pub fn connect_thinq2(
    state: &Thinq2DeviceState,
    device_id: &str,
    model_name: &str,
    deploy: Option<serde_json::Value>,
    stop: StopSignal,
    on_link: OnLink,
    resolver: Arc<Resolver>,
) -> anyhow::Result<(Thinq2Handle, mpsc::UnboundedReceiver<LgToDevice>)> {
    let opts = mqtt_options(state, device_id, resolver, &stop)?;
    eprintln!("[bridge] {device_id} connecting to {}", state.mqtt_server);

    let (client, mut eventloop) = AsyncClient::new(opts, 32);
    let (tx, rx) = mpsc::unbounded_channel();
    let mid = Arc::new(AtomicU32::new(10000));

    let sub_topic = state.sub_topic.clone();
    let prov_topic = state.prov_topic.clone();
    let pub_topic = state.pub_topic.clone();
    let did = device_id.to_string();
    let model = model_name.to_string();
    let mqtt_server = state.mqtt_server.clone();
    let mid_c = mid.clone();
    let client_c = client.clone();
    let stop_c = stop.clone();

    tokio::spawn(async move {
        let mut connected = false;
        loop {
            let event = tokio::select! {
                biased;
                _ = stop_c.stopped() => break,
                event = eventloop.poll() => event,
            };
            match event {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    connected = true;
                    eprintln!("[bridge] {did} connected");
                    on_link(true);
                    let _ = client_c.subscribe(&sub_topic, QoS::AtLeastOnce).await;
                    let m = mid_c.fetch_add(1, Ordering::SeqCst) + 1;
                    let pre = upstream_pre_deploy(m, &did, &model, deploy.as_ref()).to_string();
                    let _ = client_c
                        .publish(&prov_topic, QoS::AtLeastOnce, false, pre)
                        .await;
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    if p.topic != sub_topic {
                        continue;
                    }
                    let Ok(text) = String::from_utf8(p.payload.to_vec()) else {
                        continue;
                    };
                    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&text) else {
                        continue;
                    };
                    match classify_lg_downlink(&payload) {
                        LgDownlink::CompleteProvisioning => {
                            let m = mid_c.fetch_add(1, Ordering::SeqCst) + 1;
                            let ack = serde_json::json!({
                                "mid": m, "did": did, "kind": model,
                                "cmd": "completeProvisioning_ack",
                                "rssi": -48, "fs": "idle", "data": null, "type": 1,
                            });
                            let _ = client_c
                                .publish(&pub_topic, QoS::AtMostOnce, false, ack.to_string())
                                .await;
                        }
                        LgDownlink::Packet(buf) => {
                            eprintln!("[bridge] {did} <- {}", rethink_util::hex::encode(&buf));
                            if tx.send(LgToDevice::Packet(buf)).is_err() {
                                break;
                            }
                        }
                        LgDownlink::Ack(buf) => {
                            let shown = payload
                                .get("data")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            eprintln!("[bridge] {did} <- ack {shown}");
                            if tx.send(LgToDevice::Ack(buf)).is_err() {
                                break;
                            }
                        }
                        LgDownlink::Dropped(_) => {
                            let cmd = payload
                                .get("cmd")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            eprintln!("[bridge] {did} <- dropped {cmd}: {text}");
                        }
                    }
                }
                Ok(Event::Incoming(Incoming::Disconnect)) => {
                    eprintln!("[bridge] {did} disconnected");
                    on_link(false);
                    break;
                }
                Err(e) => {
                    connected = false;
                    eprintln!("[bridge] Error communicating with {mqtt_server}: {e}");
                    on_link(false);
                    tokio::select! {
                        biased;
                        _ = stop_c.stopped() => break,
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                _ => {}
            }
        }
        if connected && stop_c.is_stopped() {
            disconnect(&client_c, &mut eventloop).await;
        }
        // Dropping the event loop closes the connection.
    });

    let handle = Thinq2Handle {
        client,
        mid,
        device_id: device_id.to_string(),
        model_name: model_name.to_string(),
        pub_topic: state.pub_topic.clone(),
        stop,
    };
    Ok((handle, rx))
}

/// rumqttc looks the broker up itself and also names it in TLS (SNI, certificate check), so
/// with bridge DNS servers configured it is pointed at an in-process CONNECT proxy that
/// resolves through them; TLS is still rumqttc's own, end to end. Without servers the
/// connection is direct, as before.
fn mqtt_options(
    state: &Thinq2DeviceState,
    device_id: &str,
    resolver: Arc<Resolver>,
    stop: &StopSignal,
) -> anyhow::Result<MqttOptions> {
    let mqtt_url = state.mqtt_server.replace("ssl://", "mqtts://");
    let url = url::Url::parse(&mqtt_url)
        .or_else(|_| url::Url::parse(&format!("mqtts://{}", state.mqtt_server)))?;
    let host = url.host_str().unwrap_or("localhost").to_string();
    let port = url.port().unwrap_or(8883);

    let mut opts = MqttOptions::new(device_id, host.clone(), port);
    opts.set_keep_alive(std::time::Duration::from_secs(30));
    opts.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: state.ca_certificate.as_bytes().to_vec(),
        alpn: None,
        client_auth: Some((
            state.certificate.as_bytes().to_vec(),
            state.private_key.as_bytes().to_vec(),
        )),
    }));
    if !resolver.uses_system_resolver() {
        let proxy_port = resolver::connect_proxy(resolver, &host, port, stop.clone())?;
        opts.set_proxy(Proxy {
            ty: ProxyType::Http,
            auth: ProxyAuth::None,
            addr: Ipv4Addr::LOCALHOST.to_string(),
            port: proxy_port,
        });
    }
    Ok(opts)
}

/// Best-effort MQTT DISCONNECT before the connection is dropped.
async fn disconnect(client: &AsyncClient, eventloop: &mut EventLoop) {
    if client.try_disconnect().is_err() {
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while let Ok(event) = eventloop.poll().await {
            if event == Event::Outgoing(Outgoing::Disconnect) {
                break;
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{DnsStub, Leaf, TestPki, TlsStub, WAIT};
    use bytes::BytesMut;
    use rethink_util::sync::Mutex;
    use rumqttc::mqttbytes::{self, v4};
    use rumqttc::{ConnAck, ConnectReturnCode, PubAck, Publish, SubAck, SubscribeReasonCode};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

    fn state(mqtt_server: &str, pki: &TestPki, device: &Leaf) -> Thinq2DeviceState {
        Thinq2DeviceState {
            country_code: "US".into(),
            api_server: String::new(),
            mqtt_server: mqtt_server.into(),
            ca_certificate: pki.ca_pem.clone(),
            private_key: device.key_pem.clone(),
            certificate: device.cert_pem.clone(),
            pub_topic: "clip/pub".into(),
            prov_topic: "clip/prov".into(),
            sub_topic: "clip/sub".into(),
        }
    }

    async fn read_packet<S: AsyncRead + Unpin>(
        s: &mut S,
        buf: &mut BytesMut,
    ) -> Option<v4::Packet> {
        loop {
            match v4::read(buf, 1 << 20) {
                Ok(packet) => return Some(packet),
                Err(mqttbytes::Error::InsufficientBytes(_)) => {}
                Err(e) => panic!("{e}"),
            }
            let read = tokio::time::timeout(WAIT, s.read_buf(buf)).await;
            match read.expect("timed out waiting for an MQTT packet") {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    #[test]
    fn without_bridge_dns_servers_rumqttc_connects_directly() {
        let pki = TestPki::new();
        let device = pki.leaf(&["device"]);
        let opts = mqtt_options(
            &state("ssl://mqtt.test:8883", &pki, &device),
            "dev",
            Arc::new(Resolver::new(&[]).unwrap()),
            &StopSignal::new(),
        )
        .unwrap();
        assert!(opts.proxy().is_none());
        assert_eq!(opts.broker_address(), ("mqtt.test".to_string(), 8883));
    }

    #[tokio::test]
    async fn with_bridge_dns_servers_the_broker_is_found_through_them_and_tls_still_names_it() {
        let pki = TestPki::new();
        let device = pki.leaf(&["device"]);
        let mut broker = TlsStub::start(pki.server_config(&pki.leaf(&["mqtt.test"]), true)).await;
        // A name only the bridge's DNS server knows.
        let dns = DnsStub::start(vec![("mqtt.test", "127.0.0.1".parse().unwrap(), 60)]).await;
        let resolver = Arc::new(Resolver::new(&[dns.entry()]).unwrap());
        let server = format!("ssl://mqtt.test:{}", broker.port);
        let stop = StopSignal::new();
        let links = Arc::new(Mutex::new(Vec::new()));
        let seen = links.clone();
        let (handle, mut from_lg) = connect_thinq2(
            &state(&server, &pki, &device),
            "dev-t2",
            "MODEL",
            None,
            stop.clone(),
            Arc::new(move |up| seen.lock().push(up)),
            resolver,
        )
        .unwrap();

        // The broker requires the device certificate, and the client checked the broker's.
        let (mut lg, sni) = broker.next().await;
        assert_eq!(sni.as_deref(), Some("mqtt.test"));
        let mut buf = BytesMut::new();
        let mut out = BytesMut::new();
        let Some(v4::Packet::Connect(connect)) = read_packet(&mut lg, &mut buf).await else {
            panic!("expected CONNECT");
        };
        assert_eq!(connect.client_id, "dev-t2");
        ConnAck::new(ConnectReturnCode::Success, false)
            .write(&mut out)
            .unwrap();
        lg.write_all(&out.split()).await.unwrap();

        let Some(v4::Packet::Subscribe(subscribe)) = read_packet(&mut lg, &mut buf).await else {
            panic!("expected SUBSCRIBE");
        };
        assert_eq!(subscribe.filters[0].path, "clip/sub");
        let granted = SubscribeReasonCode::Success(QoS::AtLeastOnce);
        SubAck::new(subscribe.pkid, vec![granted])
            .write(&mut out)
            .unwrap();
        let Some(v4::Packet::Publish(pre_deploy)) = read_packet(&mut lg, &mut buf).await else {
            panic!("expected the preDeploy");
        };
        assert_eq!(pre_deploy.topic, "clip/prov");
        let body: serde_json::Value = serde_json::from_slice(&pre_deploy.payload).unwrap();
        assert_eq!(body["cmd"], "preDeploy");
        PubAck::new(pre_deploy.pkid).write(&mut out).unwrap();

        // LG -> device, and device -> LG.
        let downlink = r#"{"cmd":"packet","data":"0102"}"#;
        Publish::new("clip/sub", QoS::AtMostOnce, downlink)
            .write(&mut out)
            .unwrap();
        lg.write_all(&out.split()).await.unwrap();
        let down = tokio::time::timeout(WAIT, from_lg.recv()).await.unwrap();
        assert_eq!(down, Some(LgToDevice::Packet(vec![1, 2])));
        assert_eq!(links.lock().clone(), [true]);
        handle.send_from_local(&[0xAA, 0xBB]).await.unwrap();
        let Some(v4::Packet::Publish(up)) = read_packet(&mut lg, &mut buf).await else {
            panic!("expected the device packet");
        };
        assert_eq!(up.topic, "clip/pub");
        let body: serde_json::Value = serde_json::from_slice(&up.payload).unwrap();
        assert_eq!(body["cmd"], "device_packet");
        assert_eq!(body["data"], "AABB");

        // Stopping says goodbye through the proxy, then the link ends.
        stop.stop();
        let goodbye = read_packet(&mut lg, &mut buf).await;
        assert_eq!(goodbye, Some(v4::Packet::Disconnect));
        assert_eq!(read_packet(&mut lg, &mut buf).await, None);
        assert_eq!(dns.queries(), 2, "resolved once, A and AAAA");
    }
}
