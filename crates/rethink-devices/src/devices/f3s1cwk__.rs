//! F3S1CWK__ — LG WKGX201HWA WashTower washer half (AABB, class `0x33`).
//!
//! Port of upstream PR #300. 45-byte `0xEB`/`0xEC` status blocks led by a
//! `0x2b` marker (`auto_ack` enabled). Power on/off use `F0 E5 00 02 01 FF 01 02 01/00`.
//! Wash completion stays latched across the automatic power-off after End.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x33;
const STATUS_LENGTH: usize = 45;
const RECORD_MARKER: u8 = 0x2b;

const SOIL_OFFSET: usize = 2;
const TEMP_OFFSET: usize = 3;
const EXTRA_RINSE_OFFSET: usize = 4;
const EXTRA_RINSE_NONE: u8 = 0x0e;
const SPIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const REMAINING_TIME_OFFSET: usize = 14;
const INITIAL_TIME_OFFSET: usize = 16;
const ENERGY_OFFSET: usize = 18;
const STATE_OFFSET: usize = 22;
const DOWNLOADED_COURSE_OFFSET: usize = 24;
const LOAD_LEVEL_OFFSET: usize = 25;
const RINSE_COUNT_OFFSET: usize = 28;
const TCL_COUNT_OFFSET: usize = 29;
const SIGNAL_OFFSET: usize = 30;

const OPT1_OFFSET: usize = 35;
const OPT1_TURBO_WASH: u8 = 0x20;
const OPT1_PRE_WASH: u8 = 0x40;
const OPT2_OFFSET: usize = 36;
const OPT2_STEAM: u8 = 0x10;
const OPT3_OFFSET: usize = 38;
const OPT3_REMOTE_START: u8 = 0x10;
const OPT3_CHILD_LOCK: u8 = 0x20;
const OPT4_OFFSET: usize = 40;
const OPT4_EXTRA_RINSE: u8 = 0x40;

const STATE_OFF: u8 = 0x00;
const STATE_END: u8 = 0x10;
const COURSE_DOWNLOADED: u8 = 0xff;

const CMD_POWER_OFF: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x02, 0x00];
const CMD_POWER_ON: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x02, 0x01];

const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Standby"),
    (0x02, "Paused"),
    (0x03, "Detecting"),
    (0x07, "Delayed"),
    (0x08, "Soaking"),
    (0x09, "Pre-wash"),
    (0x0b, "Washing"),
    (0x0c, "Rinsing"),
    (0x0d, "Rinse hold"),
    (0x0e, "Spinning"),
    (0x10, "End"),
    (0x16, "Error"),
    (0x24, "Starting"),
];

const COURSE: &[(u8, &str)] = &[
    (0x0d, "Bedding"),
    (0x16, "Delicates"),
    (0x23, "Heavy Duty"),
    (0x3c, "Sanitary"),
    (0x2e, "Normal"),
    (0x4a, "Speed Wash"),
    (0x4e, "Spin Only"),
    (0x55, "Tub Clean"),
];

const DOWNLOADED_COURSE: &[(u8, &str)] = &[
    (0x65, "Sweat Stains"),
    (0x69, "Small Load"),
    (0x6a, "Overnight Wash"),
    (0x6f, "Color Care"),
    (0x70, "Denim"),
    (0x71, "Full Load Wash"),
    (0x73, "Beachwear"),
    (0x75, "Sports Wear"),
    (0x76, "Half Load Wash"),
    (0x78, "EconoWash"),
    (0x79, "Delicate Dresses"),
    (0x7c, "Blanket"),
    (0x7e, "Juice and food stains"),
    (0x81, "Spin Only"),
    (0x90, "Rinse + Spin"),
    (0xdc, "Bright Whites"),
    (0xdd, "Hand Wash / Wool"),
    (0xde, "Allergiene"),
    (0xdf, "Perm. Press"),
    (0xe0, "Towels"),
];

const SOIL: &[(u8, &str)] = &[
    (0x01, "Light"),
    (0x02, "Light-Normal"),
    (0x03, "Normal"),
    (0x04, "Normal-Heavy"),
    (0x05, "Heavy"),
];

const TEMP: &[(u8, &str)] = &[
    (0x0d, "Cold"),
    (0x0e, "Cool"),
    (0x10, "Warm"),
    (0x12, "Very Warm"),
    (0x13, "Hot"),
];

const SPIN: &[(u8, &str)] = &[
    (0x0c, "No Spin"),
    (0x0d, "Low"),
    (0x0e, "Medium"),
    (0x0f, "Medium High"),
    (0x10, "High"),
];

const SIGNAL: &[(u8, &str)] = &[(0x00, "Off"), (0x04, "High")];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components() -> HashMap<String, Value> {
    [
        ("power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:washing-machine","device_class":"running"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":labels(STATUS)})),
        ("wash_completed", json!({"platform":"binary_sensor","unique_id":"$deviceid-wash_completed","state_topic":"$this/wash_completed","name":"Wash completed","icon":"mdi:check-circle"})),
        ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot","device_class":"enum","options":labels(SOIL)})),
        ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMP)})),
        ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":labels(SPIN)})),
        ("rinse_count", json!({"platform":"sensor","unique_id":"$deviceid-rinse_count","state_topic":"$this/rinse_count","name":"Rinses","icon":"mdi:water-sync","state_class":"measurement"})),
        ("extra_rinse", json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra rinse","icon":"mdi:water-plus"})),
        ("extra_rinse_count", json!({"platform":"sensor","unique_id":"$deviceid-extra_rinse_count","state_topic":"$this/extra_rinse_count","name":"Extra rinse count","icon":"mdi:water-plus","state_class":"measurement"})),
        ("turbo_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_wash","state_topic":"$this/turbo_wash","name":"TurboWash","icon":"mdi:fast-forward"})),
        ("pre_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-pre_wash","state_topic":"$this/pre_wash","name":"Pre-wash","icon":"mdi:water-sync"})),
        ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:remote"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Control lock","icon":"mdi:lock-outline"})),
        ("signal", json!({"platform":"sensor","unique_id":"$deviceid-signal","state_topic":"$this/signal","name":"Signal","icon":"mdi:bell-outline","device_class":"enum","options":labels(SIGNAL),"entity_category":"diagnostic"})),
        ("load_level", json!({"platform":"sensor","unique_id":"$deviceid-load_level","state_topic":"$this/load_level","name":"Load level","icon":"mdi:weight","state_class":"measurement","entity_category":"diagnostic"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh","icon":"mdi:lightning-bolt"})),
        ("tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since Tub Clean","icon":"mdi:counter","entity_category":"diagnostic"})),
        ("power_off", json!({"platform":"button","unique_id":"$deviceid-power_off","command_topic":"$this/power_off/set","payload_press":"PRESS","name":"Power off","icon":"mdi:power-off","availability":[{"topic":"$this/power","payload_available":"ON","payload_not_available":"OFF"}]})),
        ("power_on", json!({"platform":"button","unique_id":"$deviceid-power_on","command_topic":"$this/power_on/set","payload_press":"PRESS","name":"Power on","icon":"mdi:power","availability":[{"topic":"$this/power","payload_available":"OFF","payload_not_available":"ON"}]})),
        ("downloaded_course", json!({"platform":"sensor","unique_id":"$deviceid-downloaded_course","state_topic":"$this/downloaded_course","name":"Downloaded course","icon":"mdi:download","device_class":"enum","options":labels(DOWNLOADED_COURSE)})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    wash_completed_published: Mutex<bool>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            wash_completed_published: Mutex::new(false),
        });
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
        if let Some(status) = process_common_status(buf, CLASS_BYTE, STATUS_LENGTH) {
            self.process_status(status);
        }
    }

    fn process_status(&self, status: &[u8]) {
        if status.len() < STATUS_LENGTH || status[1] != RECORD_MARKER {
            return;
        }
        let rec = &status[1..];
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_OFF;
        let is_end = state == STATE_END;
        let idle = is_off || is_end;

        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", named(STATUS, state).unwrap_or("None").into());

        let course = rec[COURSE_OFFSET];
        let course_label = if course == COURSE_DOWNLOADED {
            named(DOWNLOADED_COURSE, rec[DOWNLOADED_COURSE_OFFSET])
        } else {
            named(COURSE, course)
        };
        self.core
            .publish_property("course", course_label.unwrap_or("None").into());
        self.core.publish_property(
            "downloaded_course",
            named(DOWNLOADED_COURSE, rec[DOWNLOADED_COURSE_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        let remain = if idle {
            0
        } else {
            i64::from(u16::from_be_bytes([
                rec[REMAINING_TIME_OFFSET],
                rec[REMAINING_TIME_OFFSET + 1],
            ]))
        };
        let initial = if idle {
            0
        } else {
            i64::from(u16::from_be_bytes([
                rec[INITIAL_TIME_OFFSET],
                rec[INITIAL_TIME_OFFSET + 1],
            ]))
        };
        self.core
            .publish_property("remaining_time", PropertyValue::Int(remain));
        self.core
            .publish_property("initial_time", PropertyValue::Int(initial));
        self.core
            .publish_property("soil", named(SOIL, rec[SOIL_OFFSET]).unwrap_or("None").into());
        self.core
            .publish_property("temp", named(TEMP, rec[TEMP_OFFSET]).unwrap_or("None").into());
        self.core
            .publish_property("spin", named(SPIN, rec[SPIN_OFFSET]).unwrap_or("None").into());
        self.core.publish_property(
            "rinse_count",
            PropertyValue::Int(i64::from(rec[RINSE_COUNT_OFFSET])),
        );
        let energy = u16::from_be_bytes([rec[ENERGY_OFFSET], rec[ENERGY_OFFSET + 1]]);
        self.core
            .publish_property("energy", PropertyValue::Int(i64::from(energy)));

        let extra_rinse = rec[EXTRA_RINSE_OFFSET];
        let extra_rinse_count = if extra_rinse >= EXTRA_RINSE_NONE {
            i64::from(extra_rinse - EXTRA_RINSE_NONE)
        } else {
            0
        };
        self.core.publish_property(
            "extra_rinse_count",
            PropertyValue::Int(extra_rinse_count),
        );
        self.core
            .publish_on_off("extra_rinse", rec[OPT4_OFFSET] & OPT4_EXTRA_RINSE != 0);
        self.core
            .publish_on_off("turbo_wash", rec[OPT1_OFFSET] & OPT1_TURBO_WASH != 0);
        self.core
            .publish_on_off("pre_wash", rec[OPT1_OFFSET] & OPT1_PRE_WASH != 0);
        self.core
            .publish_on_off("steam", rec[OPT2_OFFSET] & OPT2_STEAM != 0);
        self.core
            .publish_on_off("remote_start", rec[OPT3_OFFSET] & OPT3_REMOTE_START != 0);
        self.core
            .publish_on_off("child_lock", rec[OPT3_OFFSET] & OPT3_CHILD_LOCK != 0);
        self.core.publish_property(
            "signal",
            named(SIGNAL, rec[SIGNAL_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "load_level",
            PropertyValue::Int(i64::from(rec[LOAD_LEVEL_OFFSET])),
        );
        self.core.publish_property(
            "tub_clean_count",
            PropertyValue::Int(i64::from(rec[TCL_COUNT_OFFSET])),
        );

        if is_end {
            self.core.publish_on_off("wash_completed", true);
            *self.wash_completed_published.lock() = true;
        } else if !is_off || !*self.wash_completed_published.lock() {
            self.core.publish_on_off("wash_completed", false);
            *self.wash_completed_published.lock() = true;
        }
    }
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
        if value != "PRESS" {
            return;
        }
        if prop == "power_off" {
            self.core.send(CMD_POWER_OFF);
        } else if prop == "power_on" {
            self.core.send(CMD_POWER_ON);
        }
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

    const ID: &str = "test-id";
    const DELICATES: &str = "AA6033EC002B0001120E104A00000000000000000F000F0000000001009007000001200400000000000004400000000000002B00030E0E0E1600000000000000002D002D0000000001009007000002200400000000000004400000000000EDBB";
    const DOWNLOADED: &str = "AA6033EC002B00030E0E0E1600000000000000002D002D0000000001009007000002200400000000000004400000000000002B00000E0E0FFF00000000000000001200120000003701009007000001200400000000000004400000000000F1BB";
    const EXTRA_RINSE_3: &str = "AA6033EC002B000310100F2E00000000000000003800380000000001009000000003200400000000200004000040010000002B000310110F2E00000000000000003D003D000000000100900000000420040000000020000400004001000030BB";
    const PRE_WASH_ON: &str = "AA6033EC002B0003100E0F2E00000000000000002E002E0000000001009000000001200400000000200004000000010000002B0003100E0F2E00000000000000003D003D000000000100900000000120040000000060000400000001000052BB";
    const REMOTE_ON: &str = "AA6033EC002B00050E0F0F2E00000000000000008700870000000001009000000002200400000000200004000040000000002B00050E0F0F2E00000000000000008700870000000001009000000002200400000000200004100040000000CABB";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_courses_options_and_power_commands() {
        let meta = Metadata::new("F3S1CWK__", "F3S1CWK__", "0.0.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(DELICATES));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Delicates"));
        assert_eq!(prop(&ha, "downloaded_course").as_deref(), Some("Rinse + Spin"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("45"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Cool"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Medium"));

        thinq.emit_data(&hex_decode(DOWNLOADED));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Rinse + Spin"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("18"));

        thinq.emit_data(&hex_decode(EXTRA_RINSE_3));
        assert_eq!(prop(&ha, "extra_rinse_count").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "rinse_count").as_deref(), Some("4"));

        thinq.emit_data(&hex_decode(PRE_WASH_ON));
        assert_eq!(prop(&ha, "pre_wash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbo_wash").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(REMOTE_ON));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));

        thinq.reset_recorder();
        dev.set_property("power_on", "PRESS");
        dev.set_property("power_off", "PRESS");
        dev.set_property("power_off", "ignored");
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(
            sent,
            vec!["AA0DF0E5000201FF010201C7BB", "AA0DF0E5000201FF010200C4BB"]
        );
    }
}
