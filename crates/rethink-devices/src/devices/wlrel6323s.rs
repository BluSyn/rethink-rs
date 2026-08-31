//! WLREL6323S — LG LREL6323S electric range (AABB class 0x40).
//!
//! Port of unmerged upstream PR #159. Same 62-byte 0x40EB/EC record as
//! WFV474PGV but read-only: single cavity, door bit 0x08 at offset 11,
//! aggregate cooktop flags at 36 and 38. Writes stay disabled until command
//! shapes are captured.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x40;
const STATUS_RECORD_LEN: usize = 62;
const OVEN_STATE: usize = 0;
const OVEN_MODE: usize = 1;
const OVEN_SET_TEMP: usize = 5;
const OVEN_DOOR_FLAGS: usize = 11;
const OVEN_DOOR_OPEN: u8 = 0x08;
const COOKTOP_OFFSETS: [usize; 2] = [36, 38];
const STATUS_QUERY: &str =
    "F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000";

fn oven_state(v: u8) -> &'static str {
    match v {
        0x00 => "Idle",
        0x01 => "Preheating",
        _ => "unknown",
    }
}

fn oven_mode(v: u8) -> &'static str {
    match v & 0x7f {
        0x00 => "None",
        0x01 => "Bake",
        _ => "unknown",
    }
}

fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Electric Range"})));
        cfg.components = [
            (
                "cooktop_status".into(),
                json!({
                    "platform": "binary_sensor",
                    "icon": "mdi:stove",
                    "unique_id": "$deviceid-cooktop_status",
                    "state_topic": "$this/cooktop_status",
                    "name": "Cooktop",
                    "payload_on": "ON",
                    "payload_off": "OFF",
                }),
            ),
            (
                "oven_state".into(),
                json!({
                    "platform": "sensor",
                    "device_class": "enum",
                    "icon": "mdi:stove",
                    "unique_id": "$deviceid-oven_state",
                    "state_topic": "$this/oven_state",
                    "name": "Oven State",
                    "options": ["Idle", "Preheating"],
                }),
            ),
            (
                "oven_mode".into(),
                json!({
                    "platform": "sensor",
                    "device_class": "enum",
                    "icon": "mdi:chef-hat",
                    "unique_id": "$deviceid-oven_mode",
                    "state_topic": "$this/oven_mode",
                    "name": "Oven Mode",
                    "options": ["None", "Bake"],
                }),
            ),
            (
                "oven_set_temperature".into(),
                json!({
                    "platform": "sensor",
                    "device_class": "temperature",
                    "icon": "mdi:thermometer-chevron-up",
                    "unique_id": "$deviceid-oven_set_temperature",
                    "state_topic": "$this/oven_set_temperature",
                    "name": "Oven Set Temperature",
                    "unit_of_measurement": "°F",
                }),
            ),
            (
                "oven_door".into(),
                json!({
                    "platform": "binary_sensor",
                    "device_class": "door",
                    "icon": "mdi:door",
                    "unique_id": "$deviceid-oven_door",
                    "state_topic": "$this/oven_door",
                    "name": "Oven Door",
                    "payload_on": "ON",
                    "payload_off": "OFF",
                }),
            ),
        ]
        .into_iter()
        .collect();
        core.set_config(cfg);

        let this = Arc::new(Self { core: core.clone() });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 2 || buf[0] != CLASS_BYTE {
            return;
        }
        if buf[1] == 0xeb && buf.len() == 2 + STATUS_RECORD_LEN {
            self.publish_status(&buf[2..]);
        } else if buf[1] == 0xec && buf.len() == 2 + STATUS_RECORD_LEN * 2 {
            self.publish_status(&buf[2 + STATUS_RECORD_LEN..]);
        }
    }

    fn publish_status(&self, current: &[u8]) {
        if current.len() < STATUS_RECORD_LEN {
            return;
        }
        self.core
            .publish_property("oven_state", oven_state(current[OVEN_STATE]).into());
        self.core
            .publish_property("oven_mode", oven_mode(current[OVEN_MODE]).into());
        let set = be16(current, OVEN_SET_TEMP);
        if set == 0 {
            self.core.publish_property("oven_set_temperature", "unknown".into());
        } else {
            self.core
                .publish_property("oven_set_temperature", i64::from(set).into());
        }
        self.core
            .publish_on_off("oven_door", current[OVEN_DOOR_FLAGS] & OVEN_DOOR_OPEN != 0);
        let cooktop = COOKTOP_OFFSETS
            .iter()
            .any(|&off| current.get(off).copied().unwrap_or(0) != 0);
        self.core.publish_on_off("cooktop_status", cooktop);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_QUERY));
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, _prop: &str, _value: &str) {}
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
    use crate::device_trait::DeviceHandler;
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("WLREL6323S", Device::new)
    }

    const IDLE_INITIAL_STATUS: &str =
        "AA4440EB0000000000000000000000000000000E0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000072BB";
    const COOKTOP_OFF_TO_ON: &str =
        "AA8240EC0000000000000000000000000000000E0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000001000100000000000000000000000000000000000000000000003BBB";
    const COOKTOP_ON_TO_OFF: &str =
        "AA8240EC00000000000000000000000000000006000000000000000000000000000000000000000001000100000000000000000000000000000000000000000000000000000000000000000000000000000E000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003BBB";
    const OVEN_DOOR_CLOSED_TO_OPEN: &str =
        "aa8240ec0000000000000000000000000000000e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080000000e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029bb";
    const OVEN_DOOR_OPEN_TO_CLOSED: &str =
        "aa8240ec0000000000000000000000080000000e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029bb";
    const BAKE_350_PREHEATING: &str =
        "aa8240ec00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000101000000015e0000000000000000230000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000089bb";
    const BAKE_CANCELLED: &str =
        "aa8240ec0101000000015e000000000000000023000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000bfbb";

    #[test]
    fn publishes_read_only_components() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        assert_eq!(comps["cooktop_status"]["platform"], "binary_sensor");
        assert_eq!(comps["oven_state"]["device_class"], "enum");
        assert_eq!(comps["oven_state"]["options"], json!(["Idle", "Preheating"]));
        assert_eq!(comps["oven_mode"]["options"], json!(["None", "Bake"]));
        assert_eq!(comps["oven_set_temperature"]["unit_of_measurement"], "°F");
        assert_eq!(comps["oven_door"]["device_class"], "door");
        assert!(comps["oven_state"].get("command_topic").is_none());
    }

    #[test]
    fn start_sends_captured_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA28F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000F3BB"
        );
    }

    #[test]
    fn idle_cooktop_door_and_bake() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(IDLE_INITIAL_STATUS));
        assert_eq!(prop(&ha, "oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("unknown"));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(COOKTOP_OFF_TO_ON));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(COOKTOP_ON_TO_OFF));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(OVEN_DOOR_CLOSED_TO_OPEN));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(OVEN_DOOR_OPEN_TO_CLOSED));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(BAKE_350_PREHEATING));
        assert_eq!(prop(&ha, "oven_state").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("350"));
        thinq.emit_data(&hex_decode(BAKE_CANCELLED));
        assert_eq!(prop(&ha, "oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("unknown"));
    }

    #[test]
    fn ignores_bad_frames() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("001122"));
        thinq.emit_data(&hex_decode("AA0840EB000073BB"));
        assert!(prop(&ha, "oven_state").is_none());
    }
}
