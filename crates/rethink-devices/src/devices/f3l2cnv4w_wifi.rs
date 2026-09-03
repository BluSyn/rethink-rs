//! F3L2CNV4W_WIFI front-load washer (ThinQ1, deviceType 201).
//!
//! Port of unmerged upstream PR #146. 24-byte Monitoring.protocol map and
//! enums come from LG modelJSON (via wideq), not invented offsets. PowerOff /
//! OperationStop / OperationStart match WTDN3's Control envelope; OperationStart
//! course bytes (including Option3 InitialBit 0x20) are pinned to a 2026-08-22
//! Normal-course capture. Remote PowerOn is not implemented (no modelJSON action).

use crate::device_trait::DeviceHandler;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rethink_core::device_base::default_config;
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq1Device;
use rethink_util::sync::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const INITIAL_BIT: u8 = 0x20;
const DEFAULT_COURSE_ID: u8 = 5;

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (5, "Initial"),
    (6, "Paused"),
    (7, "Error auto-off"),
    (10, "Reserved"),
    (20, "Detecting"),
    (21, "Add drain"),
    (22, "Load display"),
    (23, "Running"),
    (30, "Rinsing"),
    (31, "Rinse hold"),
    (40, "Spinning"),
    (50, "Drying"),
    (60, "End"),
    (61, "Fresh care"),
    (79, "Smart diagnosis"),
    (81, "Tub clean count alarm"),
    (101, "Smart diagnosis data"),
];

const ERRORS: &[(u8, &str)] = &[
    (0, "OK"),
    (1, "Door lock error (DE2)"),
    (2, "No fill error (IE)"),
    (3, "Not draining error (OE)"),
    (4, "Out of balance error (UE)"),
    (5, "Overfill error (FE)"),
    (6, "Water sensor error (PE)"),
    (7, "Thermistor error (tE)"),
    (8, "Locked motor error (LE)"),
    (9, "CE error"),
    (10, "dHE error"),
    (11, "Power failure error (PF)"),
    (12, "Freeze error (FF)"),
    (13, "dCE error"),
    (14, "AE error"),
    (15, "EEPROM error (EE)"),
    (16, "Suds error (Sud)"),
    (17, "Door open error (DE1)"),
    (18, "Sliding lid open error (LOE)"),
    (19, "PS error"),
];

const SOIL: &[(u8, &str)] = &[
    (0, "-"),
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];

const SPIN: &[(u8, &str)] = &[
    (0, "-"),
    (1, "No spin"),
    (2, "Low"),
    (3, "Medium"),
    (4, "High"),
    (5, "Extra high"),
];

const TEMP: &[(u8, &str)] = &[
    (0, "-"),
    (1, "Tap cold"),
    (2, "Cold"),
    (3, "Semi warm"),
    (4, "Warm"),
    (5, "Warm"),
    (6, "Hot"),
    (7, "Extra hot"),
];

const DRY_LEVEL: &[(u8, &str)] = &[
    (0, "-"),
    (5, "Turbo"),
    (6, "Wind"),
    (7, "30 min"),
    (8, "60 min"),
    (9, "90 min"),
    (10, "120 min"),
    (11, "150 min"),
];

const RINSE_COUNT: &[(u8, &str)] = &[(0, "0"), (1, "1"), (2, "2"), (3, "3")];

const AP_COURSE: &[(u8, &str)] = &[
    (1, "Tub Clean"),
    (2, "Bright Whites"),
    (3, "Bedding"),
    (4, "Heavy Duty"),
    (5, "Normal"),
    (6, "Perm Press"),
    (7, "Delicates"),
    (8, "Towels"),
    (9, "Speed Wash"),
    (10, "Download Course"),
    (11, "Spin Only"),
];

const OP_COURSE: &[(u8, &str)] = &[
    (0, "-"),
    (1, "Refresh"),
    (2, "Sanitary"),
    (3, "Allergiene"),
    (4, "Bedding"),
    (5, "Perm Press"),
    (6, "Normal"),
    (7, "Heavy Duty"),
    (8, "Bright Whites"),
    (9, "Cold Care"),
    (10, "Delicates"),
    (11, "Hand Wash"),
    (12, "Speed Wash"),
    (13, "Tub Clean"),
    (14, "Towels"),
    (15, "Small Load"),
    (16, "Rinse+Spin"),
    (17, "Rugged"),
    (18, "KidsWears"),
    (19, "WorkOut Wear"),
    (20, "Drain+Spin"),
    (21, "Sportswear"),
    (22, "Jumbo Wash"),
];

const SMART_COURSE: &[(u8, &str)] = &[
    (51, "Small Load"),
    (52, "Color Care"),
    (53, "Beachwear"),
    (54, "New Clothes"),
    (55, "Denim"),
    (59, "Swimwear"),
    (60, "Rainy Day"),
    (61, "Gym Clothes"),
    (63, "Sweat Stains"),
    (64, "Single Garments"),
    (100, "Baby Clothes"),
    (105, "Overnight Wash"),
    (106, "Econo Wash"),
    (107, "Delicate Dresses"),
    (108, "Half Load Wash"),
    (109, "Full Load Wash"),
];

struct CourseDefaults {
    soil: u8,
    spin: u8,
    water_temp: u8,
    op_course: u8,
}

fn course_defaults(id: u8) -> Option<CourseDefaults> {
    Some(match id {
        1 => CourseDefaults { soil: 0, spin: 3, water_temp: 0, op_course: 13 },
        2 => CourseDefaults { soil: 3, spin: 5, water_temp: 6, op_course: 8 },
        3 => CourseDefaults { soil: 3, spin: 3, water_temp: 4, op_course: 4 },
        4 => CourseDefaults { soil: 5, spin: 5, water_temp: 4, op_course: 7 },
        5 => CourseDefaults { soil: 3, spin: 5, water_temp: 4, op_course: 6 },
        6 => CourseDefaults { soil: 3, spin: 3, water_temp: 4, op_course: 5 },
        7 => CourseDefaults { soil: 3, spin: 3, water_temp: 2, op_course: 10 },
        8 => CourseDefaults { soil: 3, spin: 5, water_temp: 4, op_course: 14 },
        9 => CourseDefaults { soil: 1, spin: 5, water_temp: 6, op_course: 12 },
        _ => return None,
    })
}

const SELECTABLE_COURSE_NAMES: &[&str] = &[
    "Tub Clean",
    "Bright Whites",
    "Bedding",
    "Heavy Duty",
    "Normal",
    "Perm Press",
    "Delicates",
    "Towels",
    "Speed Wash",
];

fn named(table: &[(u8, &str)], v: u8) -> String {
    table
        .iter()
        .find(|(k, _)| *k == v)
        .map(|(_, n)| (*n).to_string())
        .unwrap_or_else(|| v.to_string())
}

fn course_id_for_name(name: &str) -> Option<u8> {
    SELECTABLE_COURSE_NAMES
        .iter()
        .enumerate()
        .find(|(_, n)| **n == name)
        .map(|(i, _)| (i as u8) + 1)
}

fn encode_course(ap: u8, smart: u8, d: &CourseDefaults) -> String {
    let bytes = [
        ap,
        d.soil,
        d.spin,
        d.water_temp,
        0,
        0,
        0,
        0,
        0,
        INITIAL_BIT,
        d.op_course,
        smart,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    STANDARD.encode(bytes)
}

fn encode_course_start(course_id: u8) -> Option<String> {
    course_defaults(course_id).map(|d| encode_course(course_id, 0, &d))
}

fn enum_options<'a>(table: &'a [(u8, &'a str)]) -> Vec<&'a str> {
    table.iter().map(|(_, n)| *n).collect()
}

pub struct Device {
    id: String,
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    publish_cache: Mutex<HashMap<String, String>>,
    pending_course_id: Mutex<u8>,
    course_selected_by_user: Mutex<bool>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq1Device>, meta: Metadata) -> Arc<Self> {
        let id = thinq.id().to_string();
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.components = [
            (
                "power".into(),
                json!({
                    "platform": "switch",
                    "unique_id": "$deviceid-power",
                    "state_topic": "$this/power",
                    "command_topic": "$this/power/set",
                    "name": "",
                    "icon": "mdi:washing-machine",
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
                "course_selection".into(),
                json!({
                    "platform": "select",
                    "unique_id": "$deviceid-course-selection",
                    "state_topic": "$this/course_selection",
                    "command_topic": "$this/course_selection/set",
                    "availability": [{ "topic": "$this/controls_available" }],
                    "name": "Course selection",
                    "icon": "mdi:tune-vertical-variant",
                    "options": SELECTABLE_COURSE_NAMES,
                }),
            ),
            (
                "remote_start_button".into(),
                json!({
                    "platform": "button",
                    "unique_id": "$deviceid-remote-start-button",
                    "command_topic": "$this/remote_start_button/set",
                    "availability": [{ "topic": "$this/remote_start_available" }],
                    "payload_press": "",
                    "name": "Remote Start",
                    "icon": "mdi:play-circle-outline",
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
                    "options": enum_options(STATES),
                }),
            ),
            (
                "pre_state".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-pre-state",
                    "state_topic": "$this/pre_state",
                    "name": "Previous status",
                    "icon": "mdi:history",
                    "entity_category": "diagnostic",
                    "device_class": "enum",
                    "options": enum_options(STATES),
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
                    "unique_id": "$deviceid-error-message",
                    "state_topic": "$this/error_message",
                    "name": "Error message",
                    "icon": "mdi:alert-circle-outline",
                    "device_class": "enum",
                    "entity_category": "diagnostic",
                    "options": enum_options(ERRORS),
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
                "op_course".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-op-course",
                    "state_topic": "$this/op_course",
                    "name": "Internal course code",
                    "icon": "mdi:cog-outline",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "smart_course".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-smart-course",
                    "state_topic": "$this/smart_course",
                    "name": "Downloaded course",
                    "icon": "mdi:cloud-download-outline",
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
                    "icon": "mdi:water-opacity",
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
                "temp".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-temp",
                    "state_topic": "$this/temp",
                    "name": "Water temperature",
                    "icon": "mdi:thermometer",
                }),
            ),
            (
                "dry_level".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-dry-level",
                    "state_topic": "$this/dry_level",
                    "name": "Dry level",
                    "icon": "mdi:tumble-dryer",
                }),
            ),
            (
                "rinse_count".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-rinse-count",
                    "state_topic": "$this/rinse_count",
                    "name": "Rinse count",
                    "icon": "mdi:water-sync",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "extra_rinse_count".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-extra-rinse-count",
                    "state_topic": "$this/extra_rinse_count",
                    "name": "Extra rinse count",
                    "icon": "mdi:water-plus-outline",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "child_lock".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-child-lock",
                    "state_topic": "$this/child_lock",
                    "name": "Child lock",
                    "device_class": "lock",
                    "entity_category": "diagnostic",
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
                "prewash".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-prewash",
                    "state_topic": "$this/prewash",
                    "name": "Prewash",
                    "icon": "mdi:water-outline",
                }),
            ),
            (
                "turbowash".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-turbowash",
                    "state_topic": "$this/turbowash",
                    "name": "TurboWash",
                    "icon": "mdi:speedometer",
                }),
            ),
            (
                "extra_rinse".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-extra-rinse",
                    "state_topic": "$this/extra_rinse",
                    "name": "Extra rinse",
                    "icon": "mdi:water-plus-outline",
                }),
            ),
            (
                "coldwash".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-coldwash",
                    "state_topic": "$this/coldwash",
                    "name": "Cold wash",
                    "icon": "mdi:snowflake",
                }),
            ),
            (
                "fresh_care".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-fresh-care",
                    "state_topic": "$this/fresh_care",
                    "name": "Fresh care",
                    "icon": "mdi:air-filter",
                }),
            ),
            (
                "remote_start".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-remote_start",
                    "state_topic": "$this/remote_start",
                    "name": "Remote start armed",
                    "icon": "mdi:play-circle-outline",
                }),
            ),
            (
                "tub_clean_count".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-tub-clean-count",
                    "state_topic": "$this/tub_clean_count",
                    "name": "Cycles since tub clean",
                    "icon": "mdi:counter",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "load_level".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-load-level",
                    "state_topic": "$this/load_level",
                    "name": "Load level (raw)",
                    "icon": "mdi:scale-bathroom",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "reserve_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-reserve-time",
                    "state_topic": "$this/reserve_time",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "name": "Delay wash time",
                }),
            ),
            (
                "initial_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-initial_time",
                    "state_topic": "$this/initial_time",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "name": "Initial time",
                }),
            ),
            (
                "remaining_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-remaining_time",
                    "state_topic": "$this/remaining_time",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "name": "Remaining time",
                }),
            ),
            (
                "last_cycle_energy".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-last-cycle-energy",
                    "state_topic": "$this/last_cycle_energy",
                    "device_class": "energy",
                    "state_class": "measurement",
                    "unit_of_measurement": "Wh",
                    "name": "Last cycle energy usage",
                    "icon": "mdi:lightning-bolt-outline",
                }),
            ),
            (
                "last_cycle_course".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-last-cycle-course",
                    "state_topic": "$this/last_cycle_course",
                    "name": "Last completed course",
                    "icon": "mdi:pin-outline",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "last_cycle_completed".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-last-cycle-completed",
                    "state_topic": "$this/last_cycle_completed",
                    "name": "Last cycle completed at",
                    "icon": "mdi:clock-check-outline",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "initial_bit".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-initial-bit",
                    "state_topic": "$this/initial_bit",
                    "name": "Initial bit",
                    "icon": "mdi:flag-outline",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "option3_raw".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-option3-raw",
                    "state_topic": "$this/option3_raw",
                    "name": "Option3 (raw)",
                    "icon": "mdi:code-braces",
                    "entity_category": "diagnostic",
                }),
            ),
        ]
        .into_iter()
        .collect();

        ha.publish_property(&id, "availability", "online".into());
        ha.publish_config(&id, &config);

        let this = Arc::new(Self {
            id,
            ha,
            thinq: thinq.clone(),
            publish_cache: Mutex::new(HashMap::new()),
            pending_course_id: Mutex::new(DEFAULT_COURSE_ID),
            course_selected_by_user: Mutex::new(false),
        });

        this.publish_property("course_selection", "Normal".into());
        this.publish_property("controls_available", "offline".into());
        this.publish_property("remote_start_available", "offline".into());

        let t = this.clone();
        thinq.on_data(Box::new(move |buf| t.process_data(buf)));
        let t2 = this.clone();
        thinq.on_diagmon(Box::new(move |dtype, decoded| t2.process_diagmon(dtype, decoded)));
        this
    }

    fn publish_property(&self, prop: &str, value: PropertyValue) {
        {
            let mut cache = self.publish_cache.lock();
            if let Some(prev) = cache.get(prop) {
                if value.eq_str(prev) {
                    return;
                }
            }
            cache.insert(prop.to_string(), value.as_string());
        }
        self.ha.publish_property(&self.id, prop, value);
    }

    fn publish_on_off(&self, prop: &str, on: bool) {
        let s = if on { "ON" } else { "OFF" };
        {
            let mut cache = self.publish_cache.lock();
            if cache.get(prop).map(|c| c.as_str() == s).unwrap_or(false) {
                return;
            }
            cache.insert(prop.to_string(), s.to_string());
        }
        self.ha.publish_property(&self.id, prop, s.into());
    }

    fn process_data(&self, buf: &[u8]) {
        if buf.len() < 24 {
            return;
        }
        let state = buf[0];
        let time_remain = buf[1] as i64 * 60 + buf[2] as i64;
        let time_initial = buf[3] as i64 * 60 + buf[4] as i64;
        let ap_course = buf[5];
        let error = buf[6];
        let soil = buf[7];
        let spin = buf[8];
        let temp = buf[9];
        let rinse_option = buf[10];
        let dry_level = buf[11];
        let reserve_h = buf[12];
        let reserve_m = buf[13];
        let option1 = buf[14];
        let option2 = buf[15];
        let option3 = buf[16];
        let pre_state = buf[19];
        let smart_course = buf[20];
        let tcl_count = buf[21];
        let op_course = buf[22];
        let load_level = buf[23];
        let rinse_count = rinse_option & 0x0f;
        let extra_rinse_count = (rinse_option >> 4) & 0x0f;

        self.publish_on_off("power", state > 0);
        self.publish_property("error_message", named(ERRORS, error).into());
        self.publish_on_off("error", error != 0);
        self.publish_property("status", named(STATES, state).into());
        self.publish_property(
            "controls_available",
            if state == 0 || state == 5 {
                "online"
            } else {
                "offline"
            }
            .into(),
        );
        self.publish_property(
            "remote_start_available",
            if state == 0 || state == 5 || state == 6 {
                "online"
            } else {
                "offline"
            }
            .into(),
        );
        self.publish_property("pre_state", named(STATES, pre_state).into());
        self.publish_property("course", named(AP_COURSE, ap_course).into());
        self.publish_property("op_course", named(OP_COURSE, op_course).into());
        self.publish_property("smart_course", named(SMART_COURSE, smart_course).into());
        self.publish_property("soil", named(SOIL, soil).into());
        self.publish_property("spin", named(SPIN, spin).into());
        self.publish_property("temp", named(TEMP, temp).into());
        self.publish_property("dry_level", named(DRY_LEVEL, dry_level).into());
        self.publish_property("rinse_count", named(RINSE_COUNT, rinse_count).into());
        self.publish_property(
            "extra_rinse_count",
            named(RINSE_COUNT, extra_rinse_count).into(),
        );
        self.publish_on_off("child_lock", option1 & 0x01 != 0);
        self.publish_on_off("steam", option1 & 0x04 != 0);
        self.publish_on_off("prewash", option1 & 0x08 != 0);
        self.publish_on_off("turbowash", option1 & 0x80 != 0);
        self.publish_on_off("extra_rinse", option1 & 0x40 != 0);
        self.publish_on_off("fresh_care", option2 & 0x01 != 0);
        self.publish_on_off("coldwash", option2 & 0x10 != 0);
        self.publish_on_off("remote_start", option2 & 0x80 != 0);
        self.publish_on_off("initial_bit", option3 & INITIAL_BIT != 0);
        self.publish_property("option3_raw", i64::from(option3).into());
        self.publish_property("tub_clean_count", i64::from(tcl_count).into());
        self.publish_property("load_level", i64::from(load_level).into());
        self.publish_property(
            "reserve_time",
            (i64::from(reserve_h) * 60 + i64::from(reserve_m)).into(),
        );
        self.publish_property("initial_time", time_initial.into());
        self.publish_property("remaining_time", time_remain.into());

        if !*self.course_selected_by_user.lock() && course_defaults(ap_course).is_some() {
            *self.pending_course_id.lock() = ap_course;
            self.publish_property("course_selection", named(AP_COURSE, ap_course).into());
        }
    }

    fn process_diagmon(&self, diag_mon_type: &str, decoded: &serde_json::Value) {
        if diag_mon_type != "WasherMonitoring" {
            return;
        }
        let Some(info) = decoded.pointer("/lgedmRoot/energyMonInfo") else {
            return;
        };
        if let Some(power) = info.get("power").and_then(|v| v.as_i64()) {
            self.publish_property("last_cycle_energy", power.into());
        }
        if let Some(course) = info.get("course").and_then(|v| v.as_u64()) {
            self.publish_property("last_cycle_course", named(AP_COURSE, course as u8).into());
        }
        if let Some(date) = info.get("useDate").and_then(|v| v.as_str()) {
            self.publish_property("last_cycle_completed", date.into());
        }
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if prop == "power" && mqtt_value == "OFF" {
            self.thinq.send(json!({
                "Cmd": "Control",
                "CmdOpt": "Power",
                "Value": "Off",
                "Format": "B64",
                "Data": ""
            }));
        }
        if prop == "pause" {
            self.thinq.send(json!({
                "Cmd": "Control",
                "CmdOpt": "Operation",
                "Value": "Stop",
                "Format": "B64",
                "Data": ""
            }));
        }
        if prop == "course_selection" {
            if let Some(id) = course_id_for_name(mqtt_value) {
                *self.pending_course_id.lock() = id;
                *self.course_selected_by_user.lock() = true;
                self.publish_property("course_selection", mqtt_value.into());
            }
        }
        if prop == "remote_start_button" {
            if let Some(data) = encode_course_start(*self.pending_course_id.lock()) {
                self.thinq.send(json!({
                    "Cmd": "Control",
                    "CmdOpt": "Operation",
                    "Value": "Start",
                    "Format": "B64",
                    "Data": data
                }));
            }
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.id
    }
    fn start(&self) {
        self.thinq.send(json!({ "Cmd": "Mon", "CmdOpt": "Start" }));
    }
    fn drop_device(&self) {
        self.ha
            .publish_property(&self.id, "availability", "offline".into());
    }
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
    }
    fn publish_config(&self) {}
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_trait::DeviceHandler;
    use crate::test_support::{make_t1, prop, DEVICE_ID};
    use rethink_core::{hex_decode, Thinq1Device};

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq1Device>,
        Arc<Device>,
    ) {
        make_t1("F3L2CNV4W_WIFI", Device::new)
    }

    const SAMPLE_STATE_OFF: &str = "000000000000000000000000000000000000000000000000";
    const SAMPLE_STATE_RUNNING_NORMAL: &str = "170105010a050003050410000000848100000005000c0600";
    const SAMPLE_STATE_ERROR_DE1: &str = "070000000005110000000000000000000000000000000000";
    const SAMPLE_STATE_REAL_MIDCYCLE: &str = "14011101110500030504020000000080020000056a300000";
    const SAMPLE_STATE_PAUSED: &str = "060000000000000000000000000000000000000000000000";

    fn b64_bytes(sent: &serde_json::Value) -> Vec<u8> {
        STANDARD
            .decode(sent["Data"].as_str().unwrap())
            .unwrap()
    }

    #[test]
    fn config_exposes_expected_components() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        for c in [
            "power",
            "pause",
            "course_selection",
            "remote_start_button",
            "status",
            "pre_state",
            "error",
            "error_message",
            "course",
            "op_course",
            "smart_course",
            "soil",
            "spin",
            "temp",
            "dry_level",
            "rinse_count",
            "extra_rinse_count",
            "child_lock",
            "steam",
            "prewash",
            "turbowash",
            "extra_rinse",
            "coldwash",
            "fresh_care",
            "remote_start",
            "initial_bit",
            "option3_raw",
            "tub_clean_count",
            "load_level",
            "reserve_time",
            "initial_time",
            "remaining_time",
        ] {
            assert!(comps.contains_key(c), "component {c}");
        }
        let opts = comps["status"]["options"].as_array().unwrap();
        assert!(opts.iter().any(|v| v == "Running"));
        assert!(opts.iter().any(|v| v == "Error auto-off"));
        assert_eq!(
            comps["course_selection"]["options"],
            json!(SELECTABLE_COURSE_NAMES)
        );
        assert!(comps["course_selection"].get("availability_topic").is_none());
        assert_eq!(
            comps["course_selection"]["availability"],
            json!([{ "topic": "$this/controls_available" }])
        );
        assert_eq!(
            comps["remote_start_button"]["availability"],
            json!([{ "topic": "$this/remote_start_available" }])
        );
    }

    #[test]
    fn course_selection_defaults_and_idle_apcourse_zero() {
        let (ha, thinq, _) = make();
        assert_eq!(prop(&ha, "course_selection").as_deref(), Some("Normal"));
        thinq.emit_data(&hex_decode(SAMPLE_STATE_OFF));
        assert_eq!(prop(&ha, "course_selection").as_deref(), Some("Normal"));
    }

    #[test]
    fn controls_default_offline_then_track_state() {
        let (ha, thinq, _) = make();
        assert_eq!(prop(&ha, "controls_available").as_deref(), Some("offline"));
        assert_eq!(prop(&ha, "remote_start_available").as_deref(), Some("offline"));
        thinq.emit_data(&hex_decode(SAMPLE_STATE_OFF));
        assert_eq!(prop(&ha, "controls_available").as_deref(), Some("online"));
        assert_eq!(prop(&ha, "remote_start_available").as_deref(), Some("online"));
        thinq.emit_data(&hex_decode(SAMPLE_STATE_RUNNING_NORMAL));
        assert_eq!(prop(&ha, "controls_available").as_deref(), Some("offline"));
        assert_eq!(prop(&ha, "remote_start_available").as_deref(), Some("offline"));
        thinq.emit_data(&hex_decode(SAMPLE_STATE_OFF));
        assert_eq!(prop(&ha, "controls_available").as_deref(), Some("online"));
        thinq.emit_data(&hex_decode(SAMPLE_STATE_PAUSED));
        assert_eq!(prop(&ha, "controls_available").as_deref(), Some("offline"));
        assert_eq!(prop(&ha, "remote_start_available").as_deref(), Some("online"));
    }

    #[test]
    fn off_state() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_STATE_OFF));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("-"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("-"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("-"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("0"));
    }

    #[test]
    fn running_normal() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_STATE_RUNNING_NORMAL));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "pre_state").as_deref(), Some("Initial"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Extra high"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Warm"));
        assert_eq!(prop(&ha, "rinse_count").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "extra_rinse_count").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fresh_care").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "coldwash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("12"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("70"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("65"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "op_course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_bit").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "option3_raw").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "load_level").as_deref(), Some("0"));
    }

    #[test]
    fn real_midcycle_capture() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_STATE_REAL_MIDCYCLE));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Detecting"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("48"));
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Econo Wash"));
        assert_eq!(prop(&ha, "initial_bit").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "option3_raw").as_deref(), Some("2"));
    }

    #[test]
    fn door_open_error() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_STATE_ERROR_DE1));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Error auto-off"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("Door open error (DE1)"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
    }

    #[test]
    fn short_frames_ignored() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AABBCC"));
        assert_eq!(prop(&ha, "course_selection").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "controls_available").as_deref(), Some("offline"));
        assert_eq!(prop(&ha, "remote_start_available").as_deref(), Some("offline"));
        assert!(prop(&ha, "status").is_none());
        assert!(prop(&ha, "power").is_none());
    }

    #[test]
    fn writes_and_start() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(thinq.sent()[0]["CmdOpt"], "Power");
        assert_eq!(thinq.sent()[0]["Value"], "Off");
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert!(thinq.sent().is_empty());
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(thinq.sent()[0]["CmdOpt"], "Operation");
        assert_eq!(thinq.sent()[0]["Value"], "Stop");
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(thinq.sent()[0]["Cmd"], "Mon");
        assert_eq!(thinq.sent()[0]["CmdOpt"], "Start");
    }

    #[test]
    fn remote_start_default_normal_matches_capture() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("remote_start_button", "");
        assert_eq!(thinq.sent().len(), 1);
        assert_eq!(thinq.sent()[0]["Value"], "Start");
        assert_eq!(
            b64_bytes(&thinq.sent()[0]),
            vec![5, 3, 5, 4, 0, 0, 0, 0, 0, 0x20, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn smart_course_name_is_noop() {
        let (ha, thinq, dev) = make();
        dev.set_property("course_selection", "Small Load");
        assert_eq!(prop(&ha, "course_selection").as_deref(), Some("Normal"));
        thinq.reset_recorder();
        dev.set_property("remote_start_button", "");
        assert_eq!(
            b64_bytes(&thinq.sent()[0]),
            vec![5, 3, 5, 4, 0, 0, 0, 0, 0, 0x20, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn user_course_pick_sticks_and_encodes_heavy_duty() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_STATE_RUNNING_NORMAL));
        assert_eq!(prop(&ha, "course_selection").as_deref(), Some("Normal"));
        dev.set_property("course_selection", "Heavy Duty");
        assert_eq!(prop(&ha, "course_selection").as_deref(), Some("Heavy Duty"));
        thinq.emit_data(&hex_decode(SAMPLE_STATE_RUNNING_NORMAL));
        assert_eq!(prop(&ha, "course_selection").as_deref(), Some("Heavy Duty"));
        thinq.reset_recorder();
        dev.set_property("remote_start_button", "");
        assert_eq!(
            b64_bytes(&thinq.sent()[0]),
            vec![4, 5, 5, 4, 0, 0, 0, 0, 0, 0x20, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn diagmon_energy_mon_info() {
        let (ha, thinq, _) = make();
        thinq.emit_diagmon(
            "WasherMonitoring",
            json!({
                "lgedmRoot": {
                    "energyMonInfo": {
                        "event": 2,
                        "course": 5,
                        "power": 154,
                        "option": "FAERAREFAAMFBAIAAAAAgAIAAAVqMAAA",
                        "dlCourse": 106,
                        "useDate": "20260822 23:48:24"
                    }
                }
            }),
        );
        assert_eq!(prop(&ha, "last_cycle_energy").as_deref(), Some("154"));
        assert_eq!(prop(&ha, "last_cycle_course").as_deref(), Some("Normal"));
        assert_eq!(
            prop(&ha, "last_cycle_completed").as_deref(),
            Some("20260822 23:48:24")
        );
    }

    #[test]
    fn diagmon_tubinfo_ignored() {
        let (ha, thinq, _) = make();
        thinq.emit_diagmon(
            "WasherMonitoring",
            json!({ "lgedmRoot": { "tubInfo": { "event": 2, "count": 49, "maxCount": 30 } } }),
        );
        assert!(prop(&ha, "last_cycle_energy").is_none());
    }
}
