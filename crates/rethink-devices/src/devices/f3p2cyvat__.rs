//! F3P2CYVAT__ — LG WM3600HWA front-load washer (AABB).
//!
//! Port of upstream commit bd1b36a. Class `0x20`, 44-byte status records. `0xEC` carries a previous
//! record then the current one; `0xEB` is a single current-state record. A short `0xD8` heartbeat
//! frame publishes only the tub clean count. No write commands — read-only handler.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_FRAME_LEN: usize = 92;
const STATUS_RECORD_OFFSET: usize = 48;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_STATUS_FRAME_LEN: usize = 47;
const SINGLE_RECORD_OFFSET: usize = 3;
const RECORD_LEN: usize = 44;
const RECORD_MARKER: u8 = 0x2b;

const STATUS_REQUEST: &[u8] = &[
    0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x04, 0x13, 0x14, 0x00, 0x00,
];

const HEARTBEAT_FRAME_TYPE: u8 = 0xd8;
const HEARTBEAT_FRAME_LEN: usize = 3;
const HEARTBEAT_TCL_COUNT_OFFSET: usize = 2;

const SOIL_OFFSET: usize = 2;
const TEMP_OFFSET: usize = 3;
const RINSE_OFFSET: usize = 4;
const SPIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const DOWNLOADED_COURSE_ID_OFFSET: usize = 21;
const RESERVE_TIME_OFFSET: usize = 12;
const REMAIN_TIME_OFFSET: usize = 14;
const INITIAL_TIME_OFFSET: usize = 16;
const ERROR_OFFSET: usize = 20;
const STATE_OFFSET: usize = 22;
const RINSE_COUNT_OFFSET: usize = 28;
const TUB_CLEAN_COUNT_OFFSET: usize = 29;
const OPTIONS_A_OFFSET: usize = 35;
const OPTION_A_COLD_WASH: u8 = 0x04;
const OPTION_A_PRE_WASH: u8 = 0x40;
const STEAM_OFFSET: usize = 36;
const STEAM_ON: u8 = 0x10;
const OPTIONS_B_OFFSET: usize = 40;
const OPTION_B_EXTRA_RINSE: u8 = 0x40;
const OPTION_B_DELAY_ACTIVE: u8 = 0x80;
const AI_WASH_OFFSET: usize = 41;

const STATE_OFF: u8 = 0x00;

const STATES: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Standby"),
    (27, "Standby"),
    (0x02, "Paused"),
    (28, "Paused"),
    (3, "Detecting"),
    (5, "Filling"),
    (6, "Detergent amount"),
    (7, "Delayed"),
    (9, "Pre-wash"),
    (11, "Washing"),
    (29, "Washing"),
    (12, "Rinsing"),
    (13, "Rinse hold"),
    (14, "Spinning"),
    (15, "Drying"),
    (16, "End"),
    (21, "Refreshing"),
    (23, "Error"),
    (0x24, "Confirm Start"),
];

const COURSES: &[(u8, &str)] = &[
    (0x2e, "Normal"),
    (0x23, "Heavy Duty"),
    (0x0d, "Bedding"),
    (0x05, "Allergiene"),
    (0x55, "Tub Clean"),
    (0x37, "Rinse+Spin"),
    (0x4a, "Speed Wash"),
    (0x30, "Perm Press"),
    (0x16, "Delicates"),
    (0x4e, "Spin Only"),
    (0xff, "Downloaded"),
];

const SOIL: &[(u8, &str)] = &[
    (0, "Not Selected"),
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];

const SPIN: &[(u8, &str)] = &[
    (0, "Not Selected"),
    (12, "Drain Only"),
    (13, "Low"),
    (14, "Medium"),
    (15, "High"),
    (16, "Extra High"),
];

const TEMP: &[(u8, &str)] = &[
    (0, "Not Selected"),
    (13, "Tap Cold"),
    (14, "Cold"),
    (15, "Eco Warm"),
    (16, "Warm"),
    (17, "Warm Rinse"),
    (18, "Hot"),
    (19, "Extra Hot"),
];

const RINSE: &[(u8, &str)] = &[(14, "Normal"), (15, "Plus"), (16, "Plus 2"), (17, "Plus 3")];

const ERRORS: &[(u8, &str)] = &[
    (0, "OK"),
    (2, "IE"),
    (3, "OE"),
    (4, "UE"),
    (5, "FE"),
    (6, "AE"),
    (7, "PE"),
    (8, "tE"),
    (9, "LE"),
    (10, "CE"),
    (11, "dHE"),
    (13, "FF"),
    (14, "dCE"),
    (15, "EE"),
    (16, "LOE"),
    (19, "PS"),
    (20, "DE1 - Door Open"),
    (21, "dE2"),
    (23, "VS"),
    (35, "PF"),
    (41, "sud"),
    (43, "Ed1"),
    (44, "Ed2"),
    (45, "Ed3"),
    (46, "Ed4"),
    (47, "Ed5"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, name)| *name)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 2 || buf[0] != CLASS_BYTE {
            return;
        }
        // Heartbeat frame: nothing but the tub clean count.
        if buf[1] == HEARTBEAT_FRAME_TYPE && buf.len() == HEARTBEAT_FRAME_LEN {
            self.core
                .publish_property("tub_clean_count", PropertyValue::Int(buf[HEARTBEAT_TCL_COUNT_OFFSET] as i64));
            return;
        }
        if buf[1] == STATUS_FRAME_TYPE {
            self.process_status(buf, STATUS_RECORD_OFFSET, STATUS_FRAME_LEN);
        } else if buf[1] == SINGLE_STATUS_FRAME_TYPE {
            self.process_status(buf, SINGLE_RECORD_OFFSET, SINGLE_STATUS_FRAME_LEN);
        }
    }

    fn process_status(&self, buf: &[u8], record_offset: usize, expected_len: usize) {
        if buf.len() != expected_len {
            return;
        }
        let Some(rec) = buf.get(record_offset..record_offset + RECORD_LEN) else {
            return;
        };
        if rec[0] != RECORD_MARKER {
            return;
        }
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_OFF;

        self.core.publish_on_off("power", !is_off);
        self.core.publish_property("status", named(STATES, state).unwrap_or("None").into());
        self.publish_error(rec[ERROR_OFFSET]);

        let courseByte = rec[COURSE_OFFSET];
        let course = if courseByte == 0xff {
            named(COURSES, rec[DOWNLOADED_COURSE_ID_OFFSET])
                .map(|base| format!("Downloaded ({base})"))
                .unwrap_or_else(|| "Downloaded".to_string())
        } else {
            named(COURSES, courseByte).unwrap_or("None").to_string()
        };
        self.core.publish_property("course", course.into());

        self.core.publish_property(
            "remaining_time",
            PropertyValue::Int(if is_off { 0 } else { u16be(rec, REMAIN_TIME_OFFSET) }),
        );
        self.core.publish_property(
            "initial_time",
            PropertyValue::Int(if is_off { 0 } else { u16be(rec, INITIAL_TIME_OFFSET) }),
        );
        self.core.publish_property(
            "reserve_time",
            PropertyValue::Int(if is_off { 0 } else { u16be(rec, RESERVE_TIME_OFFSET) }),
        );
        self.core
            .publish_property("tub_clean_count", PropertyValue::Int(rec[TUB_CLEAN_COUNT_OFFSET] as i64));
        self.core
            .publish_property("rinse_count", PropertyValue::Int(rec[RINSE_COUNT_OFFSET] as i64));
        self.publish_named("soil", SOIL, rec[SOIL_OFFSET]);
        self.publish_named("spin", SPIN, rec[SPIN_OFFSET]);
        self.publish_named("temp", TEMP, rec[TEMP_OFFSET]);
        self.publish_named("rinse", RINSE, rec[RINSE_OFFSET]);

        let optionsA = rec[OPTIONS_A_OFFSET];
        self.core
            .publish_on_off("pre_wash", optionsA & OPTION_A_PRE_WASH != 0);
        self.core
            .publish_on_off("cold_wash", optionsA & OPTION_A_COLD_WASH != 0);

        self.core
            .publish_on_off("steam", rec[STEAM_OFFSET] == STEAM_ON);

        let optionsB = rec[OPTIONS_B_OFFSET];
        self.core
            .publish_on_off("extra_rinse", optionsB & OPTION_B_EXTRA_RINSE != 0);
        self.core
            .publish_on_off("delay_active", optionsB & OPTION_B_DELAY_ACTIVE != 0);

        self.core
            .publish_on_off("ai_wash", rec[AI_WASH_OFFSET] == 1);
    }

    fn publish_named(&self, prop: &str, table: &'static [(u8, &'static str)], code: u8) {
        self.core
            .publish_property(prop, named(table, code).unwrap_or("None").into());
    }

    fn publish_error(&self, code: u8) {
        self.core.publish_on_off("error", code != 0);
        let message = if code == 0 {
            "-".to_string()
        } else {
            named(ERRORS, code)
                .map(str::to_string)
                .unwrap_or_else(|| format!("Unknown error (0x{code:02X})"))
        };
        self.core.publish_property("error_message", message.into());
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
}

fn u16be(rec: &[u8], offset: usize) -> i64 {
    u16::from_be_bytes([rec[offset], rec[offset + 1]]) as i64
}

fn components() -> HashMap<String, serde_json::Value> {
    [
        (
            "power".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "icon": "mdi:washing-machine",
                "device_class": "running",
            }),
        ),
        (
            "status".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-status",
                "state_topic": "$this/status",
                "name": "Status",
                "icon": "mdi:state-machine",
                "device_class": "enum",
                "options": labels(STATES),
            }),
        ),
        (
            "error".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-error",
                "state_topic": "$this/error",
                "name": "Error",
                "icon": "mdi:check-circle",
                "device_class": "problem",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "error_message".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-error_message",
                "state_topic": "$this/error_message",
                "name": "Error message",
                "icon": "mdi:alert-circle-outline",
                "device_class": "enum",
                "entity_category": "diagnostic",
                "options": labels(ERRORS),
            }),
        ),
        (
            "course".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-course",
                "state_topic": "$this/course",
                "name": "Course",
                "icon": "mdi:pin-outline",
                "device_class": "enum",
                "options": labels(COURSES),
            }),
        ),
        (
            "remaining_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-remaining_time",
                "state_topic": "$this/remaining_time",
                "name": "Remaining time",
                "icon": "mdi:timer-outline",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "initial_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-initial_time",
                "state_topic": "$this/initial_time",
                "name": "Initial time",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "reserve_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-reserve_time",
                "state_topic": "$this/reserve_time",
                "name": "Reserve time",
                "icon": "mdi:timer-sand",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "tub_clean_count".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-tub_clean_count",
                "state_topic": "$this/tub_clean_count",
                "name": "Washes since Tub Clean",
                "icon": "mdi:counter",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "soil".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-soil",
                "state_topic": "$this/soil",
                "name": "Soil level",
                "icon": "mdi:liquid-spot",
                "device_class": "enum",
                "options": labels(SOIL),
            }),
        ),
        (
            "spin".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-spin",
                "state_topic": "$this/spin",
                "name": "Spin",
                "icon": "mdi:autorenew",
                "device_class": "enum",
                "options": labels(SPIN),
            }),
        ),
        (
            "temp".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-temp",
                "state_topic": "$this/temp",
                "name": "Temperature",
                "icon": "mdi:thermometer",
                "device_class": "enum",
                "options": labels(TEMP),
            }),
        ),
        (
            "rinse".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-rinse",
                "state_topic": "$this/rinse",
                "name": "Rinse",
                "icon": "mdi:water-sync",
                "device_class": "enum",
                "options": labels(RINSE),
            }),
        ),
        (
            "rinse_count".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-rinse_count",
                "state_topic": "$this/rinse_count",
                "name": "Rinse count",
                "icon": "mdi:counter",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "pre_wash".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-pre_wash",
                "state_topic": "$this/pre_wash",
                "name": "Pre-wash",
                "icon": "mdi:water-sync",
            }),
        ),
        (
            "cold_wash".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-cold_wash",
                "state_topic": "$this/cold_wash",
                "name": "Cold wash",
                "icon": "mdi:snowflake",
            }),
        ),
        (
            "steam".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-steam",
                "state_topic": "$this/steam",
                "name": "Steam",
                "icon": "mdi:kettle-steam",
            }),
        ),
        (
            "extra_rinse".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-extra_rinse",
                "state_topic": "$this/extra_rinse",
                "name": "Extra rinse",
                "icon": "mdi:water-sync",
            }),
        ),
        (
            "delay_active".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-delay_active",
                "state_topic": "$this/delay_active",
                "name": "Delay Wash active",
                "icon": "mdi:clock-plus-outline",
            }),
        ),
        (
            "ai_wash".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-ai_wash",
                "state_topic": "$this/ai_wash",
                "name": "AI Wash",
                "icon": "mdi:creation",
            }),
        ),
    ]
    .into_iter()
    .collect()
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(STATUS_REQUEST);
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Metadata};

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "F3P2CYVAT__";

    // Real captured frames from the LG WM3600HWA, cross-checked against the LG ThinQ app.
    const BOOT_HEARTBEAT: &str = "aa0720d800fcbb";
    const HEARTBEAT_TCL_COUNT_20: &str = "aa0720d814e8bb";

    // Dial moved from Normal to Heavy Duty.
    const NORMAL_TO_HEAVY_DUTY: &str =
        "aa6020ec002b0003100e0f2e00000000000000004100410000000001007800000002120400000000000000400000010000002b0005100e102300000000000000008d008d00000000010078000000021204000000000000004000000000000abb";

    // Extra Rinse step sequence.
    const EXTRA_RINSE_STEP_TO_PLUS: &str =
        "aa6020ec002b0003100e0f2e00000000000000004100410000000001006800000002140400000000000000000000010000002b0003100f0f2e00000000000000004c004c0000000001006800000003140400000000000000000040010000d9bb";
    const EXTRA_RINSE_STEP_TO_PLUS2: &str =
        "aa6020ec002b0003100f0f2e00000000000000004c004c0000000001006800000003140400000000000000000040010000002b000310100f2e00000000000000005700570000000001006800000004140400000000000000000040010000a9bb";
    const EXTRA_RINSE_STEP_TO_PLUS3: &str =
        "aa6020ec002b000310100f2e00000000000000005700570000000001006800000004140400000000000000000040010000002b000310110f2e0000000000000000620062000000000100680000000514040000000000000000004001000079bb";
    const EXTRA_RINSE_STEP_TO_OFF: &str =
        "aa6020ec002b000310110f2e00000000000000006200620000000001006800000005140400000000000000000040010000002b0003100e0f2e00000000000000004100410000000001006800000002140400000000000000000000010000e9bb";

    // Pre Wash enabled alone.
    const PRE_WASH_ON: &str =
        "aa6020ec002b0003100f0f2e00000000000000004c004c0000000001007800000003130400000000000000400040010000002b0003100f0f2e00000000000000005b005b0000000001007800000003130400000000400000400040010000b5bb";

    // Delay Wash 1h, 2h, 14h.
    const DELAY_1H: &str =
        "aa6020ec002b0003100f0f2e00000000000000005b005b0000000001007800000003130400000000400000400040010000002b0003100f0f2e0000000000003c005b005b00000000010078000000031304000000004000004000c0010000afbb";
    const DELAY_2H: &str =
        "aa6020ec002b0003100f0f2e0000000000003c005b005b00000000010078000000031304000000004000004000c0010000002b0003100f0f2e00000000000078005b005b00000000010078000000031304000000004000004000c0010000a7bb";
    const DELAY_14H: &str =
        "aa6020ec002b0003100f0f2e0000000000030c005b005b00000000010078000000031304000000004000004000c0010000002b0003100f0f2e00000000000348005b005b00000000010078000000031304000000004000004000c0010000cdbb";

    // Steam on/off.
    const STEAM_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000004100410000000001006800000002140400000000000000000000010000002b0000000e0f2e000000000000000096009600000000010068000000021404000000000010000000000100008ebb";
    const STEAM_OFF: &str =
        "aa6020ec002b0000000e0f2e00000000000000009600960000000001006800000002140400000000001000000000010000002b0003100e0f2e000000000000000041004100000000010068000000021404000000000000000000000100008ebb";

    // Cold Wash hold.
    const COLD_WASH_HOLD: &str =
        "aa6020ec002b0003100f0f2e00000000000348004c004c00000000010078000000031304000000000000000000c0010000002b00030e0f0f2e000000000003480060006000000000010078000000031304000000000400000000c001000097bb";

    // Spin Only hold.
    const SPIN_ONLY_HOLD: &str =
        "aa6020ec002b0003100e0f2e00000000000000004100410000000001007800000002130400000000000000400000010000002b0000000e0f4e00000000000000000b000b000000000100780700000013040000000000000040000000000022bb";

    // Single-record 0xEB frame, cycle end / power off.
    const CYCLE_END_POWEROFF_GROUND_TRUTH: &str =
        "aa3320eb002b0000000e000000000000000000000100000000000000106800000000140400000000000000000000000000e7bb";

    // Door open start attempt sequence.
    const DOOR_OPEN_START_ATTEMPT: &str =
        "aa6020ec002b0003100e0f2e00000000000000004100410000000001006800000002140400000000000000000000010000002b0003100e0f2e0000000000000000410041000014002401680000000214040000000000000000000001000039bb";
    const DOOR_OPEN_ERROR_PERSISTS: &str =
        "aa6020ec002b0003100e0f2e00000000000000004100410000140024016800000002140400000000000000000000010000002b0003100e0f2e00000000000000004100410000140001246800000002140400000000000000000000010000f1bb";
    const POWEROFF_AFTER_DOOR_OPEN: &str =
        "aa6020ec002b0003100e0f2e00000000000000004100410000140001246800000002140400000000000000000000010000002b0000000e000000000000000000000100000000000000016800000000140400000000000000000000000000cdbb";

    // Downloaded course sequence.
    const DOWNLOAD_COURSE_FIRST: &str =
        "aa6020ec002b0000000e0e5500000000000000005900590000000001006800000002140400000000001000000000000000002b0003120f0fff00000000000000005b005b00000000010068000000031404000000004000000000400100005abb";
    const DOWNLOAD_COURSE_BASE_REVEALED: &str =
        "aa6020ec002b0003120f0fff00000000000000005b005b0000000001006800000003140400000000400000000040010000002b0003120f0fff00000000000000005b005b0000002e0100680000000314040000000040000000004001000021bb";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, "LG WM3600HWA", "1.0")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    #[test]
    fn config_exposes_entity_set() {
        let (ha, _, _) = make();
        let comps = &ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        for component in [
            "power", "status", "error", "error_message", "course", "remaining_time",
            "initial_time", "reserve_time", "tub_clean_count", "soil", "spin", "temp",
            "rinse", "rinse_count", "pre_wash", "cold_wash", "steam", "extra_rinse",
            "delay_active", "ai_wash",
        ] {
            assert!(comps.contains_key(component), "{component}");
        }
        assert!(crate::t2_factory("F3P2CYVAT__").is_some());
    }

    #[test]
    fn boot_heartbeat_publishes_only_tub_clean_count() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(BOOT_HEARTBEAT));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("0"));
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties.len(), 1);

        thinq.emit_data(&hex_decode(HEARTBEAT_TCL_COUNT_20));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("20"));
    }

    #[test]
    fn normal_to_heavy_duty_updates_all_fields_together() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(NORMAL_TO_HEAVY_DUTY));
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "soil", "Heavy");
        expect(&ha, "spin", "Extra High");
        expect(&ha, "temp", "Warm");
        expect(&ha, "rinse", "Normal");
        expect(&ha, "remaining_time", "141");
        expect(&ha, "initial_time", "141");
        expect(&ha, "ai_wash", "OFF");
    }

    #[test]
    fn extra_rinse_full_step_sequence() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&hex_decode(EXTRA_RINSE_STEP_TO_PLUS));
        expect(&ha, "rinse", "Plus");
        expect(&ha, "rinse_count", "3");
        expect(&ha, "remaining_time", "76");
        expect(&ha, "extra_rinse", "ON");

        thinq.emit_data(&hex_decode(EXTRA_RINSE_STEP_TO_PLUS2));
        expect(&ha, "rinse", "Plus 2");
        expect(&ha, "rinse_count", "4");
        expect(&ha, "remaining_time", "87");

        thinq.emit_data(&hex_decode(EXTRA_RINSE_STEP_TO_PLUS3));
        expect(&ha, "rinse", "Plus 3");
        expect(&ha, "rinse_count", "5");
        expect(&ha, "remaining_time", "98");

        thinq.emit_data(&hex_decode(EXTRA_RINSE_STEP_TO_OFF));
        expect(&ha, "rinse", "Normal");
        expect(&ha, "rinse_count", "2");
        expect(&ha, "remaining_time", "65");
        expect(&ha, "extra_rinse", "OFF");
    }

    #[test]
    fn pre_wash_isolated() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(PRE_WASH_ON));
        expect(&ha, "pre_wash", "ON");
        expect(&ha, "remaining_time", "91");
        expect(&ha, "rinse", "Plus");
    }

    #[test]
    fn delay_wash_16_bit_minutes() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&hex_decode(DELAY_1H));
        expect(&ha, "reserve_time", "60");
        expect(&ha, "delay_active", "ON");

        thinq.emit_data(&hex_decode(DELAY_2H));
        expect(&ha, "reserve_time", "120");

        thinq.emit_data(&hex_decode(DELAY_14H));
        expect(&ha, "reserve_time", "840");
    }

    #[test]
    fn steam_isolated() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&hex_decode(STEAM_ON));
        expect(&ha, "steam", "ON");
        expect(&ha, "soil", "Not Selected");
        expect(&ha, "temp", "Not Selected");
        expect(&ha, "remaining_time", "150");

        thinq.emit_data(&hex_decode(STEAM_OFF));
        expect(&ha, "steam", "OFF");
        expect(&ha, "soil", "Normal");
        expect(&ha, "temp", "Warm");
    }

    #[test]
    fn cold_wash_hold() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(COLD_WASH_HOLD));
        expect(&ha, "temp", "Cold");
        expect(&ha, "cold_wash", "ON");
    }

    #[test]
    fn spin_only_hold() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SPIN_ONLY_HOLD));
        expect(&ha, "course", "Spin Only");
        expect(&ha, "soil", "Not Selected");
        expect(&ha, "temp", "Not Selected");
        expect(&ha, "remaining_time", "11");
        expect(&ha, "ai_wash", "OFF");
        expect(&ha, "error", "OFF");
    }

    #[test]
    fn door_open_start_attempt() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&hex_decode(DOOR_OPEN_START_ATTEMPT));
        expect(&ha, "status", "Confirm Start");
        expect(&ha, "error", "ON");
        expect(&ha, "error_message", "DE1 - Door Open");

        thinq.emit_data(&hex_decode(DOOR_OPEN_ERROR_PERSISTS));
        expect(&ha, "status", "Standby");
        expect(&ha, "error", "ON");
        expect(&ha, "error_message", "DE1 - Door Open");

        thinq.emit_data(&hex_decode(POWEROFF_AFTER_DOOR_OPEN));
        expect(&ha, "status", "Off");
        expect(&ha, "error", "OFF");
    }

    #[test]
    fn downloaded_course_reveals_base_course() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&hex_decode(DOWNLOAD_COURSE_FIRST));
        expect(&ha, "course", "Downloaded");

        thinq.emit_data(&hex_decode(DOWNLOAD_COURSE_BASE_REVEALED));
        expect(&ha, "course", "Downloaded (Normal)");
    }

    #[test]
    fn cycle_end_poweroff_matches_ground_truth() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(CYCLE_END_POWEROFF_GROUND_TRUTH));
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "course", "None");
        expect(&ha, "soil", "Not Selected");
        expect(&ha, "temp", "Not Selected");
        expect(&ha, "rinse", "Normal");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "tub_clean_count", "20");
        expect(&ha, "pre_wash", "OFF");
        expect(&ha, "extra_rinse", "OFF");
        expect(&ha, "ai_wash", "OFF");
    }

    #[test]
    fn start_sends_readonly_status_request() {
        let (_ha, thinq, dev) = make();
        dev.start();
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA12F0ED1121010000001804131400005ABB"
        );
    }
}
