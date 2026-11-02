//! S5MPC — LG Styler, deviceType 203.
//!
//! AABB class `0x31` with its own 27-byte record, not the S5BBP's 40-byte one:
//! `31 <type> [<len16 BE> <record>]…`, the trailing record current and a leading one, when
//! present, the previous state. Frames that do not split into whole 27-byte records are not
//! state (the `31 00` acks, the 9-byte `31 72` notifications around remote-start transitions,
//! the 55-byte downloadable-course name list). The checksum is verified on receive.
//!
//! Record offsets: 0 state, 1..2 remaining h/m, 3..4 initial h/m, 5 course, 6 error (only 0
//! was ever seen, so `error_message` is free text: `Normal`, else `Code N`), 7 previous state,
//! 12..13 reservation h/m, 14 flags (0x08 remote start, 0x01 the panel-only child lock),
//! 17..18 this cycle's energy in Wh (BE; 0 while running, the total once complete), 20 smart
//! course. Unmapped state and course codes are not published. Remaining time stays at 1 min
//! through completion and power-off, so it publishes 0 then.
//!
//! Settings are `f0 24 <ctrl> 01 <value>` (01 power off — there is no remote power on; 04
//! pause). Start is `f0 26 <id> 01 <smart> <04|02 store> 00 00 <reserve h> 00 00 <params>`,
//! resume `f0 26 <base id>` and 44 zero bytes, and a smart-course download `f0 25 03 2d`, the
//! start head without store or reservation, then the params: each course's modelJSON `function`
//! defaults, with `TimeDry` folded into bit 0x80 of the first byte. A smart start is the
//! download + start pair LG's app sends; selecting a smart course downloads it at once, and
//! downloads the bridge relays from LG's app are recognised too. Reservations are 0 (start
//! now) or 3..19 h. The smart-course selection outlives handler rebuilds (in process) and
//! restarts (`RETHINK_MEMORY_FILE`, default `/app/data/rethink-course-memory.json`, keyed by
//! device id). Remote start and child lock are read-only: neither has a command. `auto_ack`
//! stays off.
//!
//! Port of upstream PR #173.

use crate::course_memory::CourseMemory;
use crate::device_trait::DeviceHandler;
use crate::js;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::{DeviceDiscovery, PropertyValue};
use rethink_core::{hex_decode, unwrap_aabb, HaConnection, Metadata, Thinq2Device};
use rethink_util::packet_codec::aabb_checksum;
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

const STATE_TAG: u8 = 0x31;
const RECORD_LEN: usize = 27;
/// A record and the 16-bit length in front of it.
const CHUNK_LEN: usize = 2 + RECORD_LEN;

const OFF_STATE: usize = 0;
const OFF_REMAIN_H: usize = 1;
const OFF_REMAIN_M: usize = 2;
const OFF_INITIAL_H: usize = 3;
const OFF_INITIAL_M: usize = 4;
const OFF_COURSE: usize = 5;
const OFF_ERROR: usize = 6;
const OFF_RESERVE_H: usize = 12;
const OFF_RESERVE_M: usize = 13;
const OFF_FLAGS_A: usize = 14;
const OFF_ENERGY_HI: usize = 17;
const OFF_ENERGY_LO: usize = 18;
const OFF_SMART_COURSE: usize = 20;

const F_CHILD_LOCK: u8 = 0x01;
const F_REMOTE_START: u8 = 0x08;

const SET_STATE: [u8; 2] = [0xf0, 0x24];
const CTRL_POWER: u8 = 0x01;
const CTRL_PAUSE: u8 = 0x04;
const RUN_COURSE: [u8; 2] = [0xf0, 0x26];
const DOWNLOAD_COURSE: [u8; 4] = [0xf0, 0x25, 0x03, 0x2d];
const HEAD_RUN: u8 = 0x04;
const HEAD_STORE: u8 = 0x02;
/// Resume carries the course id and nothing else, one byte shorter than a start block.
const RESUME_BLOCK_LEN: usize = 45;

const STATE_POWEROFF: u8 = 0;
/// Declared by the modelJSON and measured on the S5BBP; never seen on this model.
const STATE_DIAGNOSIS: u8 = 6;
const COMPLETE_LABEL: &str = "Complete";
const RESERVE_MAX: u8 = 19;
/// "Downloaded Course": only ever the base of a smart run, and the select's route to one.
const DOWNLOADED_COURSE: u8 = 10;
const DEFAULT_COURSE: u8 = 1;
const DEFAULT_SMART: i64 = 66;

/// `styler.state`, which is not positional: the running codes jump to 50 and up.
const STATES: &[(&[u8], &str)] = &[
    (&[0], "Power off"),
    (&[1], "Standby"),
    (&[2], "Running"),
    (&[3], "Pause"),
    (&[4, 58, 59], COMPLETE_LABEL),
    (&[5], "Error"),
    (&[6], "Smart diagnosis"),
    (&[7], "Storing"),
    (&[8], "Reserved"),
    (&[9], "Power-save running"),
    (&[50], "Steam preparing"),
    (&[51, 52, 53], "Refreshing"),
    (&[54, 55, 56], "Drying"),
    (&[57], "Sterilizing"),
];

/// modelJSON `Course`. 13, 14, 16, 27, 67 and 78 only appear as the base of a smart course.
const COURSES: &[(u8, &str)] = &[
    (0, "None"),
    (1, "Styling Standard"),
    (3, "Styling Quick"),
    (5, "Styling Intensive"),
    (6, "Wool/Knit"),
    (7, "Suit/Coat"),
    (8, "Functionality"),
    (10, "Downloaded Course"),
    (11, "Sterilize Standard"),
    (12, "Bedding Sterilize"),
    (13, "Baby Clothing Sanitary"),
    (14, "Doll Sanitary"),
    (15, "Auto Dry"),
    (16, "Wool/Delicate Dry"),
    (17, "Timed Dry 30"),
    (18, "Timed Dry 60"),
    (19, "Timed Dry 90"),
    (20, "Timed Dry 120"),
    (27, "Uniform Management"),
    (28, "Padding Care"),
    (30, "Fine dust"),
    (31, "Virus"),
    (32, "Jeans Care"),
    (33, "Fur/Leather Care"),
    (36, "Suit/Uniform Sterilize"),
    (67, "Silent"),
    (78, "Fur/Leather"),
];

/// modelJSON `SmartCourse`, in LG's `ControlConvertingRule` vocabulary (76 keeps LG's "Raing").
/// 1 is LG's panel-pairing pseudo-course.
const SMART_COURSES: &[(u8, &str)] = &[
    (0, "None"),
    (1, "Panel course"),
    (61, "Suits Uniforms"),
    (62, "Scarves"),
    (66, "Pants"),
    (67, "Silent"),
    (68, "Coat Warmer"),
    (69, "Static Removal"),
    (71, "Stored Items"),
    (73, "Blanket Warmer"),
    (75, "Dress Shirts"),
    (76, "Snow Raing Drying"),
    (78, "Fur Leather"),
    (93, "Jeans Management"),
    (94, "Baby Clothing Sanitary"),
    (95, "Doll Sanitary"),
    (96, "Wool Delicate Dry"),
    (97, "Rainy Days"),
    (98, "Uniform"),
    (99, "Padding Management"),
    (100, "Thin Padding Management"),
    (101, "Thick Padding Management"),
    (102, "Silk Care"),
    (112, "Athleisure Refresh"),
    (113, "Athleisure Dry"),
    (114, "Swimsuit Dry"),
    (120, "Golf Wear Care"),
    (121, "Golf Wear Dry"),
];

/// Start parameters for every course Start course can run (course 20 shares course 18's: LG
/// gives both a 60-minute `Drying1_Time` default).
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
        8,
        "0200000000000300000000000000001400000000000000000400000300000178001a780000",
    ),
    (
        11,
        "0200002d000006000008000003000023780000000000000000000000000000000000000000",
    ),
    (
        12,
        "0200002d000006000008000003000028000000000000000000000000000000000000000000",
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
];

/// Every downloadable smart course: id, base course, parameters.
const SMART_PARAMS: &[(u8, u8, &str)] = &[
    (
        61,
        10,
        "8200002d000006000008000003000023780000000000000000000000000000000000000000",
    ),
    (
        62,
        10,
        "82000000000004000000000001000010780000000000000000000000000000000000000000",
    ),
    (
        66,
        10,
        "02000000000006000000000001000023000000000000000004000000000001b40019b40000",
    ),
    (
        67,
        67,
        "8200000000000400000200000200002800000000000000000700000300000200003a000000",
    ),
    (
        68,
        10,
        "8000000000000000000000000000000a780000000000000000000000000000000000000000",
    ),
    (
        69,
        10,
        "82000000000002000000000001780005780000000000000000000000000000000000000000",
    ),
    (
        71,
        10,
        "82000000000005005a05a05a0178001aa00000000000000000000000000000000000000000",
    ),
    (
        73,
        10,
        "8000000000000000000000000000001e780000000000000000000000000000000000000000",
    ),
    (
        75,
        10,
        "82000000000007000002c80001000064780000000000000000000000000000000000000000",
    ),
    (
        76,
        10,
        "0200000000000300000000000100002d780000000000000000000000000000000000000000",
    ),
    (
        78,
        78,
        "8000000000000000000000000000001e780000000000000000000000000000000000000000",
    ),
    (
        93,
        32,
        "8200002d000006000008000003000028000000000000000000000000000000000000000000",
    ),
    (
        94,
        13,
        "0200002d00000500000800000300001f000000000000000000000000000000000000000000",
    ),
    (
        95,
        14,
        "0200002d000006000008000003000030000000000000000000000000000000000000000000",
    ),
    (
        96,
        16,
        "80000000000000000000000000000096780000000000000000000000000000000000000000",
    ),
    (
        97,
        10,
        "8000000000000000000000000000005a780000000000000000000000000000000000000000",
    ),
    (
        98,
        27,
        "02000000000000000000000005c80000000000000000000008005a07c85a01c80031b40000",
    ),
    (
        99,
        28,
        "82005a00000005005a05b45a01b4002eb40000000000000000000000000000000000000000",
    ),
    (
        100,
        17,
        "80000000000000000000000008b40037780000000000000000000000000008b4000ab40000",
    ),
    (
        101,
        17,
        "80000000000000000000000005b4006e780000000000000000000000000005b4001eb40000",
    ),
    (
        102,
        16,
        "82000000000001005a00000001b40014b40000000000000000000000000000000000000000",
    ),
    (
        112,
        10,
        "02005a00000003000005a00001a00017a00000000000000000000000000000000000000000",
    ),
    (
        113,
        17,
        "80000000000000000000000000000064780000000000000000000000000000000000000000",
    ),
    (
        114,
        17,
        "80000000000000000000000000000050780000000000000000000000000000000000000000",
    ),
    (
        120,
        8,
        "02005a00000003005a0000000000000fa00000000000000003005a03005a01a00017a00000",
    ),
    (
        121,
        17,
        "80000000000000000000000000000055780000000000000000000000000000000000000000",
    ),
];

/// Last selection per device id. Every reconnect builds a new handler, which must not fall back
/// to its defaults.
static REMEMBERED: Mutex<BTreeMap<String, Memory>> = Mutex::new(BTreeMap::new());

fn state_label(code: u8) -> Option<&'static str> {
    STATES
        .iter()
        .find(|(codes, _)| codes.contains(&code))
        .map(|&(_, label)| label)
}

fn course_label(id: u8) -> Option<&'static str> {
    COURSES
        .iter()
        .find(|&&(c, _)| c == id)
        .map(|&(_, label)| label)
}

fn smart_label(id: i64) -> Option<&'static str> {
    SMART_COURSES
        .iter()
        .find(|&&(c, _)| i64::from(c) == id)
        .map(|&(_, label)| label)
}

fn code_of(table: &[(u8, &str)], label: &str) -> Option<u8> {
    table.iter().find(|&&(_, l)| l == label).map(|&(c, _)| c)
}

fn course_params(id: u8) -> Option<&'static str> {
    COURSE_PARAMS
        .iter()
        .find(|&&(c, _)| c == id)
        .map(|&(_, params)| params)
}

fn smart_params(id: i64) -> Option<&'static (u8, u8, &'static str)> {
    SMART_PARAMS.iter().find(|&&(c, _, _)| i64::from(c) == id)
}

fn minutes(h: u8, m: u8) -> PropertyValue {
    PropertyValue::Int(i64::from(h) * 60 + i64::from(m))
}

/// `f0 26 <id> 01 <smart> <04|02> 00 00 <reserve> 00 00 <params>`.
fn start_frame(id: u8, smart: u8, params: &str, store: bool, reserve: u8) -> Vec<u8> {
    let flags = HEAD_RUN | if store { HEAD_STORE } else { 0 };
    let mut inner = RUN_COURSE.to_vec();
    inner.extend([id, 0x01, smart, flags, 0x00, 0x00, reserve, 0x00, 0x00]);
    inner.extend(hex_decode(params));
    inner
}

/// `f0 25 03 2d <base> 01 <smart> 00 00 00 00 00 00 <params>`, whatever store and reservation
/// are set to.
fn download_frame(&(smart, base, params): &(u8, u8, &str)) -> Vec<u8> {
    let mut inner = DOWNLOAD_COURSE.to_vec();
    inner.extend([base, 0x01, smart, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    inner.extend(hex_decode(params));
    inner
}

fn sensor(id: &str, name: &str, extra: Value) -> Value {
    with(
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
    with(
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

fn duration(id: &str, name: &str, icon: &str) -> Value {
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

fn reading(id: &str, name: &str, options: Vec<&str>, icon: &str) -> Value {
    sensor(
        id,
        name,
        json!({"device_class": "enum", "options": options, "icon": icon}),
    )
}

fn choice(id: &str, name: &str, options: Vec<&str>, icon: &str) -> Value {
    json!({
        "platform": "select",
        "unique_id": format!("$deviceid-{id}"),
        "state_topic": format!("$this/{id}"),
        "command_topic": format!("$this/{id}/set"),
        "name": name,
        "options": options,
        "icon": icon,
    })
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

fn with(mut base: Value, extra: Value) -> Value {
    if let (Value::Object(base), Value::Object(extra)) = (&mut base, extra) {
        base.extend(extra);
    }
    base
}

fn config(meta: &Metadata) -> DeviceDiscovery {
    let course_options = COURSE_PARAMS
        .iter()
        .map(|&(id, _)| id)
        .chain([DOWNLOADED_COURSE])
        .filter_map(course_label)
        .collect();
    let smart_options = SMART_PARAMS
        .iter()
        .filter_map(|&(id, _, _)| smart_label(id.into()))
        .collect();
    let mut config = default_config(meta, Some(json!({"name": "LG Styler"})));
    config.components = [
        ("power_off", press("power_off", "Power off", "mdi:power")),
        (
            "power",
            flag("power", "Power", json!({"icon": "mdi:power"})),
        ),
        (
            "status",
            reading(
                "status",
                "Status",
                STATES.iter().map(|&(_, label)| label).collect(),
                "mdi:hanger",
            ),
        ),
        (
            "course",
            reading(
                "course",
                "Course",
                COURSES.iter().map(|&(_, label)| label).collect(),
                "mdi:playlist-check",
            ),
        ),
        // Choosing a course does not start it, as in LG's own app.
        (
            "course_select",
            choice(
                "course_select",
                "Course select",
                course_options,
                "mdi:playlist-edit",
            ),
        ),
        (
            "smart_course",
            reading(
                "smart_course",
                "Smart course",
                SMART_COURSES.iter().map(|&(_, label)| label).collect(),
                "mdi:playlist-star",
            ),
        ),
        (
            "smart_course_select",
            choice(
                "smart_course_select",
                "Smart course select",
                smart_options,
                "mdi:playlist-edit",
            ),
        ),
        (
            "start_course",
            press("start_course", "Start course", "mdi:play-circle-outline"),
        ),
        ("pause", press("pause", "Pause", "mdi:pause-circle-outline")),
        ("resume", press("resume", "Resume", "mdi:play-pause")),
        (
            "remaining_time",
            duration("remaining_time", "Remaining time", "mdi:timer-sand"),
        ),
        (
            "initial_time",
            duration("initial_time", "Initial time", "mdi:timer-outline"),
        ),
        // Start modifiers, read by Start course.
        (
            "reserve_hours",
            json!({
                "platform": "number",
                "unique_id": "$deviceid-reserve_hours",
                "state_topic": "$this/reserve_hours",
                "command_topic": "$this/reserve_hours/set",
                "name": "Reserve hours",
                "min": 0,
                "max": RESERVE_MAX,
                "step": 1,
                "unit_of_measurement": "h",
                "icon": "mdi:calendar-clock",
                "entity_category": "config",
            }),
        ),
        (
            "store",
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-store",
                "state_topic": "$this/store",
                "command_topic": "$this/store/set",
                "name": "Store",
                "payload_on": "ON",
                "payload_off": "OFF",
                "icon": "mdi:weather-night",
                "entity_category": "config",
            }),
        ),
        // A delay before the course starts, not a wall-clock time.
        (
            "reserve_time",
            duration("reserve_time", "Reserve time", "mdi:calendar-clock"),
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
            "child_lock",
            flag(
                "child_lock",
                "Child lock",
                json!({"icon": "mdi:lock", "entity_category": "diagnostic"}),
            ),
        ),
        // The appliance starts no course while an error stands.
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
            sensor(
                "error_message",
                "Error message",
                json!({"icon": "mdi:alert-circle-outline", "entity_category": "diagnostic"}),
            ),
        ),
        (
            "smart_diagnosis",
            flag(
                "smart_diagnosis",
                "Smart diagnosis",
                json!({
                    "device_class": "problem",
                    "icon": "mdi:stethoscope",
                    "entity_category": "diagnostic",
                }),
            ),
        ),
        (
            "energy",
            sensor(
                "energy",
                "Energy",
                json!({
                    "device_class": "energy",
                    "unit_of_measurement": "Wh",
                    "state_class": "total_increasing",
                    "suggested_display_precision": 0,
                    "icon": "mdi:lightning-bolt",
                }),
            ),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    config
}

/// This handler's entry in the shared course memory file (upstream's format).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Memory {
    smart: i64,
    #[serde(rename = "smartSelected")]
    smart_selected: bool,
}

struct State {
    /// What Start course runs; `smart_selected` says which select won last.
    selected_course: u8,
    selected_smart: i64,
    smart_selected: bool,
    /// Base id of the last start: smart runs resume under their base course.
    selected_base: u8,
    reserve_hours: u8,
    store: bool,
    /// Last state the appliance reported, `None` before the first state frame.
    powered_on: Option<bool>,
    has_problem: Option<bool>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    memory: CourseMemory,
    state: Mutex<State>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        Self::with_memory_file(ha, thinq, meta, CourseMemory::default_file())
    }

    fn with_memory_file(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        memory_file: PathBuf,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            memory: CourseMemory::new(memory_file, thinq.id()),
            state: Mutex::new(State {
                selected_course: DEFAULT_COURSE,
                selected_smart: DEFAULT_SMART,
                smart_selected: false,
                selected_base: DEFAULT_COURSE,
                reserve_hours: 0,
                store: false,
                powered_on: None,
                has_problem: None,
            }),
        });

        let t = this.clone();
        thinq.on_send_packet(Box::new(move |buf| t.observe_outgoing(buf)));

        core.set_config(config(&meta));
        core.publish_property("reserve_hours", PropertyValue::Int(0));
        core.publish_on_off("store", false);
        this.restore();

        let t = this.clone();
        thinq.on_data(Box::new(move |data| t.process_data(data)));
        this
    }

    fn publish_label(&self, prop: &str, label: Option<&str>) {
        self.core
            .publish_property(prop, label.unwrap_or("None").into());
    }

    /// Picks up the selection an earlier handler for this device left, in process or on disk.
    fn restore(&self) {
        let remembered = REMEMBERED.lock().get(&self.core.id).copied();
        let remembered = remembered.or_else(|| self.memory.load::<Memory>());
        let mut st = self.state.lock();
        if let Some(memory) = remembered {
            st.selected_smart = memory.smart;
            st.smart_selected = memory.smart_selected;
            if memory.smart_selected {
                self.publish_label("course_select", course_label(DOWNLOADED_COURSE));
                self.publish_label("smart_course_select", smart_label(st.selected_smart));
                self.publish_label("smart_course", smart_label(st.selected_smart));
                return;
            }
        }
        self.publish_label("course_select", course_label(st.selected_course));
        self.publish_label("smart_course_select", smart_label(st.selected_smart));
    }

    fn remember(&self, st: &mut State) {
        let entry = Memory {
            smart: st.selected_smart,
            smart_selected: st.smart_selected,
        };
        REMEMBERED.lock().insert(self.core.id.clone(), entry);
        self.memory.save(&entry);
    }

    fn process_data(&self, data: &[u8]) {
        if data.len() < 4 || data[data.len() - 2] != aabb_checksum(&data[..data.len() - 2]) {
            return;
        }
        if let Some(inner) = self.core.process_data_envelope(data) {
            self.process_aabb(inner);
        }
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&STATE_TAG) {
            return;
        }
        let Some(body) = buf.get(2..) else {
            return;
        };
        if body.is_empty() || body.len() % CHUNK_LEN != 0 {
            return;
        }
        if body
            .chunks(CHUNK_LEN)
            .any(|chunk| usize::from(u16::from_be_bytes([chunk[0], chunk[1]])) != RECORD_LEN)
        {
            return;
        }
        let rec = &body[body.len() - RECORD_LEN..];
        let state = rec[OFF_STATE];
        let error = rec[OFF_ERROR];
        let course = rec[OFF_COURSE];
        let smart = rec[OFF_SMART_COURSE];

        let mut st = self.state.lock();
        st.powered_on = Some(state != STATE_POWEROFF);
        st.has_problem = Some(error != 0);
        self.core.publish_on_off("power", state != STATE_POWEROFF);
        let status = state_label(state);
        if let Some(status) = status {
            self.core.publish_property("status", status.into());
        }
        // While a smart course runs, `course` shows the placeholder Course select uses for it.
        // Its id reads 0 whenever none runs, which says nothing about what is installed.
        let running_smart = if smart != 0 {
            smart_label(smart.into())
        } else {
            None
        };
        if let Some(name) = running_smart {
            self.publish_label("course", course_label(DOWNLOADED_COURSE));
            self.core.publish_property("smart_course", name.into());
        } else {
            if let Some(name) = course_label(course) {
                self.core.publish_property("course", name.into());
            }
            if smart == 0 && st.smart_selected {
                if let Some(armed) = smart_label(st.selected_smart) {
                    self.core
                        .publish_property("smart_course_select", armed.into());
                    self.core.publish_property("smart_course", armed.into());
                }
            }
        }

        if let Some(&(id, base, _)) = smart_params(smart.into()) {
            st.selected_smart = id.into();
            st.selected_base = base;
            st.smart_selected = true;
            self.remember(&mut st);
            self.publish_label("smart_course_select", smart_label(id.into()));
            self.publish_label("course_select", course_label(DOWNLOADED_COURSE));
        } else if course_params(course).is_some() {
            st.selected_course = course;
            st.selected_base = course;
            st.smart_selected = false;
            self.remember(&mut st);
            self.publish_label("course_select", course_label(course));
        }
        drop(st);

        let finished = state == STATE_POWEROFF || status == Some(COMPLETE_LABEL);
        self.core.publish_property(
            "remaining_time",
            if finished {
                PropertyValue::Int(0)
            } else {
                minutes(rec[OFF_REMAIN_H], rec[OFF_REMAIN_M])
            },
        );
        self.core.publish_property(
            "initial_time",
            minutes(rec[OFF_INITIAL_H], rec[OFF_INITIAL_M]),
        );
        self.core.publish_property(
            "reserve_time",
            minutes(rec[OFF_RESERVE_H], rec[OFF_RESERVE_M]),
        );
        self.core
            .publish_flag("remote_start", rec[OFF_FLAGS_A], F_REMOTE_START);
        self.core
            .publish_flag("child_lock", rec[OFF_FLAGS_A], F_CHILD_LOCK);
        self.core.publish_on_off("error", error != 0);
        self.core.publish_property(
            "error_message",
            if error == 0 {
                "Normal".into()
            } else {
                format!("Code {error}").into()
            },
        );
        self.core
            .publish_on_off("smart_diagnosis", state == STATE_DIAGNOSIS);
        let energy = u16::from_be_bytes([rec[OFF_ENERGY_HI], rec[OFF_ENERGY_LO]]);
        self.core
            .publish_property("energy", PropertyValue::Int(energy.into()));
    }

    /// The bridge relays LG's app straight to the appliance; one of its downloads selects that
    /// smart course here as well.
    fn observe_outgoing(&self, buf: &[u8]) {
        let Some(inner) = unwrap_aabb(buf) else {
            return;
        };
        let Some(&(smart, base, _)) = SMART_PARAMS.iter().find(|e| download_frame(e) == inner)
        else {
            return;
        };
        let mut st = self.state.lock();
        if st.selected_smart == i64::from(smart) && st.smart_selected {
            return;
        }
        st.selected_smart = smart.into();
        st.selected_base = base;
        st.smart_selected = true;
        self.remember(&mut st);
        self.publish_label("smart_course_select", smart_label(smart.into()));
        self.publish_label("course_select", course_label(DOWNLOADED_COURSE));
        self.publish_label("smart_course", smart_label(smart.into()));
    }

    fn set_control(&self, ctrl: u8, value: u8) {
        let mut inner = SET_STATE.to_vec();
        inner.extend([ctrl, 0x01, value]);
        self.core.send(&inner);
    }

    /// Shows a start without waiting for the appliance, whose next frame has the last word.
    /// Known refusals are not echoed: nothing while powered off, no course while an error stands.
    fn echo(&self, prop: &str, label: Option<&str>) {
        let st = self.state.lock();
        if st.powered_on != Some(true) || st.has_problem != Some(false) {
            return;
        }
        drop(st);
        self.publish_label(prop, label);
    }

    fn select_course(&self, value: &str) {
        let Some(id) = code_of(COURSES, value) else {
            tracing::debug!("Unknown course {value}");
            return;
        };
        let mut st = self.state.lock();
        if id == DOWNLOADED_COURSE {
            // Start course then runs whatever Smart course select is set to.
            st.smart_selected = true;
        } else if course_params(id).is_some() {
            st.selected_course = id;
            st.selected_base = id;
            st.smart_selected = false;
        } else {
            tracing::debug!("Unknown course {value}");
            return;
        }
        self.remember(&mut st);
        self.core.publish_property("course_select", value.into());
    }

    fn select_smart_course(&self, value: &str) {
        let Some(entry) = code_of(SMART_COURSES, value).and_then(|id| smart_params(id.into()))
        else {
            tracing::debug!("Unknown smart course {value}");
            return;
        };
        {
            let mut st = self.state.lock();
            st.selected_smart = entry.0.into();
            st.smart_selected = true;
            self.remember(&mut st);
        }
        // Downloaded straight away, even while powered off, so the appliance holds it by Start.
        self.core.send(&download_frame(entry));
        self.core
            .publish_property("smart_course_select", value.into());
        self.publish_label("course_select", course_label(DOWNLOADED_COURSE));
        self.core.publish_property("smart_course", value.into());
    }

    fn set_reserve_hours(&self, value: &str) {
        match js::number(value).filter(|h| h.fract() == 0.0) {
            Some(h) if h == 0.0 || (3.0..=f64::from(RESERVE_MAX)).contains(&h) => {
                let hours = h as u8;
                self.state.lock().reserve_hours = hours;
                self.core
                    .publish_property("reserve_hours", PropertyValue::Int(hours.into()));
            }
            _ => tracing::debug!("Reserve hours out of range {value}"),
        }
    }

    fn start_course(&self) {
        let (frames, (prop, label)) = {
            let mut st = self.state.lock();
            if st.smart_selected {
                let entry = smart_params(st.selected_smart);
                let frames = match entry {
                    Some(entry @ &(smart, base, params)) => {
                        st.selected_base = base;
                        vec![
                            download_frame(entry),
                            start_frame(base, smart, params, st.store, st.reserve_hours),
                        ]
                    }
                    None => {
                        tracing::debug!("Course cannot start {}", st.selected_smart);
                        Vec::new()
                    }
                };
                (frames, ("smart_course", smart_label(st.selected_smart)))
            } else {
                let id = st.selected_course;
                let frames = match course_params(id) {
                    Some(params) => vec![start_frame(id, 0, params, st.store, st.reserve_hours)],
                    None => {
                        tracing::debug!("Course cannot start {id}");
                        Vec::new()
                    }
                };
                (frames, ("course", course_label(id)))
            }
        };
        for frame in frames {
            self.core.send(&frame);
        }
        self.echo(prop, label);
    }

    fn resume_course(&self) {
        let mut block = [0u8; RESUME_BLOCK_LEN];
        block[0] = self.state.lock().selected_base;
        let mut inner = RUN_COURSE.to_vec();
        inner.extend(block);
        self.core.send(&inner);
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        match prop {
            // This unit keeps reporting while off, so its next frame confirms the press.
            "power_off" => self.set_control(CTRL_POWER, 0),
            "course_select" => self.select_course(value),
            "smart_course_select" => self.select_smart_course(value),
            "reserve_hours" => self.set_reserve_hours(value),
            "store" => {
                let on = value == "ON";
                self.state.lock().store = on;
                self.core.publish_on_off("store", on);
            }
            "start_course" => self.start_course(),
            "pause" => self.set_control(CTRL_PAUSE, 0),
            "resume" => self.resume_course(),
            _ => tracing::debug!("Item does not support writing {prop}"),
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
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};
    use std::collections::HashMap;

    const MODEL_ID: &str = "S5MPC";

    // Captured 2026-09-07, each checked against LG's cloud decode of the same moment: Standby
    // with remote start on and off, Pause after Steam preparing, Steam preparing, a 19-hour
    // reservation of Styling Quick, and Power off. All read error 0 and energy 0.
    const REMOTE_ON: &str =
        "aa4031ec001b010001000100000000000000000020000000000000000001624200001b01000100010000000000000000002800000000000000000162420080bb";
    const REMOTE_OFF: &str =
        "aa4031ec001b010001000100000000000000000028000000000000000001624200001b01000100010000000000000000002000000000000000000162420080bb";
    const PAUSE: &str =
        "aa4031ec001b320027002701000100000000000028000000000000000001624200001b03002700270100320000000000002800000000000000000162420088bb";
    const PRESTEAM: &str =
        "aa4031ec001b010001000100000000000000000028000000000000000001624200001b32002700270100010000000000002800000000000000000162420009bb";
    const RESERVED: &str =
        "aa4031ec001b030027002701003200000000000028000000000000000001624200001b080014001403000300000000130008000000000000000001624200d1bb";
    const POWEROFF: &str =
        "aa4031ec001b01003b003b1c000300000000000020000000000063000001634200001b00003b003b000001000000000000200000000000000000016342006cbb";
    // After a full steam cycle: record+17 reads 0x0251, the ThinQ app's 593 Wh for the day.
    const ENERGY_AT_COMPLETION: &str =
        "aa2331eb001b00000100000000040000000000002000000251000000000163420077bb";
    // The previous record is Complete, the current one already Power off; both still 0h01m.
    const COMPLETE: &str =
        "aa4031ec001b040001012c0c003800000000000028000002510000000001634200001b000001012c000004000000000000200000025100000000016342004abb";
    // Smart course Golf Wear Dry running on its base course, Timed Dry 30.
    const SMART_RUN: &str =
        "aa4031ec001b01003b003b1c000300000000000028000000000063000001634200001b010119011911000300000000000028000000000079000001794200ffbb";
    // The panel's child-lock button, captured 2026-09-08: only flagsA's 0x01 bit moves.
    const CHILD_LOCK_ON: &str =
        "aa4031ec001b010001000100000000000000000020000000000000000001634200001b01000100010000000000000000002100000000000000000163420085bb";
    const CHILD_LOCK_OFF: &str =
        "aa4031ec001b010001000100000000000000000021000000000000000001634200001b01000100010000000000000000002000000000000000000163420085bb";
    // Not state: an ACK, a 9-byte notification of the kind around remote-start transitions,
    // and the downloadable-course name list.
    const ACK: &str = "aa083100240052bb";
    const NOTIFY_9: &str = "aa09317200c9004abb";
    const COURSE_LIST: &str =
        "aa37313102015341413337353639313431000066260000800000000000025341413337353639303230000062a80000400000000000d9bb";

    // What LG's app sent before the captured Golf Wear Dry run, reserved 3 hours ahead.
    const GOLF_WEAR_DRY_DOWNLOAD: &str =
        "aa36f025032d11017900000000000080000000000000000000000000000055780000000000000000000000000000000000000000a8bb";
    const GOLF_WEAR_DRY_START_IN_3H: &str =
        "aa34f0261101790400000300008000000000000000000000000000005578000000000000000000000000000000000000000086bb";

    struct Rig {
        id: &'static str,
        ha: Arc<MockHaConnection>,
        thinq: Arc<MockThinq2Device>,
        dev: Arc<Device>,
    }

    impl Rig {
        fn emit(&self, frame: &str) {
            self.thinq.emit_data(&hex_decode(frame));
        }

        fn properties(&self) -> HashMap<String, PropertyValue> {
            self.ha
                .device(self.id)
                .map(|d| d.properties)
                .unwrap_or_default()
        }

        fn value(&self, name: &str) -> Option<PropertyValue> {
            self.properties().remove(name)
        }

        fn prop(&self, name: &str) -> Option<String> {
            self.value(name).map(|v| v.as_string())
        }

        fn outbox(&self) -> Vec<String> {
            self.thinq
                .outbox()
                .iter()
                .map(|p| hex_encode(p).to_ascii_lowercase())
                .collect()
        }
    }

    /// One appliance per test, with its own device id and memory file, so tests run in parallel.
    struct Appliance {
        id: &'static str,
        file: PathBuf,
    }

    impl Appliance {
        fn new(id: &'static str) -> Self {
            let file = std::env::temp_dir().join(format!(
                "rethink-course-memory-s5mpc-{}-{id}.json",
                std::process::id()
            ));
            Self { id, file }
        }

        /// Upstream's `makeDevice()`: a new handler, with no memory file on disk.
        fn make(&self) -> Rig {
            let _ = std::fs::remove_file(&self.file);
            self.build()
        }

        /// A new handler over whatever memory is on disk.
        fn build(&self) -> Rig {
            let ha = MockHaConnection::new();
            let meta = Metadata::new(MODEL_ID, MODEL_ID, "2.10.95");
            let thinq = MockThinq2Device::new(self.id, meta.clone());
            let dev = Device::with_memory_file(ha.clone(), thinq.clone(), meta, self.file.clone());
            Rig {
                id: self.id,
                ha,
                thinq,
                dev,
            }
        }

        /// What a real restart does to the in-process memory.
        fn restart(&self) {
            REMEMBERED.lock().remove(self.id);
        }
    }

    impl Drop for Appliance {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.file);
        }
    }

    fn reseal(mut packet: Vec<u8>) -> Vec<u8> {
        let n = packet.len();
        packet[n - 2] = aabb_checksum(&packet[..n - 2]);
        packet
    }

    /// Model-declared state probe derived from a real frame; not capture evidence.
    fn with_current_state(state: u8) -> Vec<u8> {
        let mut packet = hex_decode(POWEROFF);
        let current = packet.len() - 2 - RECORD_LEN;
        packet[current + OFF_STATE] = state;
        reseal(packet)
    }

    fn smart_run_with(course: u8, smart: u8) -> Vec<u8> {
        let mut packet = hex_decode(SMART_RUN);
        let current = packet.len() - 2 - RECORD_LEN;
        packet[current + OFF_COURSE] = course;
        packet[current + OFF_SMART_COURSE] = smart;
        reseal(packet)
    }

    #[test]
    fn the_real_capture_corpus_is_intact_not_hand_written() {
        for frame in [
            REMOTE_ON,
            REMOTE_OFF,
            PAUSE,
            PRESTEAM,
            RESERVED,
            POWEROFF,
            SMART_RUN,
            CHILD_LOCK_ON,
            CHILD_LOCK_OFF,
            ENERGY_AT_COMPLETION,
            COMPLETE,
        ] {
            let packet = hex_decode(frame);
            let n = packet.len();
            assert_eq!(usize::from(packet[1]), n, "{frame}");
            assert_eq!(packet[n - 2], aabb_checksum(&packet[..n - 2]), "{frame}");
        }
    }

    #[test]
    fn exactly_the_scoped_entities_exist_with_the_right_writability() {
        let s5 = Appliance::new("scoped-entities");
        let rig = s5.make();
        let comps = rig.ha.device(rig.id).unwrap().config.unwrap().components;
        let mut keys: Vec<&str> = comps.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "child_lock",
                "course",
                "course_select",
                "energy",
                "error",
                "error_message",
                "initial_time",
                "pause",
                "power",
                "power_off",
                "remaining_time",
                "remote_start",
                "reserve_hours",
                "reserve_time",
                "resume",
                "smart_course",
                "smart_course_select",
                "smart_diagnosis",
                "start_course",
                "status",
                "store",
            ]
        );
        assert_eq!(comps["energy"]["name"], "Energy");
        assert_eq!(comps["energy"]["device_class"], "energy");
        assert_eq!(comps["energy"]["unit_of_measurement"], "Wh");
        assert_eq!(comps["energy"]["state_class"], "total_increasing");
        // A first-class reading like the washer's, not a diagnostic.
        assert!(comps["energy"].get("entity_category").is_none());
        assert_eq!(comps["power"]["platform"], "binary_sensor");
        assert_eq!(comps["power"]["icon"], "mdi:power");
        assert_eq!(comps["smart_diagnosis"]["device_class"], "problem");
        assert_eq!(
            comps["status"]["options"],
            json!([
                "Power off",
                "Standby",
                "Running",
                "Pause",
                "Complete",
                "Error",
                "Smart diagnosis",
                "Storing",
                "Reserved",
                "Power-save running",
                "Steam preparing",
                "Refreshing",
                "Drying",
                "Sterilizing",
            ])
        );
        for name in [
            "power_off",
            "course_select",
            "smart_course_select",
            "start_course",
            "pause",
            "resume",
            "reserve_hours",
            "store",
        ] {
            assert_eq!(
                comps[name]["command_topic"],
                format!("$this/{name}/set"),
                "{name} is writable"
            );
        }
        for name in [
            "power",
            "status",
            "course",
            "smart_course",
            "remaining_time",
            "initial_time",
            "reserve_time",
            "remote_start",
            "child_lock",
            "error",
            "error_message",
            "smart_diagnosis",
            "energy",
        ] {
            assert!(
                comps[name].get("command_topic").is_none(),
                "{name} stays read-only"
            );
            let platform = comps[name]["platform"].as_str().unwrap();
            assert!(
                platform == "sensor" || platform == "binary_sensor",
                "{name} is a sensor platform, got {platform}"
            );
        }
    }

    #[test]
    fn publishes_this_cycle_energy_from_the_record_17_wh_counter() {
        let s5 = Appliance::new("energy");
        let rig = s5.make();
        rig.emit(ENERGY_AT_COMPLETION);
        assert_eq!(rig.value("energy"), Some(PropertyValue::Int(593)));
    }

    #[test]
    fn remote_start_follows_the_verified_flags_bit() {
        let s5 = Appliance::new("remote-start");
        let rig = s5.make();
        rig.emit(REMOTE_ON);
        assert_eq!(rig.prop("power").as_deref(), Some("ON"));
        assert_eq!(rig.prop("remote_start").as_deref(), Some("ON"));
        assert_eq!(rig.prop("status").as_deref(), Some("Standby"));
        rig.emit(REMOTE_OFF);
        assert_eq!(rig.prop("remote_start").as_deref(), Some("OFF"));
        assert_eq!(rig.prop("status").as_deref(), Some("Standby"));
    }

    #[test]
    fn child_lock_follows_the_verified_flags_bit_independent_of_remote_start() {
        let s5 = Appliance::new("child-lock");
        let rig = s5.make();
        rig.emit(CHILD_LOCK_ON);
        assert_eq!(rig.prop("child_lock").as_deref(), Some("ON"));
        rig.emit(CHILD_LOCK_OFF);
        assert_eq!(rig.prop("child_lock").as_deref(), Some("OFF"));
        // The remote-start captures never set 0x01, the child-lock ones never 0x08.
        rig.emit(REMOTE_ON);
        assert_eq!(rig.prop("child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn status_tracks_the_verified_state_codes() {
        let s5 = Appliance::new("status");
        let rig = s5.make();
        rig.emit(PAUSE);
        assert_eq!(rig.prop("status").as_deref(), Some("Pause"));
        assert_eq!(rig.value("remaining_time"), Some(PropertyValue::Int(39)));
        assert_eq!(rig.value("initial_time"), Some(PropertyValue::Int(39)));
        rig.emit(PRESTEAM);
        assert_eq!(rig.prop("status").as_deref(), Some("Steam preparing"));
        rig.emit(RESERVED);
        assert_eq!(rig.prop("status").as_deref(), Some("Reserved"));
        assert_eq!(rig.value("reserve_time"), Some(PropertyValue::Int(19 * 60)));
        rig.emit(POWEROFF);
        assert_eq!(rig.prop("power").as_deref(), Some("OFF"));
        assert_eq!(rig.prop("status").as_deref(), Some("Power off"));
    }

    #[test]
    fn remaining_time_reads_0_once_the_cycle_is_done() {
        let s5 = Appliance::new("remaining-done");
        let rig = s5.make();
        rig.emit(COMPLETE);
        assert_eq!(rig.prop("status").as_deref(), Some("Power off"));
        assert_eq!(rig.value("remaining_time"), Some(PropertyValue::Int(0)));
        rig.emit(POWEROFF);
        assert_eq!(rig.value("remaining_time"), Some(PropertyValue::Int(0)));
        rig.thinq.emit_data(&with_current_state(4));
        assert_eq!(rig.prop("status").as_deref(), Some("Complete"));
        assert_eq!(rig.value("remaining_time"), Some(PropertyValue::Int(0)));
    }

    #[test]
    fn a_smart_course_base_id_still_reports_a_course_label() {
        // 13/14/16/27/67/78 never start on their own; 67 (Silent) is both a smart course and a
        // course, and with no smart course running `course` shows it directly.
        let s5 = Appliance::new("smart-base-label");
        let rig = s5.make();
        rig.thinq.emit_data(&smart_run_with(67, 0));
        assert_eq!(rig.prop("course").as_deref(), Some("Silent"));
    }

    #[test]
    fn a_smart_course_built_on_base_67_reports_downloaded_course_like_the_washer() {
        let s5 = Appliance::new("smart-base-67");
        let rig = s5.make();
        rig.thinq.emit_data(&smart_run_with(67, 67));
        assert_eq!(rig.prop("course").as_deref(), Some("Downloaded Course"));
        assert_eq!(rig.prop("smart_course").as_deref(), Some("Silent"));
    }

    #[test]
    fn uses_the_common_error_and_smart_diagnosis_states_declared_by_the_model() {
        let s5 = Appliance::new("error-diagnosis");
        let rig = s5.make();
        rig.thinq.emit_data(&with_current_state(5));
        assert_eq!(rig.prop("status").as_deref(), Some("Error"));
        assert_eq!(rig.prop("power").as_deref(), Some("ON"));
        rig.thinq.emit_data(&with_current_state(6));
        assert_eq!(rig.prop("status").as_deref(), Some("Smart diagnosis"));
        assert_eq!(rig.prop("smart_diagnosis").as_deref(), Some("ON"));
    }

    #[test]
    fn a_running_smart_course_reports_the_downloaded_course_placeholder_like_the_washer() {
        let s5 = Appliance::new("smart-run");
        let rig = s5.make();
        rig.emit(SMART_RUN);
        assert_eq!(rig.prop("course").as_deref(), Some("Downloaded Course"));
        assert_eq!(rig.prop("smart_course").as_deref(), Some("Golf Wear Dry"));
        assert_eq!(
            rig.prop("smart_course_select").as_deref(),
            Some("Golf Wear Dry")
        );
        assert_eq!(
            rig.prop("course_select").as_deref(),
            Some("Downloaded Course")
        );
    }

    #[test]
    fn a_frame_with_a_corrupted_checksum_is_dropped_not_decoded() {
        let s5 = Appliance::new("corrupted");
        let rig = s5.make();
        rig.emit(REMOTE_ON);
        let before = rig.properties();
        let mut corrupted = hex_decode(SMART_RUN);
        corrupted[10] ^= 0xff;
        rig.thinq.emit_data(&corrupted);
        assert_eq!(rig.value("course"), before.get("course").cloned());
        assert_eq!(
            rig.value("smart_course"),
            before.get("smart_course").cloned()
        );
    }

    #[test]
    fn error_smart_diagnosis_and_energy_stay_clear_on_every_captured_frame() {
        let s5 = Appliance::new("clear");
        let rig = s5.make();
        for frame in [REMOTE_ON, REMOTE_OFF, PAUSE, PRESTEAM, RESERVED, POWEROFF] {
            rig.emit(frame);
        }
        assert_eq!(rig.prop("error").as_deref(), Some("OFF"));
        assert_eq!(rig.prop("error_message").as_deref(), Some("Normal"));
        assert_eq!(rig.prop("smart_diagnosis").as_deref(), Some("OFF"));
        assert_eq!(rig.value("energy"), Some(PropertyValue::Int(0)));
    }

    #[test]
    fn non_state_frames_are_ignored() {
        let s5 = Appliance::new("non-state");
        let rig = s5.make();
        rig.emit(REMOTE_ON);
        let before = rig.properties();
        rig.emit(ACK);
        rig.emit(NOTIFY_9);
        rig.emit(COURSE_LIST);
        assert_eq!(rig.properties(), before);
    }

    #[test]
    fn each_write_reproduces_the_frame_lg_itself_sent_for_that_command() {
        let s5 = Appliance::new("captured-writes");
        // Start Standard, immediate.
        {
            let rig = s5.make();
            rig.dev.set_property("course_select", "Styling Standard");
            rig.thinq.reset_recorder();
            rig.dev.set_property("start_course", "");
            assert_eq!(
                rig.outbox(),
                ["aa34f02601010004000000000002645a00000005005a05b45a01b4001ab40000000000000000000000000000000000000000fabb"],
                "start sent one frame"
            );
        }
        // Start Standard, store + 19-hour reservation.
        {
            let rig = s5.make();
            rig.dev.set_property("course_select", "Styling Standard");
            rig.dev.set_property("store", "ON");
            rig.dev.set_property("reserve_hours", "19");
            rig.thinq.reset_recorder();
            rig.dev.set_property("start_course", "");
            assert_eq!(
                rig.outbox(),
                ["aa34f02601010006000013000002645a00000005005a05b45a01b4001ab4000000000000000000000000000000000000000091bb"],
                "reserved start sent one frame"
            );
        }
        // Start Quick, 19-hour reservation.
        {
            let rig = s5.make();
            rig.dev.set_property("course_select", "Styling Quick");
            rig.dev.set_property("reserve_hours", "19");
            rig.thinq.reset_recorder();
            rig.dev.set_property("start_course", "");
            assert_eq!(
                rig.outbox(),
                ["aa34f02603010004000013000082645a00000003005a00000001b4000eb4000000000000000000000000000000000000000076bb"],
                "quick start sent one frame"
            );
        }
        // Power off, pause, resume.
        {
            let rig = s5.make();
            rig.dev.set_property("course_select", "Styling Standard");
            rig.dev.set_property("start_course", "");
            rig.thinq.reset_recorder();
            rig.dev.set_property("pause", "");
            assert_eq!(rig.outbox(), ["aa09f02404010099bb"]);
            rig.thinq.reset_recorder();
            rig.dev.set_property("resume", "");
            assert_eq!(
                rig.outbox(),
                ["aa33f026010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a1bb"]
            );
            rig.thinq.reset_recorder();
            rig.dev.set_property("power_off", "");
            assert_eq!(rig.outbox(), ["aa09f0240101009cbb"]);
        }
    }

    #[test]
    fn smart_starts_reproduce_the_captured_download_and_start_pairs() {
        let s5 = Appliance::new("smart-starts");
        // Golf Wear Dry (base Timed Dry 30), 3-hour reservation: the reservation rides in the
        // start head only.
        {
            let rig = s5.make();
            rig.dev.set_property("smart_course_select", "Golf Wear Dry");
            rig.dev.set_property("reserve_hours", "3");
            rig.thinq.reset_recorder();
            rig.dev.set_property("start_course", "");
            assert_eq!(
                rig.outbox(),
                [GOLF_WEAR_DRY_DOWNLOAD, GOLF_WEAR_DRY_START_IN_3H],
                "smart start sent two frames"
            );
        }
        // Snow Raing Drying, 3-hour reservation.
        {
            let rig = s5.make();
            rig.dev
                .set_property("smart_course_select", "Snow Raing Drying");
            rig.dev.set_property("reserve_hours", "3");
            rig.thinq.reset_recorder();
            rig.dev.set_property("start_course", "");
            assert_eq!(
                rig.outbox(),
                [
                    "aa36f025032d0a014c0000000000000200000000000300000000000100002d78000000000000000000000000000000000000000072bb",
                    "aa34f0260a014c0400000300000200000000000300000000000100002d780000000000000000000000000000000000000000a8bb",
                ],
                "smart start sent two frames"
            );
        }
    }

    #[test]
    fn selecting_a_smart_course_downloads_it_straight_away_like_the_washer() {
        // No state frame first: the appliance is off or unknown, and the download goes anyway.
        let s5 = Appliance::new("select-downloads");
        let rig = s5.make();
        rig.dev.set_property("smart_course_select", "Golf Wear Dry");
        assert_eq!(
            rig.outbox(),
            [GOLF_WEAR_DRY_DOWNLOAD],
            "select sent only the download frame"
        );
        assert_eq!(
            rig.prop("smart_course_select").as_deref(),
            Some("Golf Wear Dry")
        );
        assert_eq!(rig.prop("smart_course").as_deref(), Some("Golf Wear Dry"));
        assert_eq!(
            rig.prop("course_select").as_deref(),
            Some("Downloaded Course")
        );
    }

    #[test]
    fn a_polled_idle_frame_does_not_wipe_an_armed_smart_course() {
        let s5 = Appliance::new("idle-keeps-armed");
        let rig = s5.make();
        rig.dev.set_property("smart_course_select", "Golf Wear Dry");
        // Off, and smart id 0: not running.
        rig.emit(POWEROFF);
        assert_eq!(rig.prop("power").as_deref(), Some("OFF"));
        assert_eq!(rig.prop("smart_course").as_deref(), Some("Golf Wear Dry"));
        assert_eq!(
            rig.prop("smart_course_select").as_deref(),
            Some("Golf Wear Dry")
        );
    }

    #[test]
    fn repeated_identical_status_frames_do_not_rewrite_the_memory_file() {
        let s5 = Appliance::new("memory-unchanged");
        let rig = s5.make();
        rig.dev.set_property("smart_course_select", "Golf Wear Dry");
        let saved = std::fs::read_to_string(&s5.file).unwrap();
        assert!(saved.contains(r#""smart":121"#), "{saved}");
        // Any further write would replace this marker.
        std::fs::write(&s5.file, "marker").unwrap();
        rig.dev.set_property("smart_course_select", "Golf Wear Dry");
        rig.dev.set_property("smart_course_select", "Golf Wear Dry");
        assert_eq!(std::fs::read_to_string(&s5.file).unwrap(), "marker");
    }

    #[test]
    fn bridge_tunnelled_app_download_updates_ha_the_same_as_a_local_select() {
        let s5 = Appliance::new("bridge-download");
        let rig = s5.make();
        // How the bridge relays LG's app to the appliance.
        rig.thinq.send_packet(&hex_decode(GOLF_WEAR_DRY_DOWNLOAD));
        assert_eq!(
            rig.prop("smart_course_select").as_deref(),
            Some("Golf Wear Dry")
        );
        assert_eq!(
            rig.prop("course_select").as_deref(),
            Some("Downloaded Course")
        );
        assert_eq!(rig.prop("smart_course").as_deref(), Some("Golf Wear Dry"));
    }

    #[test]
    fn bridge_tunnelled_downloads_are_ignored_when_the_body_matches_no_known_course() {
        let s5 = Appliance::new("bridge-other");
        let rig = s5.make();
        let before = rig.prop("smart_course_select");
        // Power off, not a download.
        rig.thinq.send_packet(&hex_decode("aa08f024010000fcbb"));
        assert_eq!(rig.prop("smart_course_select"), before);
    }

    #[test]
    fn course_select_downloaded_course_routes_start_course_to_the_pre_selected_smart_course() {
        let s5 = Appliance::new("downloaded-course-route");
        let rig = s5.make();
        rig.dev.set_property("smart_course_select", "Golf Wear Dry");
        rig.dev.set_property("reserve_hours", "3");
        // Picking the placeholder again keeps the smart selection.
        rig.dev.set_property("course_select", "Downloaded Course");
        rig.thinq.reset_recorder();
        rig.dev.set_property("start_course", "");
        assert_eq!(
            rig.outbox(),
            [GOLF_WEAR_DRY_DOWNLOAD, GOLF_WEAR_DRY_START_IN_3H],
            "smart start sent two frames"
        );
    }

    #[test]
    fn a_start_shows_up_straight_away_instead_of_waiting_for_the_appliance() {
        let s5 = Appliance::new("start-echo");
        let rig = s5.make();
        rig.emit(REMOTE_ON);
        rig.dev.set_property("course_select", "Styling Standard");
        rig.dev.set_property("start_course", "");
        assert_eq!(rig.prop("course").as_deref(), Some("Styling Standard"));
        // ...and the appliance still has the last word.
        rig.emit(REMOTE_OFF);
        assert_eq!(rig.prop("course").as_deref(), Some("None"));
    }

    #[test]
    fn reserve_hours_reject_anything_outside_0_or_3_to_19() {
        let s5 = Appliance::new("reserve-hours");
        let rig = s5.make();
        rig.dev.set_property("reserve_hours", "19");
        assert_eq!(rig.value("reserve_hours"), Some(PropertyValue::Int(19)));
        rig.dev.set_property("reserve_hours", "20");
        rig.dev.set_property("reserve_hours", "-1");
        assert_eq!(rig.value("reserve_hours"), Some(PropertyValue::Int(19)));
        // LG declares 0 (start now) or 3..19.
        rig.dev.set_property("reserve_hours", "1");
        assert_eq!(rig.value("reserve_hours"), Some(PropertyValue::Int(19)));
        rig.dev.set_property("reserve_hours", "2");
        assert_eq!(rig.value("reserve_hours"), Some(PropertyValue::Int(19)));
        rig.dev.set_property("reserve_hours", "3");
        assert_eq!(rig.value("reserve_hours"), Some(PropertyValue::Int(3)));
        rig.dev.set_property("reserve_hours", "0");
        assert_eq!(rig.value("reserve_hours"), Some(PropertyValue::Int(0)));
    }

    #[test]
    fn a_reconnect_restores_the_armed_smart_course_instead_of_resetting_to_pants() {
        let s5 = Appliance::new("reconnect");
        let first = s5.make();
        first
            .dev
            .set_property("smart_course_select", "Golf Wear Dry");
        // A fresh handler for the same device, as a reconnect builds.
        let rig = s5.make();
        assert_eq!(
            rig.prop("smart_course_select").as_deref(),
            Some("Golf Wear Dry")
        );
        assert_eq!(rig.prop("smart_course").as_deref(), Some("Golf Wear Dry"));
        assert_eq!(
            rig.prop("course_select").as_deref(),
            Some("Downloaded Course")
        );
    }

    #[test]
    fn a_fresh_idle_frame_after_restart_leaves_smart_course_untouched() {
        // Smart id 0 while off or idle means not running, not no course.
        let s5 = Appliance::new("restart-idle");
        s5.restart();
        let rig = s5.make();
        rig.emit(POWEROFF);
        assert_eq!(rig.prop("smart_course"), None);
        assert_eq!(rig.prop("power").as_deref(), Some("OFF"));
    }

    #[test]
    fn a_restart_restores_the_armed_smart_course_from_disk_memory() {
        let s5 = Appliance::new("restart-from-disk");
        let first = s5.make();
        first
            .dev
            .set_property("smart_course_select", "Golf Wear Dry");
        s5.restart();
        let rig = s5.build();
        assert_eq!(rig.prop("smart_course").as_deref(), Some("Golf Wear Dry"));
        assert_eq!(
            rig.prop("smart_course_select").as_deref(),
            Some("Golf Wear Dry")
        );
        assert_eq!(
            rig.prop("course_select").as_deref(),
            Some("Downloaded Course")
        );
    }

    #[test]
    fn a_torn_memory_file_boots_with_defaults_instead_of_throwing() {
        // What a crash mid-write left behind before saves were atomic.
        let s5 = Appliance::new("torn-memory");
        std::fs::write(&s5.file, format!(r#"{{"{}": {{"smart": 6"#, s5.id)).unwrap();
        s5.restart();
        let rig = s5.build();
        assert_eq!(
            rig.prop("course_select").as_deref(),
            Some("Styling Standard")
        );
        assert_eq!(rig.prop("smart_course"), None);
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory(MODEL_ID).is_some());
    }
}
