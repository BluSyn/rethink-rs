//! F3P3CYK2_ — LG WM4200HBA front-load washer.
//!
//! 44-byte class `0x20` records. Not an alias of the 25-byte F3L2 handlers.
//! `auto_ack` stays off. Only power-off is writable.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::json;
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
const POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];

const SOIL_OFFSET: usize = 2;
const TEMP_OFFSET: usize = 3;
const RINSE_OFFSET: usize = 4;
const SPIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const RESERVE_TIME_OFFSET: usize = 12;
const REMAIN_TIME_OFFSET: usize = 14;
const INITIAL_TIME_OFFSET: usize = 16;
const ERROR_OFFSET: usize = 20;
const STATE_OFFSET: usize = 22;
const TUB_CLEAN_COUNT_OFFSET: usize = 29;
const BUZZER_OFFSET: usize = 30;
const OPT2_OFFSET: usize = 36;
const OPT2_RINSE_SPIN: u8 = 0x20;

const STATE_OFF: u8 = 0;
const STATE_END: u8 = 16;

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Standby"),
    (27, "Standby"),
    (2, "Paused"),
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
];

const COURSES: &[(u8, &str)] = &[
    (0x05, "Allergiene"),
    (0x0d, "Bedding"),
    (0x16, "Delicates"),
    (0x19, "Drain+Spin"),
    (0x23, "Heavy Duty"),
    (0x2e, "Normal"),
    (0x30, "Perm Press"),
    (0x3c, "Sanitary"),
    (0x4a, "Speed Wash"),
    (0x4f, "Sportswear"),
    (0x54, "Towels"),
    (0x55, "Tub Clean"),
    (0x5a, "Bright Whites"),
    (0xff, "Downloaded"),
];

const SOIL: &[(u8, &str)] = &[
    (1, "Light"),
    (2, "Light/Normal"),
    (3, "Normal"),
    (4, "Normal/Heavy"),
    (5, "Heavy"),
];

const TEMP: &[(u8, &str)] = &[
    (13, "Tap Cold"),
    (14, "Cold"),
    (15, "Eco Warm"),
    (16, "Warm"),
    (17, "Warm Rinse"),
    (18, "Hot"),
    (19, "Extra Hot"),
];

const RINSE: &[(u8, &str)] = &[(14, "Normal"), (15, "Plus"), (16, "Plus 2"), (17, "Plus 3")];

const SPIN: &[(u8, &str)] = &[
    (12, "No Spin"),
    (13, "Low"),
    (14, "Medium"),
    (15, "High"),
    (16, "Extra High"),
];

const BUZZER: &[(u8, &str)] = &[(0, "Off"), (1, "1"), (2, "2"), (3, "3"), (4, "4")];

const ERRORS: &[(u8, &str)] = &[
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
    (20, "dE1 - Door Open"),
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
        let is_end = state == STATE_END;

        self.core.publish_on_off("power", !is_off);
        self.publish_error(rec[ERROR_OFFSET]);
        let status = named(STATES, state).unwrap_or("None");
        self.core.publish_property("status", status.into());
        self.core.publish_property(
            "course",
            named(COURSES, rec[COURSE_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "remaining_time",
            PropertyValue::Int(if is_off {
                0
            } else {
                u16be(rec, REMAIN_TIME_OFFSET)
            }),
        );
        self.core.publish_property(
            "initial_time",
            PropertyValue::Int(if is_off {
                0
            } else {
                u16be(rec, INITIAL_TIME_OFFSET)
            }),
        );
        self.core.publish_property(
            "reserve_time",
            PropertyValue::Int(if is_off {
                0
            } else {
                u16be(rec, RESERVE_TIME_OFFSET)
            }),
        );
        self.publish_named("soil", SOIL, rec[SOIL_OFFSET]);
        self.publish_named("temp", TEMP, rec[TEMP_OFFSET]);
        self.publish_named("rinse", RINSE, rec[RINSE_OFFSET]);
        self.publish_named("spin", SPIN, rec[SPIN_OFFSET]);
        self.core
            .publish_on_off("rinse_spin", rec[OPT2_OFFSET] & OPT2_RINSE_SPIN != 0);
        self.core.publish_property(
            "tub_clean_count",
            PropertyValue::Int(rec[TUB_CLEAN_COUNT_OFFSET] as i64),
        );
        self.publish_named("buzzer", BUZZER, rec[BUZZER_OFFSET]);
        self.core.publish_on_off("run_completed", is_end);
    }

    fn publish_named(&self, prop: &str, table: &'static [(u8, &'static str)], code: u8) {
        self.core
            .publish_property(prop, named(table, code).unwrap_or("None").into());
    }

    fn publish_error(&self, code: u8) {
        self.core.publish_on_off("error_state", code != 0);
        let message = if code == 0 {
            "-".to_string()
        } else {
            named(ERRORS, code)
                .map(str::to_string)
                .unwrap_or_else(|| format!("Unknown error (0x{code:02X})"))
        };
        self.core.publish_property("error_message", message.into());
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if prop == "power_off" && mqtt_value == "PRESS" {
            self.core.send(POWER_OFF);
        }
    }
}

fn u16be(rec: &[u8], offset: usize) -> i64 {
    u16::from_be_bytes([rec[offset], rec[offset + 1]]) as i64
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn components() -> std::collections::HashMap<String, serde_json::Value> {
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
            "power_off".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-power_off",
                "command_topic": "$this/power_off/set",
                "payload_press": "PRESS",
                "name": "Power off",
                "icon": "mdi:power-off",
                "availability": [{
                    "topic": "$this/power",
                    "payload_available": "ON",
                    "payload_not_available": "OFF",
                }],
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
            }),
        ),
        (
            "run_completed".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-run_completed",
                "state_topic": "$this/run_completed",
                "name": "Run completed",
                "icon": "mdi:check-circle",
            }),
        ),
        (
            "error_state".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-error_state",
                "state_topic": "$this/error_state",
                "name": "Error state",
                "icon": "mdi:alert-circle-outline",
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
                "entity_category": "diagnostic",
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
                "icon": "mdi:timer-sand",
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
                "name": "Delay Wash time remaining",
                "icon": "mdi:clock-outline",
                "device_class": "duration",
                "unit_of_measurement": "min",
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
            "rinse_spin".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-rinse_spin",
                "state_topic": "$this/rinse_spin",
                "name": "Rinse + Spin",
                "icon": "mdi:water-sync",
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
            "buzzer".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-buzzer",
                "state_topic": "$this/buzzer",
                "name": "Buzzer",
                "icon": "mdi:volume-high",
                "device_class": "enum",
                "options": labels(BUZZER),
                "entity_category": "diagnostic",
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const OFF_SNAPSHOT: &str = "AA3320EB002B0000000E00000000000000000000010000002D0000001069000000001E04000000000000004000000000007FBB";
    const PAUSED: &str = "AA6020EC002B0000000E0F2E0000000000000000150016000000000C2469070000011E0400000000002000100000010000002B0000000E0F2E000000000000000015001600000000020C69070000011E04000000000020004000000100001BBB";
    const RESUMED_RINSING: &str = "AA6020EC002B0000000E0F2E000000000000000015001600000000240269070000011E0400000000002000100000010000002B0000000E0F2E0000000000000000150016000000000C2469070000011E040000000000200010000001000063BB";
    const SPINNING: &str = "AA6020EC002B0000000E0F2E0000000000000000120016000100000C2469070000011E0400000000002000100000010000002B0000000E0F2E0000000000000000110016000200000E0C69070000001E040000000000200010000001000070BB";
    const END: &str = "AA6020EC002B0000000E0F2E0000000000000000010016002800000E0C69070000001E0400000000002000100000010000002B0000000E00000000000000000000010000002D0000100E69000000001E0400000000000000400000000000ADBB";
    const OFF_AFTER_END: &str = "AA6020EC002B0000000E00000000000000000000010000002D0000100E69000000001E0400000000000000400000000000002B0000000E00000000000000000000010000002D0000001069000000001E0400000000000000400000000000FDBB";
    const E2_STALE_REPLAY: &str = "AA3320E2032B0000000E0F2E0000000000000000160016002D0000240169070000011E0400000000002000100000010000CFBB";
    const DIAL_NORMAL: &str = "AA6020EC002B00030E0E0E1600000000000000002D002D00000000010069070000021E0400000000000000400000000000002B0003100E0F2E00000000000000002E002E00000000010069000000011E0400000000200000400000010000D3BB";
    const DIAL_BEDDING: &str = "AA6020EC002B0005100E1023000000000000000082008200000000010069000000011E0400000000200000400000000000002B0003100E0E0D00000000000000003C003C00000000010069070000021E040000000000000040000000000069BB";
    const DIAL_SANITARY: &str = "AA6020EC002B0003100E0E0D00000000000000003C003C00000000010069070000021E0400000000000000400000000000002B0003130E0F3C000000000000000074007400000000010069000000031E04000000000000004000000000004EBB";
    const DIAL_DOWNLOADED: &str = "AA6020EC002B0000000E0E55000000000000000059005900000000010069000000021E0400000000001000400000000000002B0003100E0FFF00000000000000002D002D00000000010069000000021E040000000000000040000000000091BB";
    const DIAL_DRAIN_SPIN: &str = "AA6020EC002B0003100E0FFF00000000000000002D002D00000044010069000000021E0400000000000000400000000000002B0000000E0F1900000000000000000B000B00000000010069070000001E040000000000000040000000000073BB";
    const DIAL_DELICATES: &str = "AA6020EC002B0003100E0E3000000000000000002A002A00000000010069000000011E0400000000200000400000000000002B00030E0E0E1600000000000000002D002D00000000010069070000021E04000000000000004000000000002BBB";
    const RINSE_SPIN_ON: &str = "AA6020EC002B0003100E0F2E00000000000000002E002E00000000010069000000011E0400000000200000400000010000002B0000000E0F2E000000000000000016001600000000010069070000011E0400000000002000400000010000D5BB";
    const RINSE_SPIN_OFF: &str = "AA6020EC002B0000000E0F2E000000000000000016001600000000010069070000011E0400000000002000400000010000002B0003100E0F2E00000000000000002E002E00000000010069000000011E0400000000200000400000010000D5BB";

    fn meta() -> Metadata {
        Metadata::new("F3P3CYK2_", "LG WM4200HBA", "2.10.119")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> PropertyValue {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .get(name)
            .cloned()
            .unwrap()
    }

    fn feed(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _dev) = make();
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        ha
    }

    #[test]
    fn config_exposes_the_conservative_entity_set() {
        let (ha, _thinq, _dev) = make();
        let cfg = ha.device(DEVICE_ID).unwrap().config.unwrap();
        for component in [
            "power",
            "status",
            "run_completed",
            "error_state",
            "error_message",
            "course",
            "remaining_time",
            "initial_time",
            "reserve_time",
            "soil",
            "temp",
            "rinse",
            "spin",
            "rinse_spin",
            "tub_clean_count",
            "buzzer",
            "power_off",
        ] {
            assert!(cfg.components.contains_key(component), "{component}");
        }
        for unsafe_name in ["power_switch", "pause", "remote_start", "course_selection"] {
            assert!(!cfg.components.contains_key(unsafe_name), "{unsafe_name}");
        }
        assert_eq!(
            cfg.components["spin"]["options"],
            json!(["No Spin", "Low", "Medium", "High", "Extra High"])
        );
        assert!(crate::t2_factory("F3P3CYK2_").is_some());
    }

    #[test]
    fn power_off_sends_the_captured_command() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power_off", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F0240101009CBB");
        dev.set_property("power_off", "not-a-press");
        assert_eq!(thinq.outbox().len(), 1);
    }

    #[test]
    fn error_index_publishes_de1_and_a_later_status_clears_it() {
        let (ha, thinq, _dev) = make();
        let mut error = hex_decode(DIAL_NORMAL);
        error[50 + 20] = 20;
        thinq.emit_data(&error);
        assert_eq!(prop(&ha, "error_state"), "ON".into());
        assert_eq!(prop(&ha, "error_message"), "dE1 - Door Open".into());
        thinq.emit_data(&hex_decode(DIAL_NORMAL));
        assert_eq!(prop(&ha, "error_state"), "OFF".into());
        assert_eq!(prop(&ha, "error_message"), "-".into());
    }

    #[test]
    fn start_sends_the_readonly_status_request() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA12F0ED1121010000001804131400005ABB"
        );
    }

    #[test]
    fn eb_snapshot_initializes_an_off_washer() {
        let ha = feed(&[OFF_SNAPSHOT]);
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "status"), "Off".into());
        assert_eq!(prop(&ha, "course"), "None".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "run_completed"), "OFF".into());
    }

    #[test]
    fn pause_resume_spin_end_and_power_off() {
        let (ha, thinq, _dev) = make();
        thinq.emit_data(&hex_decode(PAUSED));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Paused".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(21));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(22));

        thinq.emit_data(&hex_decode(RESUMED_RINSING));
        assert_eq!(prop(&ha, "status"), "Rinsing".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(21));

        thinq.emit_data(&hex_decode(SPINNING));
        assert_eq!(prop(&ha, "status"), "Spinning".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(17));

        thinq.emit_data(&hex_decode(END));
        assert_eq!(prop(&ha, "status"), "End".into());
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(1));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "run_completed"), "ON".into());

        thinq.emit_data(&hex_decode(OFF_AFTER_END));
        assert_eq!(prop(&ha, "status"), "Off".into());
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "run_completed"), "OFF".into());

        thinq.emit_data(&hex_decode(DIAL_NORMAL));
        assert_eq!(prop(&ha, "status"), "Standby".into());
        assert_eq!(prop(&ha, "run_completed"), "OFF".into());
    }

    #[test]
    fn e2_post_cycle_replay_is_ignored() {
        let ha = feed(&[END, E2_STALE_REPLAY]);
        assert_eq!(prop(&ha, "status"), "End".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(1));
        assert_eq!(prop(&ha, "run_completed"), "ON".into());
    }

    #[test]
    fn course_selections_map_to_panel_names() {
        for (frame, expected) in [
            (DIAL_NORMAL, "Normal"),
            (DIAL_BEDDING, "Bedding"),
            (DIAL_SANITARY, "Sanitary"),
            (DIAL_DOWNLOADED, "Downloaded"),
            (DIAL_DRAIN_SPIN, "Drain+Spin"),
            (DIAL_DELICATES, "Delicates"),
        ] {
            let ha = feed(&[frame]);
            assert_eq!(prop(&ha, "course"), expected.into());
        }
    }

    #[test]
    fn course_table_maps_every_code() {
        for (code, expected) in COURSES {
            let mut frame = hex_decode(DIAL_NORMAL);
            frame[50 + 6] = *code;
            let (ha, thinq, _dev) = make();
            thinq.emit_data(&frame);
            assert_eq!(prop(&ha, "course"), (*expected).into(), "{code:#x}");
        }
    }

    #[test]
    fn rinse_spin_is_an_option_on_normal() {
        let ha = feed(&[RINSE_SPIN_ON]);
        assert_eq!(prop(&ha, "course"), "Normal".into());
        assert_eq!(prop(&ha, "rinse_spin"), "ON".into());
        assert_eq!(prop(&ha, "soil"), "None".into());
        assert_eq!(prop(&ha, "temp"), "None".into());
        assert_eq!(prop(&ha, "rinse"), "Normal".into());
        assert_eq!(prop(&ha, "spin"), "High".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(22));

        let after = feed(&[RINSE_SPIN_ON, RINSE_SPIN_OFF]);
        assert_eq!(prop(&after, "rinse_spin"), "OFF".into());
        assert_eq!(prop(&after, "course"), "Normal".into());
        assert_eq!(prop(&after, "soil"), "Normal".into());
        assert_eq!(prop(&after, "temp"), "Warm".into());
    }

    #[test]
    fn normal_selection_decodes_settings() {
        let ha = feed(&[DIAL_NORMAL]);
        assert_eq!(prop(&ha, "soil"), "Normal".into());
        assert_eq!(prop(&ha, "temp"), "Warm".into());
        assert_eq!(prop(&ha, "rinse"), "Normal".into());
        assert_eq!(prop(&ha, "spin"), "High".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(46));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(46));
        assert_eq!(prop(&ha, "tub_clean_count"), PropertyValue::Int(30));
        assert_eq!(prop(&ha, "buzzer"), "4".into());
    }

    #[test]
    fn times_are_big_endian_minutes() {
        let mut frame = hex_decode(DIAL_NORMAL);
        let record = 50;
        frame[record + 12] = 0x01;
        frame[record + 13] = 0x2c;
        frame[record + 14] = 0x01;
        frame[record + 15] = 0x1e;
        frame[record + 16] = 0x01;
        frame[record + 17] = 0x2e;
        let (ha, thinq, _dev) = make();
        thinq.emit_data(&frame);
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(300));
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(286));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(302));
    }

    #[test]
    fn wrong_length_and_unrelated_frames_publish_nothing() {
        for junk in [
            "aa0720d800fcbb",
            "aa09207200c9005bbb",
            "aa0a20ec002b000000bb",
        ] {
            let (ha, thinq, _dev) = make();
            thinq.emit_data(&hex_decode(junk));
            assert!(
                ha.device(DEVICE_ID).unwrap().properties.is_empty(),
                "{junk}"
            );
        }
    }
}
