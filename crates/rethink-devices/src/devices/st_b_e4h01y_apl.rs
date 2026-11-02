//! LG Styler S5BBP (ThinQ model ST_B_E4H01Y_APL, deviceType 203, AABB class 0x31).
//!
//! Offsets were recovered by injecting single-byte-changed state frames into LG's cloud and
//! reading its decode back. A state frame is a 13-byte header, one 40-byte record at rest or two
//! (previous, then current) while the appliance is busy, and one trailing byte. The payload must
//! be whole records and agree with the record-bytes field, which also rejects the course-name
//! lists, the 35-byte non-state frame and the heartbeat. Energy monitoring and the internal
//! lighting time are deliberately not published: LG declares no unit or value list for them.
//!
//! Writes are LG's own `ControlWifi` frames, `F0 24 <type> <len> [<sub>] <value…>`, and course
//! start/resume blocks under `F0 26`. Course select is a local choice that Start course sends.
//! Writes are echoed straight away unless the appliance is known to refuse them: any setting while
//! powered off, a course while an error stands, night-care times while night care is disabled.
//!
//! Port of upstream PR #115.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::PropertyValue;
use rethink_core::{hex_decode, HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const HEADER_LEN: usize = 13;
const TRAILER_LEN: usize = 1;
const RECORD_LEN: usize = 40;
// Big-endian count of the record bytes the frame says it carries.
const RECORD_BYTES_OFF: usize = 11;
const STATE_TAG: u8 = 0x31;

// Offsets within a record; a single-record frame's record starts at whole-frame offset 15.
const OFF_STATE: usize = 2;
const OFF_REMAIN_H: usize = 3;
const OFF_REMAIN_M: usize = 4;
const OFF_INITIAL_H: usize = 5;
const OFF_INITIAL_M: usize = 6;
const OFF_COURSE: usize = 7;
const OFF_ERROR: usize = 8;
const OFF_PRE_STATE: usize = 9;
const OFF_RESERVE_H: usize = 14;
const OFF_RESERVE_M: usize = 15;
const OFF_FLAGS_A: usize = 16;
const OFF_SMART_COURSE: usize = 22;
const OFF_DOWNLOAD_COURSE1: usize = 26;
const OFF_BUZZER: usize = 30;
const OFF_FLAGS_B: usize = 31;
const OFF_END_MELODY: usize = 32;
const OFF_FLAGS_C: usize = 34;
const OFF_NC_START_H: usize = 35;
const OFF_NC_START_M: usize = 36;
const OFF_NC_END_H: usize = 37;
const OFF_NC_END_M: usize = 38;
const OFF_TCL: usize = 39;

const F_CHILD_LOCK: u8 = 0x01;
const F_NIGHT_DRY: u8 = 0x02;
const F_REMOTE_START: u8 = 0x08;
const F_REMOTE_MAINTAIN: u8 = 0x04;
const F_IS_LAST_COURSE: u8 = 0x01;
const F_SC_FINEDUST: u8 = 0x02;
const F_SC_HUMIDITY: u8 = 0x04;
const F_SC_NIGHTCARE: u8 = 0x08;

const SET_STATE: [u8; 2] = [0xf0, 0x24];
const CTRL_POWER: u8 = 0x01;
const CTRL_PAUSE: u8 = 0x04;
const CTRL_REMOTE_MAINTAIN: u8 = 0x10;
// UPCENTER settings carry a sub id between the length and the value.
const CTRL_UPCENTER: u8 = 0x13;
const UP_END_MELODY: u8 = 0x01;
const UP_BUZZER: u8 = 0x03;
const UP_SC_FINEDUST: u8 = 0x04;
const UP_SC_HUMIDITY: u8 = 0x05;
const UP_SC_NIGHTCARE: u8 = 0x06;
const UP_NC_START: u8 = 0x07;
const UP_NC_END: u8 = 0x08;
const UP_IS_LAST_COURSE: u8 = 0x09;

const RUN_COURSE: [u8; 2] = [0xf0, 0x26];
const COURSE_HEAD: [u8; 8] = [0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
// Resume carries the course id and nothing else, one byte shorter than a start block.
const RESUME_BLOCK_LEN: usize = 45;

// This model's `Course[id].function` defaults in LG's declaration order, with TimeDry folded into
// bit 0x80 of the first byte. Only Standard (1) and Quick (3) were captured off LG's converter.
// Course 20 shares course 18's block on purpose: the appliance takes the duration from the id.
const COURSE_PARAMS: &[(u8, &str)] = &[
    (
        1,
        "02645a00000005005a05b45a01b4001ab40000000000000000000000000000000000000000",
    ),
    (
        3,
        "82645a00000003005a00000001b4000eb40000000000000000000000000000000000000000",
    ),
    (
        5,
        "02645a00000007005a05c85a01c80031b40000000000000000000000000000000000000000",
    ),
    (
        6,
        "82000000000004000000000001000014780000000000000000000000000000000000000000",
    ),
    (
        7,
        "82645a00000004005a04b45a01b40017780000000000000000000000000000000000000000",
    ),
    (
        11,
        "0200002d000006000008000003000023780000000000000000000000000000000000000000",
    ),
    (
        15,
        "0000000000000000000000000000005a780000000000000000000000000000000000000000",
    ),
    (
        17,
        "8000000000000000000000000000001e780000000000000000000000000000000000000000",
    ),
    (
        18,
        "8000000000000000000000000000003c780000000000000000000000000000000000000000",
    ),
    (
        19,
        "8000000000000000000000000000005a780000000000000000000000000000000000000000",
    ),
    (
        20,
        "8000000000000000000000000000003c780000000000000000000000000000000000000000",
    ),
    (
        23,
        "80000000000000000000000000000078000000000000000000000000000000000000000000",
    ),
    (
        24,
        "800000000000000000000000000000f0000000000000000000000000000000000000000000",
    ),
    (
        28,
        "82005a00000005005a05b45a01b4002eb40000000000000000000000000000000000000000",
    ),
    (
        30,
        "82000000000000000000000005c80000000000000000000003005a02c85a01c80028c80000",
    ),
    (
        31,
        "8200002d000007000008000003000043000000000000000000000000000000000000000000",
    ),
    (
        32,
        "8200002d000006000008000003000028000000000000000000000000000000000000000000",
    ),
    (
        33,
        "8000000000000000000000000000001e780000000000000000000000000000000000000000",
    ),
    (
        36,
        "8200002d00000600000800000300002b780000000000000000000000000000000000000000",
    ),
    (
        37,
        "8200002d00000600000800000300002b780000000000000000000000000000000000000000",
    ),
    (
        38,
        "8200002d00000600000800000300002b780000000000000000000000000000000000000000",
    ),
];

/// Labels with their codes, in option order; the first code is the one writes use.
type Table = &'static [(&'static str, &'static [u8])];

const STATE_POWEROFF: u8 = 0;

// `styler.state` is not positional: the running codes start at 50, and LG collapses several
// phases into one label.
const STATE: Table = &[
    ("Power off", &[0]),
    ("Standby", &[1]),
    ("Styling", &[2]),
    ("Pause", &[3]),
    ("Course complete", &[4, 58, 59]),
    ("Check appliance", &[5]),
    ("Smart diagnosing", &[6]),
    ("Storing", &[7]),
    ("Reserved", &[8]),
    ("Power-save running", &[9]),
    ("Steam preparing", &[50]),
    ("Refreshing", &[51, 52, 53]),
    ("Drying", &[54, 55, 56]),
    ("Sterilizing", &[57]),
];

// LG's own Error table skips values; 0 and 23 both mean nothing is wrong.
const ERROR: Table = &[
    ("Normal", &[0, 23]),
    ("TE1", &[1]),
    ("TE2", &[2]),
    ("TE3", &[3]),
    ("TE4", &[4]),
    ("TE5", &[5]),
    ("E1", &[6]),
    ("E2", &[7]),
    ("Water refill", &[8]),
    ("E4", &[9]),
    ("LE2", &[10]),
    ("AE", &[11]),
    ("LE", &[18]),
    ("Water drain", &[25]),
    ("Door open", &[26]),
    ("Check door closed", &[31]),
    ("E6", &[32]),
    ("PS", &[33]),
    ("No filter", &[34]),
];
const ERROR_CLEAR: [u8; 2] = [0, 23];

// Course and smart-course bytes are LG's course ids, not positions.
const COURSE: Table = &[
    ("None", &[0]),
    ("Styling Standard", &[1]),
    ("Styling Quick", &[3]),
    ("Styling Intensive", &[5]),
    ("Wool/Knit", &[6]),
    ("Suit/Coat", &[7]),
    ("Sterilize Standard", &[11]),
    ("Auto Dry", &[15]),
    ("Timed Dry 30", &[17]),
    ("Timed Dry 60", &[18]),
    ("Timed Dry 90", &[19]),
    ("Timed Dry 120", &[20]),
    ("Room Dehumidify 120", &[23]),
    ("Room Dehumidify 240", &[24]),
    ("Padding Care", &[28]),
    ("Fine dust", &[30]),
    ("Virus", &[31]),
    ("Jeans Care", &[32]),
    ("Fur/Leather Care", &[33]),
    ("Suit/Uniform Sterilize", &[36]),
    ("Silk", &[37]),
    ("Cashmere", &[38]),
];

const SMART_COURSE: Table = &[
    ("None", &[0]),
    ("Panel course", &[1]),
    ("Suit/Uniform Sterilize", &[61]),
    ("Scarf Care", &[62]),
    ("Pants Care", &[66]),
    ("Quiet Care", &[67]),
    ("Coat warm", &[68]),
    ("Static removal", &[69]),
    ("Old-clothes Care", &[71]),
    ("Blanket warm", &[73]),
    ("Dress-shirt Dry", &[75]),
    ("Snow/Rain Dry", &[76]),
    ("Fur/Leather Care", &[78]),
    ("Jeans Care", &[93]),
    ("Baby-clothes Sterilize", &[94]),
    ("Doll Sterilize", &[95]),
    ("Wool/Knit Dry", &[96]),
    ("Rainy-season Laundry Dry", &[97]),
    ("Uniform Care", &[98]),
    ("Padding Care", &[99]),
    ("Thin Padding Dry", &[100]),
    ("Thick Padding Dry", &[101]),
    ("Yoga/Pilates Care", &[112]),
    ("Yoga/Pilates Dry", &[113]),
    ("Swimwear Dry", &[114]),
    ("Functional", &[115]),
    ("Bedding Sterilize", &[119]),
];

const BUZZER: Table = &[
    ("Mute", &[0]),
    ("Small", &[1]),
    ("Normal", &[2]),
    ("Large", &[3]),
    ("Very large", &[4]),
];

const END_MELODY: Table = &[
    ("Default sound", &[0]),
    ("Vivaldi Winter", &[1]),
    ("Bach Minuet", &[2]),
    ("Home Sweet Home", &[3]),
    ("Breeze", &[4]),
    ("Old MacDonald", &[5]),
    ("Verdi Brindisi", &[6]),
    ("Bubble", &[7]),
    ("Beethoven Symphony No.5 5", &[8]),
    ("Arirang", &[9]),
    ("Pachelbel Canon", &[10]),
    ("Beethoven Choral", &[11]),
];

// LG's own schema for both night-care times: whole and half hours only.
const HALF_HOUR_PATTERN: &str = "^([01]\\d|2[0-3]):(00|30)$";

fn label(table: Table, code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(_, codes)| codes.contains(&code))
        .map(|&(label, _)| label)
}

fn code(table: Table, label: &str) -> Option<u8> {
    table
        .iter()
        .find(|&&(l, _)| l == label)
        .map(|(_, codes)| codes[0])
}

fn options(table: Table) -> Vec<&'static str> {
    table.iter().map(|&(label, _)| label).collect()
}

fn course_params(id: u8) -> Option<&'static str> {
    COURSE_PARAMS
        .iter()
        .find(|&&(course, _)| course == id)
        .map(|&(_, params)| params)
}

/// Every course with a start block, under the name the Course sensor gives it.
fn course_options() -> Vec<&'static str> {
    COURSE_PARAMS
        .iter()
        .filter_map(|&(id, _)| label(COURSE, id))
        .collect()
}

fn hhmm(h: u8, m: u8) -> String {
    format!("{h:02}:{m:02}")
}

fn duration_minutes(hours: u8, minutes: u8) -> PropertyValue {
    (i64::from(hours) * 60 + i64::from(minutes)).into()
}

/// [`HALF_HOUR_PATTERN`] as a parser: `HH:MM` with HH 00..23 and MM 00 or 30.
fn parse_half_hour(value: &str) -> Option<(u8, u8)> {
    let &[h1, h2, b':', m1, m2] = value.as_bytes() else {
        return None;
    };
    let hour_ok = match h1 {
        b'0' | b'1' => h2.is_ascii_digit(),
        b'2' => (b'0'..=b'3').contains(&h2),
        _ => false,
    };
    let minute = match [m1, m2] {
        [b'0', b'0'] => 0,
        [b'3', b'0'] => 30,
        _ => return None,
    };
    hour_ok.then(|| ((h1 - b'0') * 10 + (h2 - b'0'), minute))
}

fn with_extra(mut component: Value, extra: Value) -> Value {
    if let (Value::Object(component), Value::Object(extra)) = (&mut component, extra) {
        component.extend(extra);
    }
    component
}

fn sensor(id: &str, name: &str, extra: Value) -> Value {
    with_extra(
        json!({
            "platform": "sensor",
            "unique_id": format!("$deviceid-{id}"),
            "state_topic": format!("$this/{id}"),
            "name": name,
        }),
        extra,
    )
}

fn flag(id: &str, name: &str, extra: Value) -> Value {
    with_extra(
        json!({
            "platform": "binary_sensor",
            "unique_id": format!("$deviceid-{id}"),
            "state_topic": format!("$this/{id}"),
            "name": name,
            "payload_on": "ON",
            "payload_off": "OFF",
        }),
        extra,
    )
}

fn minutes(id: &str, name: &str, icon: &str) -> Value {
    sensor(
        id,
        name,
        json!({
            "device_class": "duration",
            "unit_of_measurement": "min",
            "state_class": "measurement",
            "icon": icon,
        }),
    )
}

/// A reading from one of the tables; labels shared by several codes appear once, as HA requires.
fn reading(id: &str, name: &str, table: Table, extra: Value) -> Value {
    sensor(
        id,
        name,
        with_extra(
            json!({"device_class": "enum", "options": options(table)}),
            extra,
        ),
    )
}

fn toggle(id: &str, name: &str, extra: Value) -> Value {
    with_extra(
        json!({
            "platform": "switch",
            "unique_id": format!("$deviceid-{id}"),
            "state_topic": format!("$this/{id}"),
            "command_topic": format!("$this/{id}/set"),
            "name": name,
            "payload_on": "ON",
            "payload_off": "OFF",
        }),
        extra,
    )
}

fn choice(id: &str, name: &str, options: Vec<&str>, extra: Value) -> Value {
    with_extra(
        json!({
            "platform": "select",
            "unique_id": format!("$deviceid-{id}"),
            "state_topic": format!("$this/{id}"),
            "command_topic": format!("$this/{id}/set"),
            "name": name,
            "options": options,
        }),
        extra,
    )
}

fn press(id: &str, name: &str, icon: &str) -> Value {
    json!({
        "platform": "button",
        "unique_id": format!("$deviceid-{id}"),
        "command_topic": format!("$this/{id}/set"),
        "payload_press": "",
        "name": name,
        "icon": icon,
    })
}

fn half_hour_clock(id: &str, name: &str) -> Value {
    json!({
        "platform": "text",
        "unique_id": format!("$deviceid-{id}"),
        "state_topic": format!("$this/{id}"),
        "command_topic": format!("$this/{id}/set"),
        "name": name,
        "icon": "mdi:clock-time-four-outline",
        "pattern": HALF_HOUR_PATTERN,
        "min": 5,
        "max": 5,
        "entity_category": "config",
    })
}

fn components() -> HashMap<String, Value> {
    let config = |icon: &str| json!({"icon": icon, "entity_category": "config"});
    let diagnostic = |icon: &str| json!({"icon": icon, "entity_category": "diagnostic"});
    [
        (
            "power",
            toggle("power", "Power", json!({"icon": "mdi:power"})),
        ),
        (
            "status",
            reading("status", "Status", STATE, json!({"icon": "mdi:hanger"})),
        ),
        (
            "course",
            reading(
                "course",
                "Course",
                COURSE,
                json!({"icon": "mdi:playlist-check"}),
            ),
        ),
        (
            "course_select",
            choice(
                "course_select",
                "Course select",
                course_options(),
                json!({"icon": "mdi:playlist-edit"}),
            ),
        ),
        (
            "start_course",
            press("start_course", "Start course", "mdi:play-circle-outline"),
        ),
        (
            "pause_course",
            press("pause_course", "Pause", "mdi:pause-circle-outline"),
        ),
        (
            "resume_course",
            press("resume_course", "Resume", "mdi:play-pause"),
        ),
        (
            "smart_course",
            reading(
                "smart_course",
                "Smart course",
                SMART_COURSE,
                json!({"icon": "mdi:playlist-star"}),
            ),
        ),
        (
            "remaining_time",
            minutes("remaining_time", "Remaining time", "mdi:timer-sand"),
        ),
        (
            "initial_time",
            minutes("initial_time", "Initial time", "mdi:timer-outline"),
        ),
        // A delay before the course starts (LG declares 3..19 hours), not a wall-clock time.
        (
            "reserve_time",
            minutes("reserve_time", "Reserve time", "mdi:calendar-clock"),
        ),
        // The appliance refuses to start a course while an error stands.
        (
            "error",
            flag(
                "error",
                "Error",
                json!({
                    "device_class": "problem",
                    "icon": "mdi:check-circle",
                    "entity_category": "diagnostic",
                }),
            ),
        ),
        (
            "error_message",
            reading(
                "error_message",
                "Error message",
                ERROR,
                diagnostic("mdi:alert-circle-outline"),
            ),
        ),
        (
            "child_lock",
            flag("child_lock", "Child lock", json!({"icon": "mdi:lock"})),
        ),
        (
            "night_dry",
            flag("night_dry", "Store", json!({"icon": "mdi:weather-night"})),
        ),
        (
            "remote_start",
            flag(
                "remote_start",
                "Remote start",
                json!({"icon": "mdi:cellphone-check"}),
            ),
        ),
        (
            "remote_maintain",
            toggle(
                "remote_maintain",
                "Remote control allowed",
                json!({"icon": "mdi:cellphone-wireless"}),
            ),
        ),
        (
            "buzzer",
            choice(
                "buzzer",
                "Alert sound",
                options(BUZZER),
                config("mdi:volume-high"),
            ),
        ),
        (
            "end_melody",
            choice(
                "end_melody",
                "End melody",
                options(END_MELODY),
                config("mdi:music"),
            ),
        ),
        (
            "keep_last_course",
            toggle("keep_last_course", "Keep last course", config("mdi:repeat")),
        ),
        (
            "smart_care_finedust",
            toggle(
                "smart_care_finedust",
                "SmartCare Fine-dust",
                config("mdi:weather-dust"),
            ),
        ),
        (
            "smart_care_humidity",
            toggle(
                "smart_care_humidity",
                "SmartCare Humidity",
                config("mdi:water-percent"),
            ),
        ),
        (
            "smart_care_nightcare",
            toggle(
                "smart_care_nightcare",
                "SmartCare Night",
                config("mdi:sleep"),
            ),
        ),
        (
            "night_care_start",
            half_hour_clock("night_care_start", "Night-care start time"),
        ),
        (
            "night_care_end",
            half_hour_clock("night_care_end", "Night-care end time"),
        ),
        (
            "download_course",
            reading(
                "download_course",
                "Downloaded course",
                SMART_COURSE,
                diagnostic("mdi:download"),
            ),
        ),
        // Resets when the maintenance course runs, so no total_increasing state_class.
        (
            "tcl_count",
            sensor(
                "tcl_count",
                "Sterilize-tank clean alert count",
                diagnostic("mdi:counter"),
            ),
        ),
        (
            "previous_status",
            reading(
                "previous_status",
                "Previous state",
                STATE,
                diagnostic("mdi:history"),
            ),
        ),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

struct Inner {
    /// What Start course runs: LG's setCurrentCycle does not convert for this model, so the
    /// appliance has no selected-but-not-started course to read back.
    selected_course: u8,
    /// Last values the appliance itself reported; `None` until the first state frame.
    powered_on: Option<bool>,
    night_care_enabled: Option<bool>,
    has_problem: Option<bool>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    inner: Mutex<Inner>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut config = default_config(&meta, Some(json!({"name": "LG Styler"})));
        config.components = components();
        core.set_config(config);

        let this = Arc::new(Self {
            core,
            inner: Mutex::new(Inner {
                selected_course: COURSE_PARAMS[0].0,
                powered_on: None,
                night_care_enabled: None,
                has_problem: None,
            }),
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn log(&self, message: std::fmt::Arguments) {
        tracing::debug!("ST_B_E4H01Y_APL {}: {message}", self.core.id);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&STATE_TAG) || buf.len() < HEADER_LEN + RECORD_LEN + TRAILER_LEN {
            return;
        }
        let payload = buf.len() - HEADER_LEN - TRAILER_LEN;
        if payload % RECORD_LEN != 0 {
            return;
        }
        // Disagreeing with the length means a different message that happens to divide evenly.
        let declared = u16::from_be_bytes([buf[RECORD_BYTES_OFF], buf[RECORD_BYTES_OFF + 1]]);
        if usize::from(declared) != payload {
            return;
        }
        // The trailing record is the current state; a leading one, when present, is the previous.
        let start = buf.len() - TRAILER_LEN - RECORD_LEN;
        self.publish_record(&buf[start..start + RECORD_LEN]);
    }

    fn publish_label(&self, prop: &str, table: Table, code: u8) {
        self.core
            .publish_property(prop, label(table, code).unwrap_or("None").into());
    }

    fn publish_record(&self, record: &[u8]) {
        let state = record[OFF_STATE];
        let error = record[OFF_ERROR];
        let course = record[OFF_COURSE];
        let flags_a = record[OFF_FLAGS_A];
        let flags_b = record[OFF_FLAGS_B];
        let flags_c = record[OFF_FLAGS_C];
        let powered_on = state != STATE_POWEROFF;
        let has_problem = !ERROR_CLEAR.contains(&error);
        {
            let mut inner = self.inner.lock();
            inner.powered_on = Some(powered_on);
            inner.night_care_enabled = Some(flags_c & F_SC_NIGHTCARE != 0);
            inner.has_problem = Some(has_problem);
        }

        self.publish_label("status", STATE, state);
        self.core.publish_on_off("power", powered_on);
        self.publish_label("previous_status", STATE, record[OFF_PRE_STATE]);
        self.publish_label("error_message", ERROR, error);
        self.core.publish_on_off("error", has_problem);
        self.publish_label("course", COURSE, course);
        self.publish_label("smart_course", SMART_COURSE, record[OFF_SMART_COURSE]);

        // Course select follows the course being run. Idle reports course 0, which is not an
        // option, so the last choice stands.
        if course_params(course).is_some() {
            self.inner.lock().selected_course = course;
            self.publish_label("course_select", COURSE, course);
        }

        self.core.publish_property(
            "remaining_time",
            duration_minutes(record[OFF_REMAIN_H], record[OFF_REMAIN_M]),
        );
        self.core.publish_property(
            "initial_time",
            duration_minutes(record[OFF_INITIAL_H], record[OFF_INITIAL_M]),
        );
        self.core.publish_property(
            "reserve_time",
            duration_minutes(record[OFF_RESERVE_H], record[OFF_RESERVE_M]),
        );

        self.core.publish_flag("child_lock", flags_a, F_CHILD_LOCK);
        self.core.publish_flag("night_dry", flags_a, F_NIGHT_DRY);
        self.core
            .publish_flag("remote_start", flags_a, F_REMOTE_START);
        self.core
            .publish_flag("remote_maintain", flags_b, F_REMOTE_MAINTAIN);

        // A select's state must be one of its options, so an unlisted code leaves the last one.
        if let Some(buzzer) = label(BUZZER, record[OFF_BUZZER]) {
            self.core.publish_property("buzzer", buzzer.into());
        }
        if let Some(melody) = label(END_MELODY, record[OFF_END_MELODY]) {
            self.core.publish_property("end_melody", melody.into());
        }

        self.core
            .publish_flag("keep_last_course", flags_c, F_IS_LAST_COURSE);
        self.core
            .publish_flag("smart_care_finedust", flags_c, F_SC_FINEDUST);
        self.core
            .publish_flag("smart_care_humidity", flags_c, F_SC_HUMIDITY);
        self.core
            .publish_flag("smart_care_nightcare", flags_c, F_SC_NIGHTCARE);

        self.core.publish_property(
            "night_care_start",
            hhmm(record[OFF_NC_START_H], record[OFF_NC_START_M]).into(),
        );
        self.core.publish_property(
            "night_care_end",
            hhmm(record[OFF_NC_END_H], record[OFF_NC_END_M]).into(),
        );

        // Only slot 1 is meaningful: this unit holds at most one downloaded course.
        self.publish_label(
            "download_course",
            SMART_COURSE,
            record[OFF_DOWNLOAD_COURSE1],
        );

        self.core
            .publish_property("tcl_count", i64::from(record[OFF_TCL]).into());
    }

    fn set_control(&self, typ: u8, values: &[u8]) {
        let header = [SET_STATE[0], SET_STATE[1], typ, values.len() as u8];
        self.core.send(&[header.as_slice(), values].concat());
    }

    /// The length still counts only the value bytes: 1 for a flag, 2 for a clock time.
    fn set_up_center(&self, sub: u8, values: &[u8]) {
        let header = [
            SET_STATE[0],
            SET_STATE[1],
            CTRL_UPCENTER,
            values.len() as u8,
            sub,
        ];
        self.core.send(&[header.as_slice(), values].concat());
    }

    fn run_course(&self, id: u8) {
        let Some(params) = course_params(id) else {
            self.log(format_args!("Course cannot start {id}"));
            return;
        };
        self.core.send(
            &[
                RUN_COURSE.as_slice(),
                &[id],
                &COURSE_HEAD,
                &hex_decode(params),
            ]
            .concat(),
        );
    }

    fn resume_course(&self, id: u8) {
        let mut block = [0u8; RESUME_BLOCK_LEN];
        block[0] = id;
        self.core.send(&[RUN_COURSE.as_slice(), &block].concat());
    }

    /// Publishes a write straight away instead of waiting for the appliance, which can go a
    /// minute without a state frame; the next real frame still has the last word.
    fn echo(&self, prop: &str, value: &str) {
        {
            let inner = self.inner.lock();
            if inner.powered_on != Some(true) {
                return;
            }
            if prop == "course" && inner.has_problem != Some(false) {
                return;
            }
            if inner.night_care_enabled == Some(false)
                && (prop == "night_care_start" || prop == "night_care_end")
            {
                return;
            }
        }
        self.core.publish_property(prop, value.into());
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let on_off = u8::from(mqtt_value == "ON");
        match prop {
            "power" => {
                self.set_control(CTRL_POWER, &[on_off]);
                // Switched off, the appliance stops sending any frame this driver can read, so
                // the switch would never see OFF; the next real frame corrects this.
                if mqtt_value != "ON" {
                    self.inner.lock().powered_on = Some(false);
                }
                self.core.publish_on_off("power", mqtt_value == "ON");
            }
            "remote_maintain" => {
                self.set_control(CTRL_REMOTE_MAINTAIN, &[on_off]);
                self.echo(prop, mqtt_value);
            }
            "keep_last_course" => {
                self.set_up_center(UP_IS_LAST_COURSE, &[on_off]);
                self.echo(prop, mqtt_value);
            }
            "course_select" => {
                let Some(id) = code(COURSE, mqtt_value).filter(|&id| course_params(id).is_some())
                else {
                    self.log(format_args!("Unknown course {mqtt_value}"));
                    return;
                };
                self.inner.lock().selected_course = id;
                // A choice, not a command: nothing goes to the appliance until Start course.
                self.core.publish_property(prop, mqtt_value.into());
            }
            "start_course" => {
                let id = self.inner.lock().selected_course;
                self.run_course(id);
                self.echo("course", label(COURSE, id).unwrap_or("None"));
            }
            "pause_course" => self.set_control(CTRL_PAUSE, &[0]),
            "resume_course" => {
                let id = self.inner.lock().selected_course;
                self.resume_course(id);
            }
            "smart_care_finedust" => {
                self.set_up_center(UP_SC_FINEDUST, &[on_off]);
                self.echo(prop, mqtt_value);
            }
            "smart_care_humidity" => {
                self.set_up_center(UP_SC_HUMIDITY, &[on_off]);
                self.echo(prop, mqtt_value);
            }
            "smart_care_nightcare" => {
                self.set_up_center(UP_SC_NIGHTCARE, &[on_off]);
                self.echo(prop, mqtt_value);
            }
            "buzzer" => {
                let Some(level) = code(BUZZER, mqtt_value) else {
                    self.log(format_args!("Unknown alert sound {mqtt_value}"));
                    return;
                };
                self.set_up_center(UP_BUZZER, &[level]);
                self.echo(prop, mqtt_value);
            }
            "end_melody" => {
                let Some(melody) = code(END_MELODY, mqtt_value) else {
                    self.log(format_args!("Unknown end melody {mqtt_value}"));
                    return;
                };
                self.set_up_center(UP_END_MELODY, &[melody]);
                self.echo(prop, mqtt_value);
            }
            "night_care_start" | "night_care_end" => {
                // The text entity carries the pattern too, but another producer could bypass it.
                let value = mqtt_value.trim();
                let Some((hour, minute)) = parse_half_hour(value) else {
                    self.log(format_args!("Night-care time format error {mqtt_value}"));
                    return;
                };
                let sub = if prop == "night_care_start" {
                    UP_NC_START
                } else {
                    UP_NC_END
                };
                self.set_up_center(sub, &[hour, minute]);
                self.echo(prop, value);
            }
            _ => self.log(format_args!("Item does not support writing {prop}")),
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
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};
    use std::collections::HashSet;

    const STATE: &str = "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e420000040300000000000000257205bb";
    const STATE2: &str = "aaff310a003a005504000100eb002800000000010001000001000000000000200000000000000000014e420000040300000000000000251351bb";
    const READY: &str = "aaff310a003a0053ea000100eb002800000100010001000001000000000000200000000000000000014e42000004030000000000000025969ebb";
    const COURSE_STANDARD: &str = "aaff310a003a0053ea000100eb002800000000010001010001000000000000200000000000000000014e42000004030000000000000025ffd9bb";
    const ERROR_TE1: &str = "aaff310a003a0053ea000100eb002800000000010001000101000000000000200000000000000000014e420000040300000000000000252b43bb";
    const PRESTATE_RUNNING: &str = "aaff310a003a0053ea000100eb002800000000010001000002000000000000200000000000000000014e42000004030000000000000025f05fbb";
    const FLAGS_A_ALL: &str = "aaff310a003a0053ea000100eb0028000000000100010000010000000000000f0000000000000000014e42000004030000000000000025df93bb";
    const REMOTE_MAINTAIN: &str = "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e42000004040000000000000025c3aebb";
    const FLAGS_C_ALL: &str = "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e420000040300003f0000000025988abb";
    const BUZZER_LOW: &str = "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e42000001030000000000000025b075bb";
    const MELODY_1: &str = "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e4200000403010000000000002535d6bb";
    const SMART_COURSE_1: &str = "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000010000014e420000040300000000000000258234bb";
    const TCL_1: &str = "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e4200000403000000000000000116e3bb";
    const RUNNING_READY: &str = "aaff310a0062005cd0000100ec005000000000010001000001000000000000200000000000000000014e4200000403000000000000002500000100010001000000000000000000200000000000000000014e42000004030000000000000025ee3ebb";
    const RUNNING_COURSE: &str = "aaff310a0062005ce5000100ec0050000001011e011e0f0000000000000000280000000000000000014e4200000403000000000000002500000100230023010000000000000000280000000000000000014e42000004030000000000000025042abb";
    const COURSE_LIST: &str = "aaff310a0070005ccc00010a86005e0d053230332d3101073230332d312d3101073230332d312d3201073230332d312d3301073230332d312d34010453542d34010453542d35010453542d36010453542d37010453542d38010453542d39010553542d3130000553542d313201ed79bb";
    const NOT_STATE_35: &str =
        "aaff310a00230053ed000101030011100b0306100000008a00000000000000059317bb";
    const HEARTBEAT: &str = "aa0731c302f2bb";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("ST_B_E4H01Y_APL", Device::new)
    }

    fn emit(thinq: &MockThinq2Device, hex: &str) {
        thinq.emit_data(&hex_decode(hex));
    }

    fn with_byte(frame: &str, index: usize, value: u8) -> Vec<u8> {
        let mut packet = hex_decode(frame);
        packet[index] = value;
        packet
    }

    fn hex_lc(packet: &[u8]) -> String {
        hex_encode(packet).to_ascii_lowercase()
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    fn properties(ha: &MockHaConnection) -> HashMap<String, PropertyValue> {
        ha.device(DEVICE_ID).unwrap().properties
    }

    // Writable exactly where a frame the appliance accepts was established.
    #[test]
    fn exactly_the_fields_with_a_captured_command_frame_are_writable() {
        let (ha, _, _) = make();
        let writable: HashSet<&str> = [
            "power",
            "buzzer",
            "end_melody",
            "keep_last_course",
            "smart_care_finedust",
            "smart_care_humidity",
            "smart_care_nightcare",
            "night_care_start",
            "night_care_end",
            "remote_maintain",
            "course_select",
            "start_course",
            "pause_course",
            "resume_course",
        ]
        .into_iter()
        .collect();
        for (name, comp) in &components(&ha) {
            let platform = comp["platform"].as_str().unwrap();
            if writable.contains(name.as_str()) {
                let want = format!("$this/{name}/set");
                assert_eq!(
                    comp.get("command_topic").and_then(Value::as_str),
                    Some(want.as_str()),
                    "{name} is writable"
                );
            } else {
                assert_ne!(
                    platform, "button",
                    "{name} is a button with no command topic"
                );
                assert!(
                    comp.get("command_topic").is_none(),
                    "{name} stays read-only"
                );
                assert!(
                    platform == "sensor" || platform == "binary_sensor",
                    "{name} is a sensor platform, got {platform}"
                );
            }
        }
    }

    // The frames LG's own capability API emitted for each command, every enum option included.
    #[test]
    fn each_write_reproduces_the_frame_lg_itself_sent_for_that_command() {
        let cases = [
            ("power", "ON", "aa09f0240101019fbb"),
            ("power", "OFF", "aa09f0240101009cbb"),
            ("end_melody", "Default sound", "aa0af0241301010088bb"),
            ("end_melody", "Vivaldi Winter", "aa0af024130101018bbb"),
            ("end_melody", "Bach Minuet", "aa0af024130101028abb"),
            ("end_melody", "Home Sweet Home", "aa0af02413010103b5bb"),
            ("end_melody", "Breeze", "aa0af02413010104b4bb"),
            ("end_melody", "Old MacDonald", "aa0af02413010105b7bb"),
            ("end_melody", "Verdi Brindisi", "aa0af02413010106b6bb"),
            ("end_melody", "Bubble", "aa0af02413010107b1bb"),
            (
                "end_melody",
                "Beethoven Symphony No.5 5",
                "aa0af02413010108b0bb",
            ),
            ("end_melody", "Arirang", "aa0af02413010109b3bb"),
            ("end_melody", "Pachelbel Canon", "aa0af0241301010ab2bb"),
            ("end_melody", "Beethoven Choral", "aa0af0241301010bbdbb"),
            ("buzzer", "Mute", "aa0af024130103008abb"),
            ("buzzer", "Small", "aa0af02413010301b5bb"),
            ("buzzer", "Normal", "aa0af02413010302b4bb"),
            ("buzzer", "Large", "aa0af02413010303b7bb"),
            ("buzzer", "Very large", "aa0af02413010304b6bb"),
            ("smart_care_finedust", "ON", "aa0af02413010401b4bb"),
            ("smart_care_finedust", "OFF", "aa0af02413010400b5bb"),
            ("smart_care_humidity", "ON", "aa0af02413010501b7bb"),
            ("smart_care_humidity", "OFF", "aa0af02413010500b4bb"),
            ("smart_care_nightcare", "ON", "aa0af02413010601b6bb"),
            ("smart_care_nightcare", "OFF", "aa0af02413010600b7bb"),
            ("keep_last_course", "ON", "aa0af02413010901b3bb"),
            ("keep_last_course", "OFF", "aa0af02413010900b0bb"),
        ];
        for (name, value, want) in cases {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property(name, value);
            assert_eq!(thinq.outbox().len(), 1, "{name}={value} sent one frame");
            assert_eq!(hex_lc(&thinq.outbox()[0]), want, "{name}={value}");
        }
    }

    // LG's converter only emits the OFF frame; the ON frame was acknowledged and read back.
    #[test]
    fn remote_control_allowed_writes_the_frame_the_appliance_acknowledged() {
        for (value, want) in [("ON", "aa09f0241001018cbb"), ("OFF", "aa09f0241001008dbb")] {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property("remote_maintain", value);
            assert_eq!(hex_lc(&thinq.outbox()[0]), want, "remote_maintain={value}");
        }
    }

    #[test]
    fn start_course_reproduces_the_frame_lg_itself_sent_for_both_captured_courses() {
        let cases = [
            (
                "Styling Standard",
                "aa34f02601010004000000000002645a00000005005a05b45a01b4001ab40000000000000000000000000000000000000000fabb",
            ),
            (
                "Styling Quick",
                "aa34f02603010004000000000082645a00000003005a00000001b4000eb4000000000000000000000000000000000000000045bb",
            ),
        ];
        for (course, want) in cases {
            let (_, thinq, dev) = make();
            dev.set_property("course_select", course);
            thinq.reset_recorder();
            dev.set_property("start_course", "");
            assert_eq!(thinq.outbox().len(), 1, "{course} sent one frame");
            assert_eq!(hex_lc(&thinq.outbox()[0]), want, "{course}");
        }
    }

    #[test]
    fn pause_and_resume_reproduce_their_captured_frames() {
        let (_, thinq, dev) = make();
        dev.set_property("course_select", "Styling Standard");
        thinq.reset_recorder();
        dev.set_property("pause_course", "");
        assert_eq!(hex_lc(&thinq.outbox()[0]), "aa09f02404010099bb");
        thinq.reset_recorder();
        dev.set_property("resume_course", "");
        assert_eq!(
            hex_lc(&thinq.outbox()[0]),
            "aa33f026010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a1bb"
        );
    }

    #[test]
    fn a_write_shows_up_straight_away_instead_of_waiting_for_the_appliance() {
        let (ha, thinq, dev) = make();
        let mut enabled = hex_decode(READY);
        enabled[49] |= 0x08; // smartCareNightCare ON
        thinq.emit_data(&enabled);
        dev.set_property("buzzer", "Small");
        dev.set_property("smart_care_humidity", "ON");
        dev.set_property("night_care_start", "22:30");
        dev.set_property("course_select", "Styling Standard");
        dev.set_property("start_course", "");
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("Small"));
        assert_eq!(prop(&ha, "smart_care_humidity").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "night_care_start").as_deref(), Some("22:30"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Styling Standard"));
        // ...and the appliance still has the last word.
        thinq.emit_data(&enabled);
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("Very large"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
    }

    #[test]
    fn a_setting_rejected_while_powered_off_is_not_optimistically_published() {
        let (ha, thinq, dev) = make();
        emit(&thinq, STATE);
        thinq.reset_recorder();

        dev.set_property("buzzer", "Small");
        dev.set_property("smart_care_humidity", "ON");
        dev.set_property("course_select", "Styling Standard");
        dev.set_property("start_course", "");

        assert_eq!(
            thinq.outbox().len(),
            3,
            "commands are still sent so the appliance has the final say"
        );
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("Very large"));
        assert_eq!(prop(&ha, "smart_care_humidity").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Styling Standard"),
            "the local-only course choice still updates"
        );
    }

    #[test]
    fn night_care_times_rejected_while_night_care_is_disabled_are_not_optimistically_published() {
        let (ha, thinq, dev) = make();
        emit(&thinq, READY);
        thinq.reset_recorder();

        dev.set_property("night_care_start", "22:30");
        dev.set_property("night_care_end", "06:00");

        assert_eq!(
            thinq.outbox().len(),
            2,
            "valid command frames are still sent"
        );
        assert_eq!(prop(&ha, "night_care_start").as_deref(), Some("00:00"));
        assert_eq!(prop(&ha, "night_care_end").as_deref(), Some("00:00"));
    }

    #[test]
    fn no_setting_is_optimistically_published_before_the_first_authoritative_state() {
        let (ha, thinq, dev) = make();
        dev.set_property("buzzer", "Small");
        dev.set_property("course_select", "Styling Standard");
        dev.set_property("start_course", "");

        assert_eq!(
            thinq.outbox().len(),
            2,
            "valid frames still reach the appliance"
        );
        assert!(prop(&ha, "buzzer").is_none());
        assert!(prop(&ha, "course").is_none());
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Styling Standard")
        );
    }

    #[test]
    fn a_course_rejected_by_a_standing_error_is_not_optimistically_published() {
        let (ha, thinq, dev) = make();
        // State INITIAL: powered on while error TE1 is standing.
        thinq.emit_data(&with_byte(ERROR_TE1, 17, 1));
        thinq.reset_recorder();

        dev.set_property("course_select", "Styling Standard");
        dev.set_property("start_course", "");

        assert_eq!(
            thinq.outbox().len(),
            1,
            "the appliance still receives the valid course frame"
        );
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
    }

    #[test]
    fn course_select_chooses_without_commanding_the_appliance() {
        let (ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("course_select", "Sterilize Standard");
        assert!(thinq.outbox().is_empty(), "selecting sends nothing");
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Sterilize Standard")
        );
        // ...and Start course then runs the one that was chosen.
        dev.set_property("start_course", "");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(thinq.outbox()[0][4], 11, "course id 11 leads the block");
    }

    #[test]
    fn a_course_the_appliance_is_running_takes_over_the_selection() {
        let (ha, thinq, _) = make();
        emit(&thinq, COURSE_STANDARD);
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Styling Standard")
        );
        // Idle it reports NONE, which is no course at all: the last choice has to stand.
        emit(&thinq, STATE);
        assert_eq!(
            prop(&ha, "course_select").as_deref(),
            Some("Styling Standard")
        );
    }

    #[test]
    fn an_unknown_course_is_refused_rather_than_started() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("course_select", "InvalidCourse");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn the_night_care_window_writes_hour_and_minute_in_one_frame() {
        let cases = [
            ("night_care_start", "23:30", "aa0bf024130207171e4fbb"),
            ("night_care_start", "00:00", "aa0bf0241302070000b0bb"),
            ("night_care_end", "06:00", "aa0bf0241302080600b9bb"),
            ("night_care_end", "00:00", "aa0bf0241302080000b3bb"),
        ];
        for (name, value, want) in cases {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property(name, value);
            assert_eq!(hex_lc(&thinq.outbox()[0]), want, "{name}={value}");
        }
    }

    #[test]
    fn a_time_the_appliance_would_refuse_is_not_written() {
        for bad in ["23:45", "24:00", "9:30", "abc", "", "07:1"] {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property("night_care_start", bad);
            assert!(thinq.outbox().is_empty(), "refused {bad:?}");
        }
    }

    // A signed number parser would read these as 01:30 and 12:00; LG's pattern has no sign.
    #[test]
    fn a_signed_night_care_time_is_not_written() {
        for bad in ["+1:30", "12:+0"] {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property("night_care_end", bad);
            assert!(thinq.outbox().is_empty(), "refused {bad:?}");
        }
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_guessed() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("buzzer", "InvalidVolume");
        // An LG melody name, but not one this model offers.
        dev.set_property("end_melody", "Silent Night");
        // Read-only.
        dev.set_property("course", "Styling Standard");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn the_power_switch_reports_off_as_soon_as_it_is_switched_off() {
        let (ha, thinq, dev) = make();
        emit(&thinq, READY);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        dev.set_property("power", "OFF");
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        // ...and a real frame still has the last word.
        emit(&thinq, READY);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
    }

    #[test]
    fn every_user_facing_name_and_enum_option_is_english() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        let has_letter = |text: &str| text.chars().any(|c| c.is_ascii_alphabetic());
        for (key, comp) in &components(&ha) {
            if let Some(name) = comp.get("name").and_then(Value::as_str) {
                assert!(has_letter(name), "{key} name is English: {name}");
            }
        }
        for key in [
            "status",
            "previous_status",
            "course",
            "smart_course",
            "buzzer",
            "end_melody",
            "error_message",
        ] {
            let value = prop(&ha, key).unwrap_or_default();
            assert!(has_letter(&value), "{key} value is English: {value}");
        }
    }

    #[test]
    fn real_captured_frame_decodes_to_what_lg_read_from_the_same_frame() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        let expected = [
            ("status", "Power off"),
            ("previous_status", "Standby"),
            ("course", "None"),
            ("smart_course", "None"),
            ("error_message", "Normal"),
            ("error", "OFF"),
            ("remaining_time", "1"),
            ("initial_time", "1"),
            ("reserve_time", "0"),
            ("child_lock", "OFF"),
            ("night_dry", "OFF"),
            ("remote_start", "OFF"),
            ("remote_maintain", "OFF"),
            ("buzzer", "Very large"),
            ("end_melody", "Default sound"),
            ("smart_care_finedust", "OFF"),
            ("smart_care_humidity", "OFF"),
            ("smart_care_nightcare", "OFF"),
            ("keep_last_course", "OFF"),
            ("night_care_start", "00:00"),
            ("night_care_end", "00:00"),
            ("power", "OFF"),
            ("tcl_count", "37"),
            // Slot 1 holds course id 78, which LG names FUR_LEATHER.
            ("download_course", "Fur/Leather Care"),
        ];
        for (key, want) in expected {
            assert_eq!(prop(&ha, key).as_deref(), Some(want), "{key}");
        }
    }

    #[test]
    fn a_second_real_frame_with_a_different_sequence_number_decodes_identically() {
        let (ha_a, thinq_a, _) = make();
        emit(&thinq_a, STATE);
        let (ha_b, thinq_b, _) = make();
        emit(&thinq_b, STATE2);
        assert_eq!(properties(&ha_b), properties(&ha_a));
    }

    #[test]
    fn each_probed_offset_moves_exactly_the_field_lg_said_it_moves() {
        let cases = [
            (READY, "status", "Standby"),
            (COURSE_STANDARD, "course", "Styling Standard"),
            (ERROR_TE1, "error_message", "TE1"),
            (PRESTATE_RUNNING, "previous_status", "Styling"),
            (BUZZER_LOW, "buzzer", "Small"),
            (MELODY_1, "end_melody", "Vivaldi Winter"),
            (SMART_COURSE_1, "smart_course", "Panel course"),
            (TCL_1, "tcl_count", "1"),
            (REMOTE_MAINTAIN, "remote_maintain", "ON"),
        ];
        for (frame, key, want) in cases {
            let (ha, thinq, _) = make();
            emit(&thinq, STATE);
            emit(&thinq, frame);
            assert_eq!(
                prop(&ha, key).as_deref(),
                Some(want),
                "{key} after single-byte probe"
            );
        }
    }

    #[test]
    fn the_running_state_codes_decode_not_just_the_idle_ones() {
        let cases = [
            (0u8, "Power off"),
            (3, "Pause"),
            (9, "Power-save running"),
            (50, "Steam preparing"),
            (52, "Refreshing"),
            (55, "Drying"),
            (57, "Sterilizing"),
            (58, "Course complete"),
        ];
        for (code, want) in cases {
            let (ha, thinq, _) = make();
            thinq.emit_data(&with_byte(STATE, 17, code));
            assert_eq!(prop(&ha, "status").as_deref(), Some(want), "state {code}");
        }
    }

    #[test]
    fn a_code_lg_itself_does_not_define_reads_unknown_not_a_wrong_name() {
        let (ha, thinq, _) = make();
        // NOT_DEFINE_VALUE on this model.
        thinq.emit_data(&with_byte(STATE, 17, 20));
        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
    }

    #[test]
    fn unlisted_course_and_state_codes_read_unknown() {
        let (ha, thinq, _) = make();
        let mut frame = hex_decode(STATE);
        frame[22] = 2; // course
        frame[24] = 20; // preState
        frame[37] = 2; // smartCourse
        frame[41] = 2; // currentDownloadCourse1
        thinq.emit_data(&frame);
        for key in [
            "course",
            "previous_status",
            "smart_course",
            "download_course",
        ] {
            assert_eq!(prop(&ha, key).as_deref(), Some("None"), "{key}");
        }
    }

    #[test]
    fn the_error_codes_decode_and_water_refill_is_not_off_by_one() {
        let cases = [
            (0u8, "Normal", "OFF"),
            (8, "Water refill", "ON"),
            (11, "AE", "ON"),
            (18, "LE", "ON"),
            (23, "Normal", "OFF"),
            (25, "Water drain", "ON"),
            (26, "Door open", "ON"),
            (34, "No filter", "ON"),
            (12, "None", "ON"),
        ];
        for (code, want, error) in cases {
            let (ha, thinq, _) = make();
            thinq.emit_data(&with_byte(STATE, 23, code));
            assert_eq!(
                prop(&ha, "error_message").as_deref(),
                Some(want),
                "error {code}"
            );
            assert_eq!(
                prop(&ha, "error").as_deref(),
                Some(error),
                "problem flag for error {code}"
            );
        }
    }

    #[test]
    fn byte_31_is_a_bitfield_not_an_enum() {
        let (ha, thinq, _) = make();
        emit(&thinq, FLAGS_A_ALL);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
    }

    #[test]
    fn byte_49_is_a_bitfield_carrying_six_independent_flags() {
        let (ha, thinq, _) = make();
        emit(&thinq, FLAGS_C_ALL);
        assert_eq!(prop(&ha, "smart_care_finedust").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "smart_care_humidity").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "smart_care_nightcare").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "keep_last_course").as_deref(), Some("ON"));
    }

    #[test]
    fn the_night_care_window_reads_back_as_two_clock_times() {
        let (ha, thinq, _) = make();
        let mut scheduled = hex_decode(STATE);
        scheduled[50] = 23;
        scheduled[51] = 30;
        scheduled[52] = 6;
        scheduled[53] = 0;
        thinq.emit_data(&scheduled);
        assert_eq!(prop(&ha, "night_care_start").as_deref(), Some("23:30"));
        assert_eq!(prop(&ha, "night_care_end").as_deref(), Some("06:00"));
    }

    #[test]
    fn the_35_byte_frame_and_the_heartbeat_carry_no_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        let before = properties(&ha);
        emit(&thinq, NOT_STATE_35);
        emit(&thinq, HEARTBEAT);
        assert_eq!(properties(&ha), before);
    }

    #[test]
    fn hours_and_minutes_are_folded_into_one_duration() {
        let (ha, thinq, _) = make();
        let mut running = hex_decode(STATE);
        running[18] = 1;
        running[19] = 25;
        running[20] = 2;
        running[21] = 0;
        thinq.emit_data(&running);
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("85"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("120"));
    }

    #[test]
    fn a_two_record_frame_is_decoded_and_it_is_the_last_record_that_counts() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Power off"));
        emit(&thinq, RUNNING_READY);
        assert_eq!(
            prop(&ha, "status").as_deref(),
            Some("Standby"),
            "the 98-byte frame must decode"
        );
        emit(&thinq, RUNNING_COURSE);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Styling Standard"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("35"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("35"));
        // flagsA 0x28 = the always-set 0x20 bit | 0x08 remoteStart
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn the_course_name_list_frame_is_not_mistaken_for_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        let before = properties(&ha);
        emit(&thinq, COURSE_LIST);
        assert_eq!(properties(&ha), before);
    }

    #[test]
    fn a_set_reservation_reads_as_a_delay_in_minutes() {
        let (ha, thinq, _) = make();
        let mut reserved = hex_decode(STATE);
        reserved[29] = 7;
        reserved[30] = 5;
        thinq.emit_data(&reserved);
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("425"));
    }

    // HA accepts such a payload, registers the entity, and then never creates it.
    #[test]
    fn no_read_only_component_claims_entity_category_config() {
        let (ha, _, _) = make();
        for (name, comp) in &components(&ha) {
            let platform = comp.get("platform").and_then(Value::as_str).unwrap_or("");
            if platform != "sensor" && platform != "binary_sensor" {
                continue;
            }
            assert_ne!(
                comp.get("entity_category").and_then(Value::as_str),
                Some("config"),
                "{name} must not be entity_category 'config'"
            );
        }
    }

    #[test]
    fn readings_are_enums_and_the_entity_set_matches_upstream() {
        let (ha, _, _) = make();
        let c = components(&ha);
        let mut keys: Vec<&str> = c.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut want = vec![
            "power",
            "status",
            "course",
            "course_select",
            "start_course",
            "pause_course",
            "resume_course",
            "smart_course",
            "remaining_time",
            "initial_time",
            "reserve_time",
            "error",
            "error_message",
            "child_lock",
            "night_dry",
            "remote_start",
            "remote_maintain",
            "buzzer",
            "end_melody",
            "keep_last_course",
            "smart_care_finedust",
            "smart_care_humidity",
            "smart_care_nightcare",
            "night_care_start",
            "night_care_end",
            "download_course",
            "tcl_count",
            "previous_status",
        ];
        want.sort_unstable();
        assert_eq!(keys, want);

        assert_eq!(c["status"]["name"], "Status");
        assert_eq!(c["child_lock"]["name"], "Child lock");
        assert_eq!(c["remote_start"]["name"], "Remote start");
        for key in [
            "status",
            "previous_status",
            "course",
            "smart_course",
            "download_course",
            "error_message",
        ] {
            assert_eq!(c[key]["device_class"], "enum", "{key}");
        }
        // Labels shared by several codes are offered once.
        assert_eq!(
            c["status"]["options"],
            json!([
                "Power off",
                "Standby",
                "Styling",
                "Pause",
                "Course complete",
                "Check appliance",
                "Smart diagnosing",
                "Storing",
                "Reserved",
                "Power-save running",
                "Steam preparing",
                "Refreshing",
                "Drying",
                "Sterilizing"
            ])
        );
        assert_eq!(c["error_message"]["options"][0], "Normal");
        assert_eq!(c["error_message"]["options"].as_array().unwrap().len(), 19);
        assert_eq!(c["course"]["options"][0], "None");
        assert_eq!(c["course_select"]["options"], json!(course_options()));
        assert_eq!(c["error"]["platform"], "binary_sensor");
        assert_eq!(c["error"]["device_class"], "problem");
        assert_eq!(c["error"]["icon"], "mdi:check-circle");
        assert_eq!(c["error"]["entity_category"], "diagnostic");
        for key in ["remaining_time", "initial_time", "reserve_time"] {
            assert_eq!(c[key]["device_class"], "duration", "{key}");
            assert_eq!(c[key]["unit_of_measurement"], "min", "{key}");
            assert_eq!(c[key]["state_class"], "measurement", "{key}");
        }
        assert_eq!(
            c["night_care_start"]["pattern"],
            "^([01]\\d|2[0-3]):(00|30)$"
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("ST_B_E4H01Y_APL").is_some());
    }
}
