//! VCCHL2QEUK — LG WHX9016TLC heat-pump washer-dryer (AABB, 71-byte enveloped records).
//!
//! Port of upstream PR #344. Class `0x20` enveloped 71-byte status records (`auto_ack`),
//! door open/close event frames (`0x10`), `F0 A1 01 00` power-on, and `F0 E5`
//! power-off, start, and pause commands.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LENGTH: usize = 71;
const E6_RESULT_OFFSET: usize = 8;
const STATUS_REQUEST: &str = "F0ED1121010000001800";

const FLAGS_BYTE: usize = 37;
const FLAG_CHILD_LOCK: u8 = 0x20;
const FLAG_REMOTE_START: u8 = 0x10;
const OPTIONS_BYTE: usize = 34;
const OPT_TURBOWASH: u8 = 0x20;
const OPT_PREWASH: u8 = 0x40;
const STEAM_BYTE: usize = 35;
const OPT_STEAM: u8 = 0x10;
const WRINKLE_CARE_BYTE: usize = 36;
const OPT_WRINKLE_CARE: u8 = 0x20;

const EVENT_FRAME_TYPE: u8 = 0x03;
const EVENT_TYPE_OFFSET: usize = 10;
const EVENT_ID_OFFSET: usize = 13;
const EVENT_VALUE_OFFSET: usize = 18;
const DOOR_EVENT: u8 = 0x10;
const DOOR_OPEN: u8 = 0x01;
const DOOR_CLOSED: u8 = 0x02;

const COURSES: &[(u8, &str)] = &[
    (0x7a, "TurboWash 39"),
    (0x2b, "Mixed Fabric"),
    (0x53, "Timed Dry"),
    (0xa6, "AI Wash+Dry"),
    (0x86, "Quick Tub Rinse"),
    (0xab, "Cotton Dry"),
    (0xac, "Easy Care Dry"),
    (0x4b, "Quick 14"),
    (0x72, "AI Wash"),
    (0x2e, "Cotton"),
    (0x13, "Eco 40-60"),
    (0x1d, "Easy Care"),
    (0x5e, "Hand/Wool"),
    (0xad, "Wool Dry"),
    (0x55, "Tub Clean"),
    (0x37, "Rinse+Spin"),
    (0x4e, "Spin Only"),
    (0x54, "Towels"),
    (0x81, "Bed Sheets"),
];

const TEMPERATURES: &[(u8, &str)] = &[
    (0, "Off"),
    (8, "Cold"),
    (1, "20 °C"),
    (2, "30 °C"),
    (3, "40 °C"),
    (5, "60 °C"),
    (6, "95 °C"),
];

const SOIL_LEVELS: &[(u8, &str)] = &[(0, "Off"), (1, "Light"), (3, "Medium"), (5, "Heavy")];

const SPINS: &[(u8, &str)] = &[
    (0, "No spin"),
    (12, "Drain only"),
    (1, "400 rpm"),
    (2, "600 rpm"),
    (4, "800 rpm"),
    (6, "1000 rpm"),
    (8, "1200 rpm"),
    (9, "1400 rpm"),
];

const RINSES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Normal"),
    (2, "Rinse+"),
    (3, "Rinse++"),
    (4, "Rinse+Hold"),
    (5, "Normal+Hold"),
];

const DRY_LEVELS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Low"),
    (2, "Iron"),
    (3, "Cupboard"),
    (4, "Extra dry"),
    (7, "Eco"),
    (16, "Cupboard+"),
    (19, "20 min"),
    (11, "30 min"),
    (20, "40 min"),
    (23, "50 min"),
    (12, "60 min"),
    (24, "80 min"),
    (13, "90 min"),
    (25, "100 min"),
    (14, "120 min"),
    (15, "150 min"),
    (21, "180 min"),
];

const STATE_OPTIONS: &[&str] = &[
    "Off",
    "Standby",
    "Starting",
    "Paused",
    "Detecting",
    "Draining",
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
    "Freeze protection",
    "Smart diagnosis",
    "Waiting to start",
    "Dispensing",
    "Tub cleaning",
    "Steam",
    "Laundry care",
    "Dispenser cleaning",
    "Wrinkle care",
    "Cooling",
];

fn state_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Off"),
        1 => Some("Standby"),
        63 => Some("Starting"),
        2 | 35 | 61 => Some("Paused"),
        3 | 6 | 37 | 40 => Some("Detecting"),
        5 => Some("Draining"),
        7 => Some("Delayed"),
        8 => Some("Soaking"),
        9 => Some("Pre-wash"),
        11 => Some("Washing"),
        12 => Some("Rinsing"),
        13 => Some("Rinse hold"),
        14 => Some("Spinning"),
        15 => Some("Drying"),
        16 | 42 | 49 | 60 => Some("End"),
        19 | 21 => Some("Refreshing"),
        23 => Some("Error"),
        27..=29 => Some("Freeze protection"),
        34 => Some("Smart diagnosis"),
        36 => Some("Waiting to start"),
        38 | 39 => Some("Dispensing"),
        41 => Some("Tub cleaning"),
        43 => Some("Steam"),
        47 => Some("Laundry care"),
        48 => Some("Dispenser cleaning"),
        57 => Some("Wrinkle care"),
        52 | 62 => Some("Cooling"),
        _ => None,
    }
}

fn is_door_locked_state(state: u8) -> bool {
    matches!(
        state,
        3 | 5 | 6 | 8 | 9 | 11 | 12 | 14 | 15 | 19 | 21 | 37..=41 | 43 | 52
    )
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components() -> HashMap<String, Value> {
    [
        ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","icon":"mdi:washing-machine"})),
        ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":STATE_OPTIONS})),
        ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":labels(COURSES)})),
        ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMPERATURES)})),
        ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":labels(SPINS)})),
        ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot","device_class":"enum","options":labels(SOIL_LEVELS)})),
        ("rinse", json!({"platform":"sensor","unique_id":"$deviceid-rinse","state_topic":"$this/rinse","name":"Rinse","icon":"mdi:water-sync","device_class":"enum","options":labels(RINSES)})),
        ("dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:tumble-dryer","device_class":"enum","options":labels(DRY_LEVELS)})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Initial time"})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Remaining time"})),
        ("delay_end", json!({"platform":"sensor","unique_id":"$deviceid-delay_end","state_topic":"$this/delay_end","device_class":"duration","unit_of_measurement":"min","name":"Delay end","icon":"mdi:timer-sand"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
        ("door", json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:play-circle-outline"})),
        ("turbowash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"})),
        ("prewash", json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"})),
        ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
        ("wrinkle_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle care","icon":"mdi:tshirt-crew"})),
        ("tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
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
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer Dryer"})));
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
        if buf.len() == 4 && buf[0] == 0x20 && buf[1] == 0x00 && buf[3] != 0x00 {
            return;
        }
        if buf.len() == E6_RESULT_OFFSET + 1 + STATUS_LENGTH && buf[0] == 0x20 && buf[1] == 0xe6 {
            return;
        }
        if let Some(rec) = process_enveloped_status(buf, 0x20, STATUS_LENGTH) {
            self.process_status(rec);
            return;
        }
        self.process_event(buf);
    }

    fn process_event(&self, buf: &[u8]) {
        if buf.len() <= EVENT_VALUE_OFFSET || buf[0] != 0x20 || buf[1] != 0x0a {
            return;
        }
        if buf[EVENT_TYPE_OFFSET] != EVENT_FRAME_TYPE || buf[EVENT_ID_OFFSET] != DOOR_EVENT {
            return;
        }
        match buf[EVENT_VALUE_OFFSET] {
            DOOR_OPEN => self.core.publish_on_off("door", true),
            DOOR_CLOSED => self.core.publish_on_off("door", false),
            _ => {}
        }
    }

    fn process_status(&self, s: &[u8]) {
        let state = s[21];
        let on = state != 0;
        self.core.publish_on_off("power", on);
        self.core
            .publish_property("status", state_name(state).unwrap_or("None").into());
        if is_door_locked_state(state) {
            self.core.publish_on_off("door", false);
        }

        let opt_label = |table: &'static [(u8, &'static str)], code: u8| -> PropertyValue {
            if on {
                named(table, code).unwrap_or("None").into()
            } else {
                "None".into()
            }
        };
        self.core.publish_property("course", opt_label(COURSES, s[5]));
        self.core.publish_property("temp", opt_label(TEMPERATURES, s[2]));
        self.core.publish_property("spin", opt_label(SPINS, s[4]));
        self.core.publish_property("soil", opt_label(SOIL_LEVELS, s[1]));
        self.core.publish_property("rinse", opt_label(RINSES, s[27]));
        self.core.publish_property("dry_level", opt_label(DRY_LEVELS, s[6]));

        let initial_time = u16::from_be_bytes([s[15], s[16]]);
        if !on || initial_time != 0 {
            let remain = u16::from_be_bytes([s[13], s[14]]);
            self.core.publish_property(
                "remaining_time",
                if on {
                    PropertyValue::Int(i64::from(remain))
                } else {
                    "None".into()
                },
            );
            self.core.publish_property(
                "initial_time",
                if on {
                    PropertyValue::Int(i64::from(initial_time))
                } else {
                    "None".into()
                },
            );
        }
        let delay = u16::from_be_bytes([s[11], s[12]]);
        self.core.publish_property(
            "delay_end",
            if on {
                PropertyValue::Int(i64::from(delay))
            } else {
                "None".into()
            },
        );

        let opt_flag = |cond: bool| -> PropertyValue {
            if on {
                if cond { "ON" } else { "OFF" }.into()
            } else {
                "None".into()
            }
        };
        let options = s[OPTIONS_BYTE];
        self.core
            .publish_property("turbowash", opt_flag(options & OPT_TURBOWASH != 0));
        self.core
            .publish_property("prewash", opt_flag(options & OPT_PREWASH != 0));
        self.core
            .publish_property("steam", opt_flag(s[STEAM_BYTE] & OPT_STEAM != 0));
        self.core.publish_property(
            "wrinkle_care",
            opt_flag(s[WRINKLE_CARE_BYTE] & OPT_WRINKLE_CARE != 0),
        );
        // device_class: lock is inverted (OFF = locked).
        self.core
            .publish_on_off("child_lock", s[FLAGS_BYTE] & FLAG_CHILD_LOCK == 0);
        self.core
            .publish_on_off("remote_start", s[FLAGS_BYTE] & FLAG_REMOTE_START != 0);

        let energy = u16::from_be_bytes([s[17], s[18]]);
        self.core
            .publish_property("energy", PropertyValue::Int(i64::from(energy)));
        if on && initial_time != 0 {
            self.core
                .publish_property("tub_clean_count", PropertyValue::Int(i64::from(s[28])));
        }
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
    fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "power" if value == "ON" => self.core.send(&[0xf0, 0xa1, 0x01, 0x00]),
            "power" if value == "OFF" => {
                self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x02, 0x00])
            }
            "start" => self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x03, 0x01]),
            "pause" => self.core.send(&[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x03, 0x02]),
            _ => {}
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
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const PAIR_TIMED_DRY_30: &str = "AAFF200A00A000006E000100EC008E000000000C53130000000000000014001400000053010000000000001F020000000000000042000418000000000000040000000000000000000000006400041000000100000004000000000C530B000000000000001E001E00000053010000000000001F0200000000000000420004180000000000000400000000000000000000000064000410000001000000044980BB";
    const DOOR_OPENED: &str = "AAFF200A0041000084000201030006100E0602100101050025564343484C325145554B000000000000000000000102BB9483C40607000000000000000000CCF0BB";
    const DOOR_CLOSED: &str = "AAFF200A0041000085000201030006100E0602100201050025564343484C325145554B000000000000000000000102BF9483C406070000000000000000009071BB";
    const PAIR_DRYING: &str = "AAFF200A00A0000358000100EC008E0000000000531400000000000000070028012D00530F030009000000200200000000000000100104180000000000000400000000000000000000000064000410000001000000040000000000531400000000000000060028013B00530F03000900000020020000000000000010010418000000000000040000000000000000000000006400041000000100000004D054BB";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_status_door_events_and_commands() {
        let meta = Metadata::new("VCCHL2QEUK", "VCCHL2QEUK", "1.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(PAIR_TIMED_DRY_30));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Timed Dry"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("30 min"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Drain only"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("30"));

        thinq.emit_data(&hex_decode(DOOR_OPENED));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(DOOR_CLOSED));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(PAIR_DRYING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("315"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("32"));

        thinq.reset_recorder();
        dev.set_property("power", "ON");
        dev.set_property("power", "OFF");
        dev.set_property("start", "");
        dev.set_property("pause", "");
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(
            sent,
            vec![
                "AA08F0A1010011BB",
                "AA0DF0E5000201FF010200C4BB",
                "AA0DF0E5000201FF010301C6BB",
                "AA0DF0E5000201FF010302C1BB",
            ]
        );
    }
}
