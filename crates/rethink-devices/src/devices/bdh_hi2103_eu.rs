//! BDH_HI2103_EU — LG RH18U8AVCW heat-pump dryer (long AABB class `0x30`).
//!
//! Port of upstream PR #315. 42-byte records in `0xEC`/`0xEB` envelopes,
//! `F0 E5 00 02 01 FF` key/value commands for power, pause, resume, start
//! course, download cycle, and staged settings (dryness, drying mode,
//! Anti-crease, Damp Dry Signal, Timed Dry length, delayed end).

use crate::device_trait::DeviceHandler;
use crate::devices::bdh_d30007_us::{
    error_name as bdh_error_name, state_name as bdh_state_name, ERROR_OPTS, STATE_OPTS,
};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const ENVELOPE_TYPE: u8 = 0x0a;
const INNER_TYPE_OFFSET: usize = 10;
const INNER_LEN_OFFSET: usize = 11;
const RECORD_LEN: usize = 42;

const STATUS_REQUEST: &str = "F0ED112101000000180411120000";
const COMMAND_PREFIX: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff];

const SETTING_POWER: u8 = 0x02;
const SETTING_RUN: u8 = 0x03;
const SETTING_COURSE: u8 = 0x0a;
const SETTING_DOWNLOAD: u8 = 0x0b;
const SETTING_DOWNLOAD_BASE: u8 = 0x0c;
const COURSE_DOWNLOAD: u8 = 0xff;
const SETTING_WRINKLE_CARE: u8 = 0x0f;
const SETTING_DRY_LEVEL: u8 = 0x1e;
const SETTING_DRYING_MODE: u8 = 0x1f;
const SETTING_DAMP_DRY_SIGNAL: u8 = 0x34;
const SETTING_STEAM: u8 = 0x3f;
const SETTING_TIMER: u8 = 0x70;
const SETTING_DELAY: u8 = 0x7f;

const POWER_ON: u16 = 1;
const POWER_OFF: u16 = 0;
const RUN_START: u16 = 1;
const RUN_PAUSE: u16 = 2;
const RUN_RESUME: u16 = 3;

const TIMER_MIN: i64 = 30;
const TIMER_MAX: i64 = 180;
const DELAY_MIN_HOURS: i64 = 3;
const DELAY_MAX_HOURS: i64 = 19;

const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_INNER_LEN: u16 = 2 * RECORD_LEN as u16;
const STATUS_RECORD_OFFSET: usize = 13 + RECORD_LEN;
const STATUS_BUF_LEN: usize = 13 + 2 * RECORD_LEN + 1;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_INNER_LEN: u16 = RECORD_LEN as u16;
const SINGLE_RECORD_OFFSET: usize = 13;
const SINGLE_BUF_LEN: usize = 13 + RECORD_LEN + 1;

const DRY_LEVEL_OFFSET: usize = 1;
const DRYING_MODE_OFFSET: usize = 2;
const COURSE_OFFSET: usize = 5;
const DELAY_OFFSET: usize = 7;
const REMAIN_TIME_OFFSET: usize = 9;
const INITIAL_TIME_OFFSET: usize = 11;
const STATE_OFFSET: usize = 13;
const ERROR_OFFSET: usize = 15;
const SPENT_POWER_OFFSET: usize = 17;
const BUZZER_OFFSET: usize = 19;
const DOWNLOAD_COURSE_OFFSET: usize = 23;
const OPT1_OFFSET: usize = 24;
const OPT1_WRINKLE_CARE: u8 = 0x08;
const OPT1_DRUM_LIGHT: u8 = 0x20;
const OPT1_DAMP_DRY_SIGNAL: u8 = 0x40;
const OPT2_OFFSET: usize = 26;
const OPT2_DOOR_LOCK: u8 = 0x01;
const OPT2_DETECT_LOAD: u8 = 0x04;
const OPT2_CHILD_LOCK: u8 = 0x10;
const OPT2_REMOTE_START: u8 = 0x40;
const STATE_POWEROFF: u8 = 0;

const SETTINGS_ORDER: &[u8] = &[
    SETTING_DRYING_MODE,
    SETTING_DRY_LEVEL,
    SETTING_DAMP_DRY_SIGNAL,
    SETTING_STEAM,
    SETTING_WRINKLE_CARE,
    SETTING_TIMER,
    SETTING_DELAY,
];

const COURSE: &[(u8, &str)] = &[
    (7, "Eco"),
    (44, "AI Dry"),
    (2, "Towels"),
    (5, "Easy Care"),
    (16, "Anti-Allergy"),
    (23, "Baby Care"),
    (9, "Small Load"),
    (22, "Outdoor Refresh"),
    (20, "Refresh Jacket"),
    (8, "Sportswear"),
    (11, "Wool"),
    (15, "Refresh Bedding"),
    (4, "Down Blanket"),
    (19, "Drum Clean"),
    (21, "Timed Dry"),
    (13, "Cool Air"),
    (18, "Condenser Care"),
    (255, "Downloaded cycle"),
];

const DOWNLOAD_COURSE: &[(u8, &str)] = &[
    (102, "Gym Clothes"),
    (105, "Rainy Days"),
    (110, "Easy Ironing"),
    (114, "Minimize Wrinkles"),
    (126, "Silent"),
    (140, "Rack Dry"),
    (142, "Shrinkage Relief"),
    (148, "Super Dry"),
];

const DRY_LEVEL: &[(u8, &str)] = &[
    (0, "Not selected"),
    (1, "Iron"),
    (2, "Light"),
    (3, "Cupboard"),
    (4, "Very Dry"),
    (5, "Extra Dry"),
];

const DRYING_MODE: &[(u8, &str)] = &[
    (0, "Not selected"),
    (1, "Eco"),
    (2, "Balanced"),
    (3, "Turbo"),
];

const BUZZER: &[(u8, &str)] = &[(0, "Off"), (1, "1"), (2, "2"), (3, "3"), (4, "4")];

#[derive(Clone, Copy)]
struct Defaults {
    mode: u16,
    dry_level: u16,
    timer: u16,
    wrinkle_care: u16,
}

fn course_defaults(code: u8) -> Option<Defaults> {
    Some(match code {
        7 | 5 => Defaults { mode: 2, dry_level: 3, timer: 0, wrinkle_care: 0 },
        44 | 2 | 23 | 11 | 15 | 4 => Defaults { mode: 2, dry_level: 0, timer: 0, wrinkle_care: 0 },
        16 | 9 | 22 | 20 | 19 | 18 => Defaults { mode: 3, dry_level: 0, timer: 0, wrinkle_care: 0 },
        8 => Defaults { mode: 1, dry_level: 0, timer: 0, wrinkle_care: 0 },
        21 => Defaults { mode: 2, dry_level: 0, timer: 50, wrinkle_care: 0 },
        13 => Defaults { mode: 0, dry_level: 0, timer: 30, wrinkle_care: 0 },
        _ => return None,
    })
}

fn download_defaults(id: u8) -> Option<(u8, Defaults)> {
    Some(match id {
        102 => (49, Defaults { mode: 1, dry_level: 0, timer: 0, wrinkle_care: 0 }),
        105 => (50, Defaults { mode: 3, dry_level: 0, timer: 0, wrinkle_care: 0 }),
        110 => (51, Defaults { mode: 3, dry_level: 0, timer: 0, wrinkle_care: 0 }),
        114 => (7, Defaults { mode: 2, dry_level: 3, timer: 0, wrinkle_care: 1 }),
        126 => (45, Defaults { mode: 2, dry_level: 0, timer: 0, wrinkle_care: 0 }),
        140 => (12, Defaults { mode: 1, dry_level: 0, timer: 30, wrinkle_care: 0 }),
        142 => (46, Defaults { mode: 2, dry_level: 2, timer: 0, wrinkle_care: 0 }),
        148 => (17, Defaults { mode: 2, dry_level: 0, timer: 0, wrinkle_care: 0 }),
        _ => return None,
    })
}

fn to_settings_map(d: Defaults) -> HashMap<u8, u16> {
    let mut m = HashMap::new();
    for &id in SETTINGS_ORDER {
        m.insert(id, 0);
    }
    m.insert(SETTING_DRYING_MODE, d.mode);
    m.insert(SETTING_DRY_LEVEL, d.dry_level);
    m.insert(SETTING_TIMER, d.timer);
    m.insert(SETTING_WRINKLE_CARE, d.wrinkle_care);
    m
}

fn command(pairs: &[(u8, u16)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(COMMAND_PREFIX.len() + 1 + pairs.len() * 3);
    out.extend_from_slice(COMMAND_PREFIX);
    out.push(pairs.len() as u8);
    for &(id, value) in pairs {
        out.push(id);
        if id == SETTING_TIMER || id == SETTING_DELAY {
            out.push((value >> 8) as u8);
            out.push((value & 0xff) as u8);
        } else {
            out.push(value as u8);
        }
    }
    out
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

fn unmap(table: &'static [(u8, &'static str)], label: &str) -> Option<u16> {
    table.iter().find(|(_, n)| *n == label).map(|(c, _)| u16::from(*c))
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components() -> HashMap<String, Value> {
    [
        ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","icon":"mdi:tumble-dryer"})),
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("resume", json!({"platform":"button","unique_id":"$deviceid-resume","command_topic":"$this/resume/set","payload_press":"","name":"Resume","icon":"mdi:play-circle-outline"})),
        ("set_course", json!({"platform":"select","unique_id":"$deviceid-set_course","state_topic":"$this/set_course","command_topic":"$this/set_course/set","name":"Set course","icon":"mdi:tune-vertical","options":labels(COURSE)})),
        ("set_dry_level", json!({"platform":"select","unique_id":"$deviceid-set_dry_level","state_topic":"$this/set_dry_level","command_topic":"$this/set_dry_level/set","name":"Set dryness level","icon":"mdi:water-percent","options":labels(DRY_LEVEL)})),
        ("set_eco_hybrid", json!({"platform":"select","unique_id":"$deviceid-set_eco_hybrid","state_topic":"$this/set_eco_hybrid","command_topic":"$this/set_eco_hybrid/set","name":"Set drying mode","icon":"mdi:leaf","options":labels(DRYING_MODE)})),
        ("set_wrinkle_care", json!({"platform":"switch","unique_id":"$deviceid-set_wrinkle_care","state_topic":"$this/set_wrinkle_care","command_topic":"$this/set_wrinkle_care/set","name":"Set Anti-crease","icon":"mdi:tshirt-crew-outline"})),
        ("set_damp_dry_signal", json!({"platform":"switch","unique_id":"$deviceid-set_damp_dry_signal","state_topic":"$this/set_damp_dry_signal","command_topic":"$this/set_damp_dry_signal/set","name":"Set Damp Dry Signal","icon":"mdi:water-alert-outline"})),
        ("set_timer", json!({"platform":"number","unique_id":"$deviceid-set_timer","state_topic":"$this/set_timer","command_topic":"$this/set_timer/set","name":"Set Timed Dry length","icon":"mdi:timer-cog-outline","min":TIMER_MIN,"max":TIMER_MAX,"step":10,"unit_of_measurement":"min"})),
        ("set_delay_end", json!({"platform":"number","unique_id":"$deviceid-set_delay_end","state_topic":"$this/set_delay_end","command_topic":"$this/set_delay_end/set","name":"Set delayed end","icon":"mdi:timer-sand","min":0,"max":DELAY_MAX_HOURS,"step":1,"unit_of_measurement":"h"})),
        ("delay", json!({"platform":"sensor","unique_id":"$deviceid-delay","state_topic":"$this/delay","name":"Delay end","icon":"mdi:clock-start","device_class":"duration","unit_of_measurement":"min"})),
        ("set_download_course", json!({"platform":"select","unique_id":"$deviceid-set_download_course","state_topic":"$this/set_download_course","command_topic":"$this/set_download_course/set","name":"Set downloaded cycle","icon":"mdi:cloud-download-outline","options":labels(DOWNLOAD_COURSE)})),
        ("download_course_btn", json!({"platform":"button","unique_id":"$deviceid-download_course_btn","command_topic":"$this/download_course/set","payload_press":"","name":"Download cycle","icon":"mdi:download-circle-outline"})),
        ("start_course", json!({"platform":"button","unique_id":"$deviceid-start_course","command_topic":"$this/start_course/set","payload_press":"","name":"Start course","icon":"mdi:play-circle"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":STATE_OPTS})),
        ("error", json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:alert-circle","device_class":"problem","entity_category":"diagnostic"})),
        ("error_message", json!({"platform":"sensor","unique_id":"$deviceid-error_message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","device_class":"enum","options":ERROR_OPTS,"entity_category":"diagnostic"})),
        ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":labels(COURSE)})),
        ("download_course", json!({"platform":"sensor","unique_id":"$deviceid-download_course","state_topic":"$this/download_course","name":"Downloaded cycle","icon":"mdi:cloud-download-outline","device_class":"enum","options":labels(DOWNLOAD_COURSE)})),
        ("dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dryness level","icon":"mdi:water-percent","device_class":"enum","options":labels(DRY_LEVEL)})),
        ("eco_hybrid", json!({"platform":"sensor","unique_id":"$deviceid-eco_hybrid","state_topic":"$this/eco_hybrid","name":"Drying mode","icon":"mdi:leaf","device_class":"enum","options":labels(DRYING_MODE)})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
        ("buzzer", json!({"platform":"sensor","unique_id":"$deviceid-buzzer","state_topic":"$this/buzzer","name":"Buzzer","icon":"mdi:volume-high","device_class":"enum","options":labels(BUZZER),"entity_category":"diagnostic"})),
        ("wrinkle_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Anti-crease","icon":"mdi:tshirt-crew-outline"})),
        ("damp_dry_signal", json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","name":"Damp Dry Signal","icon":"mdi:water-alert-outline"})),
        ("detect_load", json!({"platform":"binary_sensor","unique_id":"$deviceid-detect_load","state_topic":"$this/detect_load","name":"Detect load","icon":"mdi:scale"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:remote"})),
        ("drum_light", json!({"platform":"binary_sensor","unique_id":"$deviceid-drum_light","state_topic":"$this/drum_light","name":"Drum light","icon":"mdi:lightbulb","entity_category":"diagnostic"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"})),
        ("door_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","device_class":"lock"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[derive(Default)]
struct State {
    pending: HashMap<u8, u16>,
    reported: HashMap<u8, u16>,
    reported_initial_time: Option<u16>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    state: Mutex<State>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            state: Mutex::new(State::default()),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
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
        if buf.len() < 13 || buf[0] != CLASS_BYTE || buf[1] != ENVELOPE_TYPE {
            return;
        }
        let inner_len = u16::from_be_bytes([buf[INNER_LEN_OFFSET], buf[INNER_LEN_OFFSET + 1]]);
        match buf[INNER_TYPE_OFFSET] {
            STATUS_FRAME_TYPE if inner_len == STATUS_INNER_LEN && buf.len() == STATUS_BUF_LEN => {
                self.process_status(&buf[STATUS_RECORD_OFFSET..STATUS_RECORD_OFFSET + RECORD_LEN]);
            }
            SINGLE_STATUS_FRAME_TYPE
                if inner_len == SINGLE_INNER_LEN && buf.len() == SINGLE_BUF_LEN =>
            {
                self.process_status(&buf[SINGLE_RECORD_OFFSET..SINGLE_RECORD_OFFSET + RECORD_LEN]);
            }
            _ => {}
        }
    }

    fn process_status(&self, rec: &[u8]) {
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_POWEROFF;
        self.core.publish_on_off("power", !is_off);
        let error = rec[ERROR_OFFSET];
        self.core.publish_property(
            "error_message",
            bdh_error_name(error).unwrap_or("None").into(),
        );
        self.core.publish_on_off("error", error != 0);
        self.core.publish_property(
            "status",
            bdh_state_name(state).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "course",
            named(COURSE, rec[COURSE_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "download_course",
            named(DOWNLOAD_COURSE, rec[DOWNLOAD_COURSE_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "dry_level",
            named(DRY_LEVEL, rec[DRY_LEVEL_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "eco_hybrid",
            named(DRYING_MODE, rec[DRYING_MODE_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "buzzer",
            named(BUZZER, rec[BUZZER_OFFSET]).unwrap_or("None").into(),
        );
        let remain = if is_off {
            0
        } else {
            i64::from(u16::from_be_bytes([
                rec[REMAIN_TIME_OFFSET],
                rec[REMAIN_TIME_OFFSET + 1],
            ]))
        };
        let initial_raw = u16::from_be_bytes([rec[INITIAL_TIME_OFFSET], rec[INITIAL_TIME_OFFSET + 1]]);
        let initial = if is_off { 0 } else { i64::from(initial_raw) };
        let energy = u16::from_be_bytes([rec[SPENT_POWER_OFFSET], rec[SPENT_POWER_OFFSET + 1]]);
        let delay = u16::from_be_bytes([rec[DELAY_OFFSET], rec[DELAY_OFFSET + 1]]);

        self.core
            .publish_property("remaining_time", PropertyValue::Int(remain));
        self.core
            .publish_property("initial_time", PropertyValue::Int(initial));
        self.core
            .publish_property("energy", PropertyValue::Int(i64::from(energy)));

        let opt1 = rec[OPT1_OFFSET];
        self.core
            .publish_on_off("wrinkle_care", opt1 & OPT1_WRINKLE_CARE != 0);
        self.core
            .publish_on_off("drum_light", opt1 & OPT1_DRUM_LIGHT != 0);
        self.core
            .publish_on_off("damp_dry_signal", opt1 & OPT1_DAMP_DRY_SIGNAL != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core
            .publish_on_off("detect_load", opt2 & OPT2_DETECT_LOAD != 0);
        self.core
            .publish_on_off("remote_start", opt2 & OPT2_REMOTE_START != 0);
        self.core
            .publish_on_off("door_lock", opt2 & OPT2_DOOR_LOCK == 0);
        self.core
            .publish_on_off("child_lock", opt2 & OPT2_CHILD_LOCK == 0);
        self.core
            .publish_property("delay", PropertyValue::Int(i64::from(delay)));

        {
            let mut st = self.state.lock();
            st.reported.insert(SETTING_COURSE, u16::from(rec[COURSE_OFFSET]));
            st.reported
                .insert(SETTING_DOWNLOAD, u16::from(rec[DOWNLOAD_COURSE_OFFSET]));
            st.reported
                .insert(SETTING_DRY_LEVEL, u16::from(rec[DRY_LEVEL_OFFSET]));
            st.reported
                .insert(SETTING_DRYING_MODE, u16::from(rec[DRYING_MODE_OFFSET]));
            st.reported
                .insert(SETTING_WRINKLE_CARE, u16::from(opt1 & OPT1_WRINKLE_CARE != 0));
            st.reported.insert(
                SETTING_DAMP_DRY_SIGNAL,
                u16::from(opt1 & OPT1_DAMP_DRY_SIGNAL != 0),
            );
            st.reported.insert(SETTING_DELAY, delay);
            st.reported_initial_time = Some(initial_raw);
        }
        self.publish_settings();
    }

    fn publish_settings(&self) {
        let (val_course, val_dl, val_dry, val_mode, val_wrinkle, val_damp, val_timer, val_delay) = {
            let st = self.state.lock();
            let v = |id: u8| st.pending.get(&id).or_else(|| st.reported.get(&id)).copied();
            (
                v(SETTING_COURSE),
                v(SETTING_DOWNLOAD),
                v(SETTING_DRY_LEVEL),
                v(SETTING_DRYING_MODE),
                v(SETTING_WRINKLE_CARE),
                v(SETTING_DAMP_DRY_SIGNAL),
                st.pending.get(&SETTING_TIMER).copied(),
                v(SETTING_DELAY),
            )
        };
        self.core.publish_property(
            "set_course",
            val_course
                .and_then(|c| named(COURSE, c as u8))
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "set_download_course",
            val_dl
                .and_then(|c| named(DOWNLOAD_COURSE, c as u8))
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "set_dry_level",
            val_dry
                .and_then(|c| named(DRY_LEVEL, c as u8))
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "set_eco_hybrid",
            val_mode
                .and_then(|c| named(DRYING_MODE, c as u8))
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "set_wrinkle_care",
            match val_wrinkle {
                Some(v) => if v != 0 { "ON" } else { "OFF" },
                None => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "set_damp_dry_signal",
            match val_damp {
                Some(v) => if v != 0 { "ON" } else { "OFF" },
                None => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "set_timer",
            val_timer.map_or_else(|| "None".into(), |v| PropertyValue::Int(i64::from(v))),
        );
        self.core.publish_property(
            "set_delay_end",
            val_delay.map_or_else(
                || "None".into(),
                |v| PropertyValue::Int(((f64::from(v)) / 60.0).round() as i64),
            ),
        );
    }

    fn pick(&self, id: u8, value: Option<u16>) {
        if let Some(v) = value {
            self.state.lock().pending.insert(id, v);
        }
        self.publish_settings();
    }

    fn clear_pending(&self, keep_course: bool) {
        {
            let mut st = self.state.lock();
            let saved = st.pending.get(&SETTING_COURSE).copied();
            st.pending.clear();
            if keep_course {
                if let Some(c) = saved {
                    st.pending.insert(SETTING_COURSE, c);
                }
            }
        }
        self.publish_settings();
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
            "power" => {
                if value == "ON" {
                    self.core.send(&command(&[(SETTING_POWER, POWER_ON)]));
                } else if value == "OFF" {
                    self.core.send(&command(&[(SETTING_POWER, POWER_OFF)]));
                }
            }
            "pause" => {
                self.core.send(&command(&[(SETTING_RUN, RUN_PAUSE)]));
            }
            "resume" => {
                let pairs = {
                    let st = self.state.lock();
                    let has_pending = SETTINGS_ORDER.iter().any(|id| st.pending.contains_key(id));
                    let mut p = Vec::new();
                    if has_pending {
                        let mut base: HashMap<u8, u16> =
                            SETTINGS_ORDER.iter().map(|&id| (id, 0)).collect();
                        for &id in SETTINGS_ORDER {
                            if let Some(&v) = st.reported.get(&id) {
                                base.insert(id, v);
                            }
                        }
                        let course = st.reported.get(&SETTING_COURSE).copied().map(|c| c as u8);
                        let def_timer = course.and_then(course_defaults).map_or(0, |d| d.timer);
                        let timer_val = if def_timer != 0 {
                            st.reported_initial_time.unwrap_or(def_timer)
                        } else {
                            0
                        };
                        base.insert(SETTING_TIMER, timer_val);
                        for &id in SETTINGS_ORDER {
                            let val = st.pending.get(&id).or_else(|| base.get(&id)).copied().unwrap_or(0);
                            p.push((id, val));
                        }
                    }
                    p.push((SETTING_RUN, RUN_RESUME));
                    p
                };
                self.core.send(&command(&pairs));
                self.clear_pending(true);
            }
            "start_course" => {
                let maybe_pairs = {
                    let st = self.state.lock();
                    let code = st
                        .pending
                        .get(&SETTING_COURSE)
                        .or_else(|| st.reported.get(&SETTING_COURSE))
                        .copied()
                        .map(|c| c as u8);
                    match code {
                        Some(COURSE_DOWNLOAD) => {
                            let id = st.reported.get(&SETTING_DOWNLOAD).copied().map(|c| c as u8);
                            let picked = st.pending.get(&SETTING_DOWNLOAD).copied().map(|c| c as u8);
                            if let (Some(id), true) = (id, picked.is_none() || picked == id) {
                                download_defaults(id).map(|(base, d)| {
                                    let base_map = to_settings_map(d);
                                    let mut p = vec![
                                        (SETTING_COURSE, u16::from(COURSE_DOWNLOAD)),
                                        (SETTING_DOWNLOAD, u16::from(id)),
                                        (SETTING_DOWNLOAD_BASE, u16::from(base)),
                                    ];
                                    for &sid in SETTINGS_ORDER {
                                        let val = st
                                            .pending
                                            .get(&sid)
                                            .or_else(|| base_map.get(&sid))
                                            .copied()
                                            .unwrap_or(0);
                                        p.push((sid, val));
                                    }
                                    p.push((SETTING_RUN, RUN_START));
                                    p
                                })
                            } else {
                                None
                            }
                        }
                        Some(c) => course_defaults(c).map(|d| {
                            let has_pending =
                                SETTINGS_ORDER.iter().any(|id| st.pending.contains_key(id));
                            let mut p = vec![(SETTING_COURSE, u16::from(c))];
                            if has_pending {
                                let base_map = to_settings_map(d);
                                for &sid in SETTINGS_ORDER {
                                    let val = st
                                        .pending
                                        .get(&sid)
                                        .or_else(|| base_map.get(&sid))
                                        .copied()
                                        .unwrap_or(0);
                                    p.push((sid, val));
                                }
                            }
                            p.push((SETTING_RUN, RUN_START));
                            p
                        }),
                        None => None,
                    }
                };
                if let Some(pairs) = maybe_pairs {
                    self.core.send(&command(&pairs));
                    self.clear_pending(false);
                }
            }
            "download_course" => {
                let maybe_pairs = {
                    let st = self.state.lock();
                    let id = st
                        .pending
                        .get(&SETTING_DOWNLOAD)
                        .or_else(|| st.reported.get(&SETTING_DOWNLOAD))
                        .copied()
                        .map(|c| c as u8);
                    id.and_then(|id| {
                        download_defaults(id).map(|(base, d)| {
                            let base_map = to_settings_map(d);
                            let mut p = vec![
                                (SETTING_DOWNLOAD, u16::from(id)),
                                (SETTING_COURSE, u16::from(COURSE_DOWNLOAD)),
                                (SETTING_DOWNLOAD_BASE, u16::from(base)),
                            ];
                            for &sid in SETTINGS_ORDER {
                                let val = st
                                    .pending
                                    .get(&sid)
                                    .or_else(|| base_map.get(&sid))
                                    .copied()
                                    .unwrap_or(0);
                                p.push((sid, val));
                            }
                            p
                        })
                    })
                };
                if let Some(pairs) = maybe_pairs {
                    self.core.send(&command(&pairs));
                    self.core.send(&hex_decode(STATUS_REQUEST));
                    self.clear_pending(false);
                }
            }
            "set_course" => self.pick(SETTING_COURSE, unmap(COURSE, value)),
            "set_download_course" => self.pick(SETTING_DOWNLOAD, unmap(DOWNLOAD_COURSE, value)),
            "set_dry_level" => self.pick(SETTING_DRY_LEVEL, unmap(DRY_LEVEL, value)),
            "set_eco_hybrid" => self.pick(SETTING_DRYING_MODE, unmap(DRYING_MODE, value)),
            "set_wrinkle_care" => {
                let v = match value {
                    "ON" => Some(1),
                    "OFF" => Some(0),
                    _ => None,
                };
                self.pick(SETTING_WRINKLE_CARE, v);
            }
            "set_damp_dry_signal" => {
                let v = match value {
                    "ON" => Some(1),
                    "OFF" => Some(0),
                    _ => None,
                };
                self.pick(SETTING_DAMP_DRY_SIGNAL, v);
            }
            "set_timer" => {
                let v = crate::js::number(value).and_then(|n| {
                    (n.fract() == 0.0 && (TIMER_MIN as f64..=TIMER_MAX as f64).contains(&n))
                        .then_some(n as u16)
                });
                self.pick(SETTING_TIMER, v);
            }
            "set_delay_end" => {
                let v = crate::js::number(value).and_then(|n| {
                    let h = n as i64;
                    (n.fract() == 0.0 && (h == 0 || (DELAY_MIN_HOURS..=DELAY_MAX_HOURS).contains(&h)))
                        .then_some((h * 60) as u16)
                });
                self.pick(SETTING_DELAY, v);
            }
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
    const WOOL: &str = "aaff300a00660008e2000100ec005400000100000800000000370000010000020000000800009400000080000000000000000000000000000000000200000b000000001d0000010000020000000b0000940000008000000000000000000000000000001687bb";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_wool_and_emits_captured_download_commands() {
        let meta = Metadata::new("BDH_HI2103_EU", "BDH_HI2103_EU", "2.11.242");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(WOOL));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Wool"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("Balanced"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("29"));
        assert_eq!(prop(&ha, "download_course").as_deref(), Some("Super Dry"));

        let downloads = [
            ("Gym Clothes", "AA21F0E5000201FF0A0B660AFF0C311F011E0034003F000F007000007F000047BB"),
            ("Minimize Wrinkles", "AA21F0E5000201FF0A0B720AFF0C071F021E0334003F000F017000007F0000ACBB"),
            ("Rack Dry", "AA21F0E5000201FF0A0B8C0AFF0C0C1F011E0034003F000F0070001E7F000064BB"),
            ("Silent", "AA21F0E5000201FF0A0B7E0AFF0C2D1F021E0034003F000F007000007F000072BB"),
        ];
        for (name, expected) in downloads {
            thinq.reset_recorder();
            dev.set_property("set_download_course", name);
            dev.set_property("download_course", "");
            let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
            assert_eq!(
                sent,
                vec![
                    expected.to_string(),
                    "AA12F0ED1121010000001804111200005EBB".to_string(),
                ],
                "{name}"
            );
        }
    }
}
