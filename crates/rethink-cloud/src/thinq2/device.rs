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

/// Firmware string for Home Assistant. An empty `softVer` falls back to `appVersion`
/// (2REBGLUB_2P__ on clip_bkn_v1.9.238 reports firmware only there).
fn deploy_sw_version(deploy: &serde_json::Value) -> Option<String> {
    fn nonempty(v: Option<&serde_json::Value>) -> Option<String> {
        v.and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    nonempty(deploy.pointer("/data/appInfo/softVer"))
        .or_else(|| nonempty(deploy.pointer("/data/appInfo/appVersion")))
}

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
            // completeProvisioning_ack is a no-op: some RTL CLIP firmware
            // (e.g. T17A1EFHU_F) never sends it. Registration is on deploy/preDeploy.
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
            // Master 8f6d19e: register immediately here so start()/status query
            // can run before the appliance speaks.
            if self.devices.lock().get(&client_id).is_none() {
                self.complete_provisioning(did, client_id);
            } else {
                eprintln!("device {did} already set up");
            }
        }
    }

    fn complete_provisioning(&self, device_id: &str, client_id: u64) {
        let meta_c = self.broker.client_meta(client_id);
        let Some(deploy) = meta_c.deploy_msg.clone() else {
            eprintln!("completeProvisioning without deploy/preDeploy");
            return;
        };
        // Same MQTT client already provisioned — only re-bind if manager lost the device
        // (e.g. pre-fix reconnect race). Otherwise ignore a duplicate deploy.
        if meta_c.has_device {
            if self.manager.get(device_id).is_some() && self.devices.lock().contains_key(&client_id)
            {
                return;
            }
            eprintln!(
                "device {device_id}: deploy on already-flagged client but missing from manager — re-registering"
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
        let sw_version = deploy_sw_version(&deploy);
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
                SendToDevice::T2Packet(buf) => clip_packet_json(&did, mid, &buf),
                SendToDevice::T2Ack(buf) => clip_outbound_json("ack", &did, mid, &buf),
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
        dev.set_deploy(deploy);
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

/// CLIP `cmd=packet` payload. Some firmware (F_C__Y___W.A__QEUK) ignores lowercase hex.
fn encode_clip_packet_hex(buf: &[u8]) -> String {
    rethink_util::hex::encode_upper(buf)
}

fn clip_outbound_json(cmd: &str, did: &str, mid: u64, buf: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "did": did,
        "mid": mid,
        "cmd": cmd,
        "type": 1,
        "data": encode_clip_packet_hex(buf),
    })
}

fn clip_packet_json(did: &str, mid: u64, buf: &[u8]) -> serde_json::Value {
    clip_outbound_json("packet", did, mid, buf)
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
    use crate::devmgr::DeviceManager;
    use crate::mqtt_broker::Broker;

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

    #[test]
    fn clip_outbound_packet_hex_is_uppercase() {
        assert_eq!(encode_clip_packet_hex(&[0xaa, 0x0e, 0xf0]), "AA0EF0");
        assert_eq!(encode_clip_packet_hex(&[0xde, 0xad]), "DEAD");
        assert!(!encode_clip_packet_hex(&[0xaa])
            .chars()
            .any(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn clip_outbound_ack_is_not_a_packet() {
        let buf = [0xaa, 0x08, 0xf0, 0x00, 0x4d, 0x04, 0xa6, 0xbb];
        let ack = clip_outbound_json("ack", "dev-1", 7, &buf);
        assert_eq!(ack["cmd"], "ack");
        assert_eq!(ack["type"], 1);
        assert_eq!(ack["did"], "dev-1");
        assert_eq!(ack["data"], "AA08F0004D04A6BB");
        assert_eq!(clip_packet_json("dev-1", 7, &buf)["cmd"], "packet");
    }

    fn clip_acceptor() -> (Arc<Broker>, Arc<DeviceManager>, Arc<DeviceAcceptor>) {
        let broker = Arc::new(Broker::new());
        let manager = DeviceManager::new();
        let acc = DeviceAcceptor::new(broker.clone(), manager.clone());
        (broker, manager, acc)
    }

    fn deploy_json(did: &str) -> serde_json::Value {
        serde_json::json!({
            "did": did,
            "cmd": "deploy",
            "kind": "T17A1EFHU_F",
            "data": {
                "appInfo": {
                    "modelName": "T17A1EFHU_F",
                    "softVer": "1.0",
                    "DeviceType": "201"
                }
            }
        })
    }

    #[test]
    fn registers_on_deploy_without_provisioning_ack() {
        let (_broker, manager, acc) = clip_acceptor();
        let did = "rtl-no-ack";
        acc.handle_mqtt(
            &format!("clip/provisioning/devices/{did}"),
            &deploy_json(did),
            7,
        );
        let dev = manager
            .get(did)
            .expect("master 8f6d19e registers immediately on deploy/preDeploy");
        assert_eq!(dev.meta.model_id, "T17A1EFHU_F");
        assert_eq!(dev.meta.model_name, "T17A1EFHU_F");
        assert_eq!(dev.meta.sw_version.as_deref(), Some("1.0"));

        acc.handle_mqtt(
            &format!("clip/message/devices/{did}"),
            &serde_json::json!({
                "did": did,
                "cmd": "completeProvisioning_ack",
            }),
            7,
        );
        assert!(
            Arc::ptr_eq(&dev, &manager.get(did).unwrap()),
            "ack is a no-op and must not replace the device"
        );

        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        dev.add_data_handler(move |_| {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        acc.handle_mqtt(
            &format!("clip/message/devices/{did}"),
            &serde_json::json!({
                "did": did,
                "cmd": "device_packet",
                "data": "AA0EF0ED1121010000001800B5BB",
            }),
            7,
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "device_packet after deploy must reach the registered device"
        );
        assert!(
            Arc::ptr_eq(&dev, &manager.get(did).unwrap()),
            "device_packet must not replace the registered device"
        );
    }

    #[test]
    #[test]
    fn deploy_payload_is_forwarded_without_rac_placeholders() {
        let (_broker, manager, acc) = clip_acceptor();
        let did = "real-deploy";
        let body = serde_json::json!({
            "did": did,
            "cmd": "deploy",
            "kind": "OVEN",
            "data": {
                "appInfo": {
                    "modelName": "OVEN",
                    "protocolVer": "4.9",
                    "softVer": "424242",
                    "panelCode": "ZZ"
                }
            }
        });
        acc.handle_mqtt(&format!("clip/provisioning/devices/{did}"), &body, 1);
        let dev = manager.get(did).expect("registered");
        let stored = dev.deploy_payload().expect("deploy stored");
        let local = crate::bridge_adapter::ConnectedAsLocal(dev);
        let forwarded = rethink_bridge::pair::upstream_pre_deploy(
            7,
            did,
            "OVEN",
            rethink_bridge::LocalDevice::deploy_payload(&local).as_ref(),
        );
        assert_eq!(stored["data"]["appInfo"]["protocolVer"], "4.9");
        assert_eq!(forwarded["data"]["appInfo"]["protocolVer"], "4.9");
        assert_eq!(forwarded["data"]["appInfo"]["panelCode"], "ZZ");
        assert_eq!(forwarded["data"]["appInfo"]["softVer"], "424242");
        assert!(forwarded["data"].get("platformInfo").is_none());
        assert!(!forwarded.to_string().contains("690409"));
    }

    fn pre_deploy_also_registers() {
        let (_broker, manager, acc) = clip_acceptor();
        let did = "pre-deploy-id";
        let mut body = deploy_json(did);
        body["cmd"] = serde_json::json!("preDeploy");
        acc.handle_mqtt(&format!("clip/provisioning/devices/{did}"), &body, 4);
        assert!(manager.get(did).is_some());
    }

    #[test]
    fn deploy_sw_version_prefers_soft_ver_over_app_version() {
        let (_broker, manager, acc) = clip_acceptor();
        let did = "softver-wins";
        let body = serde_json::json!({
            "did": did,
            "cmd": "deploy",
            "kind": "2REBGLUB_2P__",
            "data": {
                "appInfo": {
                    "modelName": "M",
                    "softVer": "1.2.3",
                    "appVersion": "clip_x",
                    "DeviceType": "101"
                }
            }
        });
        acc.handle_mqtt(&format!("clip/provisioning/devices/{did}"), &body, 1);
        let dev = manager.get(did).expect("registered");
        assert_eq!(dev.meta.model_id, "2REBGLUB_2P__");
        assert_eq!(dev.meta.model_name, "M");
        assert_eq!(dev.meta.device_type.as_deref(), Some("101"));
        assert_eq!(dev.meta.sw_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn deploy_sw_version_falls_back_to_app_version_when_soft_ver_is_empty() {
        let (_broker, manager, acc) = clip_acceptor();
        let did = "empty-softver";
        // Captured from a 2REBGLUB_2P__ (GBBSJ2CCEP) deploy: softVer empty, firmware in appVersion.
        let body = serde_json::json!({
            "did": did,
            "cmd": "deploy",
            "kind": "2REBGLUB_2P__",
            "data": {
                "appInfo": {
                    "modelName": "2REBGLUB_2P__",
                    "softVer": "",
                    "appVersion": "clip_bkn_v1.9.238",
                    "DeviceType": "101"
                }
            }
        });
        acc.handle_mqtt(&format!("clip/provisioning/devices/{did}"), &body, 1);
        let dev = manager.get(did).expect("registered");
        assert_eq!(dev.meta.sw_version.as_deref(), Some("clip_bkn_v1.9.238"));
    }

    #[test]
    fn device_packet_without_deploy_is_ignored() {
        let (_broker, manager, acc) = clip_acceptor();
        acc.handle_mqtt(
            "clip/message/devices/orphan",
            &serde_json::json!({
                "did": "orphan",
                "cmd": "device_packet",
                "data": "AA04BB",
            }),
            3,
        );
        assert!(manager.get("orphan").is_none());
    }

    #[test]
    fn outbound_clip_packet_json_uses_uppercase_hex() {
        let body = clip_packet_json("dev-x", 42, &[0xaa, 0x0e, 0xf0, 0xed]);
        assert_eq!(body["cmd"], "packet");
        assert_eq!(body["type"], 1);
        assert_eq!(body["did"], "dev-x");
        assert_eq!(body["data"], "AA0EF0ED");
        let data = body["data"].as_str().unwrap();
        assert!(!data.chars().any(|c| c.is_ascii_lowercase()));
    }
}
