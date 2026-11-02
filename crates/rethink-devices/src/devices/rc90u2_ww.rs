//! RC90U2_WW — EU heat-pump dryer (RC9DN9029).
//!
//! 25-byte class `0x30` records, same AABB dialect as the US dryers. Also
//! serves RC90V9_WW and RC90V9_AUS, and the ThinQ1 module through
//! `rc90u2_ww_t1`. `auto_ack` stays off, matching the upstream handler.
//! Bytes 17-18 (not in the spec) are the Wh used by the current cycle,
//! retained after it.
//!
//! Writes, mapped live on two more units: power on (`F0 2A`) and off (`F0 24 01`), Pause/Resume
//! (`F0 24 04`, the panel's Start/Pause toggle; `F0 24 05` is acked with a beep and ignored) and
//! Start selected cycle, an `F0 26` remote start built from the course, dry level, Eco Hybrid and
//! anti crease selects plus a one-shot delayed end. It sends only what the chosen course takes
//! (modelJSON Course section); anything else powers the dryer on with no course. A running cycle
//! ignores `F0 26`, so mid-cycle it follows a pause. Downloaded courses are not started remotely.
//! The selects mirror the panel only when the appliance's own value changes, and a reconnect
//! keeps them.
//!
//! Port of upstream PR #240.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use crate::js;
use crate::timers::{ThreadTimers, Timers};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

pub(crate) const CLASS_BYTE: u8 = 0x30;
pub(crate) const STATUS_LENGTH: usize = 27;
pub(crate) const RECORD_MARKER: u8 = 0x19;

pub(crate) const POWER_ON: &[u8] = &[0xf0, 0x2a, 0x01, 0x00];
pub(crate) const POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
pub(crate) const START_PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];

const STATE_OFF: u8 = 0x00;
const STATE_RUNNING: u8 = 2;
const STATE_PAUSED: u8 = 3;

const OPT1_RESERVATION: u8 = 0x01;
const OPT1_ANTI_CREASE: u8 = 0x02;
const OPT1_CHILD_LOCK: u8 = 0x10;
// The spec's SelfCleaning (0x20), DampDryBeep (0x40) and HandIron (0x80) belong to the family-wide
// value table: none of them is on this model's panel, or was ever seen set on three units.

/// Option2 bit 0, RemoteStart. The appliance also holds it while a cycle runs and drops it in pause.
const OPT2_REMOTE_START: u8 = 0x01;

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Ready"),
    (2, "Running"),
    (3, "Paused"),
    (4, "End"),
    (5, "Error"),
    (100, "Delayed"),
];

/// Base courses in the order the dial walks them, then the downloaded ones.
const COURSES: &[(u8, &str)] = &[
    (0x07, "Cotton"),
    (0x06, "Mixed Fabric"),
    (0x05, "Easy Care"),
    (0x04, "Duvet"),
    (0x02, "Cotton Soft"),
    (0x10, "Allergy Care"),
    (0x09, "Quick Dry"),
    (0x0e, "Warm Air"),
    (0x0d, "Cool Air"),
    (0x0c, "Rack Dry"),
    (0x0a, "Delicate"),
    (0x0b, "Wool"),
    (0x08, "Sports Wear"),
    (0x65, "Baby Wear"),
    (0x66, "Gym Clothes"),
    (0x67, "Blanket"),
    (0x68, "Blanket Refresh"),
    (0x69, "Rainy Season"),
    (0x6a, "Single Garments"),
    (0x6b, "Deodorization"),
    (0x6c, "Small Load"),
    (0x6d, "Lingerie"),
    (0x6e, "Easy Iron"),
    (0x6f, "Super Dry"),
    (0x70, "Economic Dry"),
    (0x71, "Big Size Item"),
    (0x72, "Minimize Wrinkles"),
    (0x73, "Shoes/Fabric Doll"),
    (0x74, "Full Size Load"),
];
const FIRST_SMART_COURSE: u8 = 0x65;

const DRY_LEVELS: &[(u8, &str)] = &[(1, "Iron"), (3, "Cupboard"), (4, "Extra")];
const ECO_HYBRID: &[(u8, &str)] = &[(1, "Eco"), (2, "Normal"), (3, "Time")];

const ERRORS: &[(u8, &str)] = &[
    (0, "OK"),
    (1, "Temperature sensor error (tE1)"),
    (2, "Temperature sensor error (tE2)"),
    (7, "Condenser error (CE1)"),
    (13, "Drain motor error"),
    (14, "Empty water container"),
    (15, "Door open"),
    (17, "Filter missing"),
    (19, "Unknown error (F1)"),
    (20, "Motor error (LE2)"),
    (21, "Unknown error (AE)"),
    (30, "Motor error (LE1)"),
    (37, "Door sensor error (dE4)"),
];

const NO_YES: [&str; 2] = ["No", "Yes"];

/// Components this handler no longer has, published as a bare platform so that HA removes them.
const TOMBSTONES: &[(&str, &str)] = &[
    ("damp_dry_beep", "binary_sensor"),
    ("hand_iron", "binary_sensor"),
];

/// What each base course takes in a remote start (ThinQ1 modelJSON TITAN2.0_D_WIFI_CZ, Course):
/// `(course, dry levels, Eco Hybrid values, MoreLessTime minutes)`. The first value is the
/// course default and 0 means the course has no such setting.
const COURSE_OPTIONS: &[(u8, &[u8], &[u8], u8)] = &[
    (0x02, &[0], &[3], 0),
    (0x04, &[0], &[3], 0),
    (0x05, &[3, 1], &[3, 1], 0),
    (0x06, &[3, 1, 4], &[3, 1], 0),
    (0x07, &[3, 1, 4], &[1, 3], 0),
    (0x08, &[0], &[1], 0),
    (0x09, &[0], &[3], 0),
    (0x0a, &[0], &[1], 0),
    (0x0b, &[0], &[1], 0),
    (0x0c, &[0], &[1], 180),
    (0x0d, &[0], &[0], 60),
    (0x0e, &[0], &[1, 3], 60),
    (0x10, &[0], &[3], 0),
];

/// `F0 26`: [2] course, [3] dry level, [4] Eco Hybrid, [5] MoreLessTime, [8] delayed end in
/// hours, [11] Option1 (anti crease), [12] operation. The ThinQ app sends operation 0x03; 0x01
/// starts the drum just the same and is what was tested end to end.
const START_COURSE_LEN: usize = 16;
const START_COURSE_OP: u8 = 0x01;
const PAUSE_BEFORE_START: Duration = Duration::from_millis(1200);

/// The ThinQ app offers a delayed end of 3 to 19 hours, counted from the end of the cycle.
const DELAY_MIN_HOURS: u8 = 3;
const DELAY_MAX_HOURS: u8 = 19;

/// The course and settings shown by the selects and sent by Start selected cycle. The appliance
/// does not report what will run next, so a choice made in HA is held here.
#[derive(Clone)]
struct Remote {
    course: &'static str,
    dry_level: &'static str,
    eco_hybrid: &'static str,
    anti_crease: bool,
    delay_hours: u8,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            course: "Cotton",
            dry_level: "Cupboard",
            eco_hybrid: "Eco",
            anti_crease: false,
            delay_hours: 0,
        }
    }
}

/// What the appliance itself last reported.
#[derive(Clone, Default)]
struct Seen {
    course: Option<&'static str>,
    dry_level: Option<&'static str>,
    eco_hybrid: Option<&'static str>,
    anti_crease: Option<bool>,
}

#[derive(Clone, Default)]
struct Selection {
    remote: Remote,
    seen: Seen,
    cycle_running: bool,
}

impl Selection {
    /// A value the appliance reports differently from last time replaces the choice made in HA.
    /// An off appliance reports no course, so the selects keep their last value.
    fn mirror(&mut self, panel: Seen) -> bool {
        let mut changed = false;
        if panel.course != self.seen.course {
            if let Some(course) = panel.course {
                self.remote.course = course;
                changed = true;
            }
        }
        if panel.dry_level != self.seen.dry_level {
            if let Some(dry_level) = panel.dry_level {
                self.remote.dry_level = dry_level;
                changed = true;
            }
        }
        if panel.eco_hybrid != self.seen.eco_hybrid {
            if let Some(eco_hybrid) = panel.eco_hybrid {
                self.remote.eco_hybrid = eco_hybrid;
                changed = true;
            }
        }
        if panel.anti_crease != self.seen.anti_crease {
            self.remote.anti_crease = panel.anti_crease == Some(true);
            changed = true;
        }
        self.seen = panel;
        changed
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    model_id: String,
    timers: Arc<dyn Timers>,
    selection: Mutex<Selection>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        timers: Arc<dyn Timers>,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            model_id: meta.model_id.clone(),
            timers,
            selection: Mutex::new(Selection::default()),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        config.components = components();
        for &(key, platform) in TOMBSTONES {
            config
                .components
                .insert(key.into(), json!({ "platform": platform }));
        }
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
        if status.get(1) != Some(&RECORD_MARKER) || status.len() < 2 + 21 {
            return;
        }
        let rec = &status[2..];
        let state = rec[0];
        let is_off = state == STATE_OFF;
        let is_active = state == STATE_RUNNING || state == STATE_PAUSED;
        let error = rec[6];
        let option1 = rec[14];
        self.selection.lock().cycle_running = state == STATE_RUNNING;

        self.core.publish_on_off("power", !is_off);
        self.core.publish_property(
            "error_message",
            named(ERRORS, error).unwrap_or("None").into(),
        );
        self.core.publish_on_off("error", error != 0);
        self.core
            .publish_property("status", named(STATES, state).unwrap_or("None").into());
        // The sub-phase byte keeps a stale value while idle.
        let phase = if is_active { phase_label(rec[9]) } else { None };
        self.core
            .publish_property("phase", phase.unwrap_or("None").into());
        self.core.publish_property(
            "remaining_time",
            PropertyValue::Int(if is_off { 0 } else { minutes(rec[1], rec[2]) }),
        );
        self.core.publish_property(
            "initial_time",
            PropertyValue::Int(if is_off { 0 } else { minutes(rec[3], rec[4]) }),
        );
        self.core.publish_property(
            "reserve_time",
            PropertyValue::Int(if is_off { 0 } else { minutes(rec[12], rec[13]) }),
        );
        self.core
            .publish_on_off("delay", option1 & OPT1_RESERVATION != 0);
        self.core
            .publish_on_off("remote_start", !is_off && rec[15] & OPT2_REMOTE_START != 0);
        self.core.publish_property(
            "energy",
            PropertyValue::Int(u16::from_be_bytes([rec[17], rec[18]]).into()),
        );
        self.core
            .publish_on_off("child_lock", option1 & OPT1_CHILD_LOCK != 0);

        if is_off {
            return;
        }
        let panel = Seen {
            course: named(COURSES, rec[20]).or_else(|| named(COURSES, rec[5])),
            dry_level: named(DRY_LEVELS, rec[7]),
            eco_hybrid: named(ECO_HYBRID, rec[8]),
            anti_crease: Some(option1 & OPT1_ANTI_CREASE != 0),
        };
        let mut selection = self.selection.lock();
        if selection.mirror(panel) {
            self.publish_remote(&selection.remote);
        }
    }

    fn publish_remote(&self, remote: &Remote) {
        self.core.publish_property("course", remote.course.into());
        self.core
            .publish_property("dry_level", remote.dry_level.into());
        self.core
            .publish_property("eco_hybrid", remote.eco_hybrid.into());
        self.core.publish_property(
            "anti_crease",
            if remote.anti_crease { "Yes" } else { "No" }.into(),
        );
        self.core.publish_property(
            "start_delay_end",
            PropertyValue::Int(remote.delay_hours.into()),
        );
    }

    fn start_selected(&self) {
        let (cmd, cycle_running) = {
            let mut selection = self.selection.lock();
            let Some(cmd) = start_command(&selection.remote) else {
                tracing::info!(
                    model = %self.model_id,
                    id = %self.core.id,
                    "{} cannot be started remotely",
                    selection.remote.course
                );
                return;
            };
            if selection.remote.delay_hours != 0 {
                selection.remote.delay_hours = 0;
                self.publish_remote(&selection.remote);
            }
            (cmd, selection.cycle_running)
        };
        if !cycle_running {
            self.core.send(&cmd);
            return;
        }
        self.core.send(START_PAUSE);
        let core = self.core.clone();
        self.timers
            .after(PAUSE_BEFORE_START, Box::new(move || core.send(&cmd)));
    }
}

/// Downloaded courses are not started remotely: how `F0 26` carries them is unverified.
fn start_command(remote: &Remote) -> Option<[u8; START_COURSE_LEN]> {
    let course = code(COURSES, remote.course).filter(|&c| c < FIRST_SMART_COURSE)?;
    let options = COURSE_OPTIONS.iter().find(|(c, ..)| *c == course);
    let mut cmd = [0u8; START_COURSE_LEN];
    cmd[0] = 0xf0;
    cmd[1] = 0x26;
    cmd[2] = course;
    cmd[3] = allowed(options.map(|o| o.1), code(DRY_LEVELS, remote.dry_level));
    cmd[4] = allowed(options.map(|o| o.2), code(ECO_HYBRID, remote.eco_hybrid));
    cmd[5] = options.map_or(0, |o| o.3);
    cmd[8] = remote.delay_hours;
    cmd[11] = if remote.anti_crease {
        OPT1_ANTI_CREASE
    } else {
        0x00
    };
    cmd[12] = START_COURSE_OP;
    Some(cmd)
}

/// The chosen value when the course takes it, otherwise the course default.
fn allowed(options: Option<&[u8]>, value: Option<u8>) -> u8 {
    match (options, value) {
        (None, value) => value.unwrap_or(0),
        (Some(options), Some(value)) if options.contains(&value) => value,
        (Some(options), _) => options[0],
    }
}

/// `0` clears the delay. The appliance drops anything below the minimum, so round up to it.
fn clamp_delay(value: &str) -> u8 {
    match js::number(value).map(f64::round) {
        Some(n) if n.is_finite() && n > 0.0 => {
            n.clamp(f64::from(DELAY_MIN_HOURS), f64::from(DELAY_MAX_HOURS)) as u8
        }
        _ => 0,
    }
}

fn minutes(hour: u8, min: u8) -> i64 {
    hour as i64 * 60 + min as i64
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn code(table: &[(u8, &str)], label: &str) -> Option<u8> {
    table
        .iter()
        .find(|(_, name)| *name == label)
        .map(|(c, _)| *c)
}

/// The table's own label, so a valid MQTT value can be held without copying it.
fn label(table: &'static [(u8, &'static str)], value: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(_, name)| *name == value)
        .map(|(_, name)| *name)
}

/// The spec's ProcessState 1 (Steam) is left out: this model has no steam.
fn phase_label(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Detecting"),
        2 | 3 | 4 => Some("Drying"),
        5 => Some("Cooling"),
        6 => Some("Anti-crease"),
        7 => Some("End"),
        _ => None,
    }
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn components() -> std::collections::HashMap<String, serde_json::Value> {
    let phase_options = ["Detecting", "Drying", "Cooling", "Anti-crease", "End"];
    [
        (
            "power".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-power",
                "state_topic": "$this/power",
                "command_topic": "$this/power/set",
                "name": "Power",
                "icon": "mdi:tumble-dryer",
            }),
        ),
        (
            "start_pause".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-start_pause",
                "command_topic": "$this/start_pause/set",
                "payload_press": "",
                "name": "Pause/Resume",
                "icon": "mdi:play-pause",
            }),
        ),
        (
            "start".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-start",
                "command_topic": "$this/start/set",
                "payload_press": "",
                "name": "Start selected cycle",
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
                "options": labels(STATES),
            }),
        ),
        (
            "phase".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-phase",
                "state_topic": "$this/phase",
                "name": "Phase",
                "icon": "mdi:progress-clock",
                "device_class": "enum",
                "options": phase_options,
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
                "options": labels(ERRORS),
            }),
        ),
        (
            "course".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-course",
                "state_topic": "$this/course",
                "command_topic": "$this/course/set",
                "name": "Course",
                "icon": "mdi:pin-outline",
                "options": labels(COURSES),
            }),
        ),
        (
            "dry_level".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-dry_level",
                "state_topic": "$this/dry_level",
                "command_topic": "$this/dry_level/set",
                "name": "Dry level",
                "icon": "mdi:water-percent",
                "options": labels(DRY_LEVELS),
            }),
        ),
        (
            "eco_hybrid".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-eco_hybrid",
                "state_topic": "$this/eco_hybrid",
                "command_topic": "$this/eco_hybrid/set",
                "name": "Eco Hybrid",
                "icon": "mdi:leaf",
                "options": labels(ECO_HYBRID),
            }),
        ),
        // A select, not a switch: HA's controls-card toggle would switch it on with the power.
        (
            "anti_crease".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-anti_crease",
                "state_topic": "$this/anti_crease",
                "command_topic": "$this/anti_crease/set",
                "name": "Anti crease",
                "icon": "mdi:iron-outline",
                "options": NO_YES,
            }),
        ),
        // Applies to the next Start selected cycle only, then returns to 0, as in the ThinQ app.
        (
            "start_delay_end".into(),
            json!({
                "platform": "number",
                "unique_id": "$deviceid-start_delay_end",
                "state_topic": "$this/start_delay_end",
                "command_topic": "$this/start_delay_end/set",
                "min": 0,
                "max": DELAY_MAX_HOURS,
                "step": 1,
                "mode": "box",
                "unit_of_measurement": "h",
                "name": "Delay end of the next start",
                "icon": "mdi:timer-play-outline",
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
            "initial_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-initial_time",
                "state_topic": "$this/initial_time",
                "name": "Initial time",
                "icon": "mdi:clock-outline",
                "device_class": "duration",
                "unit_of_measurement": "min",
                "entity_category": "diagnostic",
            }),
        ),
        (
            "reserve_time".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-reserve_time",
                "state_topic": "$this/reserve_time",
                "name": "Delay end",
                "icon": "mdi:timer-sand",
                "device_class": "duration",
                "unit_of_measurement": "min",
            }),
        ),
        (
            "delay".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-delay",
                "state_topic": "$this/delay",
                "name": "Delay end set",
                "icon": "mdi:timer-sand",
            }),
        ),
        (
            "remote_start".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-remote_start",
                "state_topic": "$this/remote_start",
                "name": "Remote start",
                "icon": "mdi:cellphone-wireless",
            }),
        ),
        (
            "energy".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-energy",
                "state_topic": "$this/energy",
                "name": "Energy",
                "icon": "mdi:lightning-bolt",
                "device_class": "energy",
                "unit_of_measurement": "Wh",
                "state_class": "total_increasing",
            }),
        ),
        (
            "child_lock".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-child_lock",
                "state_topic": "$this/child_lock",
                "name": "Child lock",
                "icon": "mdi:lock-outline",
                "entity_category": "diagnostic",
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
        self.publish_remote(&self.selection.lock().remote);
        request_status(&self.core);
    }
    fn take_over(&self, previous: &dyn DeviceHandler) {
        let previous: &dyn Any = previous;
        if let Some(previous) = previous.downcast_ref::<Device>() {
            let selection = previous.selection.lock().clone();
            *self.selection.lock() = selection;
        }
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "power" => {
                if value == "ON" {
                    self.core.send(POWER_ON);
                } else if value == "OFF" {
                    self.core.send(POWER_OFF);
                }
                return;
            }
            "start_pause" => {
                self.core.send(START_PAUSE);
                return;
            }
            "start" => {
                self.start_selected();
                return;
            }
            _ => {}
        }
        let mut selection = self.selection.lock();
        let remote = &mut selection.remote;
        match prop {
            "course" => {
                if let Some(course) = label(COURSES, value) {
                    remote.course = course;
                }
            }
            "dry_level" => {
                if let Some(dry_level) = label(DRY_LEVELS, value) {
                    remote.dry_level = dry_level;
                }
            }
            "eco_hybrid" => {
                if let Some(eco_hybrid) = label(ECO_HYBRID, value) {
                    remote.eco_hybrid = eco_hybrid;
                }
            }
            "anti_crease" => remote.anti_crease = value == "Yes",
            "start_delay_end" => remote.delay_hours = clamp_delay(value),
            _ => return,
        }
        self.publish_remote(remote);
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
    Device::new(ha, thinq, meta, Arc::new(ThreadTimers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ManualTimers;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";

    // Reply to the F0ED status request: on, Cotton selected, Cupboard, Eco, estimate 2:30.
    const EB_READY_COTTON: &str =
        "aa2130eb001901021e00000700030102000000000804000000000000006b00f1bb";

    // Commands, energy and Remote start: real frames from a second RC90U2.
    // Off after a completed cycle: all zero but the retained cycle energy (0x04f1 = 1265 Wh).
    const IDLE: &str = "aa2130eb0019000000000000000000000000000000040004f1040000006b0032bb";
    // Paused inside a Mixed Fabric cycle, Extra, Time: 43 min left of 1:20, 964 Wh so far.
    const PAUSED: &str = "aa2130eb001903002b011406000403040000000000040003c4020000006b00debb";
    // The moment the same cycle starts: the energy counter restarts.
    const STARTED: &str = "aa3c30ec001902011401140600040302000000000005500000010000006b00001902011401140600040302000000000005000000010000006b0089bb";
    // Cotton at 2:30: Anti crease on (Option1 bit 1), then Remote Start held on (Option2 bit 0).
    const ANTI_CREASE_ON: &str = "aa3c30ec001901021e00000700030102000000000004000000000000006b00001901021e00000700030102000000000204000000000000006b0025bb";
    const REMOTE_START_ON: &str = "aa3c30ec001901021e00000700030102000000000005200000000000006b00001901021e00000700030102000000000005000000000000006b00c5bb";
    // Ready on Cotton before and after downloading "Full Size Load" from the ThinQ app.
    const READY_COTTON: &str = "aa2130eb001901021e0300070000030200000000000500000003000000000062bb";
    const READY_DOWNLOADED: &str =
        "aa2130eb00190102280300070004030200000000000500000003740000740078bb";

    const WRITE_STATUS_REQUEST: &str = "AA0EF0ED1121010000001800B5BB";
    const WRITE_POWER_ON: &str = "AA08F02A010098BB";
    const WRITE_POWER_OFF: &str = "AA09F0240101009CBB";
    const WRITE_START_PAUSE: &str = "AA09F02404010099BB";

    fn meta_id(model: &str) -> Metadata {
        Metadata::new(model, model, "2.9.61")
    }

    fn make_with_timers(
        model: &str,
    ) -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<Device>,
        Arc<ManualTimers>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta_id(model));
        let timers = Arc::new(ManualTimers::default());
        let dev = Device::new(ha.clone(), thinq.clone(), meta_id(model), timers.clone());
        (ha, thinq, dev, timers)
    }

    fn make(model: &str) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let (ha, thinq, dev, _) = make_with_timers(model);
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> PropertyValue {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .get(name)
            .cloned()
            .unwrap()
    }

    fn empty(ha: &MockHaConnection) -> bool {
        ha.device(DEVICE_ID).unwrap().properties.is_empty()
    }

    fn last_packet(thinq: &MockThinq2Device) -> Vec<u8> {
        thinq.outbox().last().cloned().unwrap()
    }

    #[test]
    fn aliases_share_this_handler() {
        let a = crate::t2_factory("RC90U2_WW").unwrap();
        let b = crate::t2_factory("RC90V9_WW").unwrap();
        let c = crate::t2_factory("RC90V9_AUS").unwrap();
        assert!(std::ptr::fn_addr_eq(a, b));
        assert!(std::ptr::fn_addr_eq(a, c));
    }

    #[test]
    fn start_asks_for_a_status_snapshot() {
        let (_ha, thinq, dev) = make("RC90U2_WW");
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        let packet = thinq.outbox();
        assert_eq!(packet.len(), 1);
        assert_eq!(hex_encode(&packet[0]), "AA0EF0ED1121010000001800B5BB");
    }

    #[test]
    fn eb_ready_cotton() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(EB_READY_COTTON));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Ready".into());
        assert_eq!(prop(&ha, "phase"), "None".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Eco".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(150));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "error"), "OFF".into());
        assert_eq!(prop(&ha, "error_message"), "OK".into());
        assert_eq!(prop(&ha, "delay"), "OFF".into());
        assert_eq!(prop(&ha, "anti_crease"), "No".into());
        assert_eq!(prop(&ha, "child_lock"), "OFF".into());
        assert_eq!(prop(&ha, "remote_start"), "OFF".into());
        assert_eq!(prop(&ha, "energy"), PropertyValue::Int(0));
    }

    #[test]
    fn withdrawn_binary_sensors_are_tombstoned() {
        for model in ["RC90U2_WW", "RC90V9_WW", "RC90V9_AUS"] {
            let (ha, thinq, _dev) = make(model);
            thinq.emit_data(&hex_decode(EB_READY_COTTON));
            let info = ha.device(DEVICE_ID).unwrap();
            let components = &info.config.as_ref().unwrap().components;
            for key in ["damp_dry_beep", "hand_iron"] {
                assert_eq!(
                    components[key],
                    json!({"platform": "binary_sensor"}),
                    "{model}"
                );
                assert!(!info.properties.contains_key(key), "{model} {key}");
            }
            let phases = components["phase"]["options"].as_array().unwrap();
            assert!(!phases.contains(&json!("Steam")), "{model}");
        }
    }

    #[test]
    fn power_off_keeps_the_course_selects() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(EB_READY_COTTON));
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001900000000000000000000000000000804000000010000006B0004BB",
        ));
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "status"), "Off".into());
        // The selects are also what Start selected cycle sends.
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Eco".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(0));
    }

    #[test]
    fn power_on_without_a_course_leaves_the_course_select_alone() {
        let (ha, thinq, dev) = make("RC90U2_WW");
        DeviceHandler::start(dev.as_ref());
        thinq.emit_data(&hex_decode(
            "AA3C30EC001900000000000000000000000000000804000000010000006B00001901000000000000000000000000000804000000000000006B0071BB",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Ready".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(0));
    }

    #[test]
    fn course_knob_mixed_and_duvet() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001901010A00000600030302000000000804000000000000006B003FBB",
        ));
        assert_eq!(prop(&ha, "course"), "Mixed Fabric".into());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(70));

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901010A00000600030302000000000804000000000000006B00001901023700000400000302000000000804000000000000006B002ABB",
        ));
        assert_eq!(prop(&ha, "course"), "Duvet".into());
        // Timed course, no dry level: the select keeps the last one.
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(175));
    }

    #[test]
    fn timed_downloaded_and_cool_air() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901023700000400000302000000000004000000000000006B00001901030000001000000302000000000004000000000000006B003BBB",
        ));
        assert_eq!(prop(&ha, "course"), "Allergy Care".into());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(180));

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901001E00000900000302000000000004000000000000006B00001901002700000100000304000000000004000000006B00006B008FBB",
        ));
        assert_eq!(prop(&ha, "course"), "Deodorization".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(39));

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901010F00000E00000102000000000004000000000000006B00001901010000000D00000005000000000004000000000000006B001DBB",
        ));
        assert_eq!(prop(&ha, "course"), "Cool Air".into());
        // Cool Air has no Eco Hybrid: the select keeps the last one.
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "phase"), "None".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(60));
    }

    #[test]
    fn delay_end_sets_reserve_and_the_option_bit() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001901021E00000700030102030003000904000000000000006B00D0BB",
        ));
        assert_eq!(prop(&ha, "delay"), "ON".into());
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(180));
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "anti_crease"), "No".into());
        assert_eq!(prop(&ha, "child_lock"), "OFF".into());
    }

    #[test]
    fn dry_level_iron() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021E00000700030102000000000804000000000000006B00001901011E00000700010302000000000804000000000000006B0028BB",
        ));
        assert_eq!(prop(&ha, "dry_level"), "Iron".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(90));
    }

    #[test]
    fn start_cool_air_publishes_cooling() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901001E00000D00000005000000000004000000000000006B00001902001E001E0D00000005000000000005000000010000006B00C0BB",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Running".into());
        assert_eq!(prop(&ha, "phase"), "Cooling".into());
        assert_eq!(prop(&ha, "course"), "Cool Air".into());
        // No dry level nor Eco Hybrid on Cool Air: the selects keep their defaults.
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Eco".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(30));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(30));
        assert_eq!(prop(&ha, "delay"), "OFF".into());
        assert_eq!(prop(&ha, "error"), "OFF".into());
        assert_eq!(prop(&ha, "remote_start"), "ON".into());
        assert_eq!(prop(&ha, "energy"), PropertyValue::Int(0));
    }

    #[test]
    fn serial_power_and_start_events_are_ignored() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(
            "AA3730310201534141333939333530303900007FD200008000000000000253414133393933343931320000A36700004000000000000FBB",
        ));
        thinq.emit_data(&hex_decode("AA07301900AFBB"));
        thinq.emit_data(&hex_decode("AA09307200C9004BBB"));
        assert!(empty(&ha));
    }

    #[test]
    fn wrong_length_ec_is_rejected() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode("aa0830ec0019010000bb"));
        assert!(empty(&ha));
    }

    #[test]
    fn off_retains_the_last_cycle_energy() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(IDLE));
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "energy"), PropertyValue::Int(1265));
        assert_eq!(prop(&ha, "remote_start"), "OFF".into());
    }

    #[test]
    fn paused_and_started_frames() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(PAUSED));
        assert_eq!(prop(&ha, "status"), "Paused".into());
        assert_eq!(prop(&ha, "phase"), "Drying".into());
        assert_eq!(prop(&ha, "course"), "Mixed Fabric".into());
        assert_eq!(prop(&ha, "dry_level"), "Extra".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(43));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(80));
        assert_eq!(prop(&ha, "energy"), PropertyValue::Int(964));
        assert_eq!(prop(&ha, "remote_start"), "OFF".into(), "dropped in pause");

        thinq.emit_data(&hex_decode(STARTED));
        assert_eq!(prop(&ha, "status"), "Running".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(80));
        assert_eq!(prop(&ha, "energy"), PropertyValue::Int(0));
    }

    #[test]
    fn anti_crease_and_remote_start_are_separate_bits() {
        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(ANTI_CREASE_ON));
        assert_eq!(prop(&ha, "anti_crease"), "Yes".into());
        assert_eq!(prop(&ha, "remote_start"), "OFF".into());

        let (ha, thinq, _dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(REMOTE_START_ON));
        assert_eq!(prop(&ha, "anti_crease"), "No".into());
        assert_eq!(prop(&ha, "remote_start"), "ON".into());
    }

    #[test]
    fn a_downloaded_course_is_shown_but_not_started_remotely() {
        let (ha, thinq, dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(READY_DOWNLOADED));
        assert_eq!(prop(&ha, "course"), "Full Size Load".into());
        assert_eq!(prop(&ha, "dry_level"), "Extra".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(160));

        thinq.reset_recorder();
        dev.set_property("start", "");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn the_panel_is_mirrored_without_overriding_a_choice_made_in_ha() {
        let (ha, thinq, dev) = make("RC90U2_WW");
        DeviceHandler::start(dev.as_ref());
        assert_eq!(prop(&ha, "course"), "Cotton".into(), "default");

        thinq.emit_data(&hex_decode(PAUSED));
        assert_eq!(prop(&ha, "course"), "Mixed Fabric".into());
        assert_eq!(prop(&ha, "dry_level"), "Extra".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Time".into());

        dev.set_property("course", "Quick Dry");
        thinq.emit_data(&hex_decode(PAUSED));
        assert_eq!(
            prop(&ha, "course"),
            "Quick Dry".into(),
            "an unchanged appliance value does not overwrite HA"
        );

        thinq.emit_data(&hex_decode(READY_COTTON));
        assert_eq!(
            prop(&ha, "course"),
            "Cotton".into(),
            "a new value on the dial does"
        );
    }

    #[test]
    fn start_selected_cycle_sends_f0_26_with_the_selected_settings() {
        let (ha, thinq, dev) = make("RC90U2_WW");
        DeviceHandler::start(dev.as_ref());
        assert_eq!(prop(&ha, "dry_level"), "Cupboard".into());
        assert_eq!(prop(&ha, "eco_hybrid"), "Eco".into());
        assert_eq!(prop(&ha, "anti_crease"), "No".into());
        assert_eq!(prop(&ha, "start_delay_end"), PropertyValue::Int(0));

        dev.set_property("course", "Mixed Fabric");
        dev.set_property("dry_level", "Extra");
        dev.set_property("eco_hybrid", "Time");
        dev.set_property("anti_crease", "Yes");

        thinq.reset_recorder();
        dev.set_property("start", "");
        // course 0x06, dry level 0x04, Time 0x03, [11] Anti crease, [12] operation 0x01
        assert_eq!(
            hex_encode(&last_packet(&thinq)),
            "AA14F0260604030000000000000201000000B1BB"
        );

        // An unknown value is ignored rather than encoded.
        dev.set_property("course", "Not a course");
        assert_eq!(prop(&ha, "course"), "Mixed Fabric".into());
    }

    #[test]
    fn start_selected_cycle_sends_only_what_the_course_takes() {
        let (_ha, thinq, dev) = make("RC90U2_WW");
        DeviceHandler::start(dev.as_ref());
        dev.set_property("dry_level", "Extra");
        dev.set_property("eco_hybrid", "Eco");

        // Quick Dry has no dry level and runs on Time only.
        dev.set_property("course", "Quick Dry");
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(
            hex_encode(&last_packet(&thinq)),
            "AA14F0260900030000000000000001000000B4BB"
        );

        // Rack Dry is time-based: its 180 minutes go into [5].
        dev.set_property("course", "Rack Dry");
        dev.set_property("start", "");
        assert_eq!(
            hex_encode(&last_packet(&thinq)),
            "AA14F0260C0001B400000000000001000000C3BB"
        );

        // Easy Care has Iron and Cupboard only: Extra falls back to its default, Cupboard.
        dev.set_property("course", "Easy Care");
        dev.set_property("start", "");
        let cmd = last_packet(&thinq);
        assert_eq!(cmd[5], 0x03);
        assert_eq!(cmd[6], 0x01, "Eco is kept");
    }

    #[test]
    fn delay_end_of_the_next_start_is_clamped_and_cleared_after_a_start() {
        let (ha, thinq, dev) = make("RC90U2_WW");
        DeviceHandler::start(dev.as_ref());
        dev.set_property("course", "Cotton Soft");
        dev.set_property("eco_hybrid", "Time");

        // Same course, Eco Hybrid and delay as the ThinQ app's own "end in 3 hours".
        dev.set_property("start_delay_end", "3");
        thinq.reset_recorder();
        dev.set_property("start", "");
        let cmd = last_packet(&thinq);
        assert_eq!(cmd[4], 0x02); // Cotton Soft
        assert_eq!(cmd[6], 0x03); // Time
        assert_eq!(cmd[10], 3); // hours
        assert_eq!(
            prop(&ha, "start_delay_end"),
            PropertyValue::Int(0),
            "cleared after the start"
        );

        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(last_packet(&thinq)[10], 0, "the next start is not delayed");

        assert_eq!(clamp_delay("1"), 3);
        assert_eq!(clamp_delay("25"), 19);
        assert_eq!(clamp_delay("0"), 0);
        assert_eq!(clamp_delay("nope"), 0);
    }

    #[test]
    fn mid_cycle_f0_26_is_preceded_by_a_pause() {
        let (_ha, thinq, dev, timers) = make_with_timers("RC90U2_WW");
        thinq.emit_data(&hex_decode(STARTED));
        dev.set_property("course", "Quick Dry");
        thinq.reset_recorder();

        dev.set_property("start", "");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_START_PAUSE);

        timers.tick(1300);
        assert_eq!(thinq.outbox().len(), 2);
        assert!(hex_encode(&thinq.outbox()[1]).starts_with("AA14F02609"));
    }

    #[test]
    fn a_reconnect_keeps_the_choices_made_in_ha() {
        let (ha, thinq, dev) = make("RC90U2_WW");
        thinq.emit_data(&hex_decode(PAUSED));
        dev.set_property("course", "Quick Dry");
        dev.set_property("start_delay_end", "4");

        let thinq_again = MockThinq2Device::new(DEVICE_ID, meta_id("RC90U2_WW"));
        let replacement = Device::new(
            ha.clone(),
            thinq_again.clone(),
            meta_id("RC90U2_WW"),
            Arc::new(ManualTimers::default()),
        );
        replacement.take_over(dev.as_ref());
        DeviceHandler::start(replacement.as_ref());
        thinq_again.emit_data(&hex_decode(PAUSED));

        assert_eq!(prop(&ha, "course"), "Quick Dry".into());
        assert_eq!(prop(&ha, "start_delay_end"), PropertyValue::Int(4));
    }

    #[test]
    fn status_request_on_start_power_and_start_pause_commands() {
        let (_ha, thinq, dev) = make("RC90U2_WW");
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(hex_encode(&last_packet(&thinq)), WRITE_STATUS_REQUEST);

        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&last_packet(&thinq)), WRITE_POWER_ON);
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&last_packet(&thinq)), WRITE_POWER_OFF);
        dev.set_property("start_pause", "");
        assert_eq!(hex_encode(&last_packet(&thinq)), WRITE_START_PAUSE);
    }

    #[test]
    fn rc90v9_cycle_and_child_lock() {
        let (ha, thinq, _dev) = make("RC90V9_WW");
        thinq.emit_data(&hex_decode(
            "AA3C30EC001901021400000200000302000000000804000000000000006B00001901001E00000900000302000000000804000000000000006B0038BB",
        ));
        assert_eq!(prop(&ha, "status"), "Ready".into());
        assert_eq!(prop(&ha, "course"), "Quick Dry".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(30));

        thinq.emit_data(&hex_decode(
            "AA3C30EC0019020224030007000401020000000000050000AD0100000071000019020224030007000401030000000000050000AE010000007100A5BB",
        ));
        assert_eq!(prop(&ha, "status"), "Running".into());
        assert_eq!(prop(&ha, "phase"), "Drying".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "dry_level"), "Extra".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(156));
        assert_eq!(prop(&ha, "energy"), PropertyValue::Int(174));

        thinq.emit_data(&hex_decode(
            "AA3C30EC00190200030300070004010400000000000500043F010000007100001902000203000700040105000000002005000444010000007100A8BB",
        ));
        assert_eq!(prop(&ha, "phase"), "Cooling".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(2));
        assert_eq!(prop(&ha, "energy"), PropertyValue::Int(1092));

        thinq.emit_data(&hex_decode(
            "AA3C30EC00190200010300070004010500000000200500044801000000710000190400010300070004010700000000000400044902000000710058BB",
        ));
        assert_eq!(prop(&ha, "status"), "End".into());
        assert_eq!(
            prop(&ha, "energy"),
            PropertyValue::Int(1097),
            "retained after the cycle"
        );

        thinq.emit_data(&hex_decode(
            "AA3C30EC001901000000000000000000000000000004000000000000007100001901000000000000000000000000001004700000000000007100F5BB",
        ));
        assert_eq!(prop(&ha, "child_lock"), "ON".into());
    }
}
