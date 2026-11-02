//! SDH_GVX5_6211 — LG RHX5009NHB heat-pump dryer (EU, ThinQ2 deviceType 202, protocolVer 7).
//!
//! Status comes enveloped (class `0x30`): one (`0xEB`) or two (`0xEC`, current last) 50-byte
//! blocks, each led by a `0x0a` marker; all offsets were confirmed live on the appliance. Writes
//! are `F0E5` field lists. Power takes an explicit on or off; on also wakes the dryer from sleep.
//! The `set_*` entities show the panel's settings until something is picked in HA; Start resumes
//! from Pause, or sends what they show and drops the picks. `auto_ack` is on. The status block,
//! the events and the writes are documented on the wiki page Appliance:SDH_GVX5_6211.
//!
//! Port of upstream PR #246.

use crate::device_trait::DeviceHandler;
use crate::js;
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const BLOCK_LENGTH: usize = 50;
const BLOCK_MARKER: u8 = 0x0a;
const STATUS_REQUEST: &[u8] = &[0xf0, 0xed, 0x11, 0x21, 0x01, 0x00, 0x00, 0x00, 0x18, 0x00];

// F0E5 writes: 000201FF, a field count, then (field, value) pairs, each acked in a 30E6 echo.
const WRITE_HEADER: [u8; 6] = [0xf0, 0xe5, 0x00, 0x02, 0x01, 0xff];
const FIELD_POWER: u8 = 0x02;
const FIELD_RUN: u8 = 0x03;
const FIELD_COURSE: u8 = 0x0a;
const FIELD_ANTI_CREASE: u8 = 0x0f;
const FIELD_DRY_LEVEL: u8 = 0x1e;
const FIELD_DRY_MODE: u8 = 0x1f;
const FIELD_DAMP_ALERT: u8 = 0x34;
// Minutes, two bytes.
const FIELD_DRY_TIME: u8 = 0x70;
const RUN_START: u8 = 0x01;
const RUN_PAUSE: u8 = 0x02;
const RUN_RESUME: u8 = 0x03;
// Timed Dry's range, whose minimum is also its default.
const DRY_TIME_MIN: u16 = 20;
const DRY_TIME_MAX: u16 = 180;
// The cloud's answer to the dryer's 30 7F 04 after a start or resume. The run has already begun;
// a bridged dryer gets it twice and accepts both.
const START_FOLLOW_UP: &[u8] = &[0xf0, 0x24, 0x12, 0x01, 0x14];

// Record offsets, from the 0x0a marker. Times are big-endian minutes, energy big-endian Wh of the
// current (or last) cycle.
const DRY_LEVEL_OFFSET: usize = 1;
const DRY_MODE_OFFSET: usize = 2;
// Delay End, valid while FLAG2_DELAY is set.
const DELAY_OFFSET: usize = 7;
const REMAIN_TIME_OFFSET: usize = 9;
const INITIAL_TIME_OFFSET: usize = 11;
const PHASE_OFFSET: usize = 13;
const ERROR_OFFSET: usize = 15;
const ENERGY_OFFSET: usize = 17;
const FLAGS0_OFFSET: usize = 19;
const FLAG0_BEEP: u8 = 0x04;
const COURSE_OFFSET: usize = 20;
const FLAGS_OFFSET: usize = 24;
const FLAG_ANTI_CREASE: u8 = 0x08;
const FLAG_DRUM_LIGHT: u8 = 0x20;
const FLAG_DAMP_ALERT: u8 = 0x40;
const FLAGS2_OFFSET: usize = 26;
// With phase Off; Power ON wakes it.
const FLAG2_SLEEP: u8 = 0x02;
const FLAG2_DELAY: u8 = 0x08;
const FLAG2_CHILD_LOCK: u8 = 0x10;
// Armed on the panel, or by the dryer itself for any started cycle; cleared at End, by power-off
// or by a pause on the panel.
const FLAG2_REMOTE_START: u8 = 0x40;
// Latches on at the first power-on after module boot, while the phase reads 0x01 even when off:
// only the bit together with a nonzero phase means on.
const POWER_OFFSET: usize = 27;
const POWER_BIT: u8 = 0x80;
const PHASE_OFF: u8 = 0x00;
const PHASE_PAUSE: u8 = 0x03;
const PHASE_ERROR: u8 = 0x05;

// 0 on courses without a dry level.
const DRY_LEVELS: &[(u8, &str)] = &[(0x01, "Iron"), (0x03, "Cupboard"), (0x05, "Extra")];
// Each course remembers its own.
const DRY_MODES: &[(u8, &str)] = &[(0x02, "Efficiency"), (0x03, "Turbo")];

// LG's state index (modelJSON `state`, the table BDH_D30007_US uses).
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
    (25, "Dehumidification"),
    (26, "Dehumidification end"),
    (27, "End waiting"),
    (29, "Cooldown pause"),
    (30, "Cooldown standby"),
];
// Not a phase: Off with FLAG2_SLEEP set.
const SLEEP: &str = "Sleep";

// LG's error index (modelJSON `error`), labelled as in the owner's manual with the panel's code.
const ERRORS: &[(u8, &str)] = &[
    (0, "OK"),
    (1, "Temperature sensor error (tE1)"),
    (2, "Temperature sensor error (tE2)"),
    (4, "Temperature sensor error (tE4)"),
    (14, "Drain pump error (OE)"),
    (15, "Empty water (drN)"),
    (16, "Door open (dE1)"),
    (18, "No lint filter (IF)"),
    (20, "Temperature error (F1)"),
    (21, "Compressor error (LE2)"),
    (22, "Compressor error (AE)"),
    (31, "Motor locked error (LE1)"),
    (38, "Door error (dE4)"),
    (43, "dE2"),
    (44, "Power error (PS)"),
    (55, "Condenser cover error (CC)"),
];

// Course ids as Start writes them, in dial order. Condenser Care and Drum Care are button-hold
// maintenance courses; the app hides Condenser Care, yet the dryer accepts both from Start.
const COURSES: &[(u8, &str)] = &[
    (0x07, "Cotton"),
    (0x19, "Eco"),
    (0x06, "Mixed Fabric"),
    (0x05, "Easy Care"),
    (0x0b, "Wool"),
    (0x08, "Activewear"),
    (0x0a, "Download Cycle"),
    (0x02, "Towels"),
    (0x10, "Allergy Care"),
    (0x15, "Timed Dry"),
    (0x26, "TurboDry"),
    (0x1c, "AI Dry"),
    (0x12, "Condenser Care"),
    (0x13, "Drum Care"),
];
const TIMED_DRY: &str = "Timed Dry";

struct Adjustable {
    course: &'static str,
    levels: &'static [&'static str],
    modes: &'static [&'static str],
}

// The dry levels and modes a course accepts (modelJSON `selectable`); the others have fixed ones.
const ADJUSTABLE: &[Adjustable] = &[
    Adjustable {
        course: "Cotton",
        levels: &["Iron", "Cupboard", "Extra"],
        modes: &[],
    },
    Adjustable {
        course: "Eco",
        levels: &["Iron", "Cupboard", "Extra"],
        modes: &[],
    },
    Adjustable {
        course: "Mixed Fabric",
        levels: &["Iron", "Cupboard", "Extra"],
        modes: &["Efficiency", "Turbo"],
    },
    Adjustable {
        course: "Easy Care",
        levels: &["Iron", "Cupboard"],
        modes: &["Efficiency", "Turbo"],
    },
];

const DAMP_ALERT_COURSES: &[&str] = &["Cotton", "Eco", "Mixed Fabric", "Easy Care"];
// Every drying programme; not the maintenance courses, nor Download Cycle, where the downloaded
// course decides.
const ANTI_CREASE_COURSES: &[&str] = &[
    "Cotton",
    "Eco",
    "Mixed Fabric",
    "Easy Care",
    "Wool",
    "Activewear",
    "Towels",
    "Allergy Care",
    "Timed Dry",
    "TurboDry",
    "AI Dry",
];

/// What Start sends, as wire codes; `None` where nothing reported or picked it.
#[derive(Clone, Copy, Default)]
struct Settings {
    course: Option<u8>,
    level: Option<u8>,
    mode: Option<u8>,
    damp_alert: Option<bool>,
    anti_crease: Option<bool>,
    dry_time: Option<u16>,
}

impl Settings {
    fn or(self, fallback: Settings) -> Settings {
        Settings {
            course: self.course.or(fallback.course),
            level: self.level.or(fallback.level),
            mode: self.mode.or(fallback.mode),
            damp_alert: self.damp_alert.or(fallback.damp_alert),
            anti_crease: self.anti_crease.or(fallback.anti_crease),
            dry_time: self.dry_time.or(fallback.dry_time),
        }
    }
}

#[derive(Default)]
struct State {
    /// Start's settings as the panel last showed them.
    reported: Settings,
    /// Those picked in HA since the last Start.
    picked: Settings,
    /// The last status was Pause, so Start resumes instead of programming a new run.
    paused: bool,
}

fn be16(rec: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([rec[off], rec[off + 1]])
}

fn named(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, label)| *label)
}

fn unmap(table: &[(u8, &str)], label: &str) -> Option<u8> {
    table.iter().find(|(_, l)| *l == label).map(|(c, _)| *c)
}

fn labels(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, label)| *label).collect()
}

fn parse_switch(mqtt_value: &str) -> Option<bool> {
    match mqtt_value {
        "ON" => Some(true),
        "OFF" => Some(false),
        _ => None,
    }
}

fn on_off(on: Option<bool>) -> &'static str {
    match on {
        Some(true) => "ON",
        Some(false) => "OFF",
        None => "None",
    }
}

/// An F0E5 write of the given fields, each `[field, value bytes...]`.
fn write<F: AsRef<[u8]>>(fields: &[F]) -> Vec<u8> {
    let mut out = WRITE_HEADER.to_vec();
    out.push(fields.len() as u8);
    for field in fields {
        out.extend_from_slice(field.as_ref());
    }
    out
}

fn components() -> HashMap<String, Value> {
    let course_options = labels(COURSES);
    let dry_level_options = labels(DRY_LEVELS);
    let dry_mode_options = labels(DRY_MODES);
    let status_options = [labels(STATES), vec![SLEEP]].concat();
    [
        // The dryer takes an explicit on or off, so no toggle is needed.
        ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","icon":"mdi:power"})),
        // Needs Remote Start armed on the panel first.
        ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
        // What Start runs; nothing reaches the dryer before Start, as in the app.
        ("set_course", json!({"platform":"select","unique_id":"$deviceid-set_course","state_topic":"$this/set_course","command_topic":"$this/set_course/set","name":"Set course","icon":"mdi:tune-vertical","options":course_options})),
        ("set_dry_level", json!({"platform":"select","unique_id":"$deviceid-set_dry_level","state_topic":"$this/set_dry_level","command_topic":"$this/set_dry_level/set","name":"Set dry level","icon":"mdi:water-percent","options":dry_level_options})),
        ("set_dry_mode", json!({"platform":"select","unique_id":"$deviceid-set_dry_mode","state_topic":"$this/set_dry_mode","command_topic":"$this/set_dry_mode/set","name":"Set dry mode","icon":"mdi:fan","options":dry_mode_options})),
        ("set_damp_alert", json!({"platform":"switch","unique_id":"$deviceid-set_damp_alert","state_topic":"$this/set_damp_alert","command_topic":"$this/set_damp_alert/set","name":"Set damp alert","icon":"mdi:water-alert-outline"})),
        ("set_anti_crease", json!({"platform":"switch","unique_id":"$deviceid-set_anti_crease","state_topic":"$this/set_anti_crease","command_topic":"$this/set_anti_crease/set","name":"Set anti crease","icon":"mdi:tshirt-crew-outline"})),
        // Timed Dry only.
        ("set_dry_time", json!({"platform":"number","unique_id":"$deviceid-set_dry_time","state_topic":"$this/set_dry_time","command_topic":"$this/set_dry_time/set","name":"Set dry time","icon":"mdi:timer-cog-outline","min":DRY_TIME_MIN,"max":DRY_TIME_MAX,"step":1,"unit_of_measurement":"min"})),
        // Pause only; Start resumes.
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":status_options})),
        ("error", json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:alert-circle-outline","device_class":"problem","entity_category":"diagnostic"})),
        ("error_message", json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","entity_category":"diagnostic","device_class":"enum","options":labels(ERRORS)})),
        ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":course_options})),
        ("dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent","device_class":"enum","options":dry_level_options})),
        ("dry_mode", json!({"platform":"sensor","unique_id":"$deviceid-dry_mode","state_topic":"$this/dry_mode","name":"Dry mode","icon":"mdi:fan","device_class":"enum","options":dry_mode_options})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
        ("delay_end", json!({"platform":"sensor","unique_id":"$deviceid-delay_end","state_topic":"$this/delay_end","name":"Delay end","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
        ("anti_crease", json!({"platform":"binary_sensor","unique_id":"$deviceid-anti_crease","state_topic":"$this/anti_crease","name":"Anti crease","icon":"mdi:tshirt-crew-outline"})),
        ("damp_alert", json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_alert","state_topic":"$this/damp_alert","name":"Damp alert","icon":"mdi:water-alert-outline"})),
        ("drum_light", json!({"platform":"binary_sensor","unique_id":"$deviceid-drum_light","state_topic":"$this/drum_light","name":"Drum light","device_class":"light"})),
        ("beep", json!({"platform":"binary_sensor","unique_id":"$deviceid-beep","state_topic":"$this/beep","name":"Beep","icon":"mdi:volume-high","entity_category":"diagnostic"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:lock","entity_category":"diagnostic"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
        // Withdraws the entities of this fork's earlier handler, ported from upstream PR #150.
        ("power_button", json!({"platform":"button"})),
        ("delay", json!({"platform":"sensor"})),
        ("moisture_alert", json!({"platform":"binary_sensor"})),
        ("sound", json!({"platform":"binary_sensor"})),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
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
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            state: Mutex::new(State::default()),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        // The dryer reports only its LG model code, so the sales model is spelled out here.
        config.device.model = Some("RHX5009NHB".into());
        config.device.model_id = Some(meta.model_name.clone());
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(rec) = process_enveloped_status(inner, CLASS_BYTE, BLOCK_LENGTH) {
                    dev.process_status(rec);
                }
            }
        }));
        this
    }

    fn process_status(&self, rec: &[u8]) {
        if rec[0] != BLOCK_MARKER {
            return;
        }
        let phase = rec[PHASE_OFFSET];
        let flags2 = rec[FLAGS2_OFFSET];
        let is_off = rec[POWER_OFFSET] & POWER_BIT == 0 || phase == PHASE_OFF;
        let asleep = phase == PHASE_OFF && flags2 & FLAG2_SLEEP != 0;
        let error = if phase == PHASE_ERROR {
            rec[ERROR_OFFSET]
        } else {
            0
        };

        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("error_message", self.decode("error", ERRORS, error).into());
        self.core.publish_on_off("error", error != 0);
        let status = if asleep {
            SLEEP
        } else if is_off {
            "Off"
        } else {
            self.decode("state", STATES, phase)
        };
        self.core.publish_property("status", status.into());
        let course = rec[COURSE_OFFSET];
        self.core
            .publish_property("course", self.decode("course", COURSES, course).into());
        self.core.publish_property(
            "dry_level",
            self.decode("dry level", DRY_LEVELS, rec[DRY_LEVEL_OFFSET])
                .into(),
        );
        self.core.publish_property(
            "dry_mode",
            self.decode("dry mode", DRY_MODES, rec[DRY_MODE_OFFSET])
                .into(),
        );
        let minutes = |offset| {
            if is_off {
                0
            } else {
                i64::from(be16(rec, offset))
            }
        };
        self.core
            .publish_property("remaining_time", minutes(REMAIN_TIME_OFFSET).into());
        self.core
            .publish_property("initial_time", minutes(INITIAL_TIME_OFFSET).into());
        self.core
            .publish_property("energy", i64::from(be16(rec, ENERGY_OFFSET)).into());

        let delay_end = if flags2 & FLAG2_DELAY != 0 {
            i64::from(be16(rec, DELAY_OFFSET))
        } else {
            0
        };
        self.core.publish_property("delay_end", delay_end.into());
        self.core
            .publish_flag("remote_start", flags2, FLAG2_REMOTE_START);

        let flags = rec[FLAGS_OFFSET];
        self.core
            .publish_flag("anti_crease", flags, FLAG_ANTI_CREASE);
        self.core.publish_flag("damp_alert", flags, FLAG_DAMP_ALERT);
        self.core.publish_flag("drum_light", flags, FLAG_DRUM_LIGHT);
        self.core
            .publish_flag("beep", rec[FLAGS0_OFFSET], FLAG0_BEEP);
        self.core
            .publish_flag("child_lock", flags2, FLAG2_CHILD_LOCK);

        {
            let mut state = self.state.lock();
            state.paused = !is_off && phase == PHASE_PAUSE;
            state.reported = Settings {
                course: (course != 0).then_some(course),
                level: Some(rec[DRY_LEVEL_OFFSET]),
                mode: Some(rec[DRY_MODE_OFFSET]),
                damp_alert: Some(flags & FLAG_DAMP_ALERT != 0),
                anti_crease: Some(flags & FLAG_ANTI_CREASE != 0),
                // A Timed Dry run's initial time is the time set for it.
                dry_time: (named(COURSES, course) == Some(TIMED_DRY))
                    .then(|| be16(rec, INITIAL_TIME_OFFSET)),
            };
        }
        self.publish_settings();
    }

    /// A code missing from its table is logged, which lets the tables be completed from a capture.
    fn decode(&self, what: &str, table: &[(u8, &'static str)], code: u8) -> &'static str {
        let label = named(table, code);
        if label.is_none() && code != 0 {
            tracing::debug!(id = %self.core.id, "SDH_GVX5_6211 undecoded {what} {code:x}");
        }
        label.unwrap_or("None")
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "set_course" => self.pick(unmap(COURSES, mqtt_value), |s, v| s.course = Some(v)),
            "set_dry_level" => self.pick(unmap(DRY_LEVELS, mqtt_value), |s, v| s.level = Some(v)),
            "set_dry_mode" => self.pick(unmap(DRY_MODES, mqtt_value), |s, v| s.mode = Some(v)),
            "set_damp_alert" => self.pick(parse_switch(mqtt_value), |s, v| s.damp_alert = Some(v)),
            "set_anti_crease" => {
                self.pick(parse_switch(mqtt_value), |s, v| s.anti_crease = Some(v))
            }
            "set_dry_time" => {
                let minutes = js::parse_int(mqtt_value)
                    .and_then(|m| u16::try_from(m).ok())
                    .filter(|m| (DRY_TIME_MIN..=DRY_TIME_MAX).contains(m));
                self.pick(minutes, |s, v| s.dry_time = Some(v));
            }
            "power" => {
                if let Some(on) = parse_switch(mqtt_value) {
                    self.core.send(&write(&[[FIELD_POWER, u8::from(on)]]));
                }
            }
            "pause" => self.core.send(&write(&[[FIELD_RUN, RUN_PAUSE]])),
            "start" => self.start_or_resume(),
            _ => {}
        }
    }

    /// Start resumes from Pause. Otherwise it runs what the `set_*` entities show and drops the
    /// choices made in HA, so the entities follow the panel again; then the cloud's follow-up.
    fn start_or_resume(&self) {
        let paused = self.state.lock().paused;
        if paused {
            self.core.send(&write(&[[FIELD_RUN, RUN_RESUME]]));
        } else {
            let Some(start) = self.start_write() else {
                tracing::debug!(
                    id = %self.core.id,
                    "SDH_GVX5_6211 start ignored: no course picked or shown on the panel"
                );
                return;
            };
            self.core.send(&start);
            self.state.lock().picked = Settings::default();
            self.publish_settings();
        }
        self.core.send(START_FOLLOW_UP);
    }

    /// A choice made in HA, kept for Start; a value outside the entity's range leaves the choices
    /// as they were.
    fn pick<T>(&self, value: Option<T>, set: impl FnOnce(&mut Settings, T)) {
        if let Some(value) = value {
            set(&mut self.state.lock().picked, value);
        }
        self.publish_settings();
    }

    /// What Start would use: the choice made in HA, else what the panel shows.
    fn effective(&self) -> Settings {
        let state = self.state.lock();
        state.picked.or(state.reported)
    }

    fn publish_settings(&self) {
        let s = self.effective();
        let label = |table, code: Option<u8>| code.and_then(|c| named(table, c)).unwrap_or("None");
        self.core
            .publish_property("set_course", label(COURSES, s.course).into());
        self.core
            .publish_property("set_dry_level", label(DRY_LEVELS, s.level).into());
        self.core
            .publish_property("set_dry_mode", label(DRY_MODES, s.mode).into());
        self.core
            .publish_property("set_damp_alert", on_off(s.damp_alert).into());
        self.core
            .publish_property("set_anti_crease", on_off(s.anti_crease).into());
        let dry_time = match s.dry_time {
            Some(minutes) => i64::from(minutes).into(),
            None => "None".into(),
        };
        self.core.publish_property("set_dry_time", dry_time);
    }

    /// The app's start write, in its order: the course, the options the course accepts, then the
    /// start command. Without a known course there is nothing to start.
    fn start_write(&self) -> Option<Vec<u8>> {
        let s = self.effective();
        let code = s.course?;
        let course = named(COURSES, code)?;
        let adjustable = ADJUSTABLE.iter().find(|a| a.course == course);
        let mode = s.mode.unwrap_or(0);
        let level = s.level.unwrap_or(0);
        let mut fields = vec![vec![FIELD_COURSE, code]];
        if named(DRY_MODES, mode).is_some_and(|l| adjustable.is_some_and(|a| a.modes.contains(&l)))
        {
            fields.push(vec![FIELD_DRY_MODE, mode]);
        }
        if named(DRY_LEVELS, level)
            .is_some_and(|l| adjustable.is_some_and(|a| a.levels.contains(&l)))
        {
            fields.push(vec![FIELD_DRY_LEVEL, level]);
        }
        if DAMP_ALERT_COURSES.contains(&course) {
            fields.push(vec![FIELD_DAMP_ALERT, u8::from(s.damp_alert == Some(true))]);
        }
        if ANTI_CREASE_COURSES.contains(&course) {
            fields.push(vec![
                FIELD_ANTI_CREASE,
                u8::from(s.anti_crease == Some(true)),
            ]);
        }
        if course == TIMED_DRY {
            let [hi, lo] = s.dry_time.unwrap_or(DRY_TIME_MIN).to_be_bytes();
            fields.push(vec![FIELD_DRY_TIME, hi, lo]);
        }
        fields.push(vec![FIELD_RUN, RUN_START]);
        Some(write(&fields))
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    // Ask once per connect, or the entities stay blank until the next physical interaction and
    // the unanswered appliance keeps retransmitting 0x03 records instead of status.
    fn start(&self) {
        self.core.send(STATUS_REQUEST);
        self.publish_settings();
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
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};
    use std::collections::BTreeMap;

    const MODEL_ID: &str = "SDH_GVX5_6211";

    // Real captures from an LG RHX5009NHB (protocolVer 7, clip_ble_v1.9.237).

    // 0xEB reply to the F0ED status request, appliance powered off.
    const SAMPLE_EB_OFF: &str =
        "AAFF300A00440008A2000100EB00320A000000000000000000000000010000000000040000008F20000000050000000600000000000000000000000000000000006B0DBB";

    // Power button pressed, no course selected yet: block still all zeroes except the power bit.
    const SAMPLE_EC_POWERED_IDLE: &str =
        "AAFF300A00760008A9000100EC00640A000000000000000000000000010000000000040000008F20000000050000000600000000000000000000000000000000000A000000000000000000000000010000000000040000008F2000008005000000060000000000000000000000000000000000CD40BB";

    // Cotton / Cupboard / Turbo idle, Delay End armed at 8 h (previous block shows 6 h).
    const SAMPLE_EC_DELAY_8H: &str =
        "AAFF300A00760008CD000100EC00640A030300000700016800960096010000020000040700008F00000880050000000600000000000000000000000000000000000A03030000070001E000960096010000020000040700008F0000088005000000060000000000000000000000000000000000DFD6BB";

    // Remote start armed with the door ajar: the panel showed dE1 in parallel.
    const SAMPLE_EC_ERROR_DE1: &str =
        "AAFF300A00760008DB000100EC00640A030300000700000000960096010000020000040700008F00000080050000000600000000000000000000000000000000000A030300000700000000960096050110020000040700008F00000080050000000600000000000000000000000000000000002472BB";

    // Beep switched off on the Condenser Care course: rec[19] loses its default 0x04.
    const SAMPLE_EC_BEEP_OFF: &str =
        "AAFF300A0076000037000100EC00640A000300001200000000410041010000030000041200008F00004080050000000600000000000000000000000000000000000A000300001200000000410041010000030000001200008F0000408005000000060000000000000000000000000000000000B31ABB";

    // Child lock armed: rec[26] gains 0x10 next to the remote-start 0x40.
    const SAMPLE_EC_CHILD_LOCK: &str =
        "AAFF300A0076000039000100EC00640A000300001200000000410041010000030000041200008F00004080050000000600000000000000000000000000000000000A000300001200000000410041010000030000041200008F0000508005000000060000000000000000000000000000000000A304BB";

    // Anti-crease toggled on (0x08 joins the drum light bit 0x20 in rec[24]).
    const SAMPLE_EC_ANTI_CREASE: &str =
        "AAFF300A00760008E9000100EC00640A030300000700000000960096010000020000040700008F20000080050000000600000000000000000000000000000000000A030300000700000000960096010000020000040700008F2800008005000000060000000000000000000000000000000000DFF9BB";

    // Remote start armed cleanly (door closed), appliance idle on Cotton.
    const SAMPLE_EC_REMOTE_START: &str =
        "AAFF300A00760008F1000100EC00640A030300000700000000960096010000020000040700008F00000080050000000600000000000000000000000000000000000A030300000700000000960096010000020000040700008F000040800500000006000000000000000000000000000000000005F7BB";

    // Timed Dry running, 19 of 20 minutes remaining, rec[26] 0x40 (remote start) set for the run.
    const SAMPLE_EC_DRYING: &str =
        "AAFF300A0076000901000100EC00640A000300001500000000140014070100020000041500008F00004080050000000600000000000000000000000000000000000A000300001500000000130014070100020000041500008F0000408005000000060000000000000000000000000000000000F59CBB";

    // Pause pressed mid-run: phase 0x03 with the replaced phase 0x07 in rec[14].
    const SAMPLE_EC_PAUSED: &str =
        "AAFF300A0076000913000100EC00640A00030000150000000011001407010002000D041500008F00004080050000000600000000000000000000000000000000000A00030000150000000011001403070002000D041500008F20000080050000000600000000000000000000000000000000003498BB";

    // Powered off after use: the phase byte drops to 0x00 but rec[27] keeps its latched 0x80.
    const SAMPLE_EC_OFF_AFTER_USE: &str =
        "AAFF300A00760009AC000100EC00640A000000000000000000000000010000000000040000008F20000080050000000600000000000000000000000000000000000A000000000000000000000000000100000000040000008F00000080050000000600000000000000000000000000000000001912BB";

    // Eco started on the panel with remote start never armed: rec[26] 0x00 -> 0x44, the dryer
    // switching remote start on by itself (HA's Pause was then accepted).
    const SAMPLE_EC_PANEL_START: &str =
        "AAFF300A0076000AD4000100EC00640A030200001900000000640064010000000000001900008F20000480050000000000000000000000000000000000000000000A0302000019000000006400640E0100000000001900008F00004480050000000000000000000000000000000000000000001DFDBB";

    // Asleep 12 min after remote start was armed: phase Initial -> 0x00 with rec[26] 0x42 (remote
    // start kept, 0x02 = asleep), where a real power-off leaves rec[26] clear.
    const SAMPLE_EC_SLEEP: &str =
        "AAFF300A0076000AAC000100EC00640A000000000000000000000000010000000000000000008F00004080050000000000000000000000000000000000000000000A000000000000000000000000000100000000000000008F00004280050000000000000000000000000000000000000000003379BB";

    // Condenser Care run: phase 0x15 (Condenser care) -> 0x11 (Condenser clean), rec[26] gaining
    // 0x20.
    const SAMPLE_EC_CONDENSER_CLEAN: &str =
        "AAFF300A0076000A48000100EC00640A000300001200000000410041150100030000001200008F00004080050000000000000000000000000000000000000000000A000300001200000000410041111500030000001200008F00006080050000000000000000000000000000000000000000008D3CBB";

    // Eco started from HA: phase 0x0E (Detecting) while the auto-sensing course estimates its time.
    const SAMPLE_EC_DETECTING: &str =
        "AAFF300A0076000A9C000100EC00640A000000000000000000000000010000000000000000008F20004080050000000000000000000000000000000000000000000A0302000019000000006400640E0100000000001900008F0000448005000000000000000000000000000000000000000000A600BB";

    const WRITE_INIT: &str = "AA0EF0ED1121010000001800B5BB";
    // Power-field writes captured from the ThinQ app.
    const WRITE_POWER_ON: &str = "AA0DF0E5000201FF010201C7BB";
    const WRITE_POWER_OFF: &str = "AA0DF0E5000201FF010200C4BB";
    // Run-command writes captured from the ThinQ app: pause (03 = 02) and resume (03 = 03).
    const WRITE_PAUSE: &str = "AA0DF0E5000201FF010302C1BB";
    const WRITE_RESUME: &str = "AA0DF0E5000201FF010303C0BB";
    // End and Cooling frames from two real runs; rec[17..18] matched the ThinQ app exactly.
    const SAMPLE_EC_END_284WH: &str =
        "AAFF300A007600065C000100EC00640A00030002150000000001001E08110005011B041500008F00004080050000000600000000000000000000000000000000000A00030002150000000001001E04080007011C041500008F2000008005000000060000000000000000000000000000000000B4F2BB";
    const SAMPLE_EC_COOLING_144WH: &str =
        "AAFF300A007600071D000100EC00640A00030000150000000001001411070005008E041500008F00006080050000000600000000000000000000000000000000000A000300001500000000010014081100050090041500008F000040800500000006000000000000000000000000000000000023DFBB";

    const WRITE_START_FOLLOW_UP: &str = "AA09F024120114BBBB";
    // Start writes captured from the ThinQ app (Drum Care) and from a hand injection the dryer
    // accepted (Condenser Care, hidden in the app; status 00 for field 0A).
    const WRITE_START_DRUM_CARE: &str = "AA0FF0E5000201FF020A130301E6BB";
    const WRITE_START_CONDENSER_CARE: &str = "AA0FF0E5000201FF020A120301E7BB";
    // Starts captured from rethink, each field accepted and applied as picked: Cotton with nothing
    // picked, Mixed Fabric with Efficiency + Extra + damp alert + anti crease, and Timed Dry for
    // 47 min (field 70).
    const WRITE_START_COTTON: &str = "AA13F0E5000201FF040A0734000F000301A5BB";
    const WRITE_START_MIXED_ALL_OPTIONS: &str = "AA17F0E5000201FF060A061F021E0534010F0103016EBB";
    const WRITE_START_TIMED_DRY_47: &str = "AA14F0E5000201FF040A150F0070002F03013FBB";

    /// The fields of an F0E5 write as {field: value}, the Timed Dry time (70) taking two bytes.
    fn write_fields(frame: &[u8]) -> BTreeMap<u8, u32> {
        let inner = &frame[2..frame.len() - 2];
        let mut fields = BTreeMap::new();
        let mut i = 7;
        for _ in 0..inner[6] {
            let size = if inner[i] == 0x70 { 2 } else { 1 };
            let value = inner[i + 1..=i + size]
                .iter()
                .fold(0, |acc, &b| acc << 8 | u32::from(b));
            fields.insert(inner[i], value);
            i += 1 + size;
        }
        fields
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2(MODEL_ID, Device::new)
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    fn outbox(thinq: &MockThinq2Device) -> Vec<String> {
        thinq.outbox().iter().map(|p| hex_encode(p)).collect()
    }

    fn expect(ha: &MockHaConnection, expected: &[(&str, &str)]) {
        for (name, value) in expected {
            assert_eq!(prop(ha, name).as_deref(), Some(*value), "{name}");
        }
    }

    #[test]
    fn energy_is_the_per_cycle_wh_counter_at_rec_17_18() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_END_284WH);
        expect(&ha, &[("energy", "284")]);
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_COOLING_144WH);
        expect(&ha, &[("energy", "144")]);
    }

    #[test]
    fn start_sends_the_f0ed_status_request() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(outbox(&thinq), [WRITE_INIT]);
    }

    #[test]
    fn single_0xeb_frame_decodes_the_powered_off_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EB_OFF);
        expect(
            &ha,
            &[
                ("power", "OFF"),
                ("status", "Off"),
                ("remaining_time", "0"),
                ("error", "OFF"),
            ],
        );
    }

    #[test]
    fn powered_on_with_no_course_selected_the_power_bit_alone_flips_on() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_POWERED_IDLE);
        // the phase byte reads 0x01 even before a course is picked
        expect(
            &ha,
            &[("power", "ON"), ("status", "Initial"), ("course", "None")],
        );
    }

    #[test]
    fn powered_off_after_use_the_latched_rec_27_bit_does_not_fake_power_on() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_OFF_AFTER_USE);
        expect(&ha, &[("power", "OFF"), ("status", "Off")]);
    }

    #[test]
    fn doubled_0xec_frame_decodes_the_last_current_block_not_the_stale_previous_one() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_DELAY_8H);
        // previous block holds the 6 h delay; the current one 8 h
        expect(
            &ha,
            &[
                ("delay_end", "480"),
                ("course", "Cotton"),
                ("dry_level", "Cupboard"),
                ("dry_mode", "Turbo"),
                ("remaining_time", "150"),
                ("initial_time", "150"),
            ],
        );
    }

    #[test]
    fn de1_door_error_is_reported_with_its_message() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_ERROR_DE1);
        expect(
            &ha,
            &[
                ("error", "ON"),
                ("error_message", "Door open (dE1)"),
                ("status", "Error"),
            ],
        );
    }

    #[test]
    fn anti_crease_bit_is_isolated_from_the_drum_light_bit() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_ANTI_CREASE);
        expect(
            &ha,
            &[
                ("anti_crease", "ON"),
                ("damp_alert", "OFF"),
                ("drum_light", "ON"),
            ],
        );
    }

    #[test]
    fn beep_reads_rec_19_bit_0x04_default_on() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_BEEP_OFF);
        expect(&ha, &[("beep", "OFF"), ("course", "Condenser Care")]);
    }

    #[test]
    fn child_lock_reads_rec_26_bit_0x10_independently_of_remote_start() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_CHILD_LOCK);
        expect(
            &ha,
            &[("child_lock", "ON"), ("remote_start", "ON"), ("beep", "ON")],
        );
    }

    #[test]
    fn remote_start_reads_rec_26_bit_0x40_while_idle() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_REMOTE_START);
        expect(&ha, &[("remote_start", "ON")]);
    }

    #[test]
    fn a_panel_started_run_reports_the_remote_start_the_dryer_switched_on_itself() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_PANEL_START);
        expect(&ha, &[("remote_start", "ON")]);
    }

    #[test]
    fn running_cycle_drying_phase_and_countdown() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_DRYING);
        expect(
            &ha,
            &[
                ("status", "Drying"),
                ("course", "Timed Dry"),
                ("remaining_time", "19"),
                ("initial_time", "20"),
            ],
        );
    }

    #[test]
    fn pause_mid_run_reports_the_pause_phase() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_PAUSED);
        expect(&ha, &[("status", "Pause"), ("remaining_time", "17")]);
    }

    #[test]
    fn status_follows_lgs_state_indices_not_guessed_labels() {
        for (sample, status) in [
            (SAMPLE_EC_CONDENSER_CLEAN, "Condenser clean"),
            (SAMPLE_EC_DETECTING, "Detecting"),
            (SAMPLE_EC_COOLING_144WH, "Cooling"),
        ] {
            let (ha, thinq, _) = make();
            emit(&thinq, sample);
            expect(&ha, &[("status", status)]);
        }
    }

    #[test]
    fn asleep_off_phase_with_rec_26_0x02_reads_sleep_power_off_remote_start_kept() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_SLEEP);
        expect(
            &ha,
            &[
                ("status", "Sleep"),
                ("power", "OFF"),
                ("remote_start", "ON"),
            ],
        );
    }

    #[test]
    fn the_power_switch_sends_the_explicit_writes_and_other_values_send_nothing() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        dev.set_property("power", "OFF");
        dev.set_property("power", "TOGGLE");
        dev.set_property("nonsense", "");
        assert_eq!(outbox(&thinq), [WRITE_POWER_ON, WRITE_POWER_OFF]);
    }

    #[test]
    fn pause_sends_the_pause_run_command_alone() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(outbox(&thinq), [WRITE_PAUSE]);
    }

    #[test]
    fn start_programs_the_picked_course_not_the_one_the_appliance_reports() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SAMPLE_EC_DRYING); // rec[20] reports Timed Dry
        dev.set_property("set_course", "Drum Care");
        expect(&ha, &[("set_course", "Drum Care")]);
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(
            outbox(&thinq),
            [WRITE_START_DRUM_CARE, WRITE_START_FOLLOW_UP]
        );
    }

    #[test]
    fn the_set_entities_show_the_panel_until_something_is_picked_in_ha() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SAMPLE_EC_DELAY_8H); // Cotton / Cupboard / Turbo on the panel
        expect(
            &ha,
            &[
                ("set_course", "Cotton"),
                ("set_dry_level", "Cupboard"),
                ("set_dry_mode", "Turbo"),
                ("set_damp_alert", "OFF"),
                ("set_anti_crease", "OFF"),
                ("set_dry_time", "None"), // only Timed Dry has a time
            ],
        );

        // a pick outlives the panel's next report
        dev.set_property("set_course", "Eco");
        emit(&thinq, SAMPLE_EC_DELAY_8H);
        expect(&ha, &[("set_course", "Eco"), ("set_dry_level", "Cupboard")]);

        // a Timed Dry run shows the time it was set for
        emit(&thinq, SAMPLE_EC_DRYING);
        expect(&ha, &[("set_dry_time", "20")]);
    }

    #[test]
    fn start_runs_what_the_set_entities_show_then_they_follow_the_panel_again() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SAMPLE_EC_DELAY_8H); // Cotton / Cupboard / Turbo on the panel
        dev.set_property("set_course", "Eco");
        thinq.reset_recorder();
        dev.set_property("start", "");
        // Eco with the panel's Cupboard; Eco's dry mode is fixed, so it isn't sent
        assert_eq!(
            write_fields(&thinq.outbox()[0]),
            BTreeMap::from([
                (0x0a, 0x19),
                (0x1e, 0x03),
                (0x34, 0),
                (0x0f, 0),
                (0x03, 0x01)
            ])
        );
        assert_eq!(outbox(&thinq)[1], WRITE_START_FOLLOW_UP);
        expect(&ha, &[("set_course", "Cotton")]);
    }

    #[test]
    fn start_sends_nothing_with_no_course_on_the_panel_and_none_picked() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_EB_OFF);
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn start_from_pause_resumes_the_run_instead_of_programming_a_new_one() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_EC_PAUSED);
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(outbox(&thinq), [WRITE_RESUME, WRITE_START_FOLLOW_UP]);
    }

    #[test]
    fn condenser_care_hidden_in_the_app_programs_the_code_the_appliance_accepted() {
        let (_, thinq, dev) = make();
        dev.set_property("set_course", "Condenser Care");
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(outbox(&thinq)[0], WRITE_START_CONDENSER_CARE);
    }

    #[test]
    fn start_adds_the_picked_options_when_the_course_accepts_them() {
        let (ha, thinq, dev) = make();
        dev.set_property("set_course", "Mixed Fabric");
        dev.set_property("set_dry_mode", "Efficiency");
        dev.set_property("set_dry_level", "Extra");
        dev.set_property("set_damp_alert", "ON");
        dev.set_property("set_anti_crease", "ON");
        expect(&ha, &[("set_damp_alert", "ON")]);
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(
            outbox(&thinq),
            [WRITE_START_MIXED_ALL_OPTIONS, WRITE_START_FOLLOW_UP]
        );
    }

    #[test]
    fn start_leaves_out_what_the_course_does_not_accept() {
        // Cotton's dry mode is fixed at Turbo and it takes no time: a picked Efficiency and 47 min
        // are dropped
        let (_, thinq, dev) = make();
        dev.set_property("set_course", "Cotton");
        dev.set_property("set_dry_mode", "Efficiency");
        dev.set_property("set_dry_time", "47");
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(outbox(&thinq)[0], WRITE_START_COTTON);

        // a maintenance course takes none of the options
        let (_, thinq, dev) = make();
        dev.set_property("set_course", "Drum Care");
        dev.set_property("set_dry_mode", "Efficiency");
        dev.set_property("set_dry_level", "Iron");
        dev.set_property("set_damp_alert", "ON");
        dev.set_property("set_anti_crease", "ON");
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(outbox(&thinq)[0], WRITE_START_DRUM_CARE);
    }

    #[test]
    fn timed_dry_carries_the_picked_time_in_minutes_but_no_damp_alert() {
        let (ha, thinq, dev) = make();
        dev.set_property("set_course", "Timed Dry");
        dev.set_property("set_dry_time", "47");
        dev.set_property("set_damp_alert", "ON");
        expect(&ha, &[("set_dry_time", "47")]);
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(outbox(&thinq)[0], WRITE_START_TIMED_DRY_47);
    }

    #[test]
    fn frames_that_are_not_0x30_family_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, "AA08F02A010098BB");
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());
    }

    #[test]
    fn discovery_names_the_sales_model_and_withdraws_the_earlier_entities() {
        let (ha, _, _) = make();
        let config = ha.device(DEVICE_ID).unwrap().config.unwrap();
        assert_eq!(config.device.name.as_deref(), Some("LG Dryer"));
        assert_eq!(config.device.model.as_deref(), Some("RHX5009NHB"));
        let payload = serde_json::to_value(&config).unwrap();
        assert_eq!(payload["device"]["model_id"], MODEL_ID);

        let c = &config.components;
        assert_eq!(c.len(), 29);
        assert_eq!(c["power"]["platform"], "switch");
        assert_eq!(c["power"]["command_topic"], "$this/power/set");
        // The keys the #150-based handler published and this one does not.
        for (key, platform) in [
            ("power_button", "button"),
            ("delay", "sensor"),
            ("moisture_alert", "binary_sensor"),
            ("sound", "binary_sensor"),
        ] {
            assert_eq!(c[key], json!({ "platform": platform }), "{key}");
        }
        let status = c["status"]["options"].as_array().unwrap();
        assert_eq!(status.first().unwrap(), "Off");
        assert_eq!(status.last().unwrap(), "Sleep");
        assert_eq!(c["set_course"]["options"], c["course"]["options"]);
    }

    // The #150-based handler's 13 status captures are all among the samples above (its SOUND_OFF
    // is BEEP_OFF, END_144WH is COOLING_144WH). Every one decodes within the published options.
    #[test]
    fn the_earlier_handlers_captures_decode_within_the_published_options() {
        let (ha, _, _) = make();
        let components = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        for frame in [
            SAMPLE_EB_OFF,
            SAMPLE_EC_POWERED_IDLE,
            SAMPLE_EC_DELAY_8H,
            SAMPLE_EC_ERROR_DE1,
            SAMPLE_EC_BEEP_OFF,
            SAMPLE_EC_CHILD_LOCK,
            SAMPLE_EC_ANTI_CREASE,
            SAMPLE_EC_REMOTE_START,
            SAMPLE_EC_DRYING,
            SAMPLE_EC_PAUSED,
            SAMPLE_EC_OFF_AFTER_USE,
            SAMPLE_EC_END_284WH,
            SAMPLE_EC_COOLING_144WH,
        ] {
            let (ha, thinq, _) = make();
            emit(&thinq, frame);
            for key in [
                "status",
                "error_message",
                "course",
                "dry_level",
                "dry_mode",
                "set_course",
                "set_dry_level",
                "set_dry_mode",
            ] {
                let value = prop(&ha, key).unwrap();
                let options = components[key]["options"].as_array().unwrap();
                let known = options.iter().any(|o| o.as_str() == Some(value.as_str()));
                let blank_allowed = !matches!(key, "status" | "error_message");
                assert!(
                    known || (blank_allowed && value == "None"),
                    "{key} = {value}"
                );
            }
        }

        // #150 withheld remote start during a run; it is the bit the dryer set for the run
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_DRYING);
        expect(&ha, &[("remote_start", "ON")]);
        // and a pause on the panel clears it
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_PAUSED);
        expect(&ha, &[("remote_start", "OFF")]);
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_EC_END_284WH);
        expect(&ha, &[("status", "End"), ("set_dry_time", "30")]);
    }

    // #150 re-programmed the reported course alone and toggled the run. Now Start sends one write
    // with the options the course takes: anti crease off and the 20 minutes the run was set for.
    #[test]
    fn start_with_nothing_picked_restarts_the_reported_timed_dry_with_its_time() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_EC_DRYING);
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(
            outbox(&thinq),
            [
                "AA14F0E5000201FF040A150F0070001403011ABB",
                WRITE_START_FOLLOW_UP
            ]
        );
        assert_eq!(
            write_fields(&thinq.outbox()[0]),
            BTreeMap::from([(0x0a, 0x15), (0x0f, 0), (0x70, 20), (0x03, 0x01)])
        );

        // nor is there a power button any more
        thinq.reset_recorder();
        dev.set_property("power_button", "");
        assert!(thinq.outbox().is_empty());
    }
}
