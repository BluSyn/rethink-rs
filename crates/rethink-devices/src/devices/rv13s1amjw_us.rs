//! RV13S1AMJW_US — dryer half of the split-module LG WKEX200HBA WashTower (AABB).
//!
//! The split module registers its halves as two ThinQ2 devices: this dryer (deviceType 222) and
//! the `F3S1CWKB__` washer (deviceType 221). Not an alias of the RV13B6* dryers: unit `0x34`
//! frames carry 16-bit length-prefixed 28-byte records instead of marker-led ones, and the option
//! bytes sit at different offsets. `0xEC` carries the previous then the current record, `0xEB`
//! only the current one; `0xE2` post-cycle replays are not decoded. Record byte 0 is the phase,
//! not a time field. Power is a switch (`F0 24` command); `auto_ack` is off.
//!
//! Port of upstream PR #268.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const DRYER_UNIT: u8 = 0x34;
const STATUS_FRAME_TYPE: u8 = 0xec;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;

const PHASE_OFFSET: usize = 0;
const TIME_HOUR_OFFSET: usize = 1;
const TIME_MIN_OFFSET: usize = 2;
const COURSE_OFFSET: usize = 5;
// 0 on Time Dry: only the sensor courses have a dry level.
const DRY_LEVEL_OFFSET: usize = 7;
const TEMP_OFFSET: usize = 8;
const OPTIONS_OFFSET: usize = 14;
const OPTION_WRINKLE_CARE: u8 = 0x10;

const PHASE_OFF: u8 = 0x00;

const POWER_ON: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x01];
const POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];

const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Initial"),
    (0x03, "Pause"),
    (0x04, "End"),
    (0x32, "Drying"),
    (0x33, "Cooling"),
];

// 0x04 is the Downloaded course on this model; its panel has no Perm Press position.
const COURSES: &[(u8, &str)] = &[
    (0x01, "Heavy Duty"),
    (0x02, "Towels"),
    (0x03, "Normal"),
    (0x04, "Downloaded"),
    (0x05, "Delicates"),
    (0x07, "Bedding"),
    (0x08, "Antibacterial"),
    (0x09, "Small Load"),
    (0x0b, "Sportswear"),
    (0x10, "Speed Dry"),
    (0x11, "Air Dry"),
    (0x12, "Time Dry"),
    (0x15, "Steam Fresh"),
    (0x16, "Steam Sanitary"),
    (0x1a, "Super Dry"),
];

// Rotated against the panel's 1-5 scale: panel level 1 (Damp) is code 5, level 5 (Very) is code 4,
// and levels 2-4 are codes 1-3.
const DRY_LEVELS: &[(u8, &str)] = &[
    (1, "Level 2"),
    (2, "Level 3"),
    (3, "Level 4"),
    (4, "Very Dry"),
    (5, "Damp"),
];

const TEMPS: &[(u8, &str)] = &[
    (1, "Low"),
    (2, "Level 2"),
    (3, "Level 3"),
    (4, "Level 4"),
    (5, "High"),
];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

/// The 16-bit big-endian length-prefixed records of an `0xEB`/`0xEC` frame.
fn records(buf: &[u8]) -> Vec<&[u8]> {
    let mut records = Vec::new();
    let mut offset = 2;
    while offset + 2 <= buf.len() {
        let len = usize::from(u16::from_be_bytes([buf[offset], buf[offset + 1]]));
        offset += 2;
        let Some(record) = buf.get(offset..offset + len) else {
            break;
        };
        records.push(record);
        offset += len;
    }
    records
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
        let this = Arc::new(Self { core: core.clone() });
        let mut config = default_config(&meta, Some(json!({"name": "LG WashTower Dryer"})));
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
        if buf.len() >= 2
            && buf[0] == DRYER_UNIT
            && (buf[1] == STATUS_FRAME_TYPE || buf[1] == SINGLE_STATUS_FRAME_TYPE)
        {
            self.process_status(buf);
        }
    }

    fn process_status(&self, buf: &[u8]) {
        // 0xEC carries [previous, current]; 0xEB only the current record.
        let records = records(buf);
        let Some(&active) = records.get(1).or(records.first()) else {
            return;
        };
        if active.len() <= COURSE_OFFSET {
            return;
        }
        let phase = active[PHASE_OFFSET];
        let course = active[COURSE_OFFSET];
        let remaining =
            i64::from(active[TIME_HOUR_OFFSET]) * 60 + i64::from(active[TIME_MIN_OFFSET]);

        if phase == PHASE_OFF {
            self.core.publish_on_off("power", false);
            self.core.publish_property("status", "Off".into());
            self.core.publish_property("course", "None".into());
            self.core
                .publish_property("remaining_time", PropertyValue::Int(0));
            self.core.publish_property("dry_level", "None".into());
            self.core.publish_property("temp", "None".into());
            self.core.publish_on_off("wrinkle_care", false);
            return;
        }

        self.core.publish_on_off("power", true);
        self.core
            .publish_property("status", named(STATUS, phase).unwrap_or("Running").into());
        let course_label = match course {
            0 => "None".to_string(),
            code => named(COURSES, code).map_or_else(|| format!("0x{code:x}"), str::to_string),
        };
        self.core.publish_property("course", course_label.into());
        self.core.publish_property(
            "remaining_time",
            PropertyValue::Int(if course == 0 { 0 } else { remaining }),
        );
        let dry_level = active
            .get(DRY_LEVEL_OFFSET)
            .and_then(|&code| named(DRY_LEVELS, code));
        self.core
            .publish_property("dry_level", dry_level.unwrap_or("None").into());
        let temp = active.get(TEMP_OFFSET).and_then(|&code| named(TEMPS, code));
        self.core
            .publish_property("temp", temp.unwrap_or("None").into());
        let wrinkle_care = active
            .get(OPTIONS_OFFSET)
            .is_some_and(|options| options & OPTION_WRINKLE_CARE != 0);
        self.core.publish_on_off("wrinkle_care", wrinkle_care);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if prop == "power" {
            let on = mqtt_value == "ON";
            self.core.send(if on { POWER_ON } else { POWER_OFF });
        }
    }
}

fn components() -> HashMap<String, Value> {
    [
        (
            "power".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "command_topic": "$this/power/set",
                "name": "Power",
                "icon": "mdi:tumble-dryer",
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
            "course".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-course",
                "state_topic": "$this/course",
                "name": "Course",
                "icon": "mdi:pin-outline",
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
            "dry_level".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-dry_level",
                "state_topic": "$this/dry_level",
                "name": "Dry level",
                "icon": "mdi:water-percent",
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
            }),
        ),
        (
            "wrinkle_care".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-wrinkle_care",
                "state_topic": "$this/wrinkle_care",
                "name": "Wrinkle Care",
                "icon": "mdi:iron",
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
    fn start(&self) {}
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
    const MODEL_ID: &str = "RV13S1AMJW_US";

    // Real captures from the dryer unit of a split-module WKEX200HBA WashTower, during an evening
    // Normal cycle.

    // 0xEB snapshot right after a reconnect, mid-cycle: Drying, 47 min left of 59.
    const EB_DRYING: &str =
        "AA2434EB001C32002F003B03000504000400000000A90003A001000000740000000023BB";

    // 0xEC two-record frame while Drying: previous record 39 min, current 38 min.
    const EC_DRYING: &str = "AA4234EC001C320027003B03000504000400000000A90005E2010000007400000000001C320026003B03000504000400000000A900060E01000000740000000097BB";

    // 0xEC with the current record at phase End (0x04), power still on.
    const EC_END: &str = "AA4234EC001C330001003403000504000400000000A9000BCF320000007400000000001C040001003403000504000400000040A8000BD2330000007400000000CFBB";

    // 0xEC 30 s later: current record phase Off, course cleared.
    const EC_OFF: &str = "AA4234EC001C040001003403000504000400000040A8000BD2330000007400000000001C000001000100000000000400000040A8000000040000000000000000BEBB";

    const SEND_POWER_ON: &str = "AA09F0240101019FBB";
    const SEND_POWER_OFF: &str = "AA09F0240101009CBB";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, "WKEX200HBA", "2.10.123")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        let props = ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(
            props.get(name).map(|p| p.as_string()).as_deref(),
            Some(value),
            "{name}"
        );
    }

    #[test]
    fn config_exposes_expected_ha_components() {
        let (ha, _, _) = make();
        let components = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        for c in [
            "power",
            "status",
            "course",
            "remaining_time",
            "dry_level",
            "temp",
            "wrinkle_care",
        ] {
            assert!(components.contains_key(c), "component {c} present");
        }
        assert!(crate::t2_factory(MODEL_ID).is_some());
    }

    #[test]
    fn eb_snapshot_decodes_a_running_cycle() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EB_DRYING));
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Drying");
        expect(&ha, "course", "Normal");
        expect(&ha, "remaining_time", "47");
        expect(&ha, "dry_level", "Damp");
        expect(&ha, "temp", "Level 4");
        expect(&ha, "wrinkle_care", "OFF");
    }

    #[test]
    fn ec_reads_the_current_second_record_not_the_previous_one() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_DRYING));
        expect(&ha, "status", "Drying");
        expect(&ha, "remaining_time", "38");
    }

    #[test]
    fn end_phase_keeps_power_on_the_following_off_record_clears_the_cycle() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_DRYING));
        thinq.emit_data(&hex_decode(EC_END));
        expect(&ha, "status", "End");
        expect(&ha, "power", "ON");

        thinq.emit_data(&hex_decode(EC_OFF));
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "course", "None");
        expect(&ha, "dry_level", "None");
        expect(&ha, "temp", "None");
    }

    #[test]
    fn set_property_power_sends_the_f0_24_command() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), SEND_POWER_ON);

        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), SEND_POWER_OFF);
    }
}
