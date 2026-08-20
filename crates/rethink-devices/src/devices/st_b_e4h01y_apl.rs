//! ST_B_E4H01Y_APL — LG Styler (KR Baron Plus, S5BBP).
//!
//! Port of upstream PR #115. AABB class 0x31, 40-byte records (trailing record
//! is current). Offsets and command frames from 2026-07-28 LG-cloud oracle /
//! convert/control captures.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::PropertyValue;
use rethink_core::{hex_decode, HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const HEADER_LEN: usize = 13;
const TRAILER_LEN: usize = 1;
const RECORD_LEN: usize = 40;
const RECORD_BYTES_OFF: usize = 11;
const STATE_TAG: u8 = 0x31;

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
const OFF_ENERGY_HI: usize = 19;
const OFF_ENERGY_LO: usize = 20;
const OFF_SMART_COURSE: usize = 22;
const OFF_DOWNLOAD_COURSE1: usize = 26;
const OFF_BUZZER: usize = 30;
const OFF_FLAGS_B: usize = 31;
const OFF_END_MELODY: usize = 32;
const OFF_INTERNAL_LIGHT: usize = 33;
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

const CTRL_POWER: u8 = 0x01;
const CTRL_PAUSE: u8 = 0x04;
const CTRL_REMOTE_MAINTAIN: u8 = 0x10;
const CTRL_UPCENTER: u8 = 0x13;
const UP_END_MELODY: u8 = 0x01;
const UP_BUZZER: u8 = 0x03;
const UP_SC_FINEDUST: u8 = 0x04;
const UP_SC_HUMIDITY: u8 = 0x05;
const UP_SC_NIGHTCARE: u8 = 0x06;
const UP_NC_START: u8 = 0x07;
const UP_NC_END: u8 = 0x08;
const UP_IS_LAST_COURSE: u8 = 0x09;
const RESUME_BLOCK_LEN: usize = 45;

fn state_name(code: u8) -> String {
    match code {
        0 => "Power off",
        1 => "Standby",
        2 => "Styling",
        3 => "Pause",
        4 | 58 | 59 => "Course complete",
        5 => "Check appliance",
        6 => "Smart diagnosing",
        7 => "Storing",
        8 => "Reserved",
        9 => "Power-save running",
        50 => "Steam preparing",
        51 | 52 | 53 => "Refreshing",
        54 | 55 | 56 => "Drying",
        57 => "Sterilizing",
        n => return format!("Code {n}"),
    }
    .into()
}

fn error_name(code: u8) -> String {
    match code {
        0 | 23 => "Normal",
        1 => "TE1",
        2 => "TE2",
        3 => "TE3",
        4 => "TE4",
        5 => "TE5",
        6 => "E1",
        7 => "E2",
        8 => "Water refill",
        9 => "E4",
        10 => "LE2",
        11 => "AE",
        18 => "LE",
        25 => "Water drain",
        26 => "Door open",
        31 => "Check door closed",
        32 => "E6",
        33 => "PS",
        34 => "No filter",
        n => return format!("Code {n}"),
    }
    .into()
}

fn error_clear(code: u8) -> bool {
    matches!(code, 0 | 23)
}

fn course_name(id: u8) -> String {
    match id {
        0 => "None",
        1 => "Styling Standard",
        3 => "Styling Quick",
        5 => "Styling Intensive",
        6 => "Wool/Knit",
        7 => "Suit/Coat",
        11 => "Sterilize Standard",
        15 => "Auto Dry",
        17 => "Timed Dry 30",
        18 => "Timed Dry 60",
        19 => "Timed Dry 90",
        20 => "Timed Dry 120",
        23 => "Room Dehumidify 120",
        24 => "Room Dehumidify 240",
        28 => "Padding Care",
        30 => "Fine dust",
        31 => "Virus",
        32 => "Jeans Care",
        33 => "Fur/Leather Care",
        36 => "Suit/Uniform Sterilize",
        37 => "Silk",
        38 => "Cashmere",
        n => return format!("Code {n}"),
    }
    .into()
}

fn smart_course_name(id: u8) -> String {
    match id {
        0 => "None",
        1 => "Panel course",
        61 => "Suit/Uniform Sterilize",
        62 => "Scarf Care",
        66 => "Pants Care",
        67 => "Quiet Care",
        68 => "Coat warm",
        69 => "Static removal",
        71 => "Old-clothes Care",
        73 => "Blanket warm",
        75 => "Dress-shirt Dry",
        76 => "Snow/Rain Dry",
        78 => "Fur/Leather Care",
        93 => "Jeans Care",
        94 => "Baby-clothes Sterilize",
        95 => "Doll Sterilize",
        96 => "Wool/Knit Dry",
        97 => "Rainy-season Laundry Dry",
        98 => "Uniform Care",
        99 => "Padding Care",
        100 => "Thin Padding Dry",
        101 => "Thick Padding Dry",
        112 => "Yoga/Pilates Care",
        113 => "Yoga/Pilates Dry",
        114 => "Swimwear Dry",
        115 => "Functional",
        119 => "Bedding Sterilize",
        n => return format!("Code {n}"),
    }
    .into()
}

fn buzzer_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Mute"),
        1 => Some("Small"),
        2 => Some("Normal"),
        3 => Some("Large"),
        4 => Some("Very large"),
        _ => None,
    }
}

fn buzzer_code(label: &str) -> Option<u8> {
    match label {
        "Mute" => Some(0),
        "Small" => Some(1),
        "Normal" => Some(2),
        "Large" => Some(3),
        "Very large" => Some(4),
        _ => None,
    }
}

const END_MELODY_OPTIONS: &[&str] = &[
    "Default sound",
    "Vivaldi Winter",
    "Bach Minuet",
    "Home Sweet Home",
    "Breeze",
    "Old MacDonald",
    "Verdi Brindisi",
    "Bubble",
    "Beethoven Symphony No.5 5",
    "Arirang",
    "Pachelbel Canon",
    "Beethoven Choral",
];

const COURSE_IDS: &[u8] = &[
    1, 3, 5, 6, 7, 11, 15, 17, 18, 19, 20, 23, 24, 28, 30, 31, 32, 33, 36, 37, 38,
];

fn course_params(id: u8) -> Option<&'static str> {
    match id {
        1 => Some("02645a00000005005a05b45a01b4001ab40000000000000000000000000000000000000000"),
        3 => Some("82645a00000003005a00000001b4000eb40000000000000000000000000000000000000000"),
        5 => Some("02645a00000007005a05c85a01c80031b40000000000000000000000000000000000000000"),
        6 => Some("82000000000004000000000001000014780000000000000000000000000000000000000000"),
        7 => Some("82645a00000004005a04b45a01b40017780000000000000000000000000000000000000000"),
        11 => Some("0200002d000006000008000003000023780000000000000000000000000000000000000000"),
        15 => Some("0000000000000000000000000000005a780000000000000000000000000000000000000000"),
        17 => Some("8000000000000000000000000000001e780000000000000000000000000000000000000000"),
        18 => Some("8000000000000000000000000000003c780000000000000000000000000000000000000000"),
        19 => Some("8000000000000000000000000000005a780000000000000000000000000000000000000000"),
        20 => Some("8000000000000000000000000000003c780000000000000000000000000000000000000000"),
        23 => Some("80000000000000000000000000000078000000000000000000000000000000000000000000"),
        24 => Some("800000000000000000000000000000f0000000000000000000000000000000000000000000"),
        28 => Some("82005a00000005005a05b45a01b4002eb40000000000000000000000000000000000000000"),
        30 => Some("82000000000000000000000005c80000000000000000000003005a02c85a01c80028c80000"),
        31 => Some("8200002d000007000008000003000043000000000000000000000000000000000000000000"),
        32 => Some("8200002d000006000008000003000028000000000000000000000000000000000000000000"),
        33 => Some("8000000000000000000000000000001e780000000000000000000000000000000000000000"),
        36 => Some("8200002d00000600000800000300002b780000000000000000000000000000000000000000"),
        37 => Some("8200002d00000600000800000300002b780000000000000000000000000000000000000000"),
        38 => Some("8200002d00000600000800000300002b780000000000000000000000000000000000000000"),
        _ => None,
    }
}

fn course_id_for(label: &str) -> Option<u8> {
    COURSE_IDS.iter().copied().find(|&id| course_name(id) == label)
}

fn hhmm(h: u8, m: u8) -> String {
    format!("{h:02}:{m:02}")
}

fn parse_half_hour(s: &str) -> Option<(u8, u8)> {
    let s = s.trim();
    if s.len() != 5 || s.as_bytes().get(2) != Some(&b':') {
        return None;
    }
    let h: u8 = s[..2].parse().ok()?;
    let m: u8 = s[3..].parse().ok()?;
    if h > 23 || (m != 0 && m != 30) {
        return None;
    }
    Some((h, m))
}

struct Inner {
    selected_course: u8,
    powered_on: Option<bool>,
    night_care_enabled: Option<bool>,
    has_problem: Option<bool>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    inner: Mutex<Inner>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            inner: Mutex::new(Inner {
                selected_course: COURSE_IDS[0],
                powered_on: None,
                night_care_enabled: None,
                has_problem: None,
            }),
        });

        let course_options: Vec<&str> = COURSE_IDS
            .iter()
            .map(|&id| match id {
                1 => "Styling Standard",
                3 => "Styling Quick",
                5 => "Styling Intensive",
                6 => "Wool/Knit",
                7 => "Suit/Coat",
                11 => "Sterilize Standard",
                15 => "Auto Dry",
                17 => "Timed Dry 30",
                18 => "Timed Dry 60",
                19 => "Timed Dry 90",
                20 => "Timed Dry 120",
                23 => "Room Dehumidify 120",
                24 => "Room Dehumidify 240",
                28 => "Padding Care",
                30 => "Fine dust",
                31 => "Virus",
                32 => "Jeans Care",
                33 => "Fur/Leather Care",
                36 => "Suit/Uniform Sterilize",
                37 => "Silk",
                38 => "Cashmere",
                _ => "",
            })
            .collect();

        let sensor = |id: &str, name: &str, icon: &str| {
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{id}"),
                "state_topic": format!("$this/{id}"),
                "name": name,
                "icon": icon,
            })
        };
        let flag = |id: &str, name: &str, icon: &str| {
            json!({
                "platform": "binary_sensor",
                "unique_id": format!("$deviceid-{id}"),
                "state_topic": format!("$this/{id}"),
                "name": name,
                "icon": icon,
                "payload_on": "ON",
                "payload_off": "OFF",
            })
        };
        let toggle = |id: &str, name: &str, icon: &str, category: Option<&str>| {
            let mut v = json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{id}"),
                "state_topic": format!("$this/{id}"),
                "command_topic": format!("$this/{id}/set"),
                "name": name,
                "icon": icon,
                "payload_on": "ON",
                "payload_off": "OFF",
            });
            if let Some(c) = category {
                v["entity_category"] = json!(c);
            }
            v
        };
        let choice = |id: &str, name: &str, icon: &str, options: Vec<&str>, category: Option<&str>| {
            let mut v = json!({
                "platform": "select",
                "unique_id": format!("$deviceid-{id}"),
                "state_topic": format!("$this/{id}"),
                "command_topic": format!("$this/{id}/set"),
                "name": name,
                "icon": icon,
                "options": options,
            });
            if let Some(c) = category {
                v["entity_category"] = json!(c);
            }
            v
        };
        let press = |id: &str, name: &str, icon: &str| {
            json!({
                "platform": "button",
                "unique_id": format!("$deviceid-{id}"),
                "command_topic": format!("$this/{id}/set"),
                "payload_press": "",
                "name": name,
                "icon": icon,
            })
        };
        let clock = |id: &str, name: &str| {
            json!({
                "platform": "text",
                "unique_id": format!("$deviceid-{id}"),
                "state_topic": format!("$this/{id}"),
                "command_topic": format!("$this/{id}/set"),
                "name": name,
                "icon": "mdi:clock-time-four-outline",
                "pattern": "^(0[0-9]|1[0-9]|2[0-3]):(00|30)$",
                "min": 5,
                "max": 5,
                "entity_category": "config",
            })
        };

        let mut base = default_config(&meta, Some(json!({"name": "LG Styler"})));
        base.components = [
            ("power", toggle("power", "Power", "mdi:power", None)),
            ("status", sensor("status", "State", "mdi:hanger")),
            ("course", sensor("course", "Course", "mdi:playlist-check")),
            (
                "course_select",
                choice(
                    "course_select",
                    "Course select",
                    "mdi:playlist-edit",
                    course_options,
                    None,
                ),
            ),
            ("start_course", press("start_course", "Start course", "mdi:play-circle-outline")),
            ("pause_course", press("pause_course", "Pause", "mdi:pause-circle-outline")),
            ("resume_course", press("resume_course", "Resume", "mdi:play-pause")),
            ("smart_course", sensor("smart_course", "Smart course", "mdi:playlist-star")),
            (
                "remaining",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-remaining",
                    "state_topic": "$this/remaining",
                    "name": "Time remaining",
                    "icon": "mdi:timer-sand",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "state_class": "measurement",
                }),
            ),
            (
                "total",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-total",
                    "state_topic": "$this/total",
                    "name": "Total time",
                    "icon": "mdi:timer-outline",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "state_class": "measurement",
                }),
            ),
            ("reserved_at", sensor("reserved_at", "Reserved time", "mdi:calendar-clock")),
            ("error", sensor("error", "Error", "mdi:alert-circle-outline")),
            (
                "problem",
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-problem",
                    "state_topic": "$this/problem",
                    "name": "Check needed",
                    "device_class": "problem",
                    "icon": "mdi:alert",
                    "payload_on": "ON",
                    "payload_off": "OFF",
                }),
            ),
            ("child_lock", flag("child_lock", "Button lock", "mdi:lock")),
            ("night_dry", flag("night_dry", "Store", "mdi:weather-night")),
            ("remote_start", flag("remote_start", "Remote-control ready", "mdi:cellphone-check")),
            (
                "remote_maintain",
                toggle("remote_maintain", "Remote control allowed", "mdi:cellphone-wireless", None),
            ),
            (
                "buzzer",
                choice(
                    "buzzer",
                    "Alert sound",
                    "mdi:volume-high",
                    vec!["Mute", "Small", "Normal", "Large", "Very large"],
                    Some("config"),
                ),
            ),
            (
                "end_melody",
                choice(
                    "end_melody",
                    "End melody",
                    "mdi:music",
                    END_MELODY_OPTIONS.to_vec(),
                    Some("config"),
                ),
            ),
            (
                "keep_last_course",
                toggle("keep_last_course", "Keep last course", "mdi:repeat", Some("config")),
            ),
            (
                "smart_care_finedust",
                toggle(
                    "smart_care_finedust",
                    "SmartCare Fine-dust",
                    "mdi:weather-dust",
                    Some("config"),
                ),
            ),
            (
                "smart_care_humidity",
                toggle(
                    "smart_care_humidity",
                    "SmartCare Humidity",
                    "mdi:water-percent",
                    Some("config"),
                ),
            ),
            (
                "smart_care_nightcare",
                toggle("smart_care_nightcare", "SmartCare Night", "mdi:sleep", Some("config")),
            ),
            ("night_care_start", clock("night_care_start", "Night-care start time")),
            ("night_care_end", clock("night_care_end", "Night-care end time")),
            ("night_care_window", json!({"platform": "sensor"})),
            (
                "download_course",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-download_course",
                    "state_topic": "$this/download_course",
                    "name": "Downloaded course",
                    "icon": "mdi:download",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "tcl_count",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-tcl_count",
                    "state_topic": "$this/tcl_count",
                    "name": "Sterilize-tank clean alert count",
                    "icon": "mdi:counter",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "energy",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-energy",
                    "state_topic": "$this/energy",
                    "name": "Energy monitoring",
                    "icon": "mdi:flash",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "previous_status",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-previous_status",
                    "state_topic": "$this/previous_status",
                    "name": "Previous state",
                    "icon": "mdi:history",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "internal_light",
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-internal_light",
                    "state_topic": "$this/internal_light",
                    "name": "Interior light setting",
                    "icon": "mdi:lightbulb-outline",
                    "entity_category": "diagnostic",
                }),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.is_empty() || buf[0] != STATE_TAG {
            return;
        }
        if buf.len() < HEADER_LEN + RECORD_LEN + TRAILER_LEN {
            return;
        }
        let payload = buf.len() - HEADER_LEN - TRAILER_LEN;
        if payload % RECORD_LEN != 0 {
            return;
        }
        if buf.len() < RECORD_BYTES_OFF + 2 {
            return;
        }
        let declared = u16::from_be_bytes([buf[RECORD_BYTES_OFF], buf[RECORD_BYTES_OFF + 1]]) as usize;
        if declared != payload {
            return;
        }
        let record = buf.len() - TRAILER_LEN - RECORD_LEN;
        let rec = &buf[record..];
        let at = |o: usize| rec[o];
        let flags_a = at(OFF_FLAGS_A);
        let flags_b = at(OFF_FLAGS_B);
        let flags_c = at(OFF_FLAGS_C);
        let on = |bits: u8, mask: u8| bits & mask != 0;

        let state = at(OFF_STATE);
        let err = at(OFF_ERROR);
        {
            let mut inner = self.inner.lock();
            inner.powered_on = Some(state != 0);
            inner.night_care_enabled = Some(on(flags_c, F_SC_NIGHTCARE));
            inner.has_problem = Some(!error_clear(err));
        }

        self.core.publish_property("status", state_name(state).into());
        self.core.publish_on_off("power", state != 0);
        self.core
            .publish_property("previous_status", state_name(at(OFF_PRE_STATE)).into());
        self.core.publish_property("error", error_name(err).into());
        self.core.publish_on_off("problem", !error_clear(err));
        self.core
            .publish_property("course", course_name(at(OFF_COURSE)).into());
        self.core
            .publish_property("smart_course", smart_course_name(at(OFF_SMART_COURSE)).into());

        if course_params(at(OFF_COURSE)).is_some() {
            let id = at(OFF_COURSE);
            self.inner.lock().selected_course = id;
            self.core
                .publish_property("course_select", course_name(id).into());
        }

        self.core.publish_property(
            "remaining",
            PropertyValue::Int(at(OFF_REMAIN_H) as i64 * 60 + at(OFF_REMAIN_M) as i64),
        );
        self.core.publish_property(
            "total",
            PropertyValue::Int(at(OFF_INITIAL_H) as i64 * 60 + at(OFF_INITIAL_M) as i64),
        );
        let rh = at(OFF_RESERVE_H);
        let rm = at(OFF_RESERVE_M);
        self.core.publish_property(
            "reserved_at",
            if rh == 0 && rm == 0 {
                "No reservation".into()
            } else {
                hhmm(rh, rm).into()
            },
        );

        self.core.publish_on_off("child_lock", on(flags_a, F_CHILD_LOCK));
        self.core.publish_on_off("night_dry", on(flags_a, F_NIGHT_DRY));
        self.core.publish_on_off("remote_start", on(flags_a, F_REMOTE_START));
        self.core
            .publish_on_off("remote_maintain", on(flags_b, F_REMOTE_MAINTAIN));

        if let Some(label) = buzzer_name(at(OFF_BUZZER)) {
            self.core.publish_property("buzzer", label.into());
        }
        let melody = at(OFF_END_MELODY) as usize;
        if melody < END_MELODY_OPTIONS.len() {
            self.core
                .publish_property("end_melody", END_MELODY_OPTIONS[melody].into());
        }

        self.core
            .publish_on_off("keep_last_course", on(flags_c, F_IS_LAST_COURSE));
        self.core
            .publish_on_off("smart_care_finedust", on(flags_c, F_SC_FINEDUST));
        self.core
            .publish_on_off("smart_care_humidity", on(flags_c, F_SC_HUMIDITY));
        self.core
            .publish_on_off("smart_care_nightcare", on(flags_c, F_SC_NIGHTCARE));
        self.core.publish_property(
            "night_care_start",
            hhmm(at(OFF_NC_START_H), at(OFF_NC_START_M)).into(),
        );
        self.core
            .publish_property("night_care_end", hhmm(at(OFF_NC_END_H), at(OFF_NC_END_M)).into());
        self.core.publish_property(
            "download_course",
            smart_course_name(at(OFF_DOWNLOAD_COURSE1)).into(),
        );
        self.core
            .publish_property("tcl_count", PropertyValue::Int(at(OFF_TCL) as i64));
        let energy = ((at(OFF_ENERGY_HI) as i64) << 8) | at(OFF_ENERGY_LO) as i64;
        self.core.publish_property("energy", PropertyValue::Int(energy));
        self.core.publish_property(
            "internal_light",
            PropertyValue::Int(at(OFF_INTERNAL_LIGHT) as i64),
        );
    }

    fn set_control(&self, typ: u8, values: &[u8]) {
        let mut inner = vec![0xf0, 0x24, typ, values.len() as u8];
        inner.extend_from_slice(values);
        self.core.send(&inner);
    }

    fn set_up_center(&self, sub: u8, values: &[u8]) {
        let mut inner = vec![0xf0, 0x24, CTRL_UPCENTER, values.len() as u8, sub];
        inner.extend_from_slice(values);
        self.core.send(&inner);
    }

    fn run_course(&self, id: u8) {
        let Some(params) = course_params(id) else {
            return;
        };
        let mut inner = vec![0xf0, 0x26, id, 0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
        inner.extend(hex_decode(params));
        self.core.send(&inner);
    }

    fn resume_course(&self, id: u8) {
        let mut block = vec![0u8; RESUME_BLOCK_LEN];
        block[0] = id;
        let mut inner = vec![0xf0, 0x26];
        inner.extend(block);
        self.core.send(&inner);
    }

    fn echo(&self, prop: &str, value: PropertyValue) {
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
        drop(inner);
        self.core.publish_property(prop, value);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let on_off = u8::from(mqtt_value == "ON");
        match prop {
            "power" => {
                self.set_control(CTRL_POWER, &[on_off]);
                if mqtt_value != "ON" {
                    self.inner.lock().powered_on = Some(false);
                }
                self.core
                    .publish_on_off("power", mqtt_value == "ON");
            }
            "remote_maintain" => {
                self.set_control(CTRL_REMOTE_MAINTAIN, &[on_off]);
                self.echo(prop, mqtt_value.into());
            }
            "keep_last_course" => {
                self.set_up_center(UP_IS_LAST_COURSE, &[on_off]);
                self.echo(prop, mqtt_value.into());
            }
            "course_select" => {
                if let Some(id) = course_id_for(mqtt_value) {
                    self.inner.lock().selected_course = id;
                    self.core.publish_property(prop, mqtt_value.into());
                }
            }
            "start_course" => {
                let id = self.inner.lock().selected_course;
                self.run_course(id);
                self.echo("course", course_name(id).into());
            }
            "pause_course" => self.set_control(CTRL_PAUSE, &[0]),
            "resume_course" => {
                let id = self.inner.lock().selected_course;
                self.resume_course(id);
            }
            "smart_care_finedust" => {
                self.set_up_center(UP_SC_FINEDUST, &[on_off]);
                self.echo(prop, mqtt_value.into());
            }
            "smart_care_humidity" => {
                self.set_up_center(UP_SC_HUMIDITY, &[on_off]);
                self.echo(prop, mqtt_value.into());
            }
            "smart_care_nightcare" => {
                self.set_up_center(UP_SC_NIGHTCARE, &[on_off]);
                self.echo(prop, mqtt_value.into());
            }
            "buzzer" => {
                if let Some(code) = buzzer_code(mqtt_value) {
                    self.set_up_center(UP_BUZZER, &[code]);
                    self.echo(prop, mqtt_value.into());
                }
            }
            "end_melody" => {
                if let Some(code) = END_MELODY_OPTIONS.iter().position(|l| *l == mqtt_value) {
                    self.set_up_center(UP_END_MELODY, &[code as u8]);
                    self.echo(prop, mqtt_value.into());
                }
            }
            "night_care_start" | "night_care_end" => {
                if let Some((h, m)) = parse_half_hour(mqtt_value) {
                    let sub = if prop == "night_care_start" {
                        UP_NC_START
                    } else {
                        UP_NC_END
                    };
                    self.set_up_center(sub, &[h, m]);
                    self.echo(prop, mqtt_value.trim().into());
                }
            }
            _ => {}
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
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};
    use std::collections::HashSet;

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("ST_B_E4H01Y_APL", Device::new)
    }

    const STATE: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e420000040300000000000000257205bb";
    const STATE2: &str =
        "aaff310a003a005504000100eb002800000000010001000001000000000000200000000000000000014e420000040300000000000000251351bb";
    const READY: &str =
        "aaff310a003a0053ea000100eb002800000100010001000001000000000000200000000000000000014e42000004030000000000000025969ebb";
    const COURSE_STANDARD: &str =
        "aaff310a003a0053ea000100eb002800000000010001010001000000000000200000000000000000014e42000004030000000000000025ffd9bb";
    const ERROR_TE1: &str =
        "aaff310a003a0053ea000100eb002800000000010001000101000000000000200000000000000000014e420000040300000000000000252b43bb";
    const PRESTATE_RUNNING: &str =
        "aaff310a003a0053ea000100eb002800000000010001000002000000000000200000000000000000014e42000004030000000000000025f05fbb";
    const FLAGS_A_ALL: &str =
        "aaff310a003a0053ea000100eb0028000000000100010000010000000000000f0000000000000000014e42000004030000000000000025df93bb";
    const REMOTE_MAINTAIN: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e42000004040000000000000025c3aebb";
    const FLAGS_C_ALL: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e420000040300003f0000000025988abb";
    const BUZZER_LOW: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e42000001030000000000000025b075bb";
    const MELODY_1: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e4200000403010000000000002535d6bb";
    const ENERGY_HI: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000010000000000014e42000004030000000000000025a7f3bb";
    const SMART_COURSE_1: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000010000014e420000040300000000000000258234bb";
    const TCL_1: &str =
        "aaff310a003a0053ea000100eb002800000000010001000001000000000000200000000000000000014e4200000403000000000000000116e3bb";
    const RUNNING_READY: &str = concat!(
        "aaff310a0062005cd0000100ec005000000000010001000001000000000000200000000000000000014e42000004030000000000000025",
        "00000100010001000000000000000000200000000000000000014e42000004030000000000000025ee3ebb"
    );
    const RUNNING_COURSE: &str = concat!(
        "aaff310a0062005ce5000100ec0050000001011e011e0f0000000000000000280000000000000000014e42000004030000000000000025",
        "00000100230023010000000000000000280000000000000000014e42000004030000000000000025042abb"
    );
    const COURSE_LIST: &str = concat!(
        "aaff310a0070005ccc00010a86005e0d053230332d3101073230332d312d3101073230332d312d3201073230332d312d3301073230332d312d3401",
        "0453542d34010453542d35010453542d36010453542d37010453542d38010453542d39010553542d3130000553542d313201ed79bb"
    );
    const NOT_STATE_35: &str = "aaff310a00230053ed000101030011100b0306100000008a00000000000000059317bb";
    const HEARTBEAT: &str = "aa0731c302f2bb";

    fn emit(thinq: &rethink_core::MockThinq2Device, hex: &str) {
        thinq.emit_data(&hex_decode(hex));
    }

    fn with_byte(frame: &str, index: usize, value: u8) -> Vec<u8> {
        let mut p = hex_decode(frame);
        p[index] = value;
        p
    }

    fn hex_lc(pkt: &[u8]) -> String {
        hex_encode(pkt).to_ascii_lowercase()
    }

    #[test]
    fn writable_fields_match_captured_commands() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = device.config.as_ref().unwrap().components.clone();
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
        for (name, comp) in &comps {
            if comp.get("unique_id").is_none() {
                continue;
            }
            if writable.contains(name.as_str()) {
                let want = format!("$this/{name}/set");
                assert_eq!(
                    comp.get("command_topic").and_then(|v| v.as_str()),
                    Some(want.as_str()),
                    "{name} is writable"
                );
            } else {
                assert!(comp.get("command_topic").is_none(), "{name} stays read-only");
                let plat = comp["platform"].as_str().unwrap();
                assert!(
                    plat == "sensor" || plat == "binary_sensor",
                    "{name} is a sensor platform, got {plat}"
                );
            }
        }
    }

    #[test]
    fn writes_reproduce_lg_command_frames() {
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
            ("end_melody", "Beethoven Symphony No.5 5", "aa0af02413010108b0bb"),
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
        for (prop_n, val, want) in cases {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property(prop_n, val);
            assert_eq!(thinq.outbox().len(), 1, "{prop_n}={val}");
            assert_eq!(hex_lc(&thinq.outbox()[0]), want, "{prop_n}={val}");
        }
    }

    #[test]
    fn remote_maintain_writes_acknowledged_frames() {
        for (val, want) in [("ON", "aa09f0241001018cbb"), ("OFF", "aa09f0241001008dbb")] {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property("remote_maintain", val);
            assert_eq!(hex_lc(&thinq.outbox()[0]), want);
        }
    }

    #[test]
    fn start_course_matches_captured_standard_and_quick() {
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
            assert_eq!(thinq.outbox().len(), 1);
            assert_eq!(hex_lc(&thinq.outbox()[0]), want, "{course}");
        }
    }

    #[test]
    fn pause_and_resume_match_captures() {
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
    fn write_echoes_while_powered_on_until_next_frame() {
        let (ha, thinq, dev) = make();
        let mut enabled = hex_decode(READY);
        enabled[49] |= 0x08;
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
        thinq.emit_data(&enabled);
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("Very large"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
    }

    #[test]
    fn settings_not_echoed_while_powered_off() {
        let (ha, thinq, dev) = make();
        emit(&thinq, STATE);
        thinq.reset_recorder();
        dev.set_property("buzzer", "Small");
        dev.set_property("smart_care_humidity", "ON");
        dev.set_property("course_select", "Styling Standard");
        dev.set_property("start_course", "");
        assert_eq!(thinq.outbox().len(), 3);
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("Very large"));
        assert_eq!(prop(&ha, "smart_care_humidity").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Styling Standard"));
    }

    #[test]
    fn night_care_times_not_echoed_while_disabled() {
        let (ha, thinq, dev) = make();
        emit(&thinq, READY);
        thinq.reset_recorder();
        dev.set_property("night_care_start", "22:30");
        dev.set_property("night_care_end", "06:00");
        assert_eq!(thinq.outbox().len(), 2);
        assert_eq!(prop(&ha, "night_care_start").as_deref(), Some("00:00"));
        assert_eq!(prop(&ha, "night_care_end").as_deref(), Some("00:00"));
    }

    #[test]
    fn no_echo_before_first_state() {
        let (ha, thinq, dev) = make();
        dev.set_property("buzzer", "Small");
        dev.set_property("course_select", "Styling Standard");
        dev.set_property("start_course", "");
        assert_eq!(thinq.outbox().len(), 2);
        assert!(prop(&ha, "buzzer").is_none());
        assert!(prop(&ha, "course").is_none());
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Styling Standard"));
    }

    #[test]
    fn course_not_echoed_while_error_stands() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&with_byte(ERROR_TE1, 17, 1));
        thinq.reset_recorder();
        dev.set_property("course_select", "Styling Standard");
        dev.set_property("start_course", "");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "problem").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
    }

    #[test]
    fn course_select_is_local_until_start() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("course_select", "Sterilize Standard");
        assert!(thinq.outbox().is_empty());
        dev.set_property("start_course", "");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(thinq.outbox()[0][4], 11);
    }

    #[test]
    fn running_course_takes_over_selection() {
        let (ha, thinq, _) = make();
        emit(&thinq, COURSE_STANDARD);
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Styling Standard"));
        emit(&thinq, STATE);
        assert_eq!(prop(&ha, "course_select").as_deref(), Some("Styling Standard"));
    }

    #[test]
    fn unknown_course_is_refused() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("course_select", "InvalidCourse");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn night_care_window_writes_hour_and_minute() {
        let cases = [
            ("night_care_start", "23:30", "aa0bf024130207171e4fbb"),
            ("night_care_start", "00:00", "aa0bf0241302070000b0bb"),
            ("night_care_end", "06:00", "aa0bf0241302080600b9bb"),
            ("night_care_end", "00:00", "aa0bf0241302080000b3bb"),
        ];
        for (prop_n, val, want) in cases {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property(prop_n, val);
            assert_eq!(hex_lc(&thinq.outbox()[0]), want, "{prop_n}={val}");
        }
    }

    #[test]
    fn invalid_night_care_time_is_not_written() {
        for bad in ["23:45", "24:00", "9:30", "abc", "", "07:1"] {
            let (_, thinq, dev) = make();
            thinq.reset_recorder();
            dev.set_property("night_care_start", bad);
            assert!(thinq.outbox().is_empty(), "refused {bad:?}");
        }
    }

    #[test]
    fn unknown_option_is_refused() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("buzzer", "InvalidVolume");
        dev.set_property("end_melody", "Jingle Bells (intro)");
        dev.set_property("course", "Styling Standard");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn power_off_is_published_immediately() {
        let (ha, thinq, dev) = make();
        emit(&thinq, READY);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        dev.set_property("power", "OFF");
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        emit(&thinq, READY);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
    }

    #[test]
    fn captured_idle_frame_matches_lg_cloud() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Power off"));
        assert_eq!(prop(&ha, "previous_status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "remaining").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "total").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "reserved_at").as_deref(), Some("No reservation"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_maintain").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "buzzer").as_deref(), Some("Very large"));
        assert_eq!(prop(&ha, "end_melody").as_deref(), Some("Default sound"));
        assert_eq!(prop(&ha, "smart_care_finedust").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "tcl_count").as_deref(), Some("37"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "download_course").as_deref(), Some("Fur/Leather Care"));
    }

    #[test]
    fn second_real_frame_decodes_identically() {
        let (ha_a, thinq_a, _) = make();
        emit(&thinq_a, STATE);
        let (ha_b, thinq_b, _) = make();
        emit(&thinq_b, STATE2);
        let a = ha_a.device(DEVICE_ID).unwrap().properties;
        let b = ha_b.device(DEVICE_ID).unwrap().properties;
        assert_eq!(a, b);
    }

    #[test]
    fn probed_offsets_move_the_lg_field() {
        let cases = [
            (READY, "status", "Standby"),
            (COURSE_STANDARD, "course", "Styling Standard"),
            (ERROR_TE1, "error", "TE1"),
            (PRESTATE_RUNNING, "previous_status", "Styling"),
            (BUZZER_LOW, "buzzer", "Small"),
            (MELODY_1, "end_melody", "Vivaldi Winter"),
            (ENERGY_HI, "energy", "256"),
            (SMART_COURSE_1, "smart_course", "Panel course"),
            (TCL_1, "tcl_count", "1"),
            (REMOTE_MAINTAIN, "remote_maintain", "ON"),
        ];
        for (frame, key, want) in cases {
            let (ha, thinq, _) = make();
            emit(&thinq, STATE);
            emit(&thinq, frame);
            assert_eq!(prop(&ha, key).as_deref(), Some(want), "{key}");
        }
    }

    #[test]
    fn running_state_codes_decode() {
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
    fn undefined_state_code_is_shown_as_code() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&with_byte(STATE, 17, 20));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Code 20"));
    }

    #[test]
    fn error_codes_and_problem_flag() {
        let cases = [
            (0u8, "Normal", "OFF"),
            (8, "Water refill", "ON"),
            (11, "AE", "ON"),
            (18, "LE", "ON"),
            (23, "Normal", "OFF"),
            (25, "Water drain", "ON"),
            (26, "Door open", "ON"),
            (34, "No filter", "ON"),
            (12, "Code 12", "ON"),
        ];
        for (code, want, problem) in cases {
            let (ha, thinq, _) = make();
            thinq.emit_data(&with_byte(STATE, 23, code));
            assert_eq!(prop(&ha, "error").as_deref(), Some(want), "error {code}");
            assert_eq!(prop(&ha, "problem").as_deref(), Some(problem), "problem {code}");
        }
    }

    #[test]
    fn byte_31_is_a_bitfield() {
        let (ha, thinq, _) = make();
        emit(&thinq, FLAGS_A_ALL);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
    }

    #[test]
    fn byte_49_is_a_bitfield() {
        let (ha, thinq, _) = make();
        emit(&thinq, FLAGS_C_ALL);
        assert_eq!(prop(&ha, "smart_care_finedust").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "smart_care_humidity").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "smart_care_nightcare").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "keep_last_course").as_deref(), Some("ON"));
    }

    #[test]
    fn night_care_window_reads_as_clock_times() {
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
    fn retired_night_care_summary_is_a_removal() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        assert_eq!(comps["night_care_window"], json!({"platform": "sensor"}));
    }

    #[test]
    fn non_state_frames_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        let before = ha.device(DEVICE_ID).unwrap().properties;
        emit(&thinq, NOT_STATE_35);
        emit(&thinq, HEARTBEAT);
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties, before);
    }

    #[test]
    fn hours_and_minutes_fold_into_duration() {
        let (ha, thinq, _) = make();
        let mut running = hex_decode(STATE);
        running[18] = 1;
        running[19] = 25;
        running[20] = 2;
        running[21] = 0;
        thinq.emit_data(&running);
        assert_eq!(prop(&ha, "remaining").as_deref(), Some("85"));
        assert_eq!(prop(&ha, "total").as_deref(), Some("120"));
    }

    #[test]
    fn two_record_frame_uses_the_last_record() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Power off"));
        emit(&thinq, RUNNING_READY);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        emit(&thinq, RUNNING_COURSE);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Styling Standard"));
        assert_eq!(prop(&ha, "remaining").as_deref(), Some("35"));
        assert_eq!(prop(&ha, "total").as_deref(), Some("35"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn course_name_list_is_not_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, STATE);
        let before = ha.device(DEVICE_ID).unwrap().properties;
        emit(&thinq, COURSE_LIST);
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties, before);
    }

    #[test]
    fn reservation_reads_as_clock_time() {
        let (ha, thinq, _) = make();
        let mut reserved = hex_decode(STATE);
        reserved[29] = 7;
        reserved[30] = 5;
        thinq.emit_data(&reserved);
        assert_eq!(prop(&ha, "reserved_at").as_deref(), Some("07:05"));
    }

    #[test]
    fn read_only_components_are_not_config_category() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        for (name, comp) in comps {
            let plat = comp.get("platform").and_then(|v| v.as_str()).unwrap_or("");
            if plat != "sensor" && plat != "binary_sensor" {
                continue;
            }
            assert_ne!(
                comp.get("entity_category").and_then(|v| v.as_str()),
                Some("config"),
                "{name} must not be entity_category config"
            );
        }
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("ST_B_E4H01Y_APL").is_some());
    }
}
