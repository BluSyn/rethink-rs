//! ThinQ2 upstream MQTT connection to LG cloud (port of bridge/thinq2connection.ts).

use crate::pair::{
    classify_lg_downlink, format_device_packet, upstream_pre_deploy, LgDownlink, Thinq2DeviceState,
};
use crate::stop::StopSignal;
use crate::OnLink;
use rumqttc::{
    AsyncClient, Event, EventLoop, Incoming, MqttOptions, Outgoing, QoS, TlsConfiguration,
    Transport,
};
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
/// MQTT session is up and when it has dropped.
pub fn connect_thinq2(
    state: &Thinq2DeviceState,
    device_id: &str,
    model_name: &str,
    deploy: Option<serde_json::Value>,
    stop: StopSignal,
    on_link: OnLink,
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

    let sub_topic = state.sub_topic.clone();
    let prov_topic = state.prov_topic.clone();
    let pub_topic = state.pub_topic.clone();
    let did = device_id.to_string();
    let model = model_name.to_string();
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
                    eprintln!("[bridge] {did} mqtt error: {e}");
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
