//! LG WM6700HBA front-load washer (`FAFXU22007`): long AABB frames (class `0x20`) carrying
//! 51-byte status records, door/completion events, and serialized `F0 E5` / `F0 24` writes.

use crate::device_trait::DeviceHandler;
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

const CLASS_BYTE: u8 = 0x20;
const ENVELOPE_TYPE: u8 = 0x0a;
const INNER_TYPE_OFFSET: usize = 10;
const INNER_LEN_OFFSET: usize = 11;

const RECORD_LEN: usize = 51;

const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_INNER_LEN: usize = 2 * RECORD_LEN;
const STATUS_RECORD_OFFSET: usize = 13 + RECORD_LEN;
const STATUS_BUF_LEN: usize = 13 + STATUS_INNER_LEN + 1;

const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_INNER_LEN: usize = RECORD_LEN;
const SINGLE_RECORD_OFFSET: usize = 13;
const SINGLE_BUF_LEN: usize = 13 + SINGLE_INNER_LEN + 1;

const EVENT_FRAME_TYPE: u8 = 0x03;
const COMPLETION_PREFIX: &[u8] = &[0x08, 0x0b, 0x01, 0x0b, 0x08];
const COMPLETION_INNER_LEN: usize = 121;
const DOOR_EVENT_INNER_LEN: usize = 6;
const DOOR_EVENT_BUF_LEN: usize = 13 + DOOR_EVENT_INNER_LEN + 1;
const DOOR_EVENT_ID: u8 = 0x10;
const DOOR_OPEN: u8 = 1;
const DOOR_SHUT: u8 = 2;

const STATUS_REQUEST: &[u8] = &[
    0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x04, 0x11, 0x12, 0x00, 0x00,
];

const SET_PROPERTY: &[u8] = &[0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff];
const PROP_POWER: u8 = 0x02;
const PROP_CYCLE: u8 = 0x03;
const PROP_COURSE: u8 = 0x0a;
const PROP_DOWNLOADED_COURSE: u8 = 0x0b;
const PROP_BUZZER: u8 = 0x13;
const PROP_CYCLE_OPTIMIZATION: u8 = 0x4c;
const PROP_MELODY: u8 = 0x50;
const PROP_SPLASH_SCREEN: u8 = 0x51;
const PROP_QUICK_LOAD_SENSE: u8 = 0x56;
const PROP_CURRENT_TIME_DISPLAY: u8 = 0x58;

const COURSE_DOWNLOADED: u8 = 0xff;
const COLD_WASH_TEMP: u16 = 14; // Cold
const COLD_WASH_SOIL: u16 = 3; // Medium

const RESERVE_TIME_MIN: i32 = 60;
const RESERVE_TIME_MAX: i32 = 1140;
const RESERVE_TIME_STEP: i32 = 30;

const WRITE_REPLY_TYPE: u8 = 0xe6;
const WRITE_REPLY_COUNT_OFFSET: usize = 6;
const WRITE_REFUSED: u8 = 0x03;
const WRITE_BUSY: u8 = 0x13;

const COMMAND: &[u8] = &[0xf0, 0x24];
const COMMAND_REMOTE_MAINTAIN: u8 = 0x10;
const COMMAND_REPLY_TYPE: u8 = 0x00;
const COMMAND_OK: u8 = 0x00;

const NO_COURSE: u8 = 0;
const NO_COURSE_REASON: &str = "No course selected: choose a course first";

const SOIL_OFFSET: usize = 1;
const TEMP_OFFSET: usize = 2;
const RINSE_OFFSET: usize = 3;
const SPIN_OFFSET: usize = 4;
const COURSE_OFFSET: usize = 5;
const RESERVE_TIME_OFFSET: usize = 11;
const REMAIN_TIME_OFFSET: usize = 13;
const INITIAL_TIME_OFFSET: usize = 15;
const SPENT_POWER_OFFSET: usize = 17;
const LOADED_COURSE_OFFSET: usize = 20;
const STATE_OFFSET: usize = 21;
const TUB_CLEAN_COUNT_OFFSET: usize = 28;
const BUZZER_OFFSET: usize = 29;
const DETERGENT_LEVEL_OFFSET: usize = 30;
const SOFTENER_LEVEL_OFFSET: usize = 31;
const DETERGENT_AMOUNT_OFFSET: usize = 32;
const SOFTENER_AMOUNT_OFFSET: usize = 33;
const OPT1_OFFSET: usize = 34;
const OPT2_OFFSET: usize = 35;
const OPT3_OFFSET: usize = 36;
const OPT4_OFFSET: usize = 37;
const DOOR_LOCK_OFFSET: usize = 38;
const OPT5_OFFSET: usize = 39;
const OPT6_OFFSET: usize = 40;
const MELODY_OFFSET: usize = 45;
const SPLASH_SCREEN_OFFSET: usize = 46;
const NOZZLE_CLEAN_OFFSET: usize = 47;
const CURRENT_TIME_DISPLAY_OFFSET: usize = 47;
const SOFTENER_TANK_USE_OFFSET: usize = 48;
const OPT7_OFFSET: usize = 50;

const OPT1_COLD_WASH: u8 = 0x04;
const OPT1_TURBO_WASH: u8 = 0x20;
const OPT1_PRE_WASH: u8 = 0x40;
const OPT2_STEAM: u8 = 0x10;
const OPT3_FRESH_CARE: u8 = 0x40;
const OPT4_REMOTE_START: u8 = 0x10;
const OPT4_CONTROL_LOCK: u8 = 0x20;
const DOOR_LOCK_MASK: u8 = 0x01;
const OPT5_DRAWER_CLOSED: u8 = 0x04;
const OPT5_DELAY_START: u8 = 0x80;
const OPT6_REMOTE_MAINTAIN: u8 = 0x04;
const OPT6_CYCLE_OPTIMIZATION: u8 = 0x08;
const NOZZLE_CLEAN_MASK: u8 = 0x60;
const CURRENT_TIME_DISPLAY_MASK: u8 = 0x80;
const OPT7_QUICK_LOAD_SENSE: u8 = 0x04;

const STATE_POWEROFF: u8 = 0;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum BundleKey {
    Temp,
    Spin,
    Soil,
    Rinse,
    Steam,
    PreWash,
    ColdWash,
    TurboWash,
    FreshCare,
    ReserveTime,
}

const BUNDLE: &[(u8, BundleKey, usize)] = &[
    (0x1f, BundleKey::Temp, 1),
    (0x21, BundleKey::Spin, 1),
    (0x1e, BundleKey::Soil, 1),
    (0x20, BundleKey::Rinse, 1),
    (0x3e, BundleKey::Steam, 1),
    (0x34, BundleKey::PreWash, 1),
    (0x38, BundleKey::ColdWash, 1),
    (0x35, BundleKey::TurboWash, 1),
    (0x44, BundleKey::FreshCare, 1),
    (0x7f, BundleKey::ReserveTime, 2),
];

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Initial"),
    (2, "Pause"),
    (3, "Detecting"),
    (5, "Filling"),
    (6, "Detecting load"),
    (7, "Reserved"),
    (8, "Soak"),
    (9, "Pre-wash"),
    (11, "Running"),
    (12, "Rinsing"),
    (13, "Rinse hold"),
    (14, "Spinning"),
    (15, "Drying"),
    (16, "End"),
    (21, "Refreshing"),
    (23, "Error auto off"),
    (27, "Frozen prevent initial"),
    (28, "Frozen prevent pause"),
    (29, "Frozen prevent running"),
    (34, "Audible diagnosis"),
    (35, "Auto DT open pause"),
    (36, "Confirm start for control"),
    (37, "Clothing recognition"),
    (38, "Detergent input"),
    (39, "Softener input"),
    (40, "Pollution detecting"),
    (41, "Tub cleaning"),
    (42, "End remote maintain on"),
    (43, "Steam"),
    (47, "Laundry care"),
    (48, "EZDispense cleaning"),
];

const COURSES: &[(u8, &str)] = &[
    (5, "Allergiene"),
    (13, "Bedding"),
    (18, "Color Care"),
    (22, "Delicates"),
    (34, "Hand/Wool"),
    (35, "Heavy Duty"),
    (38, "Jeans"),
    (39, "Kids Wear"),
    (46, "Normal"),
    (47, "Overnight Wash"),
    (48, "Perm. Press"),
    (55, "Rinse + Spin"),
    (60, "Sanitary"),
    (68, "Small Load"),
    (74, "Quick Wash"),
    (78, "Spin Only"),
    (84, "Towels"),
    (85, "Tub Clean"),
    (90, "Bright Whites"),
    (99, "Active Wear"),
    (113, "Sweat Stains"),
    (114, "AI Wash"),
    (128, "ezDispense Nozzle Clean"),
    (130, "Swimwear"),
    (131, "Dresses"),
    (132, "Large Load"),
    (133, "X Large Load"),
];

const SOILS: &[(u8, &str)] = &[
    (1, "Light"),
    (2, "Medium Light"),
    (3, "Medium"),
    (4, "Medium Heavy"),
    (5, "Heavy"),
];

const TEMPS: &[(u8, &str)] = &[
    (13, "Tap Cold"),
    (14, "Cold"),
    (16, "Warm"),
    (18, "Hot"),
    (19, "Extra Hot"),
];

const RINSES: &[(u8, &str)] = &[(14, "Normal"), (15, "+1"), (16, "+2"), (17, "+3")];

const SPINS: &[(u8, &str)] = &[
    (12, "No Spin"),
    (13, "Low"),
    (14, "Medium"),
    (15, "High"),
    (16, "Extra High"),
];

const BUZZERS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Low"),
    (2, "Medium"),
    (3, "High"),
    (4, "Very High"),
];

const DISPENSE_LEVELS: &[(u8, &str)] = &[
    (0, "Off"),
    (4, "Minimum"),
    (1, "Less"),
    (2, "Normal"),
    (3, "More"),
    (5, "Maximum"),
];

const SOFTENER_TANK_USES: &[(u8, &str)] = &[(0, "Softener"), (1, "Link Tanks")];

const NOZZLE_CLEANS: &[(u8, &str)] = &[
    (0x20, "Detergent Tank"),
    (0x40, "Softener Tank"),
    (0x60, "Both Tanks"),
];

pub(crate) const ENDING_MELODIES: &[(u8, &str)] = &[
    (0, "Normal"),
    (1, "Vivaldi Winter"),
    (2, "Bach's Minuet in G major"),
    (3, "Sweet Home"),
    (4, "Breeze"),
    (5, "Old MacDonald"),
    (6, "Verdi La Traviata Drinking Song"),
    (7, "Bubble"),
    (8, "Beethoven Symphony No. 5"),
    (9, "Arirang"),
    (10, "Pachelbel's Canon"),
    (11, "Beethoven Symphony No. 9"),
];

pub(crate) const SPLASH_SCREENS: &[(u8, &str)] = &[
    (0, "Normal"),
    (1, "Snowman 1"),
    (2, "Snowman 2"),
    (3, "Hello Winter"),
    (4, "Sprout"),
    (5, "Earth Day"),
    (6, "The Sun"),
    (7, "Beach Umbrella"),
    (8, "Fall Leaves"),
    (9, "Halloween"),
    (10, "Happy New Year"),
    (11, "Christmas"),
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

fn downloaded_course_id(name: &str) -> Option<u8> {
    match name {
        "Overnight Wash" => Some(106),
        _ => None,
    }
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
        ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","icon":"mdi:washing-machine"})),
        ("cycle_finished", json!({"platform":"event","unique_id":"$deviceid-cycle_finished","state_topic":"$this/cycle_finished","name":"Cycle finished","event_types":["finished"]})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":options(STATES)})),
        ("course", json!({"platform":"select","unique_id":"$deviceid-course","state_topic":"$this/course","command_topic":"$this/course/set","options":options(COURSES),"name":"Course","icon":"mdi:pin-outline"})),
        ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("resume", json!({"platform":"button","unique_id":"$deviceid-resume","command_topic":"$this/resume/set","payload_press":"","name":"Resume","icon":"mdi:play-pause"})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","unit_of_measurement":"Wh","state_class":"total_increasing"})),
        ("reserve_time", json!({"platform":"number","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","command_topic":"$this/reserve_time/set","min":0,"max":RESERVE_TIME_MAX,"step":RESERVE_TIME_STEP,"mode":"box","name":"Delay Start time","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("delay_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-delay_start","state_topic":"$this/delay_start","name":"Delay Start","icon":"mdi:clock-plus-outline"})),
        ("door", json!({"platform":"binary_sensor","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","device_class":"door"})),
        ("drawer", json!({"platform":"binary_sensor","unique_id":"$deviceid-drawer","state_topic":"$this/drawer","name":"ezDispense drawer","device_class":"opening"})),
        ("tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
        ("buzzer", json!({"platform":"select","unique_id":"$deviceid-buzzer","state_topic":"$this/buzzer","command_topic":"$this/buzzer/set","options":options(BUZZERS),"name":"Buzzer","icon":"mdi:volume-high"})),
        ("soil", json!({"platform":"select","unique_id":"$deviceid-soil","state_topic":"$this/soil","command_topic":"$this/soil/set","options":options(SOILS),"name":"Soil level","icon":"mdi:liquid-spot"})),
        ("spin", json!({"platform":"select","unique_id":"$deviceid-spin","state_topic":"$this/spin","command_topic":"$this/spin/set","options":options(SPINS),"name":"Spin","icon":"mdi:autorenew"})),
        ("temp", json!({"platform":"select","unique_id":"$deviceid-temp","state_topic":"$this/temp","command_topic":"$this/temp/set","options":options(TEMPS),"name":"Temperature","icon":"mdi:thermometer"})),
        ("rinse", json!({"platform":"select","unique_id":"$deviceid-rinse","state_topic":"$this/rinse","command_topic":"$this/rinse/set","options":options(RINSES),"name":"Extra Rinse","icon":"mdi:water-sync"})),
        ("turbo_wash", json!({"platform":"switch","unique_id":"$deviceid-turbo_wash","state_topic":"$this/turbo_wash","command_topic":"$this/turbo_wash/set","name":"TurboWash","icon":"mdi:rocket-launch"})),
        ("cold_wash", json!({"platform":"switch","unique_id":"$deviceid-cold_wash","state_topic":"$this/cold_wash","command_topic":"$this/cold_wash/set","name":"Cold Wash","icon":"mdi:snowflake"})),
        ("pre_wash", json!({"platform":"switch","unique_id":"$deviceid-pre_wash","state_topic":"$this/pre_wash","command_topic":"$this/pre_wash/set","name":"Pre-wash","icon":"mdi:water-sync"})),
        ("steam", json!({"platform":"switch","unique_id":"$deviceid-steam","state_topic":"$this/steam","command_topic":"$this/steam/set","name":"Steam","icon":"mdi:kettle-steam"})),
        ("fresh_care", json!({"platform":"switch","unique_id":"$deviceid-fresh_care","state_topic":"$this/fresh_care","command_topic":"$this/fresh_care/set","name":"Fresh Care","icon":"mdi:air-filter"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Control Lock","icon":"mdi:lock","entity_category":"diagnostic"})),
        ("ez_dispense_detergent", json!({"platform":"sensor","unique_id":"$deviceid-ez_dispense_detergent","state_topic":"$this/ez_dispense_detergent","name":"Detergent Level","icon":"mdi:cup-water","device_class":"enum","options":options(DISPENSE_LEVELS)})),
        ("ez_dispense_softener", json!({"platform":"sensor","unique_id":"$deviceid-ez_dispense_softener","state_topic":"$this/ez_dispense_softener","name":"Softener Level","icon":"mdi:cup-water","device_class":"enum","options":options(DISPENSE_LEVELS)})),
        ("detergent_amount", json!({"platform":"sensor","unique_id":"$deviceid-detergent_amount","state_topic":"$this/detergent_amount","name":"Default detergent amount","icon":"mdi:cup-water","device_class":"volume","unit_of_measurement":"mL","entity_category":"diagnostic"})),
        ("softener_amount", json!({"platform":"sensor","unique_id":"$deviceid-softener_amount","state_topic":"$this/softener_amount","name":"Default softener amount","icon":"mdi:cup-water","device_class":"volume","unit_of_measurement":"mL","entity_category":"diagnostic"})),
        ("softener_tank_use", json!({"platform":"sensor","unique_id":"$deviceid-softener_tank_use","state_topic":"$this/softener_tank_use","name":"Softener Tank Use","icon":"mdi:link-variant","device_class":"enum","options":options(SOFTENER_TANK_USES),"entity_category":"diagnostic"})),
        ("nozzle_clean_tank", json!({"platform":"sensor","unique_id":"$deviceid-nozzle_clean_tank","state_topic":"$this/nozzle_clean_tank","name":"Nozzle Clean tank","icon":"mdi:spray-bottle","device_class":"enum","options":options(NOZZLE_CLEANS),"entity_category":"diagnostic"})),
        ("cycle_optimization", json!({"platform":"switch","unique_id":"$deviceid-cycle_optimization","state_topic":"$this/cycle_optimization","command_topic":"$this/cycle_optimization/set","name":"Cycle Optimization","icon":"mdi:sort","entity_category":"config"})),
        ("quick_load_sense", json!({"platform":"switch","unique_id":"$deviceid-quick_load_sense","state_topic":"$this/quick_load_sense","command_topic":"$this/quick_load_sense/set","name":"QuickLoadSense","icon":"mdi:scale","entity_category":"config"})),
        ("current_time_display", json!({"platform":"switch","unique_id":"$deviceid-current_time_display","state_topic":"$this/current_time_display","command_topic":"$this/current_time_display/set","name":"Clock on standby","icon":"mdi:clock-outline","entity_category":"config"})),
        ("melody", json!({"platform":"select","unique_id":"$deviceid-melody","state_topic":"$this/melody","command_topic":"$this/melody/set","options":options(ENDING_MELODIES),"name":"Ending melody","icon":"mdi:music-note","entity_category":"config"})),
        ("splash_screen", json!({"platform":"select","unique_id":"$deviceid-splash_screen","state_topic":"$this/splash_screen","command_topic":"$this/splash_screen/set","options":options(SPLASH_SCREENS),"name":"Splash screen","icon":"mdi:monitor-shimmer","entity_category":"config"})),
        ("door_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","icon":"mdi:lock","entity_category":"diagnostic"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote Start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        ("remote_maintain", json!({"platform":"switch","unique_id":"$deviceid-remote_maintain","state_topic":"$this/remote_maintain","command_topic":"$this/remote_maintain/set","name":"Keep Remote Start On","icon":"mdi:cellphone-wireless","entity_category":"config"})),
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
    loaded: u8,
    on: bool,
}

#[derive(Default)]
struct State {
    completion: CompletionState,
    bundle: HashMap<BundleKey, u16>,
    unsent: HashMap<BundleKey, u16>,
    course: Option<CourseState>,
    problem: Option<String>,
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
            if t == SET_PROPERTY[1] || t == COMMAND[1] {
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
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
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

    fn write_command(&self, command: u8, value: u8) {
        let inner = vec![COMMAND[0], COMMAND[1], command, 0x01, value];
        self.writes.queue_write(move || Some(inner), None);
    }

    fn write_bundle(&self, key: BundleKey, value: u16) {
        {
            let mut st = self.model.state.lock();
            if st.bundle.len() != BUNDLE.len() {
                return;
            }
            if st.course.is_some_and(|c| c.on && c.code == NO_COURSE) {
                self.model
                    .set_problem(&mut st, Some(NO_COURSE_REASON.to_string()));
                return;
            }
            st.unsent.insert(key, value);
            if key == BundleKey::ColdWash && value == 1 {
                st.unsent.insert(BundleKey::Temp, COLD_WASH_TEMP);
                st.unsent.insert(BundleKey::Soil, COLD_WASH_SOIL);
            }
        }
        let model = self.model.clone();
        self.writes.queue_write(
            move || {
                let mut st = model.state.lock();
                let unsent: Vec<(BundleKey, u16)> = st.unsent.drain().collect();
                for (k, v) in unsent {
                    st.bundle.insert(k, v);
                }
                let mut inner = Vec::with_capacity(SET_PROPERTY.len() + 1 + 21);
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

    fn write_cycle(&self, cycle: u8) {
        if cycle != 1 {
            return self.write_props(&[PROP_CYCLE, cycle]);
        }
        let course = self.model.state.lock().course;
        let Some(course) = course else {
            return;
        };
        if course.code != COURSE_DOWNLOADED {
            return self.write_props(&[PROP_COURSE, course.code, PROP_CYCLE, cycle]);
        }
        if let Some(id) = downloaded_course_id(label_of(COURSES, course.loaded)) {
            self.write_props(&[
                PROP_COURSE,
                COURSE_DOWNLOADED,
                PROP_DOWNLOADED_COURSE,
                id,
                PROP_CYCLE,
                cycle,
            ]);
        }
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
        if buf[1] == COMMAND_REPLY_TYPE {
            self.process_command_reply(buf);
            return;
        }
        if buf[1] == WRITE_REPLY_TYPE {
            let own = self
                .writes
                .write_answered(SET_PROPERTY[1], |_| true)
                .is_some_and(|w| w.own);
            if buf.len() <= WRITE_REPLY_COUNT_OFFSET {
                return;
            }
            let count = usize::from(buf[WRITE_REPLY_COUNT_OFFSET]);
            let record_offset = WRITE_REPLY_COUNT_OFFSET + 1 + 2 * count;
            let results: Vec<Option<u8>> = (0..count)
                .map(|i| buf.get(WRITE_REPLY_COUNT_OFFSET + 2 + 2 * i).copied())
                .collect();
            let failed = results
                .iter()
                .filter_map(|&r| r)
                .find(|&r| !write_ok(r));
            if own {
                let mut st = self.model.state.lock();
                self.model.set_problem(
                    &mut st,
                    failed.map(|r| format!("Command refused (0x{r:02x})")),
                );
            }
            if results
                .iter()
                .any(|&r| r == Some(WRITE_REFUSED) || r == Some(WRITE_BUSY))
            {
                return;
            }
            self.process_status(buf, record_offset, record_offset + RECORD_LEN);
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
                if inner_len == DOOR_EVENT_INNER_LEN {
                    self.process_door_event(buf);
                }
            }
            _ => {}
        }
    }

    fn process_command_reply(&self, buf: &[u8]) {
        if buf.len() < 4 || buf[2] != COMMAND[1] {
            return;
        }
        let own = self
            .writes
            .write_answered(COMMAND[1], |_| true)
            .is_some_and(|w| w.own);
        if !own {
            return;
        }
        let mut st = self.model.state.lock();
        self.model.set_problem(
            &mut st,
            (buf[3] != COMMAND_OK).then(|| format!("Command refused (0x{:02x})", buf[3])),
        );
    }

    fn process_door_event(&self, buf: &[u8]) {
        if buf.len() != DOOR_EVENT_BUF_LEN
            || buf[13] != DOOR_EVENT_ID
            || buf[17] != DOOR_EVENT_ID
        {
            return;
        }
        match buf[18] {
            DOOR_OPEN => self.model.core.publish_on_off("door", true),
            DOOR_SHUT => self.model.core.publish_on_off("door", false),
            _ => {}
        }
    }

    fn process_status(&self, buf: &[u8], record_offset: usize, expected_len: usize) {
        if buf.len() != expected_len || buf.len() < record_offset + RECORD_LEN {
            return;
        }
        let rec = &buf[record_offset..record_offset + RECORD_LEN];
        let state = rec[STATE_OFFSET];
        let is_off = state == STATE_POWEROFF;
        let course = rec[COURSE_OFFSET];

        {
            let mut st = self.model.state.lock();
            if matches!(state, 0 | 1) {
                st.completion = CompletionState::Unarmed;
            } else if st.completion != CompletionState::Completed
                && matches!(state, 11 | 12 | 14 | 41 | 48)
            {
                st.completion = CompletionState::Armed;
            }
            st.course = Some(CourseState {
                code: course,
                loaded: rec[LOADED_COURSE_OFFSET],
                on: !is_off,
            });
            if st.problem.as_deref() == Some(NO_COURSE_REASON) && course != NO_COURSE {
                self.model.set_problem(&mut st, None);
            }
            st.bundle.insert(BundleKey::Temp, u16::from(rec[TEMP_OFFSET]));
            st.bundle.insert(BundleKey::Spin, u16::from(rec[SPIN_OFFSET]));
            st.bundle.insert(BundleKey::Soil, u16::from(rec[SOIL_OFFSET]));
            st.bundle
                .insert(BundleKey::Rinse, u16::from(rec[RINSE_OFFSET]));
            st.bundle
                .insert(BundleKey::Steam, bit_u16(rec[OPT2_OFFSET], OPT2_STEAM));
            st.bundle.insert(
                BundleKey::PreWash,
                bit_u16(rec[OPT1_OFFSET], OPT1_PRE_WASH),
            );
            st.bundle.insert(
                BundleKey::ColdWash,
                bit_u16(rec[OPT1_OFFSET], OPT1_COLD_WASH),
            );
            st.bundle.insert(
                BundleKey::TurboWash,
                bit_u16(rec[OPT1_OFFSET], OPT1_TURBO_WASH),
            );
            st.bundle.insert(
                BundleKey::FreshCare,
                bit_u16(rec[OPT3_OFFSET], OPT3_FRESH_CARE),
            );
            st.bundle
                .insert(BundleKey::ReserveTime, read_u16_be(rec, RESERVE_TIME_OFFSET));
        }

        let core = &self.model.core;
        core.publish_on_off("power", !is_off);
        core.publish_property("status", label_of(STATES, state).into());
        let course_code = if course == COURSE_DOWNLOADED {
            rec[LOADED_COURSE_OFFSET]
        } else {
            course
        };
        core.publish_property("course", label_of(COURSES, course_code).into());
        core.publish_property("soil", label_of(SOILS, rec[SOIL_OFFSET]).into());
        core.publish_property("spin", label_of(SPINS, rec[SPIN_OFFSET]).into());
        core.publish_property("temp", label_of(TEMPS, rec[TEMP_OFFSET]).into());
        core.publish_property("rinse", label_of(RINSES, rec[RINSE_OFFSET]).into());
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
        core.publish_property(
            "reserve_time",
            i64::from(read_u16_be(rec, RESERVE_TIME_OFFSET)).into(),
        );
        core.publish_flag("delay_start", rec[OPT5_OFFSET], OPT5_DELAY_START);
        core.publish_property("tub_clean_count", i64::from(rec[TUB_CLEAN_COUNT_OFFSET]).into());
        core.publish_property("buzzer", label_of(BUZZERS, rec[BUZZER_OFFSET]).into());
        core.publish_property(
            "ez_dispense_detergent",
            label_of(DISPENSE_LEVELS, rec[DETERGENT_LEVEL_OFFSET]).into(),
        );
        core.publish_property(
            "ez_dispense_softener",
            label_of(DISPENSE_LEVELS, rec[SOFTENER_LEVEL_OFFSET]).into(),
        );
        core.publish_property("detergent_amount", i64::from(rec[DETERGENT_AMOUNT_OFFSET]).into());
        core.publish_property("softener_amount", i64::from(rec[SOFTENER_AMOUNT_OFFSET]).into());
        core.publish_property(
            "softener_tank_use",
            label_of(SOFTENER_TANK_USES, rec[SOFTENER_TANK_USE_OFFSET]).into(),
        );
        core.publish_property(
            "nozzle_clean_tank",
            label_of(NOZZLE_CLEANS, rec[NOZZLE_CLEAN_OFFSET] & NOZZLE_CLEAN_MASK).into(),
        );

        let opt1 = rec[OPT1_OFFSET];
        core.publish_flag("turbo_wash", opt1, OPT1_TURBO_WASH);
        core.publish_flag("cold_wash", opt1, OPT1_COLD_WASH);
        core.publish_flag("pre_wash", opt1, OPT1_PRE_WASH);
        core.publish_flag("steam", rec[OPT2_OFFSET], OPT2_STEAM);
        core.publish_flag("fresh_care", rec[OPT3_OFFSET], OPT3_FRESH_CARE);
        core.publish_flag("child_lock", rec[OPT4_OFFSET], OPT4_CONTROL_LOCK);
        core.publish_flag(
            "cycle_optimization",
            rec[OPT6_OFFSET],
            OPT6_CYCLE_OPTIMIZATION,
        );
        core.publish_flag("remote_maintain", rec[OPT6_OFFSET], OPT6_REMOTE_MAINTAIN);
        core.publish_flag("door_lock", rec[DOOR_LOCK_OFFSET], DOOR_LOCK_MASK);
        core.publish_flag("remote_start", rec[OPT4_OFFSET], OPT4_REMOTE_START);
        core.publish_on_off("drawer", rec[OPT5_OFFSET] & OPT5_DRAWER_CLOSED == 0);
        core.publish_flag(
            "quick_load_sense",
            rec[OPT7_OFFSET],
            OPT7_QUICK_LOAD_SENSE,
        );
        core.publish_property("melody", label_of(ENDING_MELODIES, rec[MELODY_OFFSET]).into());
        core.publish_property(
            "splash_screen",
            label_of(SPLASH_SCREENS, rec[SPLASH_SCREEN_OFFSET]).into(),
        );
        core.publish_flag(
            "current_time_display",
            rec[CURRENT_TIME_DISPLAY_OFFSET],
            CURRENT_TIME_DISPLAY_MASK,
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
            "start" => return self.write_cycle(1),
            "pause" => return self.write_cycle(2),
            "resume" => return self.write_cycle(3),
            "steam" | "pre_wash" | "cold_wash" | "turbo_wash" | "fresh_care" => {
                let v = match value {
                    "ON" => 1,
                    "OFF" => 0,
                    _ => return,
                };
                let key = match prop {
                    "steam" => BundleKey::Steam,
                    "pre_wash" => BundleKey::PreWash,
                    "cold_wash" => BundleKey::ColdWash,
                    "turbo_wash" => BundleKey::TurboWash,
                    _ => BundleKey::FreshCare,
                };
                return self.write_bundle(key, v);
            }
            "soil" | "temp" | "rinse" | "spin" => {
                let (table, key) = match prop {
                    "soil" => (SOILS, BundleKey::Soil),
                    "temp" => (TEMPS, BundleKey::Temp),
                    "rinse" => (RINSES, BundleKey::Rinse),
                    _ => (SPINS, BundleKey::Spin),
                };
                let Some(idx) = code_of(table, value) else {
                    return;
                };
                if prop == "soil" {
                    let st = self.model.state.lock();
                    let cold_wash = st
                        .unsent
                        .get(&BundleKey::ColdWash)
                        .or_else(|| st.bundle.get(&BundleKey::ColdWash))
                        .copied();
                    if cold_wash == Some(1) && u16::from(idx) > COLD_WASH_SOIL {
                        return;
                    }
                }
                return self.write_bundle(key, u16::from(idx));
            }
            "power" => match value {
                "ON" => self.write_props(&[PROP_POWER, 1]),
                "OFF" => self.write_props(&[PROP_POWER, 0]),
                _ => {}
            },
            "buzzer" => {
                if let Some(idx) = code_of(BUZZERS, value) {
                    self.write_props(&[PROP_BUZZER, idx]);
                }
            }
            "reserve_time" => {
                if let Ok(minutes) = value.parse::<i32>() {
                    if (minutes == 0
                        || (RESERVE_TIME_MIN..=RESERVE_TIME_MAX).contains(&minutes))
                        && minutes % RESERVE_TIME_STEP == 0
                    {
                        self.write_bundle(BundleKey::ReserveTime, minutes as u16);
                    }
                }
            }
            "cycle_optimization" | "quick_load_sense" | "current_time_display" => {
                let v = match value {
                    "ON" => 1,
                    "OFF" => 0,
                    _ => return,
                };
                let p = match prop {
                    "quick_load_sense" => PROP_QUICK_LOAD_SENSE,
                    "current_time_display" => PROP_CURRENT_TIME_DISPLAY,
                    _ => PROP_CYCLE_OPTIMIZATION,
                };
                self.write_props(&[p, v]);
            }
            "remote_maintain" => match value {
                "ON" => self.write_command(COMMAND_REMOTE_MAINTAIN, 1),
                "OFF" => self.write_command(COMMAND_REMOTE_MAINTAIN, 0),
                _ => {}
            },
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
                if let Some(id) = downloaded_course_id(value) {
                    self.write_props(&[
                        PROP_COURSE,
                        COURSE_DOWNLOADED,
                        PROP_DOWNLOADED_COURSE,
                        id,
                    ]);
                } else if let Some(code) = code_of(COURSES, value) {
                    self.write_props(&[PROP_COURSE, code]);
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

    // Captured from a WM6700HBA: status query reply (Normal, idle, Tap Cold, High, Medium, 46m).
    const EB_IDLE: &str = "aaff200a0045000042000100eb003300030d0e0f2e00000000000000002e002e0000002e01000600000001150402002d1e20000000000c3800000000000004010004553dbb";
    const RINSING: &str = "aaff200a00780001f4000100ec006600000e0e0f3700000000000000001200120003003701001007000001150102002d1e00000000000c380000000000000401000400000e0e0f370000000000000000120012000000370c011007000001150102002d1e00000010000c3800000000000004010004d549bb";
    const DOOR_LOCKED: &str = "aaff200a0078000213000100ec006600000e0e0f370000000000000000120012000000370c011007000001150102002d1e00000010000c380000000000000401000400000e0e0f370000000000000000120012000000370c011007000001150102002d1e00000010010c3800000000000004010004012cbb";
    const POWER_OFF: &str = "aaff200a0078000144000100ec006600030d0e0f2e00000000000000001a001a0002002e01000601000001150102002d1e20000000000c38000000000000040100040000000e002e00000000000000000100000002002e00010600000000150102002d1e20000000000c3800000000000004010004d966bb";
    const DOOR_SHUT_EVENT: &str = "aaff200a0018000305000101030006100b010b1002e639bb";

    fn setup() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("FAFXU22007", "FAFXU22007", "0.0.0");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);
        (ha, thinq, dev)
    }

    #[test]
    fn decodes_connect_snapshot_and_door_events() {
        let (ha, thinq, dev) = setup();
        dev.start();
        assert_eq!(
            thinq.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec!["AA12F0ED1121010000001804111200005EBB"]
        );

        thinq.emit_data(&hex_decode(EB_IDLE));
        let prop = |k: &str| ha.device(ID).unwrap().properties.get(k).map(|v| v.as_string());
        assert_eq!(prop("power").as_deref(), Some("ON"));
        assert_eq!(prop("status").as_deref(), Some("Initial"));
        assert_eq!(prop("course").as_deref(), Some("Normal"));
        assert_eq!(prop("soil").as_deref(), Some("Medium"));
        assert_eq!(prop("temp").as_deref(), Some("Tap Cold"));
        assert_eq!(prop("rinse").as_deref(), Some("Normal"));
        assert_eq!(prop("spin").as_deref(), Some("High"));
        assert_eq!(prop("remaining_time").as_deref(), Some("46"));
        assert_eq!(prop("initial_time").as_deref(), Some("46"));
        assert_eq!(prop("tub_clean_count").as_deref(), Some("21"));
        assert_eq!(prop("buzzer").as_deref(), Some("Very High"));
        assert_eq!(prop("turbo_wash").as_deref(), Some("ON"));
        assert_eq!(prop("drawer").as_deref(), Some("OFF"));
        assert_eq!(prop("problem").as_deref(), Some("OFF"));
        assert_eq!(prop("problem_reason").as_deref(), Some("OK"));

        thinq.emit_data(&hex_decode(RINSING));
        thinq.emit_data(&hex_decode(DOOR_LOCKED));
        assert_eq!(prop("status").as_deref(), Some("Rinsing"));
        assert_eq!(prop("remote_start").as_deref(), Some("ON"));
        assert_eq!(prop("door_lock").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(DOOR_SHUT_EVENT));
        assert_eq!(prop("door").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(POWER_OFF));
        assert_eq!(prop("power").as_deref(), Some("OFF"));
        assert_eq!(prop("remaining_time").as_deref(), Some("0"));
        dev.drop_device();
    }

    #[test]
    fn writes_match_app_frames_and_serialize_until_reply() {
        let (_ha, thinq, dev) = setup();
        thinq.emit_data(&hex_decode(EB_IDLE));
        thinq.reset_recorder();

        dev.set_property("course", "Quick Wash");
        dev.set_property("pause", "");
        assert_eq!(
            thinq.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec!["AA0DF0E5000201FF010A4AB6BB"]
        );

        // Answer first write -> second write goes out.
        let mut reply = vec![0xaa, 0x3e, 0x20, 0xe6, 0x00, 0x02, 0x01, 0xff, 0x01, 0x0a, 0x00];
        reply.extend_from_slice(&vec![0u8; RECORD_LEN]);
        reply.push(0x00);
        reply.push(0xbb);
        thinq.emit_data(&reply);
        assert_eq!(
            thinq.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec!["AA0DF0E5000201FF010A4AB6BB", "AA0DF0E5000201FF010302C1BB"]
        );

        // Cold Wash forces Temp=Cold (14) and Soil=Medium (3) in the bundle.
        thinq.emit_data(&reply);
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(EB_IDLE));
        dev.set_property("cold_wash", "ON");
        assert_eq!(
            thinq.outbox().iter().map(|b| hex_encode(b)).collect::<Vec<_>>(),
            vec!["AA20F0E5000201FF0A1F0E210F1E03200E3E0034003801350144007F0000AEBB"]
        );
        dev.drop_device();
    }
}
