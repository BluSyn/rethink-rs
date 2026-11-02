//! F3P2CYUBE__ — LG WM4000HWA front-load washer (ThinQ2, deviceType 201).
//!
//! Same AABB family as F3L2CYU__, with a longer record: class `0x20`, 45-byte status blocks
//! (a leading byte, then a 44-byte record led by a `0x2B` marker). `0xEB` carries one block
//! (seen at reconnect) and `0xEC` the previous then the current one; other frame types are not
//! decoded. Offsets are relative to the marker, and enum codes are the modelJson
//! MonitoringValue indices. `rec[38]` bit `0x40` (LG's doorClose) is a panel activity timer,
//! not the door, so it is not published; the door lock is not in the frame. `auto_ack` is on.
//!
//! Writes (`F0 E5`) are packets captured from the LG cloud, and the washer ignores them unless
//! Remote Start is armed. `start` runs the cycle dialed at the panel. `start_course` starts one
//! of 13 courses at its defaults, with the `delay` hours folded in; there is no per-option cycle
//! builder because each course restricts its options. `specialty` downloads one of 17
//! SmartCourses into the `0xFF` slot, `downloaded_course` reads it back and `Downloaded Course`
//! runs it. `power_off` (WMOff) is a two-step select: the washer then drops off Wi-Fi until it
//! is powered on at the panel. Power-on (WMWakeup) is not supported.
//!
//! Port of upstream PR #171.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const STATUS_LENGTH: usize = 45;
const RECORD_MARKER: u8 = 0x2b;

const SOIL_OFFSET: usize = 2;
const TEMP_OFFSET: usize = 3;
const RINSE_OFFSET: usize = 4;
const SPIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const RESERVE_OFFSET: usize = 12;
const REMAIN_OFFSET: usize = 14;
const INITIAL_OFFSET: usize = 16;
const ENERGY_OFFSET: usize = 19;
const STATE_OFFSET: usize = 22;
const PRESTATE_OFFSET: usize = 23;
const DOWNLOAD_COURSE_OFFSET: usize = 24;
const CYCLES_OFFSET: usize = 29;
const SIGNAL_OFFSET: usize = 30;
const SIGNAL_BIT: u8 = 0x04;
const OPTS35_OFFSET: usize = 35;
const OPT35_COLD_WASH: u8 = 0x04;
const OPT35_TURBO_WASH: u8 = 0x20;
const OPT35_PRE_WASH: u8 = 0x40;
const OPTS36_OFFSET: usize = 36;
const OPT36_STEAM: u8 = 0x10;
const OPT36_RINSE_SPIN: u8 = 0x20;
const OPTS37_OFFSET: usize = 37;
const OPT37_FRESH_CARE: u8 = 0x40;
const OPTS38_OFFSET: usize = 38;
const OPT38_REMOTE_START: u8 = 0x10;
const OPT38_CHILD_LOCK: u8 = 0x20;
const OPT38_ADD_ITEM: u8 = 0x80;
const AIDDLED_OFFSET: usize = 41;
const AIDDLED_BIT: u8 = 0x01;

/// `rec[4]` with no extra rinse; each added rinse counts up from here.
const RINSE_NONE: u8 = 0x0e;
const MAX_EXTRA_RINSES: i64 = 3;
/// `rec[23]` of the blank record the module sends right after Power.
const BLANK_PRESTATE: u8 = 0x10;

const STATE_OFF: u8 = 0x00;

const STATES: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Initial"),
    (0x02, "Paused"),
    (0x03, "Detecting"),
    (0x05, "Add Drain"),
    (0x06, "Detergent Amount"),
    (0x07, "Reserved"),
    (0x09, "Pre-wash"),
    (0x0b, "Running"),
    (0x0c, "Rinsing"),
    (0x0d, "Rinse Hold"),
    (0x0e, "Spinning"),
    (0x0f, "Drying"),
    (0x10, "End"),
    (0x15, "Refreshing"),
    (0x17, "Error Auto Off"),
    (0x1b, "Frozen Prevent Initial"),
    (0x1c, "Frozen Prevent Pause"),
    (0x1d, "Frozen Prevent Running"),
    (0x22, "Audible Diagnosis"),
    (0x23, "Auto DT Open Pause"),
    (0x24, "Confirm Start For Control"),
];

const SOIL: &[(u8, &str)] = &[
    (0, "None"),
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];

const TEMP: &[(u8, &str)] = &[
    (0, "None"),
    (13, "Tap Cold"),
    (14, "Cold"),
    (15, "Eco Warm"),
    (16, "Warm"),
    (17, "Warm Rinse"),
    (18, "Hot"),
    (19, "Extra Hot"),
];

const SPIN: &[(u8, &str)] = &[
    (0, "None"),
    (12, "No Spin"),
    (13, "Low"),
    (14, "Medium"),
    (15, "High"),
    (16, "Extra High"),
];

/// Dial positions, read off the dial against the cloud (the modelJson has no course codes).
const COURSES: &[(u8, &str)] = &[
    (0x00, "None"),
    (0x05, "Allergiene"),
    (0x0d, "Bedding"),
    (0x16, "Delicates"),
    (0x23, "Heavy Duty"),
    (0x2e, "Normal"),
    (0x30, "Perm. Press"),
    (0x3c, "Sanitary"),
    (0x4a, "Speed Wash"),
    (0x4e, "Spin Only"),
    (0x54, "Towels"),
    (0x55, "Tub Clean"),
    (0x5a, "Bright Whites"),
    (0xff, "Downloaded Course"),
];

const START_COURSE_OPTIONS: &[&str] = &[
    "Normal",
    "Heavy Duty",
    "Towels",
    "Perm. Press",
    "Delicates",
    "Bedding",
    "Bright Whites",
    "Allergiene",
    "Sanitary",
    "Speed Wash",
    "Tub Clean",
    "Spin Only",
    "Downloaded Course",
];

/// The app's delay-start ceiling.
const DELAY_MAX_HOURS: u16 = 19;

struct SmartCourse {
    /// The washer's SmartCourse id (the byte after `10 0b`, echoed at `rec[24]`), which is
    /// not the modelJson id.
    code: u8,
    name: &'static str,
    /// WMDownload inner frame, captured from the app.
    download: &'static str,
}

const SPECIALTY_COURSES: &[SmartCourse] = &[
    SmartCourse {
        code: 0x65,
        name: "Sweat Stains",
        download:
            "f0e5000201ff100b650aff0c2e1f10210f1e013d00200e220010003e0034003800350144007f0000",
    },
    SmartCourse {
        code: 0x67,
        name: "Swimwear",
        download:
            "f0e5000201ff100b670aff0c161f0e210d1e013d00200e220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0x68,
        name: "Baby Clothes",
        download:
            "f0e5000201ff100b680aff0c2e1f12210f1e033d00200f220010003e0034013800350044007f0000",
    },
    SmartCourse {
        code: 0x69,
        name: "Small Load",
        download:
            "f0e5000201ff100b690aff0c441f10210f1e033d00200e220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0x6a,
        name: "Overnight",
        download:
            "f0e5000201ff100b6a0aff0c2e1f10210d1e033d00200e220010003e0034003800350044017f0000",
    },
    SmartCourse {
        code: 0x6b,
        name: "Single Garments",
        download:
            "f0e5000201ff100b6b0aff0c4a1f12210f1e013d00200e220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0x6d,
        name: "Rainy Day",
        download:
            "f0e5000201ff100b6d0aff0c2e1f1021101e033d00200e220010003e0034003800350144007f0000",
    },
    SmartCourse {
        code: 0x6e,
        name: "Gym Clothes",
        download:
            "f0e5000201ff100b6e0aff0c4f1f10210e1e013d00200e220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0x6f,
        name: "Color Care",
        download:
            "f0e5000201ff100b6f0aff0c2e1f0e210e1e033d00200e220010003e0034003800350144007f0000",
    },
    SmartCourse {
        code: 0x70,
        name: "Denim",
        download:
            "f0e5000201ff100b700aff0c2e1f0e210e1e033d00200e220010003e0034003800350144007f0000",
    },
    SmartCourse {
        code: 0x71,
        name: "Full Load",
        download:
            "f0e5000201ff100b710aff0c2e1f10210f1e053d00200f220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0x73,
        name: "Beachwear",
        download:
            "f0e5000201ff100b730aff0c161f0e210e1e013d00200e220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0x74,
        name: "New Clothes",
        download:
            "f0e5000201ff100b740aff0c2e1f0e210d1e013d00200e220010003e0034003800350144007f0000",
    },
    SmartCourse {
        code: 0x76,
        name: "Half Load",
        download:
            "f0e5000201ff100b760aff0c2e1f10210f1e033d00200e220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0x78,
        name: "EconoWash",
        download:
            "f0e5000201ff100b780aff0c2e1f0e210f1e033d00200e220010003e0034003801350044007f0000",
    },
    SmartCourse {
        code: 0x79,
        name: "Delicate Dresses",
        download:
            "f0e5000201ff100b790aff0c161f0e210d1e013d00200e220010003e0034003800350044007f0000",
    },
    SmartCourse {
        code: 0xdd,
        name: "Hand Wash/Wool",
        download:
            "f0e5000201ff100bdd0aff0c221f10210d1e033d00200e220010003e0034003800350044007f0000",
    },
];

/// Rests first in the one-shot selects so that every pick is a change.
const UNARMED: &str = "unknown";
const POWER_OFF_OPTION: &str = "Power Off";

const CMD_START: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x03, 0x01];
const CMD_PAUSE: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x03, 0x02];
const CMD_RESUME: &[u8] = &[
    0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x02, 0x44, 0x00, 0x03, 0x03,
];
const CMD_POWER_OFF: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x01, 0x02, 0x00];

/// config/start for a course at its defaults: no override pairs, then the delay in minutes.
fn config_start(course: u8, delay_minutes: u16) -> [u8; 14] {
    let [delay_hi, delay_lo] = delay_minutes.to_be_bytes();
    [
        0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff, 0x03, 0x0a, course, 0x7f, delay_hi, delay_lo, 0x03,
        0x01,
    ]
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn code_of(table: &'static [(u8, &'static str)], name: &str) -> Option<u8> {
    table
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(code, _)| *code)
}

fn state_name(code: u8) -> String {
    named(STATES, code)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Unknown ({code})"))
}

fn downloaded_course_name(code: u8) -> String {
    match SPECIALTY_COURSES.iter().find(|c| c.code == code) {
        Some(course) => course.name.to_string(),
        None if code == 0 => "None".to_string(),
        None => format!("0x{code:02x}"),
    }
}

fn u16be(rec: &[u8], offset: usize) -> i64 {
    u16::from_be_bytes([rec[offset], rec[offset + 1]]) as i64
}

/// The first `0xEC` after Power carries a current record that is blank except for `rec[4]` and
/// `rec[23]` for about a second.
fn is_blank(rec: &[u8]) -> bool {
    rec[RINSE_OFFSET] == RINSE_NONE
        && rec[PRESTATE_OFFSET] == BLANK_PRESTATE
        && rec
            .iter()
            .enumerate()
            .all(|(i, &b)| b == 0 || i == 0 || i == RINSE_OFFSET || i == PRESTATE_OFFSET)
}

/// Rounded to whole hours and clamped to the app's range; anything unparsable is 0.
fn delay_hours(value: &str) -> u16 {
    let hours = value.trim().parse::<f64>().unwrap_or(0.0);
    let hours = if hours.is_nan() { 0.0 } else { hours.round() };
    hours.clamp(0.0, f64::from(DELAY_MAX_HOURS)) as u16
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    /// Folded into the next `start_course`; 0 starts now.
    delay_minutes: Mutex<u16>,
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
            delay_minutes: Mutex::new(0),
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
        let rec = &status[1..];
        if rec[0] != RECORD_MARKER || is_blank(rec) {
            return;
        }
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_OFF;

        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", state_name(state).into());
        self.core
            .publish_property("previous_status", state_name(rec[PRESTATE_OFFSET]).into());
        let course = rec[COURSE_OFFSET];
        self.core
            .publish_property("course_code", format!("0x{course:02x}").into());
        self.core
            .publish_property("course", named(COURSES, course).unwrap_or("unknown").into());
        self.core.publish_property(
            "downloaded_course",
            downloaded_course_name(rec[DOWNLOAD_COURSE_OFFSET]).into(),
        );
        let reserve = u16be(rec, RESERVE_OFFSET);
        self.core.publish_on_off("delay_wash", reserve > 0);
        self.core
            .publish_property("reserve_time", PropertyValue::Int(reserve));
        // Zeroed while off: the washer keeps stale bytes after power-off.
        let minutes = |offset| if is_off { 0 } else { u16be(rec, offset) };
        self.core
            .publish_property("remaining_time", PropertyValue::Int(minutes(REMAIN_OFFSET)));
        self.core
            .publish_property("initial_time", PropertyValue::Int(minutes(INITIAL_OFFSET)));
        self.publish_named("soil", SOIL, rec[SOIL_OFFSET]);
        self.publish_named("temp", TEMP, rec[TEMP_OFFSET]);
        let extra_rinses =
            (i64::from(rec[RINSE_OFFSET]) - i64::from(RINSE_NONE)).clamp(0, MAX_EXTRA_RINSES);
        self.core.publish_on_off("extra_rinse", extra_rinses > 0);
        self.core
            .publish_property("extra_rinse_count", PropertyValue::Int(extra_rinses));
        let opts35 = rec[OPTS35_OFFSET];
        self.core
            .publish_on_off("cold_wash", opts35 & OPT35_COLD_WASH != 0);
        self.core
            .publish_on_off("turbo_wash", opts35 & OPT35_TURBO_WASH != 0);
        self.core
            .publish_on_off("pre_wash", opts35 & OPT35_PRE_WASH != 0);
        let opts36 = rec[OPTS36_OFFSET];
        self.core.publish_on_off("steam", opts36 & OPT36_STEAM != 0);
        self.core
            .publish_on_off("rinse_spin", opts36 & OPT36_RINSE_SPIN != 0);
        self.core
            .publish_on_off("fresh_care", rec[OPTS37_OFFSET] & OPT37_FRESH_CARE != 0);
        self.core
            .publish_on_off("signal", rec[SIGNAL_OFFSET] & SIGNAL_BIT != 0);
        let opts38 = rec[OPTS38_OFFSET];
        self.core
            .publish_on_off("remote_start", opts38 & OPT38_REMOTE_START != 0);
        self.core
            .publish_on_off("child_lock", opts38 & OPT38_CHILD_LOCK != 0);
        self.core
            .publish_on_off("add_item", opts38 & OPT38_ADD_ITEM != 0);
        self.core
            .publish_on_off("ai_dd", rec[AIDDLED_OFFSET] & AIDDLED_BIT != 0);
        self.publish_named("spin", SPIN, rec[SPIN_OFFSET]);
        self.core
            .publish_property("cycles", PropertyValue::Int(rec[CYCLES_OFFSET].into()));
        self.core
            .publish_property("energy", PropertyValue::Int(rec[ENERGY_OFFSET].into()));
        let delay_minutes = *self.delay_minutes.lock();
        self.core
            .publish_property("delay", PropertyValue::Int((delay_minutes / 60).into()));
    }

    fn publish_named(&self, prop: &str, table: &'static [(u8, &'static str)], code: u8) {
        self.core
            .publish_property(prop, named(table, code).unwrap_or("unknown").into());
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "start" => self.core.send(CMD_START),
            "pause" => self.core.send(CMD_PAUSE),
            "resume" => self.core.send(CMD_RESUME),
            "power_off" => {
                if mqtt_value == POWER_OFF_OPTION {
                    self.core.send(CMD_POWER_OFF);
                    self.core.publish_property("power_off", UNARMED.into());
                }
            }
            "start_course" => {
                if let Some(course) = code_of(COURSES, mqtt_value) {
                    let delay_minutes = *self.delay_minutes.lock();
                    self.core.send(&config_start(course, delay_minutes));
                }
            }
            "delay" => {
                let hours = delay_hours(mqtt_value);
                *self.delay_minutes.lock() = hours * 60;
                self.core
                    .publish_property("delay", PropertyValue::Int(hours.into()));
            }
            "specialty" => {
                if let Some(course) = SPECIALTY_COURSES.iter().find(|c| c.name == mqtt_value) {
                    self.core.send(&hex_decode(course.download));
                    self.core.publish_property("specialty", UNARMED.into());
                }
            }
            _ => {}
        }
    }
}

fn components() -> HashMap<String, Value> {
    let specialty_options: Vec<&str> = std::iter::once(UNARMED)
        .chain(SPECIALTY_COURSES.iter().map(|c| c.name))
        .collect();
    [
        (
            "power".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "name": "Power",
                "icon": "mdi:washing-machine",
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
            "previous_status".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-previous_status",
                "state_topic": "$this/previous_status",
                "name": "Previous status",
                "icon": "mdi:history",
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
            }),
        ),
        (
            "course_code".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-course_code",
                "state_topic": "$this/course_code",
                "name": "Course code",
                "icon": "mdi:pound",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "downloaded_course".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-downloaded_course",
                "state_topic": "$this/downloaded_course",
                "name": "Downloaded course",
                "icon": "mdi:download-box-outline",
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
            "delay_wash".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-delay_wash",
                "state_topic": "$this/delay_wash",
                "name": "Delay Wash",
                "icon": "mdi:clock-plus-outline",
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
            "initial_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-initial_time",
                "state_topic": "$this/initial_time",
                "name": "Cycle time",
                "icon": "mdi:timer-sand",
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
            "extra_rinse".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-extra_rinse",
                "state_topic": "$this/extra_rinse",
                "name": "Extra rinse",
                "icon": "mdi:water-plus",
            }),
        ),
        (
            "extra_rinse_count".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-extra_rinse_count",
                "state_topic": "$this/extra_rinse_count",
                "name": "Extra rinse count",
                "icon": "mdi:water-plus",
                "state_class": "measurement",
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
            "turbo_wash".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-turbo_wash",
                "state_topic": "$this/turbo_wash",
                "name": "TurboWash",
                "icon": "mdi:rocket-launch",
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
            "fresh_care".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-fresh_care",
                "state_topic": "$this/fresh_care",
                "name": "FreshCare",
                "icon": "mdi:tumble-dryer",
            }),
        ),
        (
            "signal".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-signal",
                "state_topic": "$this/signal",
                "name": "Signal",
                "icon": "mdi:bell",
            }),
        ),
        (
            "remote_start".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-remote_start",
                "state_topic": "$this/remote_start",
                "name": "Remote Start",
                "icon": "mdi:cellphone-wireless",
            }),
        ),
        (
            "child_lock".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-child_lock",
                "state_topic": "$this/child_lock",
                "name": "Child lock",
                "icon": "mdi:account-lock",
            }),
        ),
        (
            "add_item".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-add_item",
                "state_topic": "$this/add_item",
                "name": "Add Item",
                "icon": "mdi:tshirt-crew",
            }),
        ),
        (
            "ai_dd".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-ai_dd",
                "state_topic": "$this/ai_dd",
                "name": "AI DD",
                "icon": "mdi:brain",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "start".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-start",
                "command_topic": "$this/start/set",
                "payload_press": "",
                "name": "Start (as dialed)",
                "icon": "mdi:play",
            }),
        ),
        (
            "pause".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-pause",
                "command_topic": "$this/pause/set",
                "payload_press": "",
                "name": "Pause",
                "icon": "mdi:pause-circle-outline",
            }),
        ),
        (
            "resume".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-resume",
                "command_topic": "$this/resume/set",
                "payload_press": "",
                "name": "Resume",
                "icon": "mdi:play-pause",
            }),
        ),
        (
            "power_off".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-power_off",
                "command_topic": "$this/power_off/set",
                "state_topic": "$this/power_off",
                "options": [UNARMED, POWER_OFF_OPTION],
                "name": "Power Off",
                "icon": "mdi:power",
            }),
        ),
        (
            "start_course".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-start_course",
                "command_topic": "$this/start_course/set",
                "options": START_COURSE_OPTIONS,
                "name": "Start course",
                "icon": "mdi:play-box-outline",
            }),
        ),
        (
            "specialty".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-specialty",
                "command_topic": "$this/specialty/set",
                "state_topic": "$this/specialty",
                "options": specialty_options,
                "name": "Specialty cycle (download)",
                "icon": "mdi:download-box-outline",
            }),
        ),
        (
            "delay".into(),
            json!({
                "platform": "number",
                "unique_id": "$deviceid-delay",
                "command_topic": "$this/delay/set",
                "state_topic": "$this/delay",
                "min": 0,
                "max": DELAY_MAX_HOURS,
                "step": 1,
                "mode": "box",
                "unit_of_measurement": "h",
                "name": "Delay start",
                "icon": "mdi:clock-plus-outline",
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
            }),
        ),
        (
            "cycles".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-cycles",
                "state_topic": "$this/cycles",
                "name": "Cycles Since Clean",
                "icon": "mdi:counter",
                "state_class": "total",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "energy".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-energy",
                "state_topic": "$this/energy",
                "name": "Course energy",
                "icon": "mdi:lightning-bolt",
                "state_class": "measurement",
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
        request_status(&self.core);
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
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";

    // Real frames captured from the washer in bridge mode, labelled against the LG cloud's own
    // washerDryer deltas and the LG ThinQ integration at matching timestamps.

    // 0xEB single-record frame right after (re)connect, washer off.
    const EB_RECONNECT_OFF: &str =
        "aa3320eb002b0000000e0000000000000000000001000000000000000169000000003b04000000000000000000000000009ebb";
    // Off and idle; the cycle counter (59) survives power-off.
    const OFF_IDLE: &str =
        "aa6020ec002b0000000e0000000000000000000000000000000000010069000000003b0400000000000000400000000000002b0000000e0000000000000000000001000000000000000169000000003b04000000000000004000000000000ebb";
    // Power on: Initial, course 0x2E at its defaults, 46-minute estimate.
    const POWER_ON_SELECTING: &str =
        "aa6020ec002b0000000e0000000000000000000000000000000000010069000000003b0400000000000000400000000000002b0003100e0f2e00000000000000002e002e00000000010069000000013b04000000002000004000000100007dbb";
    // Temperature pressed twice (Warm -> Hot -> Tap Cold), then soil and extra rinse.
    const TEMP_HOT: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010069000000013b0400000000200000400000010000002b0003120e0f2e00000000000000002e002e00000000010069000000013b0400000000200000400000010000adbb";
    const TEMP_TAP_COLD: &str =
        "aa6020ec002b0003120e0f2e00000000000000002e002e00000000010069000000013b0400000000200000400000010000002b00030d0e0f2e00000000000000002e002e00000000010069000000013b0400000000200000400000010000a0bb";
    const SOIL_NORMAL_HEAVY: &str =
        "aa6020ec002b00030d0e0f2e00000000000000002e002e00000000010069000000013b0400000000200000400000010000002b00040d0e0f2e000000000000000033003300000000010069000000013b0400000000200000400000010000aebb";
    const EXTRA_RINSE_ON: &str =
        "aa6020ec002b00040d0e0f2e000000000000000033003300000000010069000000013b0400000000200000400000010000002b00040d0f0f2e000000000000000038003800000000010069000000023b040000000020000040004001000007bb";
    // Running after Detecting, 40 of 40 minutes left.
    const RUNNING: &str =
        "aa6020ec002b00040d0f0f2e000000000000000038003800000000032469000000023b0400000000200000100040010000002b00040d0f0f2e0000000000000000280028000000000b0369030000023b040000000020000010004001000001bb";
    // Rinsing. The cloud reported 17 then 15 minutes left, courseSpendPower 48 then 61, and
    // the extra rinse dropped.
    const RINSING_17_MIN: &str =
        "aa6020ec002b00000d0f0f2e0000000000000000120028002f00000c0b69030000023b0400000000200000100040010000002b00000d0f0f2e0000000000000000110028003000000c0b69030000023b04000000002000001000400100000fbb";
    const RINSING_EXTRA_RINSE_DROPPED: &str =
        "aa6020ec002b00000d0f0f2e0000000000000000100028003300000c0b69030000023b0400000000200000100040010000002b00000d0e0f2e00000000000000000f0028003d00000c0b69030000013b040000000020000010000001000070bb";
    // Cold wash toggled off, then on, on Normal (it forces temp to Cold).
    const COLD_WASH_OFF: &str =
        "aa6020ec002b00030e0e0f2e000000000000000042004200000000010069000000011e0400000000240000400000010000002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200000400000010000b3bb";
    const COLD_WASH_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200000400000010000002b00030e0e0f2e000000000000000042004200000000010069000000011e0400000000240000400000010000b3bb";
    // TurboWash toggled on Speed Wash, where it is not course-locked.
    const SPEED_TURBO_ON: &str =
        "aa6020ec002b0001120e104a00000000000000000f000f00000000010069070000011e0400000000000000400000000000002b0001120e104a00000000000000000f000f00000000010069070000011e040000000020000040000000000033bb";
    const SPEED_TURBO_OFF: &str =
        "aa6020ec002b0001120e104a00000000000000000f000f00000000010069070000011e0400000000200000000000000000002b0001120e104a00000000000000000f000f00000000010069070000011e0400000000000000000000000000b3bb";
    // Steam, pre-wash and remote start, each set on Normal.
    const STEAM_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200000000000010000002b0000000e0f2e000000000000000083008300000000010069000000011e0400000000201000000000010000b6bb";
    const PREWASH_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200000000000010000002b0003100e0f2e00000000000000003d003d00000000010069000000011e0400000000600000000000010000cfbb";
    const REMOTE_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200000000000010000002b0003100e0f2e00000000000000002e002e00000000010069000000011e040000000020000010000001000019bb";
    // Child lock, FreshCare and Delay Wash (2 h) toggled on Normal.
    const CHILD_LOCK_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200000400000010000002b0003100e0f2e00000000000000002e002e00000000010069000000011e040000000020000060000001000089bb";
    const CHILD_LOCK_OFF: &str =
        "aa6020ec002b0005100e1023000000000000000082008200000000010069000000011e0400000000200000400000000000002b0003100e0f2e00000000000000002e002e00000000010069000000011e04000000002000004000000100000ebb";
    const FRESH_CARE_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200000400000010000002b0003100e0f2e00000000000000002e002e00000000010069000000011e0400000000200040400000010000a9bb";
    const FRESH_CARE_OFF: &str =
        "aa6020ec002b0005100e1023000000000000000082008200000000010069000000011e0400000000200000400000000000002b0003100e0f2e00000000000000002e002e00000000010069000000011e04000000002000004000000100000ebb";
    const DELAY_120: &str =
        "aa6020ec002b0003100e0f2e0000000000003c002e002e00000000010069000000011e0400000000200000000080010000002b0003100e0f2e00000000000078002e002e00000000010069000000011e0400000000200000000080010000a5bb";
    const DELAY_OFF: &str =
        "aa6020ec002b0005100e1023000000000000000082008200000000010069000000011e0400000000200000400000000000002b0003100e0f2e00000000000000002e002e00000000010069000000011e04000000002000004000000100000ebb";
    // Rinse+Spin subcycle on, then off.
    const RINSE_SPIN_ON: &str =
        "aa6020ec002b0000000e0f2e000000000000000016001600020000010069070000011e0400000000002000000000010000002b0000000e0f2e000000000000000016001600020000010069070000011e04000000000020000000000100009dbb";
    const RINSE_SPIN_OFF: &str =
        "aa6020ec002b0000000e0f2e000000000000000016001600020000010069070000011e0400000000002000000000010000002b0005100e1023000000000000000082008200020000010069000000011e0400000000200000000000000000f6bb";
    // Extra-rinse sweep, RINSE_N = N extra rinses.
    const RINSE_0: &str =
        "aa6020ec002b0000000e1023000000000000000019001900020000010069070000011e0400000000002000000000000000002b0005100e1023000000000000000082008200020000010069000000011e0400000000200000000000000000cbbb";
    const RINSE_1: &str =
        "aa6020ec002b000310110f2e00000000000000003d003d00020000010069000000041e0400000000200000000040010000002b0003100f0f2e000000000000000033003300020000010069000000021e0400000000200000000040010000a5bb";
    const RINSE_2: &str =
        "aa6020ec002b0003100f0f2e000000000000000033003300020000010069000000021e0400000000200000000040010000002b000310100f2e000000000000000038003800020000010069000000031e0400000000200000000040010000b1bb";
    const RINSE_3: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00020000010069000000011e0400000000200000000000010000002b000310110f2e00000000000000003d003d00020000010069000000041e0400000000200000000040010000f1bb";
    // Signal (chime) toggled off, then on.
    const SIGNAL_OFF: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010073000000011e0400000000200000000000010000002b0003100e0f2e00000000000000002e002e00000000010073000000011e000000000020000000000001000019bb";
    const SIGNAL_ON: &str =
        "aa6020ec002b0003100e0f2e00000000000000002e002e00000000010073000000011e0000000000200000000000010000002b0003100e0f2e00000000000000002e002e00000000010073000000011e040000000020000000000001000019bb";

    /// Offset of record B's `0x2B` marker in a `0xEC` frame.
    const REC_B: usize = 50;

    fn meta() -> Metadata {
        Metadata::new("F3P2CYUBE__", "F3P2CYUBE__", "2.10.119")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<PropertyValue> {
        ha.device(DEVICE_ID)?.properties.get(name).cloned()
    }

    fn expect(ha: &MockHaConnection, name: &str, value: impl Into<PropertyValue>) {
        assert_eq!(prop(ha, name), Some(value.into()), "{name}");
    }

    fn first_packet(thinq: &MockThinq2Device) -> String {
        hex_encode(&thinq.outbox()[0]).to_lowercase()
    }

    /// A `0xEB` frame whose record is zero apart from the marker and `rec[offset]`.
    fn eb_with(offset: usize, code: u8) -> Vec<u8> {
        let mut rec = [0u8; 44];
        rec[0] = RECORD_MARKER;
        rec[offset] = code;
        let mut body = vec![0x20, 0xeb, 0x00];
        body.extend_from_slice(&rec);
        let mut frame = vec![0xaa, (body.len() + 4) as u8];
        frame.extend_from_slice(&body);
        frame.extend_from_slice(&[0x00, 0xbb]);
        frame
    }

    #[test]
    fn eb_single_record_frames_decode_with_the_same_offsets_as_ec_record_b() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, EB_RECONNECT_OFF);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "cycles", 59);
        expect(&ha, "remaining_time", 0);
    }

    #[test]
    fn off_and_idle_zeroes_the_timers_and_keeps_the_cycle_counter() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, OFF_IDLE);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "remaining_time", 0);
        expect(&ha, "initial_time", 0);
        expect(&ha, "cycles", 59);
    }

    #[test]
    fn power_on_reports_initial_with_the_course_defaults_and_estimate() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, POWER_ON_SELECTING);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Initial");
        expect(&ha, "course_code", "0x2e");
        expect(&ha, "soil", "Normal");
        expect(&ha, "temp", "Warm");
        expect(&ha, "extra_rinse", "OFF");
        expect(&ha, "spin", "High");
        expect(&ha, "remaining_time", 46);
        expect(&ha, "initial_time", 46);
        expect(&ha, "extra_rinse_count", 0);
    }

    #[test]
    fn single_variable_changes_decode_temp_soil_and_extra_rinse() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, TEMP_HOT);
        expect(&ha, "temp", "Hot");
        emit(&thinq, TEMP_TAP_COLD);
        expect(&ha, "temp", "Tap Cold");
        emit(&thinq, SOIL_NORMAL_HEAVY);
        expect(&ha, "soil", "Normal-Heavy");
        expect(&ha, "remaining_time", 51);
        emit(&thinq, EXTRA_RINSE_ON);
        expect(&ha, "extra_rinse", "ON");
        expect(&ha, "extra_rinse_count", 1);
        expect(&ha, "remaining_time", 56);
    }

    #[test]
    fn signal_toggles_on_rec30_bit_0x04() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, SIGNAL_OFF);
        expect(&ha, "signal", "OFF");
        emit(&thinq, SIGNAL_ON);
        expect(&ha, "signal", "ON");
    }

    #[test]
    fn running_reports_state_previous_state_countdown_and_total() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, RUNNING);
        expect(&ha, "status", "Running");
        expect(&ha, "previous_status", "Detecting");
        expect(&ha, "remaining_time", 40);
        expect(&ha, "initial_time", 40);
        expect(&ha, "cycles", 59);
    }

    #[test]
    fn rinsing_remaining_minutes_and_course_energy_match_the_cloud() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, RINSING_17_MIN);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "previous_status", "Running");
        expect(&ha, "remaining_time", 17);
        expect(&ha, "energy", 48);
        emit(&thinq, RINSING_EXTRA_RINSE_DROPPED);
        expect(&ha, "remaining_time", 15);
        expect(&ha, "energy", 61);
        expect(&ha, "extra_rinse", "OFF");
        expect(&ha, "extra_rinse_count", 0);
    }

    #[test]
    fn cold_wash_toggles_on_rec35_bit_0x04() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, COLD_WASH_OFF);
        expect(&ha, "cold_wash", "OFF");
        emit(&thinq, COLD_WASH_ON);
        expect(&ha, "cold_wash", "ON");
    }

    #[test]
    fn turbo_wash_toggles_on_rec35_bit_0x20_and_0x4a_is_speed_wash() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, SPEED_TURBO_ON);
        expect(&ha, "turbo_wash", "ON");
        expect(&ha, "course", "Speed Wash");
        expect(&ha, "cold_wash", "OFF");
        emit(&thinq, SPEED_TURBO_OFF);
        expect(&ha, "turbo_wash", "OFF");
    }

    #[test]
    fn steam_pre_wash_and_remote_start_bits() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, STEAM_ON);
        expect(&ha, "steam", "ON");
        expect(&ha, "pre_wash", "OFF");
        expect(&ha, "remote_start", "OFF");
        emit(&thinq, PREWASH_ON);
        expect(&ha, "pre_wash", "ON");
        expect(&ha, "steam", "OFF");
        // Normal locks TurboWash on.
        expect(&ha, "turbo_wash", "ON");
        emit(&thinq, REMOTE_ON);
        expect(&ha, "remote_start", "ON");
        expect(&ha, "pre_wash", "OFF");
    }

    #[test]
    fn child_lock_is_rec38_bit_0x20_apart_from_remote_start() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, CHILD_LOCK_ON);
        expect(&ha, "child_lock", "ON");
        expect(&ha, "remote_start", "OFF");
        emit(&thinq, CHILD_LOCK_OFF);
        expect(&ha, "child_lock", "OFF");
    }

    #[test]
    fn fresh_care_toggles_on_rec37_bit_0x40() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, FRESH_CARE_ON);
        expect(&ha, "fresh_care", "ON");
        emit(&thinq, FRESH_CARE_OFF);
        expect(&ha, "fresh_care", "OFF");
    }

    #[test]
    fn every_dial_course_code_decodes_to_its_cloud_name() {
        let (ha, thinq, _dev) = make();
        for (code, name) in [
            (0x05, "Allergiene"),
            (0x0d, "Bedding"),
            (0x16, "Delicates"),
            (0x23, "Heavy Duty"),
            (0x2e, "Normal"),
            (0x30, "Perm. Press"),
            (0x3c, "Sanitary"),
            (0x4a, "Speed Wash"),
            (0x4e, "Spin Only"),
            (0x54, "Towels"),
            (0x55, "Tub Clean"),
            (0x5a, "Bright Whites"),
            (0xff, "Downloaded Course"),
        ] {
            thinq.emit_data(&eb_with(COURSE_OFFSET, code));
            assert_eq!(prop(&ha, "course"), Some(name.into()), "course {code:#x}");
            expect(&ha, "course_code", format!("0x{code:02x}"));
        }
    }

    #[test]
    fn delay_wash_reserve_minutes_are_16_bit_big_endian() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, DELAY_120);
        expect(&ha, "reserve_time", 120);
        expect(&ha, "delay_wash", "ON");
        emit(&thinq, DELAY_OFF);
        expect(&ha, "reserve_time", 0);
        expect(&ha, "delay_wash", "OFF");
    }

    #[test]
    fn rinse_spin_subcycle_reports_temp_and_soil_as_none() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, RINSE_SPIN_ON);
        expect(&ha, "rinse_spin", "ON");
        expect(&ha, "temp", "None");
        expect(&ha, "soil", "None");
        expect(&ha, "steam", "OFF");
        emit(&thinq, RINSE_SPIN_OFF);
        expect(&ha, "rinse_spin", "OFF");
        expect(&ha, "soil", "Heavy");
    }

    #[test]
    fn extra_rinse_sweep_from_0_to_3() {
        let (ha, thinq, _dev) = make();
        for (frame, count) in [(RINSE_0, 0), (RINSE_1, 1), (RINSE_2, 2), (RINSE_3, 3)] {
            emit(&thinq, frame);
            expect(&ha, "extra_rinse", if count > 0 { "ON" } else { "OFF" });
            expect(&ha, "extra_rinse_count", count);
        }
    }

    #[test]
    fn power_off_select_sends_the_captured_wmoff_packet() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power_off", "Power Off");
        assert_eq!(first_packet(&thinq), "aa0df0e5000201ff010200c4bb");
    }

    #[test]
    fn start_pause_and_resume_send_their_captured_packets() {
        let (_ha, thinq, dev) = make();
        for (prop, want) in [
            ("start", "aa0df0e5000201ff010301c6bb"),
            ("pause", "aa0df0e5000201ff010302c1bb"),
            ("resume", "aa0ff0e5000201ff024400030389bb"),
        ] {
            thinq.reset_recorder();
            dev.set_property(prop, "");
            assert_eq!(first_packet(&thinq), want, "{prop}");
        }
    }

    #[test]
    fn start_course_builds_the_config_start_command() {
        let (_ha, thinq, dev) = make();
        for (course, want) in [
            ("Normal", "aa12f0e5000201ff030a2e7f0000030104bb"),
            ("Heavy Duty", "aa12f0e5000201ff030a237f0000030113bb"),
            ("Downloaded Course", "aa12f0e5000201ff030aff7f0000030177bb"),
        ] {
            thinq.reset_recorder();
            dev.set_property("start_course", course);
            assert_eq!(first_packet(&thinq), want, "{course}");
        }
    }

    #[test]
    fn delay_folds_into_the_start_course_blob() {
        let (_ha, thinq, dev) = make();
        dev.set_property("delay", "1");
        thinq.reset_recorder();
        dev.set_property("start_course", "Normal");
        assert_eq!(first_packet(&thinq), "aa12f0e5000201ff030a2e7f003c0301d8bb");
    }

    #[test]
    fn specialty_select_sends_each_captured_smartcourse_download() {
        let (_ha, thinq, dev) = make();
        for (name, frame) in [
            (
                "Sweat Stains",
                "aa2cf0e5000201ff100b650aff0c2e1f10210f1e013d00200e220010003e0034003800350144007f00007bbb",
            ),
            (
                "Swimwear",
                "aa2cf0e5000201ff100b670aff0c161f0e210d1e013d00200e220010003e0034003800350044007f000046bb",
            ),
            (
                "Baby Clothes",
                "aa2cf0e5000201ff100b680aff0c2e1f12210f1e033d00200f220010003e0034013800350044007f000063bb",
            ),
            (
                "Small Load",
                "aa2cf0e5000201ff100b690aff0c441f10210f1e033d00200e220010003e0034003800350044007f00001cbb",
            ),
            (
                "Overnight",
                "aa2cf0e5000201ff100b6a0aff0c2e1f10210d1e033d00200e220010003e0034003800350044017f000066bb",
            ),
            (
                "Single Garments",
                "aa2cf0e5000201ff100b6b0aff0c4a1f12210f1e013d00200e220010003e0034003800350044007f000004bb",
            ),
            (
                "Rainy Day",
                "aa2cf0e5000201ff100b6d0aff0c2e1f1021101e033d00200e220010003e0034003800350144007f00006cbb",
            ),
            (
                "Gym Clothes",
                "aa2cf0e5000201ff100b6e0aff0c4f1f10210e1e013d00200e220010003e0034003800350044007f000003bb",
            ),
            (
                "Color Care",
                "aa2cf0e5000201ff100b6f0aff0c2e1f0e210e1e033d00200e220010003e0034003800350144007f000062bb",
            ),
            (
                "Denim",
                "aa2cf0e5000201ff100b700aff0c2e1f0e210e1e033d00200e220010003e0034003800350144007f00006dbb",
            ),
            (
                "Full Load",
                "aa2cf0e5000201ff100b710aff0c2e1f10210f1e053d00200f220010003e0034003800350044007f00006bbb",
            ),
            (
                "Beachwear",
                "aa2cf0e5000201ff100b730aff0c161f0e210e1e013d00200e220010003e0034003800350044007f000075bb",
            ),
            (
                "New Clothes",
                "aa2cf0e5000201ff100b740aff0c2e1f0e210d1e013d00200e220010003e0034003800350144007f00006cbb",
            ),
            (
                "Half Load",
                "aa2cf0e5000201ff100b760aff0c2e1f10210f1e033d00200e220010003e0034003800350044007f000015bb",
            ),
            (
                "EconoWash",
                "aa2cf0e5000201ff100b780aff0c2e1f0e210f1e033d00200e220010003e0034003801350044007f000014bb",
            ),
            (
                "Delicate Dresses",
                "aa2cf0e5000201ff100b790aff0c161f0e210d1e013d00200e220010003e0034003800350044007f000070bb",
            ),
            (
                "Hand Wash/Wool",
                "aa2cf0e5000201ff100bdd0aff0c221f10210d1e033d00200e220010003e0034003800350044007f0000ccbb",
            ),
        ] {
            thinq.reset_recorder();
            dev.set_property("specialty", name);
            assert_eq!(first_packet(&thinq), frame, "{name}");
        }
    }

    #[test]
    fn specialty_select_snaps_back_to_unknown_after_firing() {
        let (ha, thinq, dev) = make();
        dev.set_property("specialty", "Denim");
        expect(&ha, "specialty", "unknown");
        thinq.reset_recorder();
        dev.set_property("specialty", "unknown");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn downloaded_course_decodes_rec24_to_its_smartcourse_name() {
        let (ha, thinq, _dev) = make();
        for (code, name) in [
            (0x69, "Small Load"),
            (0x70, "Denim"),
            (0x6a, "Overnight"),
            (0xdd, "Hand Wash/Wool"),
            (0x00, "None"),
            (0xab, "0xab"),
        ] {
            thinq.emit_data(&eb_with(DOWNLOAD_COURSE_OFFSET, code));
            assert_eq!(
                prop(&ha, "downloaded_course"),
                Some(name.into()),
                "rec[24]={code:#x}"
            );
        }
    }

    #[test]
    fn remaining_and_initial_time_are_plain_16_bit_minutes() {
        let mut frame = hex_decode(RUNNING);
        assert_eq!(frame[REC_B], RECORD_MARKER);
        frame[REC_B + 14..REC_B + 16].copy_from_slice(&286u16.to_be_bytes());
        frame[REC_B + 16..REC_B + 18].copy_from_slice(&286u16.to_be_bytes());
        let (ha, thinq, _dev) = make();
        thinq.emit_data(&frame);
        expect(&ha, "status", "Running");
        expect(&ha, "remaining_time", 286);
        expect(&ha, "initial_time", 286);
    }

    #[test]
    fn blank_record_b_after_power_is_dropped() {
        let (ha, thinq, _dev) = make();
        emit(&thinq, RUNNING);
        expect(&ha, "cycles", 59);
        let downloaded = prop(&ha, "downloaded_course");
        let mut frame = hex_decode(RUNNING);
        frame[REC_B + 1..REC_B + 44].fill(0);
        frame[REC_B + 4] = 0x0e;
        frame[REC_B + 23] = 0x10;
        thinq.emit_data(&frame);
        expect(&ha, "cycles", 59);
        expect(&ha, "status", "Running");
        assert_eq!(prop(&ha, "downloaded_course"), downloaded);
    }

    #[test]
    fn start_requests_status_and_the_model_is_registered() {
        let (_ha, thinq, dev) = make();
        dev.start();
        assert_eq!(first_packet(&thinq), "aa0ef0ed1121010000001800b5bb");
        assert!(crate::t2_factory("F3P2CYUBE__").is_some());
    }
}
