//! T1789EFH_F top-load washer (AABB).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Arc;

fn status_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Fill / Sense"),
        (0x02, "Paused"),
        (0x03, "Wash (initial)"),
        (0x05, "Wash (main)"),
        (0x06, "Rinse / Drain"),
        (0x07, "Rinse / Drain"),
        (0x08, "Spin"),
    ])
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        // Unacked, the WT7300CW repeats frames and never streams 0xEC records.
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self { core: core.clone() });

        let mut options: Vec<&str> = status_map().values().copied().collect();
        options.sort();
        options.dedup();
        // Keep insertion order of unique values as in TS [...new Set(Object.values(STATUS))]
        let opts: Vec<&str> = {
            let m = status_map();
            let order = [0x00u8, 0x01, 0x02, 0x03, 0x05, 0x06, 0x07, 0x08];
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for k in order {
                if let Some(v) = m.get(&k) {
                    if seen.insert(*v) {
                        out.push(*v);
                    }
                }
            }
            out
        };

        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        let mut components = Map::new();
        components.insert(
            "power".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "icon": "mdi:washing-machine",
                "device_class": "running",
            }),
        );
        components.insert(
            "status".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "icon": "mdi:state-machine",
                "device_class": "enum",
                "options": opts,
            }),
        );
        components.insert(
            "remaining_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-remaining_time",
                "state_topic": "$this/remaining_time",
                "name": "Remaining time",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        );
        base.components = components.into_iter().collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_record(&self, rec: &[u8]) {
        if rec.len() < 5 {
            return;
        }
        let phase = rec[2];
        let mins = rec[4];
        let status = status_map().get(&phase).copied().unwrap_or("None");
        self.core.publish_on_off("power", phase != 0);
        self.core.publish_property("status", status.into());
        self.core
            .publish_property("remaining_time", (mins as i64).into());
    }

    fn process_aabb(&self, buf: &[u8]) {
        // 27-byte records. 0xEC carries previous then current; read the current one.
        if let Some(rec) = process_common_status(buf, 0x20, 27) {
            self.process_record(rec);
        }
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        // Read-only family status query. Actuating commands are 0xF0E5.
        self.core.send(&hex_decode("F0ED1121010000001800"));
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
    }
    fn publish_config(&self) {
        self.core.republish_config();
    }
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{
        hex_decode, hex_encode, Metadata, MockHaConnection, MockThinq2Device, Thinq2Device,
    };

    const DEVICE_ID: &str = "test-id";

    fn meta() -> Metadata { Metadata::new("T1789EFH_F", "LG WT7300CW", "1.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    #[test]
    fn config_components() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("power"));
        assert!(comps.contains_key("status"));
        assert!(comps.contains_key("remaining_time"));
        let opts = comps["status"]["options"].as_array().unwrap();
        assert!(opts.iter().any(|v| v == "Wash (main)"));
    }

    #[test]
    fn eb_and_ec_frames() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA2120EB00000000000000000000000000000000000000000000000000000000BB"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));

        thinq.emit_data(&hex_decode("AA2120EB000005001E0000000000000000000000000000000000000000000000BB"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Wash (main)"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("30"));

        thinq.emit_data(&hex_decode(
            "AA3C20EC0019050018011A0200050304000000000410000000050000006400001906001D011E0200000304000000000410000000050000006400FABB"
        ));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinse / Drain"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("29"));

        thinq.emit_data(&hex_decode(
            "AA3C20EC001902002B00340800030104000000000410000000050000006400001902002B00340800030104000000000010000000050000006400A9BB"
        ));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("43"));
    }

    #[test]
    fn ignores_wrong_frames() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA2130EB00000000000000000000000000000000000000000000000000000000BB"));
        assert!(prop(&ha, "power").is_none());
        thinq.emit_data(&hex_decode("AA0720D80EE2BB"));
        assert!(prop(&ha, "power").is_none());
        thinq.emit_data(&hex_decode("AA2120EB0000FF00000000000000000000000000000000000000000000000000BB"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
    }

    #[test]
    fn start_requests_status() {
        let (_ha, thinq, dev) = make();
        DeviceHandler::start(dev.as_ref());
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(sent, vec!["AA0EF0ED1121010000001800B5BB".to_string()]);
    }

    #[test]
    fn acks_heartbeat_and_settings_but_not_status_records() {
        let (_ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA09207200000010BB"));
        thinq.emit_data(&hex_decode("AA0720D8219FBB"));
        thinq.emit_data(&hex_decode(
            "AA2120E2031903003B003A010003030400000000400000000201000000640046BB",
        ));
        let acks: Vec<String> = thinq
            .sent()
            .iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            acks,
            vec![
                "AA08F00072044DBB".to_string(),
                "AA08F000D8042BBB".to_string(),
                "AA08F000E204DDBB".to_string(),
            ]
        );
        assert!(thinq.outbox().is_empty());

        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(
            "AA3C20EC0019050018011A0200050304000000000410000000050000006400001906001D011E0200000304000000000410000000050000006400FABB",
        ));
        assert!(thinq.sent().iter().all(|m| m.cmd != "ack"));
    }
}
