//! WLREL6323S — LG LREL6323S electric range (AABB class 0x40).
//!
//! Same 62-byte 0x40EB/EC record as WFV474PGV but read-only: single cavity,
//! door bit 0x08 at offset 11, and one aggregate cooktop status (normal
//! elements set bytes 36 and 38, the warming zone 37 and 39).
//!
//! Port of upstream commit eedaf3a (PR #159).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
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
const COOKTOP_OFFSETS: [usize; 4] = [36, 37, 38, 39];
const STATUS_QUERY: &str =
    "F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000";

const OVEN_STATES: &[(u8, &str)] = &[
    (0x00, "Idle"),
    (0x01, "Preheating"),
    (0x02, "Cooking"),
    (0x04, "Cooling"),
    (0x09, "Cleaning"),
];

const OVEN_MODES: &[(u8, &str)] = &[
    (0x00, "None"),
    (0x01, "Bake"),
    (0x03, "Convection Bake"),
    (0x04, "Convection Roast"),
    (0x07, "Broil"),
    (0x08, "Warm"),
    (0x0d, "EasyClean"),
    (0x0f, "Self Clean"),
    (0x10, "Air Fry"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

// HA rejects values outside the declared options, so an undecoded byte publishes "None".
fn oven_state(value: u8) -> Option<&'static str> {
    let state = named(OVEN_STATES, value);
    if state.is_none() {
        tracing::debug!("WLREL6323S undecoded oven state {value:x}");
    }
    state
}

fn oven_mode(value: u8) -> Option<&'static str> {
    let mode = named(OVEN_MODES, value);
    if mode.is_none() {
        tracing::debug!("WLREL6323S undecoded oven mode {value:x}");
    }
    mode
}

fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
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
                "oven_status".into(),
                json!({
                    "platform": "sensor",
                    "device_class": "enum",
                    "icon": "mdi:stove",
                    "unique_id": "$deviceid-oven_status",
                    "state_topic": "$this/oven_status",
                    "name": "Oven Status",
                    "options": labels(OVEN_STATES),
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
                    "options": labels(OVEN_MODES),
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
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        match process_common_status(buf, CLASS_BYTE, STATUS_RECORD_LEN) {
            Some(current) => self.publish_status(current),
            None => tracing::debug!("WLREL6323S undecoded frame {:02x?}", buf),
        }
    }

    fn publish_status(&self, current: &[u8]) {
        self.core.publish_property(
            "oven_status",
            oven_state(current[OVEN_STATE]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "oven_mode",
            oven_mode(current[OVEN_MODE]).unwrap_or("None").into(),
        );
        // Zero is no setpoint (nothing programmed, or a mode without one), not a reading.
        let set = be16(current, OVEN_SET_TEMP);
        if set == 0 {
            self.core
                .publish_property("oven_set_temperature", "None".into());
        } else {
            self.core
                .publish_property("oven_set_temperature", i64::from(set).into());
        }
        self.core
            .publish_on_off("oven_door", current[OVEN_DOOR_FLAGS] & OVEN_DOOR_OPEN != 0);
        let cooktop = COOKTOP_OFFSETS.iter().any(|&off| current[off] != 0);
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
    use std::collections::BTreeMap;

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("WLREL6323S", Device::new)
    }

    fn props(ha: &rethink_core::MockHaConnection) -> BTreeMap<String, String> {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.as_string()))
            .collect()
    }

    // Real captures from the physical panel, 2026-08-30. EC frames carry the previous 62-byte
    // record followed by the current one.
    const IDLE_INITIAL_STATUS: &str =
        "AA4440EB0000000000000000000000000000000E0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000072BB";
    // Left-front, left-rear and right-front elements all produced this same aggregate transition.
    const COOKTOP_OFF_TO_ON: &str =
        "AA8240EC0000000000000000000000000000000E0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000001000100000000000000000000000000000000000000000000003BBB";
    const COOKTOP_ON_TO_OFF: &str =
        "AA8240EC00000000000000000000000000000006000000000000000000000000000000000000000001000100000000000000000000000000000000000000000000000000000000000000000000000000000E000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003BBB";
    // The center-rear warming zone sets the adjacent pair of aggregate bytes (37 and 39).
    const WARMING_ZONE_OFF_TO_ON: &str =
        "AA8240EC0000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000010001000000000000000000000000000000000000000000000033BB";
    const OVEN_DOOR_CLOSED_TO_OPEN: &str =
        "aa8240ec0000000000000000000000000000000e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080000000e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029bb";
    const OVEN_DOOR_OPEN_TO_CLOSED: &str =
        "aa8240ec0000000000000000000000080000000e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029bb";
    // Panel-started Bake at 350°F (0x015E), cancelled ten seconds later.
    const BAKE_350_PREHEATING: &str =
        "aa8240ec00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000101000000015e0000000000000000230000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000089bb";
    const BAKE_CANCELLED: &str =
        "aa8240ec0101000000015e000000000000000023000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000bfbb";
    // Further panel-started modes, 2026-09-07.
    const CONVECTION_BAKE_PREHEATING: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010300000001450000000000000000230000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090bb";
    const CONVECTION_ROAST_PREHEATING: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010400000001450000000000000000230000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000093bb";
    const AIR_FRY_COOKING: &str =
        "aa8240ec000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002100000000190000000000000000023000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004bbb";
    const BROIL_COOKING: &str =
        "aa8240ec00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000207000000000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f6bb";
    const WARM_COOKING: &str =
        "aa8240ec00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000208000000000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f1bb";
    const EASY_CLEAN: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090d000a00000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000efbb";
    const SELF_CLEAN: &str =
        "aa8240ec0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090f000004000000000000000000004200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e3bb";
    const SELF_CLEAN_CANCELLED: &str =
        "aa8240ec090f313b0300000000000001000000420000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000100000002000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000007cbb";

    #[test]
    fn publishes_read_only_oven_and_aggregate_cooktop_components() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        assert_eq!(comps["cooktop_status"]["platform"], "binary_sensor");
        assert_eq!(comps["oven_status"]["device_class"], "enum");
        assert_eq!(
            comps["oven_status"]["options"],
            json!(["Idle", "Preheating", "Cooking", "Cooling", "Cleaning"])
        );
        assert_eq!(comps["oven_mode"]["device_class"], "enum");
        assert_eq!(
            comps["oven_mode"]["options"],
            json!([
                "None",
                "Bake",
                "Convection Bake",
                "Convection Roast",
                "Broil",
                "Warm",
                "EasyClean",
                "Self Clean",
                "Air Fry",
            ])
        );
        assert_eq!(comps["oven_set_temperature"]["unit_of_measurement"], "°F");
        assert_eq!(comps["oven_door"]["device_class"], "door");
        assert!(comps["oven_door"].get("icon").is_none());
        assert!(comps["oven_status"].get("command_topic").is_none());
    }

    #[test]
    fn start_sends_the_captured_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA28F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000F3BB"
        );
    }

    #[test]
    fn decodes_the_initial_idle_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(IDLE_INITIAL_STATUS));
        let expected: BTreeMap<String, String> = [
            ("oven_status", "Idle"),
            ("oven_mode", "None"),
            ("oven_set_temperature", "None"),
            ("oven_door", "OFF"),
            ("cooktop_status", "OFF"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(props(&ha), expected);
    }

    #[test]
    fn decodes_aggregate_cooktop_transitions_from_the_current_ec_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(COOKTOP_OFF_TO_ON));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(COOKTOP_ON_TO_OFF));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("OFF"));
    }

    #[test]
    fn includes_the_warming_zone_in_aggregate_cooktop_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(WARMING_ZONE_OFF_TO_ON));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("ON"));
    }

    #[test]
    fn decodes_oven_door_open_and_closed_transitions() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(OVEN_DOOR_CLOSED_TO_OPEN));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(OVEN_DOOR_OPEN_TO_CLOSED));
        assert_eq!(prop(&ha, "oven_door").as_deref(), Some("OFF"));
    }

    #[test]
    fn decodes_panel_started_bake_at_350_and_cancellation() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(BAKE_350_PREHEATING));
        assert_eq!(prop(&ha, "oven_status").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("350"));

        thinq.emit_data(&hex_decode(BAKE_CANCELLED));
        assert_eq!(prop(&ha, "oven_status").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("None"));
    }

    #[test]
    fn decodes_every_captured_cooking_mode() {
        let (ha, thinq, _) = make();
        for (packet, status, mode, temperature) in [
            (
                CONVECTION_BAKE_PREHEATING,
                "Preheating",
                "Convection Bake",
                "325",
            ),
            (
                CONVECTION_ROAST_PREHEATING,
                "Preheating",
                "Convection Roast",
                "325",
            ),
            (AIR_FRY_COOKING, "Cooking", "Air Fry", "400"),
            (BROIL_COOKING, "Cooking", "Broil", "None"),
            (WARM_COOKING, "Cooking", "Warm", "None"),
            (EASY_CLEAN, "Cleaning", "EasyClean", "None"),
            (SELF_CLEAN, "Cleaning", "Self Clean", "None"),
            (SELF_CLEAN_CANCELLED, "Cooling", "None", "None"),
        ] {
            thinq.emit_data(&hex_decode(packet));
            assert_eq!(prop(&ha, "oven_status").as_deref(), Some(status), "{mode}");
            assert_eq!(prop(&ha, "oven_mode").as_deref(), Some(mode));
            assert_eq!(
                prop(&ha, "oven_set_temperature").as_deref(),
                Some(temperature),
                "{mode}"
            );
        }
    }

    #[test]
    fn does_not_mask_unknown_oven_mode_bits_into_a_known_mode() {
        assert_eq!(oven_mode(0x81), None);
    }

    #[test]
    fn ignores_frames_outside_the_envelope_and_unexpected_status_lengths() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("001122"));
        thinq.emit_data(&hex_decode("AA0840EB000073BB"));
        assert!(props(&ha).is_empty());
    }
}
