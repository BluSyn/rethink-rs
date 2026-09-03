//! ThinQ2 device acceptor — listens on the internal MQTT broker for CLIP traffic.

use super::provisioning::generate_deploy_response;
use crate::devmgr::{ConnectedDevice, DeviceManager, Platform, SendToDevice};
use crate::mqtt_broker::{Broker, PublishPacket};
use base64::Engine;
use chrono::{Datelike, Timelike};
use rethink_core::metadata::Metadata;
use rethink_util::sync::Mutex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

pub struct DeviceAcceptor {
    broker: Arc<Broker>,
    manager: Arc<DeviceManager>,
    clients_by_id: Mutex<HashMap<String, u64>>,
    devices: Mutex<HashMap<u64, Arc<ConnectedDevice>>>,
}

impl DeviceAcceptor {
    pub fn new(broker: Arc<Broker>, manager: Arc<DeviceManager>) -> Arc<Self> {
        let acceptor = Arc::new(Self {
            broker: broker.clone(),
            manager,
            clients_by_id: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
        });

        let acc = acceptor.clone();
        broker.on_publish(Arc::new(move |packet, client| {
            if client.is_none() {
                rethink_core::logging::log(
                    "outgoing",
                    &[
                        &packet.topic,
                        &String::from_utf8_lossy(&packet.payload),
                        "retain:",
                        &packet.retain.to_string(),
                    ],
                );
                return;
            }
            let client_id = client.unwrap();
            rethink_core::logging::log(
                "incoming",
                &[
                    &packet.topic,
                    &String::from_utf8_lossy(&packet.payload),
                    "retain:",
                    &packet.retain.to_string(),
                ],
            );
            if !packet.topic.contains("clip/") {
                return;
            }
            let mut payload_bytes = packet.payload.as_slice();
            if payload_bytes.last() == Some(&0) {
                payload_bytes = &payload_bytes[..payload_bytes.len() - 1];
            }
            let payload: serde_json::Value = match serde_json::from_slice(payload_bytes) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("clip parse error: {e}");
                    return;
                }
            };
            acc.handle_mqtt(&packet.topic, &payload, client_id);
        }));

        let acc2 = acceptor.clone();
        broker.on_disconnect(Arc::new(move |client_id| {
            acc2.disconnected(client_id);
        }));

        acceptor
    }

    fn handle_mqtt(&self, topic: &str, payload: &serde_json::Value, client_id: u64) {
        let topic = clip_relative_topic(topic);

        let did = payload.get("did").and_then(|v| v.as_str()).unwrap_or("");
        let cmd = payload.get("cmd").and_then(|v| v.as_str()).unwrap_or("");

        if topic_ends_with_device("clip/message/devices/", &topic, did) {
            if cmd == "completeProvisioning_ack" {
                self.complete_provisioning(did, payload, client_id);
            }
            if cmd == "device_packet" {
                let meta = self.broker.client_meta(client_id);
                if meta
                    .deploy_msg
                    .as_ref()
                    .and_then(|d| d.get("did"))
                    .and_then(|v| v.as_str())
                    == Some(did)
                {
                    if let Some(dev) = self.devices.lock().get(&client_id).cloned() {
                        // Heal: reconnect race may have removed this id from DeviceManager
                        // while the MQTT session + acceptor still hold the device.
                        if self.manager.get(did).is_none() {
                            eprintln!(
                                "device {did}: re-accepting into manager (was orphaned from UI/HA)"
                            );
                            self.manager.accept(dev.clone());
                        }
                        if let Some(data) = payload.get("data").and_then(|d| d.as_str()) {
                            if let Ok(buf) = rethink_util::hex::decode(data) {
                                dev.notify_data(&buf);
                            }
                        }
                    }
                }
            }
            if cmd == "req_timesync" {
                self.time_sync_request(client_id);
            }
        }

        if topic_ends_with_device("clip/provisioning/devices/", &topic, did)
            && (cmd == "preDeploy" || cmd == "deploy")
        {
            let mut meta = self.broker.client_meta(client_id);
            meta.deploy_msg = Some(payload.clone());
            self.broker.set_client_meta(client_id, meta);
            let resp = generate_deploy_response(payload);
            self.broker.publish(
                PublishPacket {
                    topic: format!("lime/devices/{did}"),
                    payload: serde_json::to_vec(&resp).unwrap_or_default(),
                    retain: false,
                    qos: 0,
                    dup: false,
                },
                None,
            );
        }
    }

    fn complete_provisioning(&self, device_id: &str, _payload: &serde_json::Value, client_id: u64) {
        let meta_c = self.broker.client_meta(client_id);
        let Some(deploy) = meta_c.deploy_msg.clone() else {
            eprintln!("completeProvisioning_ack received without deploy/preDeploy");
            return;
        };
        // Same MQTT client already provisioned — only re-bind if manager lost the device
        // (e.g. pre-fix reconnect race). Otherwise ignore duplicate ack.
        if meta_c.has_device {
            if self.manager.get(device_id).is_some() && self.devices.lock().contains_key(&client_id)
            {
                return;
            }
            eprintln!(
                "device {device_id}: completeProvisioning_ack on already-flagged client but missing from manager — re-registering"
            );
        }

        if let Some(old) = self.clients_by_id.lock().get(device_id).copied() {
            if old != client_id {
                eprintln!("device {device_id} already connected, dropping the old one");
                self.broker.destroy_client(old);
            }
        }
        self.clients_by_id
            .lock()
            .insert(device_id.to_string(), client_id);

        let model_id = deploy
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let model_name = deploy
            .pointer("/data/appInfo/modelName")
            .and_then(|v| v.as_str())
            .unwrap_or(&model_id)
            .to_string();
        let sw_version = deploy
            .pointer("/data/appInfo/softVer")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let device_type = deploy
            .pointer("/data/appInfo/DeviceType")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let meta = Metadata {
            model_id,
            model_name,
            device_type,
            sw_version,
        };

        let device_slot: Arc<Mutex<Option<Arc<ConnectedDevice>>>> = Arc::new(Mutex::new(None));
        let slot_emit = device_slot.clone();
        let emit = Arc::new(move |buf: Vec<u8>| {
            if let Some(dev) = slot_emit.lock().as_ref() {
                dev.notify_data(&buf);
            }
        });

        let broker = self.broker.clone();
        let did = device_id.to_string();
        let slot_send = device_slot.clone();
        let send_to = Arc::new(move |msg: SendToDevice| {
            if let Some(d) = slot_send.lock().as_ref() {
                d.notify_send(msg.clone());
            }
            let mid = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let messagestr = match msg {
                SendToDevice::T2Packet(buf) => serde_json::json!({
                    "did": did,
                    "mid": mid,
                    "cmd": "packet",
                    "type": 1,
                    "data": rethink_util::hex::encode(&buf),
                }),
                SendToDevice::T2Clip {
                    cmd,
                    msg_type,
                    data,
                } => serde_json::json!({
                    "did": did,
                    "mid": mid,
                    "cmd": cmd,
                    "type": msg_type,
                    "data": data,
                }),
                SendToDevice::T1Json(_) => return,
            };
            broker.publish(
                PublishPacket {
                    topic: format!("lime/devices/{did}"),
                    payload: serde_json::to_vec(&messagestr).unwrap_or_default(),
                    retain: false,
                    qos: 0,
                    dup: false,
                },
                None,
            );
        });

        let dev =
            ConnectedDevice::new(device_id.to_string(), Platform::Thinq2, meta, emit, send_to);
        *device_slot.lock() = Some(dev.clone());

        let mut cm = self.broker.client_meta(client_id);
        cm.has_device = true;
        cm.device_id = Some(device_id.to_string());
        self.broker.set_client_meta(client_id, cm);

        self.devices.lock().insert(client_id, dev.clone());
        self.manager.accept(dev);
    }

    fn time_sync_request(&self, client_id: u64) {
        let meta = self.broker.client_meta(client_id);
        let Some(device_id) = meta
            .deploy_msg
            .as_ref()
            .and_then(|d| d.get("did"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        else {
            return;
        };
        let now = chrono::Utc::now();
        let mut buf = [0u8; 7];
        buf[0] = (now.year() % 100) as u8;
        buf[1] = now.month0() as u8;
        buf[2] = now.day() as u8;
        buf[3] = now.hour() as u8;
        buf[4] = now.minute() as u8;
        buf[5] = now.second() as u8;
        buf[6] = now.weekday().num_days_from_sunday() as u8;
        let mid = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let payload = serde_json::json!({
            "did": device_id,
            "mid": mid,
            "cmd": "resp_timesync",
            "type": 1,
            "data": base64::engine::general_purpose::STANDARD.encode(buf),
        });
        self.broker.publish(
            PublishPacket {
                topic: format!("lime/devices/{device_id}"),
                payload: serde_json::to_vec(&payload).unwrap_or_default(),
                retain: false,
                qos: 0,
                dup: false,
            },
            None,
        );
    }

    fn disconnected(&self, client_id: u64) {
        if let Some(dev) = self.devices.lock().remove(&client_id) {
            self.clients_by_id.lock().remove(&dev.id);
            dev.notify_close();
        }
    }
}

/// Normalize broker topics to `clip/...` without allocating when already relative.
fn clip_relative_topic(topic: &str) -> Cow<'_, str> {
    if let Some(idx) = topic.find("/clip") {
        Cow::Owned(format!("clip{}", &topic[idx + 5..]))
    } else if let Some(idx) = topic.find("clip/") {
        Cow::Borrowed(&topic[idx..])
    } else {
        Cow::Borrowed(topic)
    }
}

fn topic_ends_with_device(prefix: &str, topic: &str, did: &str) -> bool {
    topic.strip_prefix(prefix).is_some_and(|rest| rest == did)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_relative_topic_strips_broker_prefix() {
        assert_eq!(
            clip_relative_topic("v1/clip/message/devices/abc").as_ref(),
            "clip/message/devices/abc"
        );
        assert_eq!(
            clip_relative_topic("clip/message/devices/abc").as_ref(),
            "clip/message/devices/abc"
        );
        assert!(topic_ends_with_device(
            "clip/message/devices/",
            "clip/message/devices/abc",
            "abc"
        ));
        assert!(!topic_ends_with_device(
            "clip/message/devices/",
            "clip/message/devices/abc",
            "ab"
        ));
        assert!(topic_ends_with_device(
            "clip/provisioning/devices/",
            "clip/provisioning/devices/dev1",
            "dev1"
        ));
    }
}
