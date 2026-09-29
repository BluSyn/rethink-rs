//! ThinQ2 upstream MQTT connection to LG cloud (port of bridge/thinq2connection.ts).

use crate::pair::{
    classify_lg_downlink, format_device_packet, upstream_pre_deploy, LgDownlink, Thinq2DeviceState,
};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, TlsConfiguration, Transport};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Cloneable handle: local→LG send + stop.
#[derive(Clone)]
pub struct Thinq2Handle {
    client: AsyncClient,
    mid: Arc<AtomicU32>,
    device_id: String,
    model_name: String,
    pub_topic: String,
    stopped: Arc<AtomicBool>,
}

impl Thinq2Handle {
    pub async fn send_from_local(&self, data: &[u8]) -> anyhow::Result<()> {
        if self.stopped.load(Ordering::SeqCst) {
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
        self.stopped.store(true, Ordering::SeqCst);
        let c = self.client.clone();
        tokio::spawn(async move {
            let _ = c.disconnect().await;
        });
    }
}

/// LG → appliance message after the connection has answered provisioning itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LgToDevice {
    Packet(Vec<u8>),
    Ack(Vec<u8>),
}

/// Open LG MQTT; returns handle + channel of LG→local packets and delivery acks.
pub async fn connect_thinq2(
    state: &Thinq2DeviceState,
    device_id: &str,
    model_name: &str,
    deploy: Option<serde_json::Value>,
) -> anyhow::Result<(Thinq2Handle, mpsc::UnboundedReceiver<LgToDevice>)> {
    let mqtt_url = state.mqtt_server.replace("ssl://", "mqtts://");
    let url = url::Url::parse(&mqtt_url)
        .or_else(|_| url::Url::parse(&format!("mqtts://{}", state.mqtt_server)))?;
    let host = url.host_str().unwrap_or("localhost").to_string();
    let port = url.port().unwrap_or(8883);

    eprintln!("[bridge] {device_id} connecting to {}", state.mqtt_server);

    let mut opts = MqttOptions::new(device_id, host, port);
    opts.set_keep_alive(std::time::Duration::from_secs(30));
    opts.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: state.ca_certificate.as_bytes().to_vec(),
        alpn: None,
        client_auth: Some((
            state.certificate.as_bytes().to_vec(),
            state.private_key.as_bytes().to_vec(),
        )),
    }));

    let (client, mut eventloop) = AsyncClient::new(opts, 32);
    let (tx, rx) = mpsc::unbounded_channel();
    let mid = Arc::new(AtomicU32::new(10000));
    let stopped = Arc::new(AtomicBool::new(false));

    let sub_topic = state.sub_topic.clone();
    let prov_topic = state.prov_topic.clone();
    let pub_topic = state.pub_topic.clone();
    let did = device_id.to_string();
    let model = model_name.to_string();
    let mid_c = mid.clone();
    let client_c = client.clone();
    let stopped_c = stopped.clone();

    tokio::spawn(async move {
        loop {
            if stopped_c.load(Ordering::SeqCst) {
                break;
            }
            match eventloop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    eprintln!("[bridge] {did} connected");
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
                    break;
                }
                Err(e) => {
                    eprintln!("[bridge] {did} mqtt error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                _ => {}
            }
        }
    });

    let handle = Thinq2Handle {
        client,
        mid,
        device_id: device_id.to_string(),
        model_name: model_name.to_string(),
        pub_topic: state.pub_topic.clone(),
        stopped,
    };
    Ok((handle, rx))
}

