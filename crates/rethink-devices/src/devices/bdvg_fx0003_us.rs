//! LG DLEX6700B electric dryer (`BDVG_FX0003_US`): long AABB frames (class `0x30`) carrying
//! 42-byte status records, door/power/completion events, and serialized `F0 E5` writes.

use crate::device_trait::DeviceHandler;
use crate::devices::fafxu22007::{ENDING_MELODIES, SPLASH_SCREENS};
use rethink_core::device_base::{
    default_config, AabbDeviceCore, AabbWriteQueue, AwaitedWrite, WriteModel, WRITE_TIMEOUT,
};
use rethink_core::ha::HaConnection;
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const CLASS_BYTE: u8 = 0x30;
const ENVELOPE_TYPE: u8 = 0x0a;
const INNER_TYPE_OFFSET: usize = 10;
const INNER_LEN_OFFSET: usize = 11;

const RECORD_LEN: usize = 42;

const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_INNER_LEN: usize = 2 * RECORD_LEN;
const STATUS_RECORD_OFFSET: usize = 13 + RECORD_LEN;
const STATUS_BUF_LEN: usize = 13 + STATUS_INNER_LEN + 1;

const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_INNER_LEN: usize = RECORD_LEN;
const SINGLE_RECORD_OFFSET: usize = 13;
const SINGLE_BUF_LEN: usize = 13 + SINGLE_INNER_LEN + 1;

const STATUS_REQUEST: &[u8] = &[
    0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x04, 0x11, 0x12, 0x00, 0x00,
];
const STATUS_REQUEST_TYPE: u8 = 0xed;

const SET_PROPERTY: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff];
const PROP_POWER: u8 = 0x02;
const PROP_CYCLE: u8 = 0x03;
const PROP_COURSE: u8 = 0x0a;
const PROP_DOWNLOADED_COURSE: u8 = 0x0b;
const PROP_BUZZER: u8 = 0x13;
const PROP_CYCLE_OPTIMIZATION: u8 = 0x4c;
const PROP_MELODY: u8 = 0x50;
const PROP_SPLASH_SCREEN: u8 = 0x51;
const PROP_KEEP_FRESH: u8 = 0x57;
const PROP_CURRENT_TIME_DISPLAY: u8 = 0x58;

const COURSE_DOWNLOADED: u8 = 0xff;
const COURSE_TIMED_DRY: u8 = 21;

const WRITE_REPLY_TYPE: u8 = 0xe6;
const WRITE_REPLY_PROP_OFFSET: usize = 7;
const WRITE_REPLY_RESULT_OFFSET: usize = 8;
const WRITE_REFUSED: u8 = 0x03;

const COMMAND_TYPE: u8 = 0x24;
const COMMAND_REPLY_TYPE: u8 = 0x00;

const EVENT_FRAME_TYPE: u8 = 0x03;
const COMPLETION_PREFIX: &[u8] = &[0x03, 0x0a, 0x02, 0x04, 0x03];
const COMPLETION_INNER_LEN: usize = 28;
const EVENT_INNER_LEN: usize = 18;
const EVENT_PAYLOAD_OFFSET: usize = 13;
const EVENT_ID_OFFSET: usize = 0;
const EVENT_POWER_OFFSET: usize = 9;
const EVENT_DOOR_OFFSET: usize = 16;
const DOOR_OPEN: u8 = 1;
const EVENT_ID_DOOR: u8 = 0x0a;

const DRY_LEVEL_OFFSET: usize = 1;
const TEMP_OFFSET: usize = 3;
const COURSE_OFFSET: usize = 5;
const REMAIN_TIME_OFFSET: usize = 9;
const INITIAL_TIME_OFFSET: usize = 11;
const STATE_OFFSET: usize = 13;
const SPENT_POWER_OFFSET: usize = 17;
const BUZZER_OFFSET: usize = 19;
const DOWNLOADED_COURSE_OFFSET: usize = 23;
const OPT1_OFFSET: usize = 24;
const OPT2_OFFSET: usize = 25;
const OPT3_OFFSET: usize = 26;
const OPT4_OFFSET: usize = 27;
const OPT5_OFFSET: usize = 28;
const MELODY_OFFSET: usize = 32;
const SPLASH_SCREEN_OFFSET: usize = 33;

const OPT1_LOW_STATIC: u8 = 0x02;
const OPT1_WRINKLE_CARE: u8 = 0x08;
const OPT1_DRUM_LIGHT: u8 = 0x20;
const OPT1_DAMP_DRY_SIGNAL: u8 = 0x40;
const OPT2_STEAM: u8 = 0x04;
const OPT2_ENERGY_SAVER: u8 = 0x80;
const OPT3_CONTROL_LOCK: u8 = 0x10;
const OPT3_REMOTE_START: u8 = 0x40;
const OPT4_CYCLE_OPTIMIZATION: u8 = 0x01;
const OPT4_REMOTE_MAINTAIN: u8 = 0x02;
const OPT5_KEEP_FRESH: u8 = 0x10;
const OPT5_CURRENT_TIME_DISPLAY: u8 = 0x20;

const STATE_POWEROFF: u8 = 0;

const NO_COURSE: u8 = 0;
const NO_COURSE_REASON: &str = "No course selected: choose a course first";
const OFF_REASON: &str = "The dryer is off";
const NO_STATUS_REASON: &str = "No status from the dryer: try again";
const DRY_TIME_REASON: &str = "Dry Time is for Timed Dry only";
const TIMED_DRY_LEVEL_REASON: &str = "Timed Dry has no dry level";
const DRY_TIME_MIN: i32 = 10;
const DRY_TIME_MAX: i32 = 100;
const DRY_TIME_STEP: i32 = 10;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum BundleKey {
    Temp,
    DryLevel,
    Steam,
    DampDrySignal,
    EnergySaver,
    LowStatic,
    WrinkleCare,
    DryTime,
}

const BUNDLE: &[(u8, BundleKey, usize)] = &[
    (0x20, BundleKey::Temp, 1),
    (0x1e, BundleKey::DryLevel, 1),
    (0x3f, BundleKey::Steam, 1),
    (0x34, BundleKey::DampDrySignal, 1),
    (0x38, BundleKey::EnergySaver, 1),
    (0x36, BundleKey::LowStatic, 1),
    (0x0f, BundleKey::WrinkleCare, 1),
    (0x70, BundleKey::DryTime, 2),
];

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Initial"),
    (2, "Running"),
    (3, "Pause"),
    (4, "End"),
    (5, "Error"),
    (6, "Audible diagnosis"),
    (7, "Drying"),
    (8, "Cooling"),
    (9, "Wrinkle care"),
    (10, "Reserved"),
    (11, "Delay load"),
    (12, "Spin reserve"),
    (13, "Auto test"),
    (14, "Detecting"),
    (15, "Steam"),
    (16, "Clothing recognition"),
    (17, "Condenser clean"),
    (18, "Bedding brushing"),
    (19, "Dry refreshing"),
    (20, "Allergy care"),
    (21, "Condenser care"),
    (22, "End remote maintain on"),
    (23, "Dry ready"),
    (24, "Laundry care"),
];

const COURSES: &[(u8, &str)] = &[
    (44, "AI Dry"),
    (21, "Timed Dry"),
    (4, "Bedding"),
    (38, "Steam Sanitary"),
    (2, "Towels"),
    (10, "Delicates"),
    (7, "Normal"),
    (9, "Quick Dry"),
    (16, "Power Dry"),
    (37, "Steam Fresh"),
    (35, "Air Dry"),
    (3, "Jeans"),
    (53, "Blanket Refresh"),
    (26, "Perm. Press"),
    (29, "Heavy Duty"),
    (42, "Super Dry"),
    (8, "Active Wear"),
    (32, "Low Temp Dry"),
    (54, "Overnight Dry"),
    (51, "Easy Ironing"),
    (24, "Small Load"),
    (55, "Large Load Dry"),
    (56, "X Large Load Dry"),
];

const DOWNLOADED_COURSES: &[(u8, &str)] = &[(114, "Wrinkle Prevention"), (140, "Rack Dry")];

const DRY_LEVELS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Damp"),
    (2, "Less"),
    (3, "Normal"),
    (4, "More"),
    (5, "Very"),
];

const TEMPS: &[(u8, &str)] = &[
    (1, "Low"),
    (2, "Medium Low"),
    (3, "Medium"),
    (4, "Medium High"),
    (5, "High"),
];

const BUZZERS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Low"),
    (2, "Medium"),
    (3, "High"),
    (4, "Very High"),
];

fn label_of(table: &[(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|&&(c, _)| c == code)
        .map(|&(_, s)| s)
        .unwrap_or("None")
}

fn code_of(table: &[(u8, &str)], label: &str) -> Option<u8> {
    table.iter().find(|&&(_, s)| s == label).map(|&(c, _)| c)
}

fn options(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|&(_, s)| s).collect()
}

fn all_course_options() -> Vec<&'static str> {
    let mut out = options(COURSES);
    out.extend(options(DOWNLOADED_COURSES));
    out
}

fn write_ok(result: u8) -> bool {
    matches!(result, 0x00 | 0x10 | 0x11)
}

fn read_u16_be(buf: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([buf[offset], buf[offset + 1]])
}

fn bit_u16(byte: u8, mask: u8) -> u16 {
    if byte & mask != 0 {
        1
    } else {
        0
    }
}

fn components() -> HashMap<String, Value> {
    let entries: &[(&str, Value)] = &[
        ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","icon":"mdi:tumble-dryer"})),
        ("cycle_finished", json!({"platform":"event","unique_id":"$deviceid-cycle_finished","state_topic":"$this/cycle_finished","name":"Cycle finished","event_types":["finished"]})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":options(STATES)})),
        ("course", json!({"platform":"select","unique_id":"$deviceid-course","state_topic":"$this/course","command_topic":"$this/course/set","options":all_course_options(),"name":"Course","icon":"mdi:pin-outline"})),
        ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("resume", json!({"platform":"button","unique_id":"$deviceid-resume","command_topic":"$this/resume/set","payload_press":"","name":"Resume","icon":"mdi:play-pause"})),
        ("dry_level", json!({"platform":"select","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","command_topic":"$this/dry_level/set","options":options(DRY_LEVELS),"name":"Dry level","icon":"mdi:water-percent"})),
        ("temp", json!({"platform":"select","unique_id":"$deviceid-temp","state_topic":"$this/temp","command_topic":"$this/temp/set","options":options(TEMPS),"name":"Temperature","icon":"mdi:thermometer"})),
        ("dry_time", json!({"platform":"number","unique_id":"$deviceid-dry_time","state_topic":"$this/dry_time","command_topic":"$this/dry_time/set","min":DRY_TIME_MIN,"max":DRY_TIME_MAX,"step":DRY_TIME_STEP,"mode":"box","device_class":"duration","unit_of_measurement":"min","name":"Dry time","icon":"mdi:timer-edit-outline"})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","unit_of_measurement":"Wh","state_class":"total_increasing"})),
        ("buzzer", json!({"platform":"select","unique_id":"$deviceid-buzzer","state_topic":"$this/buzzer","command_topic":"$this/buzzer/set","options":options(BUZZERS),"name":"Buzzer","icon":"mdi:volume-high"})),
        ("door", json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"})),
        ("drum_light", json!({"platform":"binary_sensor","unique_id":"$deviceid-drum_light","state_topic":"$this/drum_light","name":"Drum light","icon":"mdi:lightbulb"})),
        ("steam", json!({"platform":"switch","unique_id":"$deviceid-steam","state_topic":"$this/steam","command_topic":"$this/steam/set","name":"Steam","icon":"mdi:weather-fog"})),
        ("wrinkle_care", json!({"platform":"switch","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","command_topic":"$this/wrinkle_care/set","name":"Wrinkle care","icon":"mdi:iron-outline"})),
        ("low_static", json!({"platform":"switch","unique_id":"$deviceid-low_static","state_topic":"$this/low_static","command_topic":"$this/low_static/set","name":"Low static","icon":"mdi:flash-off-outline"})),
        ("damp_dry_signal", json!({"platform":"switch","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","command_topic":"$this/damp_dry_signal/set","name":"Damp dry signal","icon":"mdi:bell-outline"})),
        ("energy_saver", json!({"platform":"switch","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","command_topic":"$this/energy_saver/set","name":"Energy saver","icon":"mdi:leaf"})),
        ("keep_fresh", json!({"platform":"switch","unique_id":"$deviceid-keep_fresh","state_topic":"$this/keep_fresh","command_topic":"$this/keep_fresh/set","name":"KeepFresh","icon":"mdi:rotate-3d-variant"})),
        ("current_time_display", json!({"platform":"switch","unique_id":"$deviceid-current_time_display","state_topic":"$this/current_time_display","command_topic":"$this/current_time_display/set","name":"Clock on standby","icon":"mdi:clock-outline","entity_category":"config"})),
        ("cycle_optimization", json!({"platform":"switch","unique_id":"$deviceid-cycle_optimization","state_topic":"$this/cycle_optimization","command_topic":"$this/cycle_optimization/set","name":"Cycle optimization","icon":"mdi:auto-fix","entity_category":"config"})),
        ("melody", json!({"platform":"select","unique_id":"$deviceid-melody","state_topic":"$this/melody","command_topic":"$this/melody/set","options":options(ENDING_MELODIES),"name":"Ending melody","icon":"mdi:music-note","entity_category":"config"})),
        ("splash_screen", json!({"platform":"select","unique_id":"$deviceid-splash_screen","state_topic":"$this/splash_screen","command_topic":"$this/splash_screen/set","options":options(SPLASH_SCREENS),"name":"Splash screen","icon":"mdi:monitor-shimmer","entity_category":"config"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Control lock","icon":"mdi:lock-outline","entity_category":"diagnostic"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        ("remote_maintain", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_maintain","state_topic":"$this/remote_maintain","name":"Keep Remote Start On","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        ("problem", json!({"platform":"binary_sensor","unique_id":"$deviceid-problem","state_topic":"$this/problem","name":"Problem","device_class":"problem","entity_category":"diagnostic"})),
        ("problem_reason", json!({"platform":"sensor","unique_id":"$deviceid-problem_reason","state_topic":"$this/problem_reason","name":"Problem reason","icon":"mdi:alert-circle-outline","entity_category":"diagnostic"})),
    ];
    entries
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum CompletionState {
    #[default]
    Unarmed,
    Armed,
    Completed,
}

#[derive(Clone, Copy)]
struct CourseState {
    code: u8,
    on: bool,
}

#[derive(Default)]
struct State {
    completion: CompletionState,
    pending_power: Option<String>,
    problem: Option<String>,
    course: Option<CourseState>,
    bundle: HashMap<BundleKey, u16>,
    unsent: HashMap<BundleKey, u16>,
}

struct Model {
    core: Arc<AabbDeviceCore>,
    state: Mutex<State>,
}

impl Model {
    fn set_problem(&self, st: &mut State, reason: Option<String>) {
        if st.problem == reason {
            return;
        }
        st.problem = reason;
        self.publish_problem_locked(st);
    }

    fn publish_problem_locked(&self, st: &State) {
        self.core.publish_on_off("problem", st.problem.is_some());
        self.core
            .publish_property("problem_reason", st.problem.as_deref().unwrap_or("OK").into());
    }
}

impl WriteModel for Model {
    fn write_type(&self, inner: &[u8]) -> Option<u8> {
        if inner.len() >= 2 && inner[0] == SET_PROPERTY[0] {
            let t = inner[1];
            if t == SET_PROPERTY[1] || t == COMMAND_TYPE || t == STATUS_REQUEST_TYPE {
                return Some(t);
            }
        }
        None
    }

    fn write_timed_out(&self, _expired: &[AwaitedWrite]) {}
}

pub struct Device {
    model: Arc<Model>,
    writes: AabbWriteQueue,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        Self::with_write_timeout(ha, thinq, meta, WRITE_TIMEOUT)
    }

    fn with_write_timeout(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        write_timeout: Duration,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let model = Arc::new(Model {
            core: core.clone(),
            state: Mutex::default(),
        });
        let writes = AabbWriteQueue::new_resuming(core.clone(), model.clone(), write_timeout);
        let this = Arc::new(Self { model, writes });
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| dev.process_data(data)));
        this
    }

    fn write_props(&self, pairs: &[u8]) {
        let mut inner = Vec::with_capacity(SET_PROPERTY.len() + 1 + pairs.len());
        inner.extend_from_slice(SET_PROPERTY);
        inner.push((pairs.len() / 2) as u8);
        inner.extend_from_slice(pairs);
        self.writes.queue_write(move || Some(inner), None);
    }

    fn write_bundle(&self, key: BundleKey, value: u16) {
        let need_status_query = {
            let mut st = self.model.state.lock();
            if st.course.is_some_and(|c| c.on && c.code == NO_COURSE) {
                self.model
                    .set_problem(&mut st, Some(NO_COURSE_REASON.to_string()));
                return;
            }
            if key == BundleKey::DryLevel {
                let current = st
                    .unsent
                    .get(&key)
                    .or_else(|| st.bundle.get(&key))
                    .copied();
                if current != Some(value) {
                    st.unsent.insert(BundleKey::EnergySaver, 0);
                }
            }
            st.unsent.insert(key, value);
            !st.course.is_some_and(|c| c.on)
        };
        if need_status_query {
            let model = self.model.clone();
            self.writes.queue_write(
                move || {
                    let st = model.state.lock();
                    if st.course.is_some_and(|c| c.on) {
                        None
                    } else {
                        Some(STATUS_REQUEST.to_vec())
                    }
                },
                None,
            );
        }
        let model = self.model.clone();
        self.writes.queue_write(
            move || {
                let mut st = model.state.lock();
                let reason = match st.course {
                    None => Some(NO_STATUS_REASON),
                    Some(c) if !c.on => Some(OFF_REASON),
                    Some(c) if c.code == NO_COURSE => Some(NO_COURSE_REASON),
                    _ => None,
                };
                if reason.is_some() || st.bundle.len() != BUNDLE.len() {
                    st.unsent.clear();
                    model.set_problem(&mut st, Some(reason.unwrap_or(NO_STATUS_REASON).to_string()));
                    return None;
                }
                let unsent: Vec<(BundleKey, u16)> = st.unsent.drain().collect();
                for (k, v) in unsent {
                    st.bundle.insert(k, v);
                }
                if st.course.is_some_and(|c| c.code == COURSE_TIMED_DRY) {
                    st.bundle.insert(BundleKey::DryLevel, 0);
                } else {
                    st.bundle.insert(BundleKey::DryTime, 0);
                }
                let mut inner = Vec::with_capacity(SET_PROPERTY.len() + 1 + 17);
                inner.extend_from_slice(SET_PROPERTY);
                inner.push(BUNDLE.len() as u8);
                for &(prop, k, width) in BUNDLE {
                    let v = *st.bundle.get(&k)?;
                    inner.push(prop);
                    if width == 2 {
                        inner.push((v >> 8) as u8);
                        inner.push((v & 0xff) as u8);
                    } else {
                        inner.push(v as u8);
                    }
                }
                Some(inner)
            },
            None,
        );
    }

    fn process_data(&self, buf: &[u8]) {
        if let Some(inner) = self.model.core.process_data_envelope(buf) {
            self.process_aabb(inner);
            self.writes.next_write();
        }
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 2 || buf[0] != CLASS_BYTE {
            return;
        }
        if buf[1] == WRITE_REPLY_TYPE {
            self.process_write_reply(buf);
            return;
        }
        if buf[1] == COMMAND_REPLY_TYPE && buf.get(2) == Some(&COMMAND_TYPE) {
            self.writes.write_answered(COMMAND_TYPE, |_| true);
            return;
        }
        if buf.len() < 13 || buf[1] != ENVELOPE_TYPE {
            return;
        }
        let inner_len = usize::from(read_u16_be(buf, INNER_LEN_OFFSET));
        match buf[INNER_TYPE_OFFSET] {
            STATUS_FRAME_TYPE => {
                if inner_len == STATUS_INNER_LEN {
                    self.process_status(buf, STATUS_RECORD_OFFSET, STATUS_BUF_LEN);
                }
            }
            SINGLE_STATUS_FRAME_TYPE => {
                if inner_len == SINGLE_INNER_LEN {
                    self.writes.write_answered(STATUS_REQUEST_TYPE, |_| true);
                    self.process_status(buf, SINGLE_RECORD_OFFSET, SINGLE_BUF_LEN);
                }
            }
            EVENT_FRAME_TYPE => {
                if inner_len == COMPLETION_INNER_LEN
                    && buf.len() == 13 + COMPLETION_INNER_LEN + 1
                    && &buf[13..18] == COMPLETION_PREFIX
                {
                    let fire = {
                        let mut st = self.model.state.lock();
                        if st.completion == CompletionState::Armed {
                            st.completion = CompletionState::Completed;
                            true
                        } else {
                            false
                        }
                    };
                    if fire {
                        let event = json!({"event_type": "finished"}).to_string();
                        self.model
                            .core
                            .ha
                            .publish_event(&self.model.core.id, "cycle_finished", &event);
                    }
                    return;
                }
                if inner_len == EVENT_INNER_LEN && buf.len() >= EVENT_PAYLOAD_OFFSET + EVENT_INNER_LEN {
                    self.process_event(&buf[EVENT_PAYLOAD_OFFSET..EVENT_PAYLOAD_OFFSET + EVENT_INNER_LEN]);
                }
            }
            _ => {}
        }
    }

    fn process_write_reply(&self, buf: &[u8]) {
        let Some(write) = self.writes.write_answered(SET_PROPERTY[1], |_| true) else {
            return;
        };
        self.take_in_write(&write.inner, buf);
        if !write.own {
            return;
        }
        let count = usize::from(buf.get(WRITE_REPLY_PROP_OFFSET - 1).copied().unwrap_or(0));
        let failed = (0..count)
            .filter_map(|i| buf.get(WRITE_REPLY_RESULT_OFFSET + 2 * i).copied())
            .find(|&r| !write_ok(r));
        let pending = {
            let mut st = self.model.state.lock();
            self.model.set_problem(
                &mut st,
                failed.map(|r| format!("Command refused (0x{r:02x})")),
            );
            if buf.len() > WRITE_REPLY_RESULT_OFFSET && buf[WRITE_REPLY_PROP_OFFSET] == PROP_POWER {
                st.pending_power.take()
            } else {
                None
            }
        };
        if let Some(value) = pending {
            if buf[WRITE_REPLY_RESULT_OFFSET] != WRITE_REFUSED {
                self.model.core.publish_property("power", value.into());
            }
        }
    }

    fn take_in_write(&self, inner: &[u8], reply: &[u8]) {
        let count = usize::from(reply.get(WRITE_REPLY_PROP_OFFSET - 1).copied().unwrap_or(0));
        let mut results = HashMap::with_capacity(count);
        for i in 0..count {
            if let (Some(&prop), Some(&res)) = (
                reply.get(WRITE_REPLY_PROP_OFFSET + 2 * i),
                reply.get(WRITE_REPLY_RESULT_OFFSET + 2 * i),
            ) {
                results.insert(prop, res);
            }
        }
        let accepted = |prop: u8| {
            results
                .get(&prop)
                .copied()
                .is_some_and(write_ok)
        };

        let record_at = WRITE_REPLY_PROP_OFFSET + 2 * count;
        if (accepted(PROP_COURSE) || accepted(PROP_CYCLE_OPTIMIZATION))
            && reply.len() >= record_at + RECORD_LEN
        {
            self.take_in_course(&reply[record_at..record_at + RECORD_LEN]);
        }

        let Some(&write_count) = inner.get(SET_PROPERTY.len()) else {
            return;
        };
        let mut values = Vec::new();
        let mut at = SET_PROPERTY.len() + 1;
        for _ in 0..usize::from(write_count) {
            let Some(&prop) = inner.get(at) else {
                return;
            };
            let entry = BUNDLE.iter().find(|&&(p, _, _)| p == prop);
            let width = entry.map(|&(_, _, w)| w).unwrap_or(1);
            if inner.len() < at + 1 + width {
                return;
            }
            if let Some(&(_, key, w)) = entry {
                if accepted(prop) {
                    let v = if w == 2 {
                        read_u16_be(inner, at + 1)
                    } else {
                        u16::from(inner[at + 1])
                    };
                    values.push((key, v));
                }
            }
            at += 1 + width;
        }
        let mut st = self.model.state.lock();
        for (k, v) in values {
            st.bundle.insert(k, v);
        }
    }

    fn process_event(&self, payload: &[u8]) {
        let on = payload[EVENT_POWER_OFFSET] != 0;
        if !on {
            let mut st = self.model.state.lock();
            st.completion = CompletionState::Unarmed;
            if let Some(c) = st.course.as_mut() {
                c.on = false;
            }
        }
        self.model.core.publish_on_off("power", on);
        if payload[EVENT_ID_OFFSET] == EVENT_ID_DOOR {
            self.model
                .core
                .publish_on_off("door", payload[EVENT_DOOR_OFFSET] == DOOR_OPEN);
        }
    }

    fn take_in_course(&self, rec: &[u8]) {
        let code = rec[COURSE_OFFSET];
        let is_off = rec[STATE_OFFSET] == STATE_POWEROFF;
        let timed_dry = code == COURSE_TIMED_DRY;
        let initial_time = read_u16_be(rec, INITIAL_TIME_OFFSET);

        {
            let mut st = self.model.state.lock();
            st.course = Some(CourseState { code, on: !is_off });
            if st.problem.as_deref() == Some(NO_COURSE_REASON) && code != NO_COURSE {
                self.model.set_problem(&mut st, None);
            }
            st.bundle.insert(BundleKey::Temp, u16::from(rec[TEMP_OFFSET]));
            st.bundle
                .insert(BundleKey::DryLevel, u16::from(rec[DRY_LEVEL_OFFSET]));
            st.bundle.insert(
                BundleKey::DryTime,
                if timed_dry { initial_time } else { 0 },
            );
        }

        let core = &self.model.core;
        let course_label = if code == COURSE_DOWNLOADED {
            label_of(DOWNLOADED_COURSES, rec[DOWNLOADED_COURSE_OFFSET])
        } else {
            label_of(COURSES, code)
        };
        core.publish_property("course", course_label.into());
        core.publish_property("dry_level", label_of(DRY_LEVELS, rec[DRY_LEVEL_OFFSET]).into());
        core.publish_property("temp", label_of(TEMPS, rec[TEMP_OFFSET]).into());
        if timed_dry && !is_off {
            core.publish_property("dry_time", i64::from(initial_time).into());
        } else {
            core.publish_property("dry_time", "None".into());
        }
    }

    fn process_status(&self, buf: &[u8], record_offset: usize, expected_len: usize) {
        if buf.len() != expected_len || buf.len() < record_offset + RECORD_LEN {
            return;
        }
        let rec = &buf[record_offset..record_offset + RECORD_LEN];
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_POWEROFF;

        {
            let mut st = self.model.state.lock();
            if matches!(state, 0 | 1) {
                st.completion = CompletionState::Unarmed;
            } else if st.completion != CompletionState::Completed && matches!(state, 7 | 8) {
                st.completion = CompletionState::Armed;
            }
        }
        self.take_in_course(rec);
        {
            let mut st = self.model.state.lock();
            st.bundle
                .insert(BundleKey::Steam, bit_u16(rec[OPT2_OFFSET], OPT2_STEAM));
            st.bundle.insert(
                BundleKey::DampDrySignal,
                bit_u16(rec[OPT1_OFFSET], OPT1_DAMP_DRY_SIGNAL),
            );
            st.bundle.insert(
                BundleKey::EnergySaver,
                bit_u16(rec[OPT2_OFFSET], OPT2_ENERGY_SAVER),
            );
            st.bundle.insert(
                BundleKey::LowStatic,
                bit_u16(rec[OPT1_OFFSET], OPT1_LOW_STATIC),
            );
            st.bundle.insert(
                BundleKey::WrinkleCare,
                bit_u16(rec[OPT1_OFFSET], OPT1_WRINKLE_CARE),
            );
        }

        let core = &self.model.core;
        core.publish_on_off("power", !is_off);
        core.publish_property("status", label_of(STATES, state).into());
        core.publish_property("buzzer", label_of(BUZZERS, rec[BUZZER_OFFSET]).into());
        core.publish_property(
            "remaining_time",
            if is_off {
                0i64
            } else {
                i64::from(read_u16_be(rec, REMAIN_TIME_OFFSET))
            }
            .into(),
        );
        core.publish_property(
            "initial_time",
            if is_off {
                0i64
            } else {
                i64::from(read_u16_be(rec, INITIAL_TIME_OFFSET))
            }
            .into(),
        );
        core.publish_property("energy", i64::from(read_u16_be(rec, SPENT_POWER_OFFSET)).into());
        core.publish_flag("drum_light", rec[OPT1_OFFSET], OPT1_DRUM_LIGHT);
        core.publish_flag("low_static", rec[OPT1_OFFSET], OPT1_LOW_STATIC);
        core.publish_flag("wrinkle_care", rec[OPT1_OFFSET], OPT1_WRINKLE_CARE);
        core.publish_flag("damp_dry_signal", rec[OPT1_OFFSET], OPT1_DAMP_DRY_SIGNAL);
        core.publish_flag("steam", rec[OPT2_OFFSET], OPT2_STEAM);
        core.publish_flag("energy_saver", rec[OPT2_OFFSET], OPT2_ENERGY_SAVER);
        core.publish_flag("child_lock", rec[OPT3_OFFSET], OPT3_CONTROL_LOCK);
        core.publish_flag("remote_start", rec[OPT3_OFFSET], OPT3_REMOTE_START);
        core.publish_flag(
            "cycle_optimization",
            rec[OPT4_OFFSET],
            OPT4_CYCLE_OPTIMIZATION,
        );
        core.publish_flag("remote_maintain", rec[OPT4_OFFSET], OPT4_REMOTE_MAINTAIN);
        core.publish_flag("keep_fresh", rec[OPT5_OFFSET], OPT5_KEEP_FRESH);
        core.publish_flag(
            "current_time_display",
            rec[OPT5_OFFSET],
            OPT5_CURRENT_TIME_DISPLAY,
        );
        core.publish_property("melody", label_of(ENDING_MELODIES, rec[MELODY_OFFSET]).into());
        core.publish_property(
            "splash_screen",
            label_of(SPLASH_SCREENS, rec[SPLASH_SCREEN_OFFSET]).into(),
        );
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.model.core.id
    }

    fn start(&self) {
        self.model.core.send(STATUS_REQUEST);
        let st = self.model.state.lock();
        self.model.publish_problem_locked(&st);
    }

    fn retire(&self) {
        self.writes.stop();
    }

    fn drop_device(&self) {
        self.writes.stop();
        self.model.core.drop_device();
    }

    fn publish_config(&self) {
        self.model.core.republish_config();
    }

    fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "start" => self.write_props(&[PROP_CYCLE, 1]),
            "pause" => self.write_props(&[PROP_CYCLE, 2]),
            "resume" => self.write_props(&[PROP_CYCLE, 3]),
            "steam" | "damp_dry_signal" | "energy_saver" | "low_static" | "wrinkle_care" => {
                let v = match value {
                    "ON" => 1,
                    "OFF" => 0,
                    _ => return,
                };
                let key = match prop {
                    "steam" => BundleKey::Steam,
                    "damp_dry_signal" => BundleKey::DampDrySignal,
                    "energy_saver" => BundleKey::EnergySaver,
                    "low_static" => BundleKey::LowStatic,
                    _ => BundleKey::WrinkleCare,
                };
                self.write_bundle(key, v);
            }
            "power" => {
                let v = match value {
                    "ON" => 1,
                    "OFF" => 0,
                    _ => return,
                };
                self.model.state.lock().pending_power = Some(value.to_string());
                self.write_props(&[PROP_POWER, v]);
            }
            "buzzer" => {
                if let Some(idx) = code_of(BUZZERS, value) {
                    self.write_props(&[PROP_BUZZER, idx]);
                }
            }
            "dry_level" => {
                let Some(idx) = code_of(DRY_LEVELS, value) else {
                    return;
                };
                {
                    let mut st = self.model.state.lock();
                    if st.course.is_some_and(|c| c.on && c.code == COURSE_TIMED_DRY) {
                        self.model
                            .set_problem(&mut st, Some(TIMED_DRY_LEVEL_REASON.to_string()));
                        return;
                    }
                }
                self.write_bundle(BundleKey::DryLevel, u16::from(idx));
            }
            "temp" => {
                if let Some(idx) = code_of(TEMPS, value) {
                    self.write_bundle(BundleKey::Temp, u16::from(idx));
                }
            }
            "dry_time" => {
                let Ok(minutes) = value.parse::<i32>() else {
                    return;
                };
                if !(DRY_TIME_MIN..=DRY_TIME_MAX).contains(&minutes)
                    || minutes % DRY_TIME_STEP != 0
                {
                    return;
                }
                {
                    let mut st = self.model.state.lock();
                    if st.course.is_some_and(|c| c.on && c.code != COURSE_TIMED_DRY) {
                        self.model
                            .set_problem(&mut st, Some(DRY_TIME_REASON.to_string()));
                        return;
                    }
                }
                self.write_bundle(BundleKey::DryTime, minutes as u16);
            }
            "current_time_display" => match value {
                "ON" => self.write_props(&[PROP_CURRENT_TIME_DISPLAY, 1]),
                "OFF" => self.write_props(&[PROP_CURRENT_TIME_DISPLAY, 0]),
                _ => {}
            },
            "cycle_optimization" | "keep_fresh" => {
                let v = match value {
                    "ON" => 1,
                    "OFF" => 0,
                    _ => return,
                };
                let p = if prop == "keep_fresh" {
                    PROP_KEEP_FRESH
                } else {
                    PROP_CYCLE_OPTIMIZATION
                };
                self.write_props(&[p, v]);
            }
            "melody" => {
                if let Some(idx) = code_of(ENDING_MELODIES, value) {
                    self.write_props(&[PROP_MELODY, idx]);
                }
            }
            "splash_screen" => {
                if let Some(idx) = code_of(SPLASH_SCREENS, value) {
                    self.write_props(&[PROP_SPLASH_SCREEN, idx]);
                }
            }
            "course" => {
                if let Some(code) = code_of(COURSES, value) {
                    self.write_props(&[PROP_COURSE, code]);
                } else if let Some(dl) = code_of(DOWNLOADED_COURSES, value) {
                    self.write_props(&[
                        PROP_COURSE,
                        COURSE_DOWNLOADED,
                        PROP_DOWNLOADED_COURSE,
                        dl,
                    ]);
                }
            }
            _ => {}
        }
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
    use rethink_core::ha::MockHaConnection;
    use rethink_core::thinq::{hex_decode, hex_encode, MockThinq2Device};

    const ID: &str = "test-id";

    // Captured frames from a DLEX6700B electric dryer.
    const DIAL_AI_DRY: &str = "aaff300a00660002e7000100ec005400000001003500000000140014010000020000041c00000000000081030000000001000000000000000000030004002c00000000290029010000020000041b000000000000810300000000010000000000000000d87dbb";
    const DIAL_TIMED_DRY: &str = "aaff300a0066000303000100ec005400030004002c00000000290029010000020000041b000000000000810300000000010000000000000000000000050015000000000a000a010000020000041200000000000081030000000001000000000000000066e1bb";
    const DRYING: &str = "aaff300a0066000219000100ec0054000000010015000000000a000a0100000200000112000000200000810300000000010000000000000000000000010015000000000a000a070100020000011200000000004081030000000001000000000000000061e3bb";
    const ONE_MINUTE_IN: &str = "aaff300a0066000246000100ec0054000000010015000000000a000a07010002000001120000000000408103000000000100000000000000000000000100150000000009000a07010002002b01120000000000408103000000000100000000000000006f54bb";
    const END: &str = "aaff300a006600032c000100ec00540000000100150000000001000a08070005009804120000000000408103000000000100000000000000000000000100150000000001000b16080007009c04120000002000408303000000000100000000000000007ab8bb";
    const REC_BEFORE_DRY_LEVEL_DAMP: &str = "aaff300a0066000572000100ec005400030004002c00000000290029010000020000041b0000002000008103000000000100000000000000000003000400070000000039003901000002000004030000002080008103000000000100000000000000003f28bb";
    const E6_REMOTE_ON_REPLY: &str = "aa3730e6000201ff010200000000000000000000002900290001000207680400000000200000010300000000010000000000000000bcbb";
    const E6_CHIME_REFUSED: &str = "aa3730e6000201ff011303000000000000000000002900290001000207680400000000200000010300000000010000000000000000a8bb";

    fn setup() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("BDVG_FX0003_US", "BDVG_FX0003_US", "0.0.0");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);
        (ha, thinq, dev)
    }

    #[test]
    fn decodes_courses_cycle_progress_and_energy() {
        let (ha, thinq, dev) = setup();
        let prop = |k: &str| ha.device(ID).unwrap().properties.get(k).map(|v| v.as_string());
        thinq.emit_data(&hex_decode(DIAL_AI_DRY));
        assert_eq!(prop("course").as_deref(), Some("AI Dry"));
        assert_eq!(prop("dry_level").as_deref(), Some("Normal"));
        assert_eq!(prop("temp").as_deref(), Some("Medium High"));
        assert_eq!(prop("dry_time").as_deref(), Some("None"));
        assert_eq!(prop("remaining_time").as_deref(), Some("41"));

        thinq.emit_data(&hex_decode(DIAL_TIMED_DRY));
        assert_eq!(prop("course").as_deref(), Some("Timed Dry"));
        assert_eq!(prop("dry_time").as_deref(), Some("10"));

        thinq.emit_data(&hex_decode(DRYING));
        thinq.emit_data(&hex_decode(ONE_MINUTE_IN));
        thinq.emit_data(&hex_decode(END));
        assert_eq!(prop("status").as_deref(), Some("End remote maintain on"));
        assert_eq!(prop("remote_maintain").as_deref(), Some("ON"));
        assert_eq!(prop("energy").as_deref(), Some("156"));
        dev.drop_device();
    }

    #[test]
    fn writes_bundle_clears_energy_saver_and_tracks_power_and_refusals() {
        let (_ha, thinq, dev) = setup();
        thinq.emit_data(&hex_decode(REC_BEFORE_DRY_LEVEL_DAMP));
        thinq.reset_recorder();

        dev.set_property("dry_level", "Damp");
        assert_eq!(
            thinq.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec!["AA1CF0E5000201FF0820041E013F003400380036000F007000001DBB"]
        );

        // Fresh device for power & refusal replies.
        dev.drop_device();
        let (ha2, thinq2, dev2) = setup();
        let prop2 = |k: &str| ha2.device(ID).unwrap().properties.get(k).map(|v| v.as_string());
        dev2.set_property("power", "ON");
        thinq2.emit_data(&hex_decode(E6_REMOTE_ON_REPLY));
        assert_eq!(prop2("power").as_deref(), Some("ON"));

        dev2.set_property("buzzer", "High");
        thinq2.emit_data(&hex_decode(E6_CHIME_REFUSED));
        assert_eq!(prop2("problem").as_deref(), Some("ON"));
        assert_eq!(prop2("problem_reason").as_deref(), Some("Command refused (0x03)"));
        dev2.drop_device();
    }
}
