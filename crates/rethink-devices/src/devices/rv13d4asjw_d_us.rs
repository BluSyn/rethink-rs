//! RV13D4ASJW_D_US — LG DLEX4000W electric dryer (AABB, ThinQ2 deviceType 202).
//!
//! Class `0x30`, 29-byte status blocks: a leading byte, then the 28-byte record from its `0x1b`
//! marker. `0xEB` carries one block, `0xEC` the previous block then the current one. The record
//! layout and course codes match RV13B6ES_D_US_WIFI (Wrinkle Care in rec[15] bit 0x10), but the
//! course and option sets differ: a plain On/Off Signal, TurboSteam, a Time Dry selector, an AI
//! lamp and a load-item count. `auto_ack` is on.
//!
//! Every write needs Remote Start armed at the panel; unarmed, the dryer ignores it. Pause and
//! power off are the washers' `0xF024` actions. Start and resume are one `0xF026` packet built from
//! the last record, told apart by the new-cycle bit 0x40 at offset 10; the app edits a paused cycle
//! by resending it with that bit clear and one field changed. `start_course` / `start_json` compose
//! a start for a dial or smart course the way the app commits one, and `specialty` sends a captured
//! `0xF025` SmartCourse download into the Downloaded slot.
//!
//! Not decoded: rec[15] bit 0x40 (drum light and panel activity were not separable), error codes,
//! and a door sensor (opening the door produces no distinct byte).
//!
//! Port of upstream PR #200.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const STATUS_LENGTH: usize = 29;
const RECORD_MARKER: u8 = 0x1b;
const STATUS_REQUEST: &[u8] = &[0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00];

// Offsets relative to the record's 0x1b marker.
const PHASE_OFFSET: usize = 1;
// [hour][minute]: the countdown while running, the estimate before that.
const TIME_HOUR_OFFSET: usize = 2;
const TIME_MIN_OFFSET: usize = 3;
// [hour][minute]: the initial estimate, which stays fixed while the countdown runs.
const INITIAL_TIME_HOUR_OFFSET: usize = 4;
const INITIAL_TIME_MIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const DRY_LEVEL_OFFSET: usize = 8;
const TEMP_OFFSET: usize = 9;
// Only meaningful on Time Dry.
const TIME_DRY_OFFSET: usize = 10;
const SIGNAL_OFFSET: usize = 11;
// Signed minutes added to or taken off the course default.
const MORE_LESS_TIME_OFFSET: usize = 12;
const FLAGS_OFFSET: usize = 15;
const FLAG_CONTROL_LOCK: u8 = 0x01;
const FLAG_REDUCE_STATIC: u8 = 0x02;
const FLAG_DAMP_DRY_SIGNAL: u8 = 0x08;
const FLAG_WRINKLE_CARE: u8 = 0x10;
// Drum light / panel activity, not a cycle setting: stripped from every command.
const FLAG_PANEL_ACTIVE: u8 = 0x40;
const OPT2_OFFSET: usize = 16;
const OPT2_REMOTE_START: u8 = 0x01;
const OPT2_ENERGY_SAVER: u8 = 0x02;
const OPT2_TURBO_STEAM: u8 = 0x04;
// Energy Saver switched on by the dryer itself (Normal at the Normal dry level).
const OPT2_ENERGY_SAVER_AUTO: u8 = 0x80;
const OPT3_OFFSET: usize = 17;
const OPT3_AI: u8 = 0x04;
const PREV_PHASE_OFFSET: usize = 20;
// A load-item step; LOAD_ITEMS maps it to the count the display shows.
const LOAD_ITEM_OFFSET: usize = 23;
// The SmartCourse code held in the Downloaded slot.
const DOWNLOAD_COURSE_OFFSET: usize = 24;

const PHASE_OFF: u8 = 0x00;
const PHASE_PAUSED: u8 = 0x03;
const COURSE_TIME_DRY: u8 = 0x12;

const CMD_START: [u8; 2] = [0xf0, 0x26];
const CMD_PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];
const CMD_POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
const START_OPTS_BASE: u8 = 0x01;
const START_OPTS_NEW_CYCLE: u8 = 0x40;
const START_OPTS_ENERGY_SAVER: u8 = 0x02;
// Reduce Static engages this load-item step, on the panel and in the app's start.
const REDUCE_STATIC_LOAD_ITEM: u8 = 5;
// `f0 25 03 15`: WMDownload with a 21-byte body.
const DOWNLOAD_HEADER_LEN: usize = 4;

// Unknown phases read "Running".
const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Initial"),
    (0x03, "Pause"),
    (0x32, "Drying"),
    (0x33, "Cooling"),
    (0x04, "End"),
    (0x38, "Wrinkle Care"),
];
const COURSE: &[(u8, &str)] = &[
    (0x01, "Heavy Duty"),
    (0x02, "Towels"),
    (0x03, "Normal"),
    (0x04, "Perm. Press"),
    (0x05, "Delicates"),
    (0x07, "Bedding"),
    // Not dial positions: the base courses the smart courses run on.
    (0x06, "Ultra Delicate"),
    (0x09, "Small Load"),
    (0x0a, "Khaki/Jean"),
    (0x0b, "Sportswear"),
    (0x0c, "Kidswear"),
    (0x0d, "Low Temp Dry"),
    (0x0e, "Jumbo Dry"),
    (0x13, "Rack Dry"),
    (0x08, "Antibacterial"),
    (0x10, "Speed Dry"),
    (0x11, "Air Dry"),
    (0x12, "Time Dry"),
    (0x15, "Steam Fresh"),
    (0x16, "Steam Sanitary"),
    // The Downloaded dial position.
    (0x1a, "Super Dry"),
];
const DRY_LEVEL: &[(u8, &str)] = &[
    (1, "Damp"),
    (2, "Less"),
    (3, "Normal"),
    (4, "More"),
    (5, "Very"),
];
const TEMP: &[(u8, &str)] = &[
    (1, "Ultra Low"),
    (2, "Low"),
    (3, "Medium"),
    (4, "Mid High"),
    (5, "High"),
];
const SIGNAL: &[(u8, &str)] = &[(0x00, "Off"), (0x04, "On")];
// Selector 1..5 = 20..60 minutes.
const TIME_DRY: &[(u8, &str)] = &[
    (1, "20 min"),
    (2, "30 min"),
    (3, "40 min"),
    (4, "50 min"),
    (5, "60 min"),
];
const LOAD_ITEMS: &[(u8, i64)] = &[(1, 7), (2, 9), (3, 11), (4, 14), (5, 16), (6, 18)];
const SMARTCOURSE: &[(u8, &str)] = &[
    (0x64, "Super Dry"),
    (0x65, "Denim"),
    (0x66, "Kids' Clothes"),
    (0x67, "Small Load"),
    (0x68, "Ultra Delicate"),
    (0x6b, "Gym Clothes"),
    (0x6c, "Blankets"),
    (0x6d, "Blanket Refresh"),
    (0x6e, "Rainy Days"),
    (0x71, "Socks"),
    (0x72, "Overnight Dry"),
    (0x73, "Bedding / Curtains"),
    (0xca, "Wrinkle Prevention"),
    (0xcc, "Static Reduce"),
    (0xcd, "Half Load Dry"),
    (0xce, "Full Load Dry"),
    (0xd0, "Rack Dry"),
];

// The app's captured download of each SmartCourse. The body has the start packet's layout:
// course 0, temp 3, Time Dry 6, flags 7, then the SmartCourse code at 11 after the second copy of
// the course, dry level 15, load item 16, More/Less 17. It stores the cycle in the Downloaded slot
// and selects it. The codes are not contiguous, so every entry is the exact frame the app sent.
const SPECIALTY_DOWNLOAD: &[(&str, &str)] = &[
    ("Super Dry", "f02503151a0000050000000001001a64000000050000000000"),
    ("Denim", "f02503150a0000030000000001000a65000000030000000000"),
    ("Kids' Clothes", "f02503150c0000050000000001000c66000000030000000000"),
    ("Small Load", "f0250315090000050000000001000967000000030000000000"),
    ("Ultra Delicate", "f0250315060000010000000001000668000000030000000000"),
    ("Gym Clothes", "f02503150b0000050000000001000b6b000000030000000000"),
    ("Blankets", "f02503150e0000030000000001000e6c000000050000000000"),
    ("Blanket Refresh", "f025031512000001000001000100126d000000000000000000"),
    ("Rainy Days", "f02503150d0000020000000001000d6e000000030000000000"),
    ("Socks", "f0250315120000050000020001001271000000000005000000"),
    ("Overnight Dry", "f02503150d0000020000001001000d72000000030000000000"),
    ("Bedding / Curtains", "f0250315070000030000000001000773000000050000000000"),
    ("Wrinkle Prevention", "f02503150300000300000010010003ca000000030000000000"),
    ("Static Reduce", "f02503150300000300000002010003cc000000030500000000"),
    ("Half Load Dry", "f02503150300000300000000010003cd000000040000000000"),
    ("Full Load Dry", "f02503150100000500000000010001ce000000050000000000"),
    ("Rack Dry", "f02503151300000000000000010013d0000000000000000000"),
];

#[derive(Clone, Copy)]
struct CourseDefaults {
    temp: u8,
    dry_level: u8,
    energy_saver: bool,
    time_dry: u8,
}

const fn defaults(temp: u8, dry_level: u8) -> CourseDefaults {
    CourseDefaults {
        temp,
        dry_level,
        energy_saver: false,
        time_dry: 0,
    }
}

// The model JSON's defaults for each dial course `start_course` offers.
const COURSE_DEFAULTS: &[(&str, CourseDefaults)] = &[
    ("Heavy Duty", defaults(5, 3)),
    ("Towels", defaults(4, 3)),
    (
        "Normal",
        CourseDefaults {
            energy_saver: true,
            ..defaults(4, 3)
        },
    ),
    ("Delicates", defaults(2, 3)),
    ("Perm. Press", defaults(3, 3)),
    ("Bedding", defaults(3, 3)),
    ("Antibacterial", defaults(5, 5)),
    ("Speed Dry", defaults(5, 0)),
    ("Air Dry", defaults(0, 0)),
    ("Steam Fresh", defaults(4, 0)),
    ("Steam Sanitary", defaults(5, 0)),
    (
        "Time Dry",
        CourseDefaults {
            time_dry: 3,
            ..defaults(5, 0)
        },
    ),
];

/// A smart course runs as its base course with the fields of its download.
struct SmartCourse {
    course: u8,
    temp: u8,
    time_dry: u8,
    flags: u8,
    code: u8,
    dry_level: u8,
    load_item: u8,
    more_less: i8,
}

fn smart_course(name: &str) -> Option<SmartCourse> {
    let frame = hex_decode(lookup(SPECIALTY_DOWNLOAD, name)?);
    let body = &frame[DOWNLOAD_HEADER_LEN..];
    Some(SmartCourse {
        course: body[0],
        temp: body[3],
        time_dry: body[6],
        flags: body[7],
        code: body[11],
        dry_level: body[15],
        load_item: body[16],
        more_less: body[17] as i8,
    })
}

/// The start/resume packet from the appliance's own record, with the given flags and verb/options
/// bytes. The app sends this shape for a start and for a resume-apply.
fn start_packet_from_record(rec: &[u8], flags: u8, opts: u8) -> Vec<u8> {
    let course = rec[COURSE_OFFSET];
    let time_dry = if course == COURSE_TIME_DRY {
        rec[TIME_DRY_OFFSET]
    } else {
        0
    };
    vec![
        CMD_START[0],
        CMD_START[1],
        course,
        0,
        0,
        rec[TEMP_OFFSET],
        0,
        0,
        time_dry,
        flags,
        opts,
        0,
        course,
        0,
        0,
        0,
        0,
        rec[DRY_LEVEL_OFFSET],
        rec[LOAD_ITEM_OFFSET],
        rec[MORE_LESS_TIME_OFFSET],
        0,
        0,
        0,
    ]
}

/// The start the app commits for a composed course: the course defaults, then the request's
/// overrides. `req` is `{"course": ...}` plus any of `temp` and `dry_level` (labels), `minutes`
/// (Time Dry only), `more_less_time`, `wrinkle_care`, `reduce_static` and `energy_saver`, e.g.
/// `{"course":"Time Dry","temp":"Medium","minutes":38,"wrinkle_care":true}`. `None` for an
/// unknown course, a value that does not map or a value of the wrong type, rather than a guess.
fn build_course_start(req: &Value) -> Option<Vec<u8>> {
    let name = req.get("course")?.as_str()?;
    let smart = smart_course(name);
    let (course, def) = match &smart {
        Some(s) => (
            s.course,
            CourseDefaults {
                temp: s.temp,
                dry_level: s.dry_level,
                energy_saver: false,
                time_dry: s.time_dry,
            },
        ),
        None => (unmap(COURSE, name)?, lookup(COURSE_DEFAULTS, name)?),
    };
    // temp, dry_level and minutes apply if present at all; elsewhere null is the default.
    let temp = match req.get("temp") {
        Some(t) => unmap(TEMP, t.as_str()?)?,
        None => def.temp,
    };
    let dry_level = match req.get("dry_level") {
        Some(d) => unmap(DRY_LEVEL, d.as_str()?)?,
        None => def.dry_level,
    };
    let mut time_dry = def.time_dry;
    let mut more_less = js_round(match non_null(req, "more_less_time") {
        Some(m) => m.as_f64()?,
        None => smart.as_ref().map_or(0.0, |s| f64::from(s.more_less)),
    });
    if let Some(minutes) = req.get("minutes") {
        if course != COURSE_TIME_DRY {
            return None;
        }
        // The nearest selector step, the remainder as the trim: 38 -> 40 with -2.
        let m = js_round(minutes.as_f64()?);
        let step = (js_round(m / 10.0) - 1.0).clamp(1.0, 5.0);
        time_dry = step as u8;
        more_less = m - (step + 1.0) * 10.0;
    }
    let option = |key: &str, default: bool| match non_null(req, key) {
        Some(v) => v.as_bool(),
        None => Some(default),
    };
    let reduce_static = option("reduce_static", false)?;
    let mut flags = smart.as_ref().map_or(0, |s| s.flags);
    if option("wrinkle_care", false)? {
        flags |= FLAG_WRINKLE_CARE;
    }
    if reduce_static {
        flags |= FLAG_REDUCE_STATIC;
    }
    let load_item = if reduce_static {
        REDUCE_STATIC_LOAD_ITEM
    } else {
        smart.as_ref().map_or(0, |s| s.load_item)
    };
    let energy_saver = option("energy_saver", def.energy_saver)?;
    let opts = START_OPTS_BASE
        | START_OPTS_NEW_CYCLE
        | if energy_saver {
            START_OPTS_ENERGY_SAVER
        } else {
            0
        };
    Some(vec![
        CMD_START[0],
        CMD_START[1],
        course,
        0,
        0,
        temp,
        0,
        0,
        time_dry,
        flags,
        opts,
        0,
        course,
        smart.map_or(0, |s| s.code),
        0,
        0,
        0,
        dry_level,
        load_item,
        js_byte(more_less),
        0,
        0,
        0,
    ])
}

fn non_null<'a>(req: &'a Value, key: &str) -> Option<&'a Value> {
    req.get(key).filter(|v| !v.is_null())
}

fn energy_saver_opt(rec: &[u8]) -> u8 {
    if rec[OPT2_OFFSET] & OPT2_ENERGY_SAVER != 0 {
        START_OPTS_ENERGY_SAVER
    } else {
        0
    }
}

fn time_dry_label(rec: &[u8]) -> &'static str {
    if rec[COURSE_OFFSET] == COURSE_TIME_DRY {
        named(TIME_DRY, rec[TIME_DRY_OFFSET]).unwrap_or("None")
    } else {
        "None"
    }
}

/// JavaScript's `Math.round`: halves round toward +infinity.
fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// The low byte of `n`, as JavaScript's `n & 0xff` takes it.
fn js_byte(n: f64) -> u8 {
    (n.trunc() % 256.0) as i64 as u8
}

fn named<T: Copy>(table: &[(u8, T)], code: u8) -> Option<T> {
    table.iter().find(|(c, _)| *c == code).map(|(_, v)| *v)
}

fn unmap(table: &[(u8, &str)], label: &str) -> Option<u8> {
    table.iter().find(|(_, l)| *l == label).map(|(c, _)| *c)
}

fn lookup<T: Copy>(table: &[(&str, T)], name: &str) -> Option<T> {
    table.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

fn labels(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, label)| *label).collect()
}

fn names<T>(table: &[(&'static str, T)]) -> Vec<&'static str> {
    table.iter().map(|(name, _)| *name).collect()
}

fn components() -> HashMap<String, Value> {
    let dry_level_options = [vec!["None"], labels(DRY_LEVEL)].concat();
    let temp_options = [vec!["None"], labels(TEMP)].concat();
    let time_dry_options = [vec!["None"], labels(TIME_DRY)].concat();
    let start_course_options = [
        vec!["unknown"],
        names(COURSE_DEFAULTS),
        names(SPECIALTY_DOWNLOAD),
    ]
    .concat();
    let specialty_options = [vec!["unknown"], names(SPECIALTY_DOWNLOAD)].concat();
    [
        ("power".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:tumble-dryer","device_class":"running"})),
        ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine"})),
        ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
        ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time estimate","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("dry_level".into(), json!({"platform":"select","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","command_topic":"$this/dry_level/set","options":dry_level_options,"name":"Dry level","icon":"mdi:water-percent"})),
        ("temp".into(), json!({"platform":"select","unique_id":"$deviceid-temp","state_topic":"$this/temp","command_topic":"$this/temp/set","options":temp_options,"name":"Temperature","icon":"mdi:thermometer"})),
        ("more_less_time".into(), json!({"platform":"number","unique_id":"$deviceid-more_less_time","state_topic":"$this/more_less_time","command_topic":"$this/more_less_time/set","min":-30,"max":30,"step":5,"name":"More/Less time","icon":"mdi:plus-minus-variant","device_class":"duration","unit_of_measurement":"min"})),
        ("remote_start".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        ("load_item".into(), json!({"platform":"sensor","unique_id":"$deviceid-load_item","state_topic":"$this/load_item","name":"Load items","icon":"mdi:tshirt-crew-outline","state_class":"measurement"})),
        ("signal".into(), json!({"platform":"sensor","unique_id":"$deviceid-signal","state_topic":"$this/signal","name":"Signal","icon":"mdi:bell-outline","entity_category":"diagnostic"})),
        ("time_dry".into(), json!({"platform":"select","unique_id":"$deviceid-time_dry","state_topic":"$this/time_dry","command_topic":"$this/time_dry/set","options":time_dry_options,"name":"Time Dry duration","icon":"mdi:timer-sand"})),
        ("ai".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-ai","state_topic":"$this/ai","name":"AI sensing","icon":"mdi:brain","entity_category":"diagnostic"})),
        ("energy_saver_auto".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver_auto","state_topic":"$this/energy_saver_auto","name":"Energy Saver automatic","icon":"mdi:leaf","entity_category":"diagnostic"})),
        ("previous_status".into(), json!({"platform":"sensor","unique_id":"$deviceid-previous_status","state_topic":"$this/previous_status","name":"Previous status","icon":"mdi:history","entity_category":"diagnostic"})),
        ("start".into(), json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start / Resume","icon":"mdi:play"})),
        ("pause".into(), json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        // A select rather than a button: remote power-off drops the Wi-Fi module until the panel's
        // Power button brings it back.
        ("power_off".into(), json!({"platform":"select","unique_id":"$deviceid-power_off","command_topic":"$this/power_off/set","state_topic":"$this/power_off","options":["unknown","Power Off"],"name":"Power Off","icon":"mdi:power"})),
        ("start_course".into(), json!({"platform":"select","unique_id":"$deviceid-start_course","command_topic":"$this/start_course/set","state_topic":"$this/start_course","options":start_course_options,"name":"Start course (defaults)","icon":"mdi:play-circle-outline"})),
        ("downloaded_course".into(), json!({"platform":"sensor","unique_id":"$deviceid-downloaded_course","state_topic":"$this/downloaded_course","name":"Downloaded course","icon":"mdi:download-circle-outline"})),
        ("specialty".into(), json!({"platform":"select","unique_id":"$deviceid-specialty","command_topic":"$this/specialty/set","state_topic":"$this/specialty","options":specialty_options,"name":"Download specialty cycle","icon":"mdi:cloud-download-outline"})),
        ("reduce_static".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-reduce_static","state_topic":"$this/reduce_static","name":"Reduce static","icon":"mdi:flash-off-outline"})),
        ("damp_dry_signal".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","name":"Damp Dry Signal","icon":"mdi:water-alert-outline"})),
        ("control_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-control_lock","state_topic":"$this/control_lock","name":"Control Lock","icon":"mdi:lock","entity_category":"diagnostic"})),
        ("energy_saver".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","name":"Energy Saver","icon":"mdi:leaf"})),
        ("turbo_steam".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_steam","state_topic":"$this/turbo_steam","name":"TurboSteam","icon":"mdi:kettle-steam"})),
        ("wrinkle_care".into(), json!({"platform":"switch","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","command_topic":"$this/wrinkle_care/set","name":"Wrinkle Care","icon":"mdi:iron-outline"})),
    ]
    .into_iter()
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    /// The current record from its `0x1b` marker. Every start is built from it.
    last_record: Mutex<Option<Vec<u8>>>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            last_record: Mutex::new(None),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG DLEX4000W Dryer"})));
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
        if rec[0] != RECORD_MARKER {
            return;
        }
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;

        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", named(STATUS, phase).unwrap_or("Running").into());
        self.publish_label("course", COURSE, rec[COURSE_OFFSET]);
        let remaining = if is_off {
            0
        } else {
            rec[TIME_HOUR_OFFSET] as i64 * 60 + rec[TIME_MIN_OFFSET] as i64
        };
        self.core
            .publish_property("remaining_time", PropertyValue::Int(remaining));
        let initial = if is_off {
            0
        } else {
            rec[INITIAL_TIME_HOUR_OFFSET] as i64 * 60 + rec[INITIAL_TIME_MIN_OFFSET] as i64
        };
        self.core
            .publish_property("initial_time", PropertyValue::Int(initial));
        self.publish_label("dry_level", DRY_LEVEL, rec[DRY_LEVEL_OFFSET]);
        self.publish_label("temp", TEMP, rec[TEMP_OFFSET]);
        let load_items = named(LOAD_ITEMS, rec[LOAD_ITEM_OFFSET]).unwrap_or(0);
        self.core
            .publish_property("load_item", PropertyValue::Int(load_items));
        self.publish_label("signal", SIGNAL, rec[SIGNAL_OFFSET]);
        let more_less = if is_off {
            0
        } else {
            rec[MORE_LESS_TIME_OFFSET] as i8 as i64
        };
        self.core
            .publish_property("more_less_time", PropertyValue::Int(more_less));

        let flags = rec[FLAGS_OFFSET];
        self.core
            .publish_on_off("control_lock", flags & FLAG_CONTROL_LOCK != 0);
        self.core
            .publish_on_off("reduce_static", flags & FLAG_REDUCE_STATIC != 0);
        self.core
            .publish_on_off("damp_dry_signal", flags & FLAG_DAMP_DRY_SIGNAL != 0);
        self.core
            .publish_on_off("wrinkle_care", flags & FLAG_WRINKLE_CARE != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core
            .publish_on_off("energy_saver", opt2 & OPT2_ENERGY_SAVER != 0);
        self.core
            .publish_on_off("turbo_steam", opt2 & OPT2_TURBO_STEAM != 0);
        self.core
            .publish_on_off("remote_start", opt2 & OPT2_REMOTE_START != 0);
        self.core
            .publish_on_off("energy_saver_auto", opt2 & OPT2_ENERGY_SAVER_AUTO != 0);
        self.core
            .publish_on_off("ai", rec[OPT3_OFFSET] & OPT3_AI != 0);
        self.core
            .publish_property("time_dry", time_dry_label(rec).into());
        self.publish_label("previous_status", STATUS, rec[PREV_PHASE_OFFSET]);
        self.publish_label("downloaded_course", SMARTCOURSE, rec[DOWNLOAD_COURSE_OFFSET]);
        *self.last_record.lock() = Some(rec.to_vec());
    }

    fn publish_label(&self, prop: &str, table: &[(u8, &'static str)], code: u8) {
        self.core
            .publish_property(prop, named(table, code).unwrap_or("None").into());
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "pause" => self.core.send(CMD_PAUSE),
            "power_off" => {
                if value == "Power Off" {
                    self.core.send(CMD_POWER_OFF);
                    self.core.publish_property("power_off", "unknown".into());
                }
            }
            "wrinkle_care" => self.set_wrinkle_care(value == "ON"),
            "dry_level" | "temp" | "time_dry" | "more_less_time" => self.edit_setting(prop, value),
            "specialty" => {
                if let Some(frame) = lookup(SPECIALTY_DOWNLOAD, value) {
                    self.core.send(&hex_decode(frame));
                    self.core.publish_property("specialty", "unknown".into());
                }
            }
            "start_course" | "start_json" => {
                let req = if prop == "start_course" {
                    Some(json!({ "course": value }))
                } else {
                    serde_json::from_str(value).ok()
                };
                if let Some(packet) = req.as_ref().and_then(build_course_start) {
                    self.core.send(&packet);
                }
                if prop == "start_course" {
                    self.core.publish_property("start_course", "unknown".into());
                }
            }
            "start" => self.start_as_dialed(),
            _ => {}
        }
    }

    /// Wrinkle Care is editable only while paused, as the app's resume-apply. Otherwise the switch
    /// snaps back to the panel's state.
    fn set_wrinkle_care(&self, on: bool) {
        let Some(rec) = self.last_record.lock().clone() else {
            return;
        };
        if rec[PHASE_OFFSET] != PHASE_PAUSED {
            self.core
                .publish_on_off("wrinkle_care", rec[FLAGS_OFFSET] & FLAG_WRINKLE_CARE != 0);
            return;
        }
        let flags = (rec[FLAGS_OFFSET] & !FLAG_PANEL_ACTIVE & !FLAG_WRINKLE_CARE)
            | if on { FLAG_WRINKLE_CARE } else { 0 };
        let opts = START_OPTS_BASE | energy_saver_opt(&rec);
        self.core.send(&start_packet_from_record(&rec, flags, opts));
    }

    /// The same for dry level, temperature, the Time Dry selector and More/Less: a resume-apply
    /// with the one field changed.
    fn edit_setting(&self, prop: &str, value: &str) {
        let Some(mut rec) = self.last_record.lock().clone() else {
            return;
        };
        if rec[PHASE_OFFSET] != PHASE_PAUSED {
            self.publish_label("dry_level", DRY_LEVEL, rec[DRY_LEVEL_OFFSET]);
            self.publish_label("temp", TEMP, rec[TEMP_OFFSET]);
            self.core
                .publish_property("time_dry", time_dry_label(&rec).into());
            self.core.publish_property(
                "more_less_time",
                PropertyValue::Int(rec[MORE_LESS_TIME_OFFSET] as i8 as i64),
            );
            return;
        }
        match prop {
            "dry_level" => {
                let Some(level) = unmap(DRY_LEVEL, value) else {
                    return;
                };
                rec[DRY_LEVEL_OFFSET] = level;
            }
            "temp" => {
                let Some(temp) = unmap(TEMP, value) else {
                    return;
                };
                rec[TEMP_OFFSET] = temp;
            }
            "time_dry" => {
                let Some(step) = unmap(TIME_DRY, value) else {
                    return;
                };
                if rec[COURSE_OFFSET] != COURSE_TIME_DRY {
                    return;
                }
                rec[TIME_DRY_OFFSET] = step;
            }
            _ => {
                let Some(minutes) = crate::js::number(value)
                    .map(js_round)
                    .filter(|m| m.is_finite())
                else {
                    return;
                };
                rec[MORE_LESS_TIME_OFFSET] = minutes.clamp(-30.0, 30.0) as i8 as u8;
            }
        }
        let flags = rec[FLAGS_OFFSET] & !FLAG_PANEL_ACTIVE;
        let opts = START_OPTS_BASE | energy_saver_opt(&rec);
        self.core.send(&start_packet_from_record(&rec, flags, opts));
    }

    /// Starts what is set at the panel, or resumes a paused cycle.
    fn start_as_dialed(&self) {
        let Some(rec) = self.last_record.lock().clone() else {
            return;
        };
        let new_cycle = if rec[PHASE_OFFSET] == PHASE_PAUSED {
            0
        } else {
            START_OPTS_NEW_CYCLE
        };
        let flags = rec[FLAGS_OFFSET] & !FLAG_PANEL_ACTIVE;
        let opts = START_OPTS_BASE | new_cycle | energy_saver_opt(&rec);
        self.core.send(&start_packet_from_record(&rec, flags, opts));
    }
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
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "RV13D4ASJW_D_US";

    // Real frames captured from the LG DLEX4000W, each cross-checked against the LG cloud's
    // decoded state, and the exact packets the LG app sent.

    // 0xEB single record right after power-on, nothing selected: Initial, 1 min.
    const EB_INITIAL_POWER_ON: &str =
        "aa2330eb001b010001000100000000000400000040a8000000000000006400000003bb";
    // Remote Start pressed at the panel: rec[16] bit 0x01.
    const REMOTE_START_ON: &str =
        "aa4030ec001b010001000100000000000400000040a80000000000000064000000001b010001000100000000000400000040a90000000000000064000000b6bb";
    // Dial to Heavy Duty: Normal dry level, High, 54 min.
    const HEAVY_DUTY: &str =
        "aa4030ec001b010001000100000000000400000040a80000000000000064000000001b010036003601000305000400000040a8000000000000006400000000bb";
    // Antibacterial: 1 h 10 min, Very dry.
    const ANTIBACTERIAL: &str =
        "aa4030ec001b010037003707000303000400000040a80000000000000064000000001b01010a010a08000505000400000040a80000000000000064000000d4bb";
    // Steam Fresh: no dry level, Mid High, load-item code 2 in rec[23].
    const STEAM_FRESH_LOAD_ITEM: &str =
        "aa4030ec001b01010a010a08000505000400000040a80000000000000064000000001b01000a000a15000004000400000040a8000000000000026400000060bb";
    // Normal at the Normal dry level turns Energy Saver on by itself (rec[16] 0x02).
    const NORMAL_ENERGY_SAVER: &str =
        "aa4030ec001b010036003601000305000400000040a80000000000000064000000001b010039003903000304000400000040aa000000000000006400000084bb";
    // Air Dry: no heat (temp 0), 30 min.
    const AIR_DRY: &str =
        "aa4030ec001b010019001910000005000400000040a80000000000000064000000001b01001e001e11000000000400000040a8000000000000006400000027bb";
    // The Downloaded dial position with Super Dry loaded (rec[6] 0x1a).
    const DOWNLOADED_SUPER_DRY: &str =
        "aa4030ec001b01001e001e11000000000400000040a80000000000000064000000001b01003b003b1a000505000400000040a800000000640000640000007cbb";
    // Reduce Static on (rec[15] 0x02); the load-item code went to 5 in the same frame.
    const REDUCE_STATIC_ON: &str =
        "aa4030ec001b010039003903000304000400000000aa0000000000000064000000001b010037003703000304000400000002aa000000000000056400000008bb";
    // Control Lock on: rec[15] bit 0x01.
    const CHILD_LOCK_ON: &str =
        "aa4030ec001b010039003903000304000400000000aa0000000000000264000000001b010039003903000304000400000001aa00000000000002640000000abb";
    // Dry level stepped to Damp.
    const DRY_LEVEL_DAMP: &str =
        "aa4030ec001b01003b003b03000504000400000000a80000000000000064000000001b01001e001e03000104000400000000a8000000000000006400000071bb";
    // Dry level stepped to Very.
    const DRY_LEVEL_VERY: &str =
        "aa4030ec001b010032003203000404000400000000a80000000000000064000000001b01003b003b03000504000400000000a800000000000000640000001abb";
    // Damp Dry Signal on: rec[15] bit 0x08.
    const DAMP_DRY_SIGNAL_ON: &str =
        "aa4030ec001b010036003601000305000400000000a80000000000000064000000001b010036003601000305000400000008a8000000000000006400000005bb";
    // Wrinkle Care on: rec[15] bit 0x10.
    const WRINKLE_CARE_ON: &str =
        "aa4030ec001b010036003601000305000400000000a80000000000000064000000001b010036003601000305000400000010a800000000000000640000000dbb";
    // Wrinkle Care off again.
    const WRINKLE_CARE_OFF: &str =
        "aa4030ec001b010036003601000305000400000010a80000000000000064000000001b010036003601000305000400000000a800000000000000640000000dbb";
    // Signal pressed: rec[11] 0x04 -> 0x00. Panel-confirmed only; the cloud has no signal field.
    const SIGNAL_OFF: &str =
        "aa4030ec001b010036003601000305000400000000a80000000000000064000000001b010036003601000305000000000000a8000000000000006400000011bb";
    // TurboSteam on: rec[16] bit 0x04.
    const TURBO_STEAM_ON: &str =
        "aa4030ec001b010036003601000305000400000000a80000000000000064000000001b010036003601000305000400000000ac000000000000006400000019bb";
    // Time Dry with More Time pressed three times: rec[12] = +15, 55 min.
    const TIME_DRY_PLUS_15: &str =
        "aa4030ec001b01003200321200000503040a000040a80000000000000064000000001b01003700371200000503040f000040a80000000000000064000000a8bb";
    // Temp stepped to Ultra Low on Time Dry.
    const TEMP_ULTRA_LOW: &str =
        "aa4030ec001b01003700371200000503040f000040a80000000000000064000000001b01003700371200000103040f000040a800000000000000640000005dbb";
    // Time Dry selector stepped to 50 (rec[10] = 4).
    const TIME_DRY_50: &str =
        "aa4030ec001b010028002812000005030400000040280000000000000064000000001b01003200321200000504040000004028000000000000006400000092bb";
    // Time Dry selector stepped to 60: 1 h 00 as [hour][minute].
    const TIME_DRY_60: &str =
        "aa4030ec001b010032003212000005040400000040280000000000000064000000001b0101000100120000050504000000402800000000000000640000002ebb";
    // Time Dry selector stepped to 20.
    const TIME_DRY_20: &str =
        "aa4030ec001b010100010012000005050400000040280000000000000064000000001b01001400141200000501040000004028000000000000006400000069bb";
    // Energy Saver switched off by hand on Normal: rec[16] loses 0x02 and 0x80, and the AI lamp
    // (rec[17] 0x04) lights.
    const NORMAL_ENERGY_SAVER_OFF_AI: &str =
        "aa4030ec001b010039003903000304000400000040aa0000000000000064000000001b01002900290300030400040000004028040000000000006400000069bb";
    // Remote Start armed on that Normal setup: the record the app's Start was issued against.
    const ARMED_NORMAL: &str =
        "aa4030ec001b010029002903000304000400000040280400000000000064000000001b010029002903000304000400000040290400000000000064000000cabb";
    // App Start: Drying, previous phase Initial.
    const DRYING: &str =
        "aa4030ec001b010029002903000304000400000040290400000000000064000000001b3200290029030003040004000000002900001c0100000064000000ffbb";
    // App Pause: Pause, previous phase Drying.
    const PAUSED: &str =
        "aa4030ec001b3200290029030003040004000000002900001c0100000064000000001b03002900290300030400040000004029000039320000006400000046bb";
    // App Resume: Drying, previous phase Pause.
    const RESUMED: &str =
        "aa4030ec001b030029002903000304000400000040290000393200000064000000001b3200290029030003040004000000002900004103000000640000006fbb";
    // App Power Off mid-cycle: course, dry level and temp reset to 0.
    const POWERED_OFF: &str =
        "aa4030ec001b320028002903000304000400000000290000550300000064000000001b00002800280000000000040000004028000068320000006400000039bb";
    // Right after the app downloaded Denim: base course Khaki/Jean, 0x65 in the slot (rec[24]).
    const DENIM_DOWNLOADED: &str =
        "aa4030ec001b01001e001e12000005020400000000290000000000000064000000001b01002300230a00030300040000000029000000006500006500000024bb";
    // The app's start of the downloaded Denim: base course 0x0a with 0x65 at offset 11.
    const DENIM_START_HEX: &str = "aa1bf0260a0000030000000041000a650000000300dd0000002dbb";
    // Heavy Duty with Reduce Static pressed (load-item code 5), Remote Start armed.
    const ARMED_HEAVY_DUTY_REDUCE_STATIC: &str =
        "aa4030ec001b010034003401000305000400000042a80000000000000565000000001b010034003401000305000400000042a9000000000000056500000084bb";
    // The app's start from that record: flags 0x02 and the load-item code 5 at offset 18.
    const HEAVY_DUTY_REDUCE_STATIC_START_HEX: &str =
        "aa1bf02601000005000000024100010000000003050000000078bb";
    // Paused on Towels with Remote Start armed.
    const PAUSED_TOWELS: &str =
        "aa4030ec001b320037003702000304000400000000a900001c0100000065000000001b030037003702000304000400000040a9000034320000006500000013bb";
    // The app's resume-apply from there with Wrinkle Care on: new-cycle bit clear, flags 0x10.
    const RESUME_APPLY_WRINKLE_CARE_HEX: &str =
        "aa1bf026020000040000001001000200000000030000000000a2bb";
    // A Normal run with Wrinkle Care, to its end: Cooling, previous phase Drying.
    const COOLING: &str =
        "aa4030ec001b320038003903000304000400000010ab0000040100000073000000001b330001000203000304000400000010ab00000632000000730000009fbb";
    // End, previous phase Cooling.
    const END: &str =
        "aa4030ec001b330001000303000304000400000010ab0000083200000073000000001b040001000303000304000400000050aa00000a3300000073000000fcbb";
    // 30 s after End the after-cycle tumble begins: Wrinkle Care, previous phase End.
    const WRINKLE_CARE_TUMBLE: &str =
        "aa4030ec001b040001000303000304000400000050aa00000a3300000073000000001b380001000103000304000400000050ab00000a040000007300000095bb";
    // Powered off by hand 114 minutes into the tumble.
    const WRINKLE_CARE_POWERED_OFF: &str =
        "aa4030ec001b380001013703000304000400000010ab0000180400000073000000001b000001000100000000000400000040a80000183800000073000000e3bb";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, MODEL_ID, "2.10.121")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn feed(frame: &str) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    /// Sent packets as lowercase hex, the way the upstream fixtures are written.
    fn outbox(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .outbox()
            .iter()
            .map(|p| hex_encode(p).to_lowercase())
            .collect()
    }

    #[test]
    fn eb_single_record_frame_after_power_on_decodes_with_the_ec_offsets() {
        let ha = feed(EB_INITIAL_POWER_ON);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Initial");
        expect(&ha, "course", "None");
        expect(&ha, "remaining_time", "1");
    }

    #[test]
    fn remote_start_is_rec16_bit_0x01() {
        expect(&feed(REMOTE_START_ON), "remote_start", "ON");
        expect(&feed(HEAVY_DUTY), "remote_start", "OFF");
    }

    #[test]
    fn dial_courses_decode_by_rec6_with_the_cloud_confirmed_defaults() {
        let hd = feed(HEAVY_DUTY);
        expect(&hd, "course", "Heavy Duty");
        expect(&hd, "dry_level", "Normal");
        expect(&hd, "temp", "High");
        expect(&hd, "remaining_time", "54");
        expect(&hd, "initial_time", "54");
        expect(&feed(AIR_DRY), "course", "Air Dry");
        expect(&feed(AIR_DRY), "temp", "None"); // 0 = NO_TEMP
        expect(&feed(AIR_DRY), "remaining_time", "30");
    }

    #[test]
    fn hour_and_minute_fields_combine_into_one_duration() {
        let ha = feed(ANTIBACTERIAL);
        expect(&ha, "course", "Antibacterial");
        expect(&ha, "remaining_time", "70");
        expect(&ha, "initial_time", "70");
        expect(&ha, "dry_level", "Very");
    }

    #[test]
    fn the_downloaded_position_reports_the_loaded_smart_course() {
        expect(&feed(DOWNLOADED_SUPER_DRY), "course", "Super Dry");
    }

    #[test]
    fn load_items_map_the_rec23_code_to_the_count_the_display_shows() {
        expect(&feed(STEAM_FRESH_LOAD_ITEM), "load_item", "9"); // code 2 -> 9 items
        expect(&feed(REDUCE_STATIC_ON), "load_item", "16"); // code 5 -> 16 items
        expect(&feed(HEAVY_DUTY), "load_item", "0");
    }

    #[test]
    fn rec15_option_bits() {
        expect(&feed(CHILD_LOCK_ON), "control_lock", "ON");
        expect(&feed(REDUCE_STATIC_ON), "reduce_static", "ON");
        expect(&feed(DAMP_DRY_SIGNAL_ON), "damp_dry_signal", "ON");
        expect(&feed(WRINKLE_CARE_ON), "wrinkle_care", "ON");
        expect(&feed(WRINKLE_CARE_OFF), "wrinkle_care", "OFF");
        let hd = feed(HEAVY_DUTY);
        expect(&hd, "control_lock", "OFF");
        expect(&hd, "reduce_static", "OFF");
        expect(&hd, "damp_dry_signal", "OFF");
        expect(&hd, "wrinkle_care", "OFF");
    }

    #[test]
    fn rec16_option_bits() {
        expect(&feed(NORMAL_ENERGY_SAVER), "energy_saver", "ON");
        expect(&feed(HEAVY_DUTY), "energy_saver", "OFF");
        expect(&feed(TURBO_STEAM_ON), "turbo_steam", "ON");
        expect(&feed(HEAVY_DUTY), "turbo_steam", "OFF");
    }

    #[test]
    fn dry_level_and_temperature_steps() {
        expect(&feed(DRY_LEVEL_DAMP), "dry_level", "Damp");
        expect(&feed(DRY_LEVEL_VERY), "dry_level", "Very");
        expect(&feed(TEMP_ULTRA_LOW), "temp", "Ultra Low");
        expect(&feed(STEAM_FRESH_LOAD_ITEM), "temp", "Mid High");
    }

    #[test]
    fn more_less_time_is_a_signed_minute_offset_in_rec12() {
        let ha = feed(TIME_DRY_PLUS_15);
        expect(&ha, "course", "Time Dry");
        expect(&ha, "more_less_time", "15");
        expect(&ha, "remaining_time", "55");
        expect(&feed(HEAVY_DUTY), "more_less_time", "0");
    }

    #[test]
    fn signal_beeper_byte_panel_confirmed_only() {
        expect(&feed(SIGNAL_OFF), "signal", "Off");
        expect(&feed(HEAVY_DUTY), "signal", "On");
    }

    #[test]
    fn time_dry_duration_selector_rec10() {
        expect(&feed(TIME_DRY_20), "time_dry", "20 min");
        expect(&feed(TIME_DRY_50), "time_dry", "50 min");
        let ha = feed(TIME_DRY_60);
        expect(&ha, "time_dry", "60 min");
        expect(&ha, "remaining_time", "60");
        expect(&feed(HEAVY_DUTY), "time_dry", "None");
    }

    #[test]
    fn ai_lamp_rec17_0x04_and_energy_saver_automatic_rec16_0x80() {
        let auto = feed(NORMAL_ENERGY_SAVER);
        expect(&auto, "energy_saver", "ON");
        expect(&auto, "energy_saver_auto", "ON");
        expect(&auto, "ai", "OFF");
        let manual = feed(NORMAL_ENERGY_SAVER_OFF_AI);
        expect(&manual, "energy_saver", "OFF");
        expect(&manual, "energy_saver_auto", "OFF");
        expect(&manual, "ai", "ON");
    }

    #[test]
    fn a_full_cycle_ends_cooling_end_wrinkle_care_tumble_then_power_off_from_the_tumble() {
        let c = feed(COOLING);
        expect(&c, "status", "Cooling");
        expect(&c, "previous_status", "Drying");
        expect(&c, "remaining_time", "1");
        let e = feed(END);
        expect(&e, "status", "End");
        expect(&e, "previous_status", "Cooling");
        let w = feed(WRINKLE_CARE_TUMBLE);
        expect(&w, "status", "Wrinkle Care");
        expect(&w, "previous_status", "End");
        expect(&w, "wrinkle_care", "ON");
        expect(&w, "power", "ON");
        let off = feed(WRINKLE_CARE_POWERED_OFF);
        expect(&off, "status", "Off");
        expect(&off, "previous_status", "Wrinkle Care");
        expect(&off, "power", "OFF");
    }

    #[test]
    fn phases_through_an_app_driven_start_pause_resume_and_power_off() {
        let d = feed(DRYING);
        expect(&d, "status", "Drying");
        expect(&d, "previous_status", "Initial");
        let p = feed(PAUSED);
        expect(&p, "status", "Pause");
        expect(&p, "previous_status", "Drying");
        expect(&feed(RESUMED), "previous_status", "Pause");
        let off = feed(POWERED_OFF);
        expect(&off, "power", "OFF");
        expect(&off, "status", "Off");
        expect(&off, "course", "None");
        expect(&off, "remaining_time", "0");
    }

    #[test]
    fn start_reproduces_the_app_command_byte_for_byte_from_the_last_record() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(ARMED_NORMAL));
        dev.set_property("start", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf0260300000400000000410003000000000300000000007cbb"]
        );
    }

    #[test]
    fn start_from_a_pause_is_the_same_packet_without_the_new_cycle_bit() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(PAUSED));
        dev.set_property("start", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf026030000040000000001000300000000030000000000bcbb"]
        );
    }

    #[test]
    fn pause_and_power_off_are_the_captured_f024_frames_and_power_off_needs_the_select_value() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(DRYING));
        dev.set_property("pause", "");
        dev.set_property("power_off", "unknown");
        dev.set_property("power_off", "Power Off");
        assert_eq!(outbox(&thinq), ["aa09f02404010099bb", "aa09f0240101009cbb"]);
    }

    #[test]
    fn downloaded_course_reads_back_from_rec24() {
        expect(&feed(DOWNLOADED_SUPER_DRY), "downloaded_course", "Super Dry");
        let ha = feed(DENIM_DOWNLOADED);
        expect(&ha, "downloaded_course", "Denim");
        expect(&ha, "course", "Khaki/Jean");
        expect(&ha, "temp", "Medium");
        expect(&ha, "dry_level", "Normal");
    }

    #[test]
    fn specialty_download_sends_the_captured_wmdownload_frame_and_rearms_the_select() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(ARMED_NORMAL));
        dev.set_property("specialty", "Denim");
        dev.set_property("specialty", "Socks"); // Time Dry 30 + More Time 5 ride in the body
        dev.set_property("specialty", "Overnight Dry"); // Wrinkle Care in the flags byte
        dev.set_property("specialty", "Static Reduce"); // Reduce Static + load item 5 at body[16]
        dev.set_property("specialty", "Rack Dry"); // base course 0x13, no heat, no sensing
        assert_eq!(
            outbox(&thinq),
            [
                "aa1df02503150a0000030000000001000a6500000003000000000021bb",
                "aa1df0250315120000050000020001001271000000000005000000c3bb",
                "aa1df02503150d0000020000001001000d72000000030000000000c3bb",
                "aa1df02503150300000300000002010003cc00000003050000000081bb",
                "aa1df02503151300000000000000010013d0000000000000000000bebb",
            ]
        );
        expect(&ha, "specialty", "unknown");
    }

    #[test]
    fn start_json_rebuilds_the_three_app_committed_starts_byte_for_byte() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(ARMED_NORMAL));
        dev.set_property("start_json", &json!({"course": "Normal"}).to_string());
        dev.set_property(
            "start_json",
            &json!({"course": "Normal", "wrinkle_care": true}).to_string(),
        );
        dev.set_property(
            "start_json",
            &json!({"course": "Time Dry", "temp": "Medium", "minutes": 38}).to_string(),
        );
        dev.set_property("start_course", "Normal");
        // refused, nothing sent
        dev.set_property("start_json", &json!({"course": "Bogus"}).to_string());
        // minutes only on Time Dry
        dev.set_property(
            "start_json",
            &json!({"course": "Heavy Duty", "minutes": 30}).to_string(),
        );
        assert_eq!(
            outbox(&thinq),
            [
                "aa1bf0260300000400000000430003000000000300000000007ebb",
                "aa1bf0260300000400000010430003000000000300000000006ebb",
                "aa1bf0261200000300000300410012000000000000fe00000011bb",
                "aa1bf0260300000400000000430003000000000300000000007ebb",
            ]
        );
    }

    #[test]
    fn a_smart_course_starts_as_its_base_course_with_the_code_at_offset_11() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(ARMED_NORMAL));
        dev.set_property(
            "start_json",
            &json!({"course": "Denim", "more_less_time": -35}).to_string(),
        );
        assert_eq!(outbox(&thinq), [DENIM_START_HEX]);
    }

    #[test]
    fn start_as_dialed_carries_reduce_static_and_the_load_item_code_the_way_the_app_does() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(ARMED_HEAVY_DUTY_REDUCE_STATIC));
        dev.set_property("start", "");
        dev.set_property(
            "start_json",
            &json!({"course": "Heavy Duty", "reduce_static": true}).to_string(),
        );
        assert_eq!(
            outbox(&thinq),
            [HEAVY_DUTY_REDUCE_STATIC_START_HEX, HEAVY_DUTY_REDUCE_STATIC_START_HEX]
        );
    }

    #[test]
    fn wrinkle_care_switch_while_paused_sends_the_resume_apply_packet_the_app_sent() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(PAUSED_TOWELS));
        dev.set_property("wrinkle_care", "ON");
        assert_eq!(outbox(&thinq), [RESUME_APPLY_WRINKLE_CARE_HEX]);
        // not paused: nothing is sent and the switch snaps back to the panel's state
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(HEAVY_DUTY));
        dev.set_property("wrinkle_care", "ON");
        assert!(thinq.outbox().is_empty());
        expect(&ha, "wrinkle_care", "OFF");
    }

    #[test]
    fn dry_level_temperature_and_more_less_edits_while_paused_are_resume_apply_packets() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(PAUSED_TOWELS));
        dev.set_property("dry_level", "More");
        dev.set_property("temp", "Low");
        dev.set_property("more_less_time", "10");
        dev.set_property("time_dry", "50 min"); // not on Time Dry: refused, nothing sent
        let sent: Vec<Vec<u8>> = thinq
            .outbox()
            .iter()
            .map(|b| b[2..b.len() - 2].to_vec()) // inner bytes
            .collect();
        assert_eq!(sent.len(), 3);
        for p in &sent {
            assert_eq!(p[1], 0x26);
            assert_eq!(p[2], 0x02); // Towels, from the record
            assert_eq!(p[10], 0x01); // resume-apply: new-cycle bit clear
        }
        assert_eq!(sent[0][17], 4); // dry level More
        assert_eq!(sent[1][5], 2); // temp Low
        assert_eq!(sent[2][19], 10); // More/Less +10
    }

    #[test]
    fn registered_and_start_requests_a_status_snapshot() {
        assert!(crate::t2_factory(MODEL_ID).is_some());
        let (_, thinq, dev) = make();
        dev.start();
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
    }

    #[test]
    fn more_less_payloads_parse_like_javascript_number() {
        for (payload, byte) in [("", 0u8), ("0x10", 16), (" 12.6 ", 13), ("-40", (-30i8) as u8)] {
            let (_, thinq, dev) = make();
            thinq.emit_data(&hex_decode(PAUSED_TOWELS));
            thinq.reset_recorder();
            dev.set_property("more_less_time", payload);
            let sent = thinq.outbox();
            assert_eq!(sent.len(), 1, "{payload:?}");
            assert_eq!(sent[0][2 + 19], byte, "{payload:?}");
        }
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(PAUSED_TOWELS));
        thinq.reset_recorder();
        dev.set_property("more_less_time", "abc");
        assert!(thinq.outbox().is_empty());
    }
}
