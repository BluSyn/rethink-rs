//! TLXA5CFPH — LG WTX5-12W top-load washer (AABB, 67-byte enveloped records, read-only).
//!
//! Port of upstream PR #342. Long class `0x20` AABB envelope with `auto_ack`.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_REQUEST: &str = "F0ED112101000000180411120000";
const CLASS_BYTE: u8 = 0x20;
const RECORD_LEN: usize = 67;

const TEMP_OFFSET: usize = 2;
const RINSES_REMAINING_OFFSET: usize = 3;
const SPIN_OFFSET: usize = 4;
const COURSE_OFFSET: usize = 5;
const WASH_TIME_OFFSET: usize = 8;
const WATER_LEVEL_OFFSET: usize = 9;
const REMAIN_TIME_OFFSET: usize = 14;
const INITIAL_TIME_OFFSET: usize = 16;
const STATE_OFFSET: usize = 21;
const FLAGS_OFFSET: usize = 38;
const STATE_POWEROFF: u8 = 0;
const FLAG_LID_LOCK: u8 = 0x01;

const STATUS_OPTIONS: &[&str] = &[
    "Off",
    "Standby",
    "Paused",
    "Detecting",
    "Filling",
    "Detergent amount",
    "Delayed",
    "Soaking",
    "Pre-wash",
    "Washing",
    "Rinsing",
    "Rinse hold",
    "Spinning",
    "Drying",
    "End",
    "Refreshing",
    "Error",
    "Tub cleaning",
];

fn status_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Off"),
        1 | 27 => Some("Standby"),
        2 | 28 => Some("Paused"),
        3 => Some("Detecting"),
        5 => Some("Filling"),
        6 => Some("Detergent amount"),
        7 => Some("Delayed"),
        8 => Some("Soaking"),
        9 => Some("Pre-wash"),
        11 | 29 => Some("Washing"),
        12 => Some("Rinsing"),
        13 => Some("Rinse hold"),
        14 => Some("Spinning"),
        15 => Some("Drying"),
        16 => Some("End"),
        21 => Some("Refreshing"),
        23 => Some("Error"),
        41 => Some("Tub cleaning"),
        _ => None,
    }
}

const COURSE: &[(u8, &str)] = &[
    (0x09, "Duvet"),
    (0x2d, "Allergy Care"),
    (0x2e, "Cotton"),
    (0x37, "Rinse + Spin"),
    (0x3e, "Eco Cotton"),
    (0x4a, "Quick Wash"),
    (0x4e, "Spin Only"),
    (0x5e, "Delicates"),
    (0x72, "AI Wash"),
    (0xff, "Downloaded"),
];

const SPIN: &[(u8, &str)] = &[
    (0, "Not selected"),
    (13, "Low"),
    (14, "Medium"),
    (15, "High"),
    (16, "Extra High"),
    (17, "Dry Intensive"),
    (18, "Delicate"),
];

const TEMP: &[(u8, &str)] = &[
    (0, "Not selected"),
    (8, "Cold"),
    (9, "Warm"),
    (10, "Hot"),
];

const WASH_TIME: &[(u8, &str)] = &[
    (0, "Not selected"),
    (1, "3 min"),
    (2, "6 min"),
    (3, "10 min"),
    (4, "12 min"),
    (5, "14 min"),
    (6, "17 min"),
    (7, "19 min"),
    (8, "21 min"),
    (9, "23 min"),
    (10, "25 min"),
];

const WATER_LEVEL: &[(u8, &str)] = &[
    (0, "Level 1"),
    (1, "Level 2"),
    (2, "Level 3"),
    (3, "Level 4"),
    (4, "Level 5"),
    (5, "Level 6"),
    (6, "Level 7"),
    (7, "Level 8"),
    (8, "Level 9"),
    (9, "Level 10"),
    (10, "Not selected"),
    (11, "Auto"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components() -> HashMap<String, Value> {
    [
        ("power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:washing-machine","device_class":"running"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":STATUS_OPTIONS})),
        ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":labels(COURSE)})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMP)})),
        ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":labels(SPIN)})),
        ("rinses_remaining", json!({"platform":"sensor","unique_id":"$deviceid-rinses_remaining","state_topic":"$this/rinses_remaining","name":"Rinses remaining","icon":"mdi:water-sync","state_class":"measurement"})),
        ("wash_time", json!({"platform":"sensor","unique_id":"$deviceid-wash_time","state_topic":"$this/wash_time","name":"Wash time","icon":"mdi:timer-cog-outline","device_class":"enum","options":labels(WASH_TIME)})),
        ("water_level", json!({"platform":"sensor","unique_id":"$deviceid-water_level","state_topic":"$this/water_level","name":"Water level","icon":"mdi:waves-arrow-up","device_class":"enum","options":labels(WATER_LEVEL)})),
        ("lid_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-lid_lock","state_topic":"$this/lid_lock","name":"Lid lock","icon":"mdi:lock","entity_category":"diagnostic"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
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
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self { core: core.clone() });
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(rec) = process_enveloped_status(inner, CLASS_BYTE, RECORD_LEN) {
                    dev.process_status(rec);
                }
            }
        }));
        this
    }

    fn process_status(&self, rec: &[u8]) {
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_POWEROFF;
        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", status_name(state).unwrap_or("None").into());
        self.core.publish_property(
            "course",
            named(COURSE, rec[COURSE_OFFSET]).unwrap_or("None").into(),
        );
        let remain = if is_off {
            0
        } else {
            i64::from(u16::from_le_bytes([
                rec[REMAIN_TIME_OFFSET],
                rec[REMAIN_TIME_OFFSET + 1],
            ]))
        };
        let initial = if is_off {
            0
        } else {
            i64::from(u16::from_le_bytes([
                rec[INITIAL_TIME_OFFSET],
                rec[INITIAL_TIME_OFFSET + 1],
            ]))
        };
        self.core
            .publish_property("remaining_time", PropertyValue::Int(remain));
        self.core
            .publish_property("initial_time", PropertyValue::Int(initial));
        self.core
            .publish_property("temp", named(TEMP, rec[TEMP_OFFSET]).unwrap_or("None").into());
        self.core
            .publish_property("spin", named(SPIN, rec[SPIN_OFFSET]).unwrap_or("None").into());
        self.core.publish_property(
            "rinses_remaining",
            PropertyValue::Int(i64::from(rec[RINSES_REMAINING_OFFSET])),
        );
        self.core.publish_property(
            "wash_time",
            named(WASH_TIME, rec[WASH_TIME_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "water_level",
            named(WATER_LEVEL, rec[WATER_LEVEL_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core
            .publish_on_off("lid_lock", rec[FLAGS_OFFSET] & FLAG_LID_LOCK != 0);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_REQUEST));
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
    use rethink_core::{MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const POWER_ON: &str = "aaff200a00980008ac000100ec0086000308020e2e00000603000000003900390000002e00008b00000002110400000000000000020020300000000000000400000000000000000000000000000000000000000308020e2e0000060b000000003900000000002e01018b000000021104000000000000000208203000000000000004000000000000000000000000000000000000003a96bb";
    const WASHING: &str = "aaff200a00980008d6000100ec0086000508030e0900000a0b000000006c00000000002e03038b00000003110400000000000000100900300000000000000400000000000000000000000000000000000000000508030e0900000906000000006a006a0000002e0b0b8b00000003110400000000000000100100300000000000000400000000000000000000000000000000000000854dbb";
    const OFF: &str = "aaff200a00980008e4000100ec0086000508030e0900000906000000006a006a0001002e02028b00000003110400000000000000000000300000000000000400000000000000000000000000000000000000000508030e0900000906000000006a006a0001002e00008b0000000311040000000000000000000030000000000000040000000000000000000000000000000000000066bbbb";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_captured_frames() {
        let meta = Metadata::new("TLXA5CFPH", "TLXA5CFPH", "3.0.79");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let _dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(POWER_ON));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Cold"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "rinses_remaining").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "wash_time").as_deref(), Some("17 min"));
        assert_eq!(prop(&ha, "water_level").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "lid_lock").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(WASHING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Duvet"));
        assert_eq!(prop(&ha, "water_level").as_deref(), Some("Level 7"));
        assert_eq!(prop(&ha, "lid_lock").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(OFF));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
    }
}
