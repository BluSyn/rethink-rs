//! F_V__F___W.B__ECON — LG CV9014WC2 washer/dryer.
//!
//! The AABB washer protocol: 39-byte records in a class `0x20` envelope, read at the
//! Y_V8_Y___W.B32QEUK offsets. The 0x60-byte live-state frame (`EC`) carries two consecutive
//! snapshots, and only the second, newer one is published. The 0x39-byte frame carries one; the
//! captured one is typed `E2` and is read like `EB`. Spin and temperature are published as RPM and
//! °C.
//!
//! Staging a course loads its own spin, temperature, rinse, dry and option bytes; spin,
//! temperature, rinse and dry can then be changed and a delay in hours added. Set staged program
//! sends the result as `F0 25 03 15 …`, Start staged program as `F0 26 …`; left untouched, the
//! latter reproduces the start frames captured from the washer. Downloaded cycles are not
//! supported. `auto_ack` stays off.
//!
//! Port of upstream PR #190.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{request_status, set_power_start_pause};
use crate::washer_common::{
    course_name, drying_mode, error_options, fy_steam_name, washer_status_options, ERRORS, FY_STEAM,
};
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const STATUS_LENGTH: usize = 39;

// The envelope fields `process_enveloped_status` checks, for the `E2` record it does not take.
const FRAME_TYPE_ENVELOPE: u8 = 0x0a;
const ENVELOPE_HEADER_LEN: usize = 13;
const ENVELOPE_TYPE_OFFSET: usize = 10;
const ENVELOPE_LEN_OFFSET: usize = 11;
const RECORD_TYPE_E2: u8 = 0xe2;

const POWER_TOGGLE: &[u8] = &[0xf0, 0x2a, 0x01, 0x00];

const MAX_DELAY_HOURS: u8 = 19;

const LOCK_REMOTE_START: u8 = 0x02;
const LOCK_DOOR: u8 = 0x40;
const LOCK_CHILD: u8 = 0x80;

const OPT_TURBO_WASH: u8 = 0x01;
const OPT_CREASE_CARE: u8 = 0x02;
const OPT_ECO_HYBRID: u8 = 0x08;
const OPT_PRE_WASH: u8 = 0x40;
const OPT_STEAM: u8 = 0x80;

const SET_PROGRAM: &[u8] = &[0xf0, 0x25, 0x03, 0x15];
const START_PROGRAM: &[u8] = &[0xf0, 0x26];

/// Each course with the bytes it starts with: course, mode, spin, temperature, rinse, dry,
/// options.
const COURSES: &[(&str, [u8; 7])] = &[
    ("Cotton", [0x01, 0x03, 0xff, 0x04, 0x01, 0x00, 0x00]),
    ("Synthetics", [0x02, 0x03, 0xff, 0x04, 0x01, 0x00, 0x00]),
    ("Cotton+", [0x04, 0x03, 0xff, 0x06, 0x01, 0x00, 0x00]),
    ("Mix", [0x07, 0x03, 0x07, 0x04, 0x01, 0x00, 0x00]),
    ("Quick 14", [0x0c, 0x03, 0x02, 0x02, 0x01, 0x00, 0x01]),
    ("Steam refresh", [0x0d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80]),
    ("Drum Clean", [0x12, 0x03, 0x01, 0x06, 0x01, 0x00, 0x00]),
    ("Wash + Dry", [0x13, 0x03, 0xff, 0x04, 0x01, 0x02, 0x00]),
    ("Drying", [0x18, 0x00, 0x01, 0x00, 0x00, 0x02, 0x00]),
    ("Wool", [0x1b, 0x03, 0x05, 0x03, 0x01, 0x00, 0x00]),
    ("Delicate", [0x20, 0x03, 0x05, 0x02, 0x01, 0x00, 0x00]),
    ("Allergy Care", [0x2d, 0x03, 0xff, 0x06, 0x01, 0x00, 0x80]),
    ("TurboWash 39", [0x31, 0x03, 0x09, 0x04, 0x01, 0x00, 0x01]),
];

/// The staged course and its settings.
#[derive(Clone, Copy)]
struct Program {
    course: u8,
    mode: u8,
    spin: u8,
    temp: u8,
    rinse: u8,
    dry: u8,
    options: u8,
}

impl Program {
    fn of_course(name: &str) -> Option<Self> {
        let &(_, [course, mode, spin, temp, rinse, dry, options]) =
            COURSES.iter().find(|(n, _)| *n == name)?;
        Some(Self {
            course,
            mode,
            spin,
            temp,
            rinse,
            dry,
            options,
        })
    }

    /// `header`, then the part Set and Start staged program share.
    fn command(&self, header: &[u8], delay: u8) -> Vec<u8> {
        let Self {
            course,
            mode,
            spin,
            temp,
            rinse,
            dry,
            options,
        } = *self;
        let mut cmd = header.to_vec();
        cmd.extend_from_slice(&[
            course, mode, spin, temp, rinse, dry, delay, 0, 0, options, 0x03, 0, 0, 0, 0, 0,
        ]);
        cmd
    }
}

const SPINS: &[(u8, &str)] = &[
    (0x01, "0"),
    (0x02, "400"),
    (0x05, "800"),
    (0x07, "1000"),
    (0x09, "1200"),
    (0xff, "1400"),
];

const TEMPERATURES: &[(u8, &str)] = &[
    (0x01, "Cold"),
    (0x02, "20"),
    (0x03, "30"),
    (0x04, "40"),
    (0x05, "50"),
    (0x06, "60"),
];

const RINSES: &[(u8, &str)] = &[(0x01, "Normal"), (0x02, "Rinse+")];

/// The washer reports 1400 RPM as 0x0a as well as 0xff.
fn spin_rpm(code: u8) -> Option<i64> {
    match code {
        0x01 => Some(0),
        0x02 => Some(400),
        0x05 => Some(800),
        0x07 => Some(1000),
        0x09 => Some(1200),
        0x0a | 0xff => Some(1400),
        _ => None,
    }
}

fn temperature_celsius(code: u8) -> Option<i64> {
    match code {
        0x02 => Some(20),
        0x03 => Some(30),
        0x04 => Some(40),
        0x05 => Some(50),
        0x06 => Some(60),
        _ => None,
    }
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or("None")
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn code_of(table: &'static [(u8, &'static str)], name: &str) -> Option<u8> {
    table.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

/// This washer's own course names first, then the shared table's.
fn course_label(code: u8) -> &'static str {
    COURSES
        .iter()
        .find(|(_, bytes)| bytes[0] == code)
        .map(|(name, _)| *name)
        .or_else(|| course_name(code.into()))
        .unwrap_or("None")
}

fn error_label(code: u8) -> &'static str {
    ERRORS
        .get(usize::from(code))
        .copied()
        .flatten()
        .unwrap_or("None")
}

fn drying_label(code: u8) -> &'static str {
    drying_mode(code.into()).unwrap_or("None")
}

fn drying_code(name: &str) -> Option<u8> {
    (0..=u8::MAX).find(|&code| drying_mode(code.into()) == Some(name))
}

/// The shared drying modes in code order, which is also their upstream option order.
fn drying_options() -> Vec<&'static str> {
    (0..=u8::MAX)
        .filter_map(|code| drying_mode(code.into()))
        .collect()
}

/// One record under envelope type `E2`, framed like `EB`.
fn e2_record(buf: &[u8]) -> Option<&[u8]> {
    if buf.len() < ENVELOPE_HEADER_LEN
        || buf[0] != CLASS_BYTE
        || buf[1] != FRAME_TYPE_ENVELOPE
        || buf[ENVELOPE_TYPE_OFFSET] != RECORD_TYPE_E2
    {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([
        buf[ENVELOPE_LEN_OFFSET],
        buf[ENVELOPE_LEN_OFFSET + 1],
    ]));
    let end = ENVELOPE_HEADER_LEN + len;
    // A single trailing byte is the long framing's checksum.
    (len == STATUS_LENGTH && (end..=end + 1).contains(&buf.len()))
        .then(|| &buf[ENVELOPE_HEADER_LEN..end])
}

fn components() -> HashMap<String, Value> {
    let courses: Vec<&str> = COURSES.iter().map(|(name, _)| *name).collect();
    [
        ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"","icon":"mdi:washing-machine"})),
        ("start", json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
        ("pause", json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
        ("power_toggle", json!({"platform":"button","unique_id":"$deviceid-power_toggle","command_topic":"$this/power_toggle/set","payload_press":"","name":"Power toggle","icon":"mdi:power-cycle"})),
        ("stage_program", json!({"platform":"select","unique_id":"$deviceid-stage_program","state_topic":"$this/stage_program","command_topic":"$this/stage_program/set","name":"Staged: Program","icon":"mdi:format-list-bulleted","options":courses})),
        ("stage_spin", json!({"platform":"select","unique_id":"$deviceid-stage_spin","state_topic":"$this/stage_spin","command_topic":"$this/stage_spin/set","name":"Staged: Spin","icon":"mdi:autorenew","options":labels(SPINS)})),
        ("stage_temp", json!({"platform":"select","unique_id":"$deviceid-stage_temp","state_topic":"$this/stage_temp","command_topic":"$this/stage_temp/set","name":"Staged: Temperature","icon":"mdi:thermometer","options":labels(TEMPERATURES)})),
        ("stage_rinse", json!({"platform":"select","unique_id":"$deviceid-stage_rinse","state_topic":"$this/stage_rinse","command_topic":"$this/stage_rinse/set","name":"Staged: Rinse","icon":"mdi:water-sync","options":labels(RINSES)})),
        ("stage_dry", json!({"platform":"select","unique_id":"$deviceid-stage_dry","state_topic":"$this/stage_dry","command_topic":"$this/stage_dry/set","name":"Staged: Dry","icon":"mdi:tumble-dryer","options":drying_options()})),
        ("stage_delay", json!({"platform":"number","unique_id":"$deviceid-stage_delay","state_topic":"$this/stage_delay","command_topic":"$this/stage_delay/set","name":"Staged: Delay","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"h","min":0,"max":MAX_DELAY_HOURS,"step":1})),
        ("set_program", json!({"platform":"button","unique_id":"$deviceid-set_program","command_topic":"$this/set_program/set","payload_press":"","name":"Set staged program","icon":"mdi:upload-outline"})),
        ("start_program", json!({"platform":"button","unique_id":"$deviceid-start_program","command_topic":"$this/start_program/set","payload_press":"","name":"Start staged program","icon":"mdi:play-box-outline"})),
        ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":washer_status_options(FY_STEAM)})),
        ("error", json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:check-circle","device_class":"problem","entity_category":"diagnostic"})),
        ("error_message", json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","device_class":"enum","entity_category":"diagnostic","options":error_options()})),
        ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
        ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","device_class":"temperature","unit_of_measurement":"°C","suggested_display_precision":0})),
        ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","unit_of_measurement":"RPM"})),
        ("dry", json!({"platform":"sensor","unique_id":"$deviceid-dry","state_topic":"$this/dry","name":"Dry","icon":"mdi:tumble-dryer","device_class":"enum","options":drying_options()})),
        ("cycles", json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycle count","icon":"mdi:counter"})),
        ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:play-circle-outline"})),
        ("door_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","device_class":"lock"})),
        ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:account-lock-outline"})),
        ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
        ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","device_class":"duration","unit_of_measurement":"min"})),
        ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","device_class":"duration","unit_of_measurement":"min"})),
        ("turbo_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_wash","state_topic":"$this/turbo_wash","name":"Turbo wash","icon":"mdi:weather-windy"})),
        ("crease_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-crease_care","state_topic":"$this/crease_care","name":"Crease care","icon":"mdi:iron-outline"})),
        ("eco_hybrid", json!({"platform":"binary_sensor","unique_id":"$deviceid-eco_hybrid","state_topic":"$this/eco_hybrid","name":"Eco hybrid","icon":"mdi:leaf"})),
        ("pre_wash", json!({"platform":"binary_sensor","unique_id":"$deviceid-pre_wash","state_topic":"$this/pre_wash","name":"Pre-wash","icon":"mdi:water-outline"})),
        ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:heat-wave"})),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// What Set/Start staged program sends: the staged course with its settings, and the delay.
#[derive(Default)]
struct Staged {
    program: Option<Program>,
    delay: u8,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    staged: Mutex<Staged>,
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
            staged: Mutex::new(Staged::default()),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
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
        if let Some(record) =
            process_enveloped_status(buf, CLASS_BYTE, STATUS_LENGTH).or_else(|| e2_record(buf))
        {
            self.process_record(record);
        }
    }

    // Upstream's snapshot starts at byte 2 of the record.
    fn process_record(&self, s: &[u8]) {
        let status = s[2];
        let time_remain = i64::from(s[3]) * 60 + i64::from(s[4]);
        let time_initial = i64::from(s[5]) * 60 + i64::from(s[6]);
        let error = s[8];
        let options = s[16];
        let locks = s[17];

        self.core.publish_on_off("power", status > 0);
        self.core
            .publish_property("error_message", error_label(error).into());
        self.core.publish_on_off("error", error != 0);
        self.core
            .publish_property("status", fy_steam_name(status).into());
        self.core
            .publish_property("course", course_label(s[7]).into());
        self.publish_number("spin", spin_rpm(s[10]));
        self.publish_number("temp", temperature_celsius(s[11]));
        self.core
            .publish_property("dry", drying_label(s[13]).into());
        self.core
            .publish_property("cycles", i64::from(s[23]).into());
        self.core
            .publish_on_off("remote_start", locks & LOCK_REMOTE_START != 0);
        // HA's lock class uses OFF for locked.
        self.core
            .publish_on_off("door_lock", locks & LOCK_DOOR == 0);
        self.core
            .publish_on_off("child_lock", locks & LOCK_CHILD != 0);
        self.core
            .publish_on_off("turbo_wash", options & OPT_TURBO_WASH != 0);
        self.core
            .publish_on_off("crease_care", options & OPT_CREASE_CARE != 0);
        self.core
            .publish_on_off("eco_hybrid", options & OPT_ECO_HYBRID != 0);
        self.core
            .publish_on_off("pre_wash", options & OPT_PRE_WASH != 0);
        self.core.publish_on_off("steam", options & OPT_STEAM != 0);
        self.core
            .publish_property("initial_time", time_initial.into());
        self.core
            .publish_property("remaining_time", time_remain.into());
        self.core
            .publish_property("energy", i64::from(s[31]).into());
    }

    fn publish_number(&self, prop: &str, value: Option<i64>) {
        match value {
            Some(value) => self.core.publish_property(prop, PropertyValue::Int(value)),
            None => self.core.publish_property(prop, "None".into()),
        }
    }

    /// Staging a course resets every other staged setting to that course's own.
    fn stage_program(&self, value: &str) {
        let Some(program) = Program::of_course(value) else {
            return;
        };
        *self.staged.lock() = Staged {
            program: Some(program),
            delay: 0,
        };
        self.core.publish_property("stage_program", value.into());
        self.core
            .publish_property("stage_spin", named(SPINS, program.spin).into());
        self.core
            .publish_property("stage_temp", named(TEMPERATURES, program.temp).into());
        self.core
            .publish_property("stage_rinse", named(RINSES, program.rinse).into());
        self.core
            .publish_property("stage_dry", drying_label(program.dry).into());
        self.core
            .publish_property("stage_delay", PropertyValue::Int(0));
    }

    /// Changes the staged course, if any: staging one resets its settings anyway.
    fn stage_setting(
        &self,
        prop: &str,
        value: &str,
        code: Option<u8>,
        apply: fn(&mut Program, u8),
    ) {
        let Some(code) = code else {
            return;
        };
        if let Some(program) = self.staged.lock().program.as_mut() {
            apply(program, code);
        }
        self.core.publish_property(prop, value.into());
    }

    fn stage_delay(&self, value: &str) {
        let Some(hours) = crate::js::number(value) else {
            return;
        };
        if hours.fract() != 0.0 || !(0.0..=f64::from(MAX_DELAY_HOURS)).contains(&hours) {
            return;
        }
        self.staged.lock().delay = hours as u8;
        self.core
            .publish_property("stage_delay", PropertyValue::Int(hours as i64));
    }

    /// Nothing is sent until a course has been staged.
    fn send_staged(&self, header: &[u8]) {
        let command = {
            let staged = self.staged.lock();
            staged
                .program
                .map(|program| program.command(header, staged.delay))
        };
        if let Some(command) = command {
            self.core.send(&command);
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        request_status(&self.core);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        set_power_start_pause(&self.core, prop, value);
        match prop {
            "power_toggle" => self.core.send(POWER_TOGGLE),
            "stage_program" => self.stage_program(value),
            "stage_spin" => {
                self.stage_setting(prop, value, code_of(SPINS, value), |p, c| p.spin = c)
            }
            "stage_temp" => {
                self.stage_setting(prop, value, code_of(TEMPERATURES, value), |p, c| p.temp = c)
            }
            "stage_rinse" => {
                self.stage_setting(prop, value, code_of(RINSES, value), |p, c| p.rinse = c)
            }
            "stage_dry" => self.stage_setting(prop, value, drying_code(value), |p, c| p.dry = c),
            "stage_delay" => self.stage_delay(value),
            "set_program" => self.send_staged(SET_PROGRAM),
            "start_program" => self.send_staged(START_PROGRAM),
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
    use crate::test_support::{prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const MODEL_ID: &str = "F_V__F___W.B__ECON";

    // Captured while manually changing the selected course from Cotton+ to TurboWash 39. The
    // first/older snapshot reports Cotton+, the second/newer one TurboWash 39.
    const COURSE_CHANGE_60: &str = concat!(
        "aaff200a006000a7a0000100ec004e00",
        "0002041804180400030a060100000000",
        "40000004040014003c00000200000000",
        "00000000000000000600270027310003",
        "0904010000000142200001010014003c",
        "00000100000000000000000000034abb",
    );

    // Captured while manually changing from Synthetics to Allergy Care. The second/newer
    // snapshot reports spin byte 0x0a, which the CV9014WC2 reports as 1400 RPM.
    const ALLERGY_CARE_1400RPM_60: &str = concat!(
        "aaff200a006000a7bb000100ec004e00",
        "0004023102310200030a040100000000",
        "42210001010014003c00000300000000",
        "000000000000000004023002302d0003",
        "0a06010000008002200001010014003c",
        "000005000000000000000000000b95bb",
    );

    // Captured while manually selecting the Drying course. The second/newer snapshot reports
    // course 0x18 (Drying) and dry mode 0x02 (Auto).
    const DRYING_AUTO_60: &str = concat!(
        "aaff200a006000a897000100ec004e00",
        "0006000e000e0c000302020100000001",
        "42200001010014003c00000100000000",
        "00000000000000000201000100180000",
        "0000000200000040000006090014003c",
        "00000000000000000000000000d3dfbb",
    );

    // Captured with Cotton selected at 40 °C and 1200 RPM.
    const COTTON_40C_1200RPM_39: &str = concat!(
        "aaff200a0039006fec00010ae200270000",
        "040217021701000309040100000000022100010100190000000004000109",
        "00000000000000da09bb",
    );

    // Exact F026 start-program frames captured from the CV9014WC2. They validate the per-course
    // defaults used when a course is staged and started without overriding its spin,
    // temperature, rinse, dry mode, delay or option values.
    const START_PROGRAM_CAPTURES: [(&str, &str); 6] = [
        ("Cotton", "AA16F0260103FF04010000000000030000000000B4BB"),
        ("Synthetics", "AA16F0260203FF04010000000000030000000000B7BB"),
        ("Cotton+", "AA16F0260403FF06010000000000030000000000B3BB"),
        ("Mix", "AA16F02607030704010000000000030000000000BABB"),
        (
            "Steam refresh",
            "AA16F0260D00000000000000008003000000000033BB",
        ),
        ("Quick 14", "AA16F0260C030202010000000001030000000000BBBB"),
    ];

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new(MODEL_ID, "CV9014WC2", "test");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);
        (ha, thinq, dev)
    }

    fn feed(frame: &str) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn assert_start_program(program: &str) {
        let (_, frame) = START_PROGRAM_CAPTURES
            .iter()
            .find(|(name, _)| *name == program)
            .expect("captured program");
        let (_, thinq, dev) = make();
        thinq.reset_recorder();

        dev.set_property("stage_program", program);
        // Staging only changes the local staged values; nothing is sent until start_program.
        assert!(thinq.outbox().is_empty());

        dev.set_property("start_program", "");
        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 1);
        assert_eq!(hex_encode(&outbox[0]), *frame);
    }

    #[test]
    fn device_handler_can_be_constructed() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).expect("device");
        assert!(device.config.is_some());
    }

    #[test]
    fn packet_0x60_publishes_the_second_newer_econ_snapshot() {
        let ha = feed(COURSE_CHANGE_60);
        // The older snapshot is Cotton+; these are all the newer one's values, so they also show
        // the older snapshot was not published.
        expect(&ha, "course", "TurboWash 39");
        expect(&ha, "remaining_time", "39");
        expect(&ha, "initial_time", "39");
        expect(&ha, "spin", "1200");
        expect(&ha, "temp", "40");
        expect(&ha, "dry", "Off");
        expect(&ha, "turbo_wash", "ON");
        expect(&ha, "remote_start", "ON");
    }

    #[test]
    fn packet_0x60_decodes_reported_0x0a_spin_as_1400_rpm() {
        let ha = feed(ALLERGY_CARE_1400RPM_60);
        expect(&ha, "course", "Allergy Care");
        expect(&ha, "remaining_time", "168");
        expect(&ha, "initial_time", "168");
        expect(&ha, "spin", "1400");
        expect(&ha, "temp", "60");
        expect(&ha, "dry", "Off");
        expect(&ha, "steam", "ON");
        expect(&ha, "remote_start", "ON");
    }

    #[test]
    fn packet_0x60_decodes_captured_drying_course_and_auto_dry_mode() {
        let ha = feed(DRYING_AUTO_60);
        expect(&ha, "course", "Drying");
        expect(&ha, "dry", "Auto");
        expect(&ha, "remaining_time", "60");
        expect(&ha, "initial_time", "60");
    }

    #[test]
    fn packet_0x39_decodes_a_captured_cotton_cycle_state() {
        let ha = feed(COTTON_40C_1200RPM_39);
        expect(&ha, "power", "ON");
        // Upstream's old shared label for code 4 was Measuring.
        expect(&ha, "status", "Detecting");
        expect(&ha, "error", "OFF");
        expect(&ha, "course", "Cotton");
        expect(&ha, "remaining_time", "143");
        expect(&ha, "initial_time", "143");
        expect(&ha, "spin", "1200");
        expect(&ha, "temp", "40");
        expect(&ha, "dry", "Off");
        expect(&ha, "remote_start", "ON");
        expect(&ha, "child_lock", "OFF");
        expect(&ha, "steam", "OFF");
        expect(&ha, "cycles", "25");
        expect(&ha, "energy", "9");
    }

    #[test]
    fn start_program_sends_captured_cotton_command() {
        assert_start_program("Cotton");
    }

    #[test]
    fn start_program_sends_captured_synthetics_command() {
        assert_start_program("Synthetics");
    }

    #[test]
    fn start_program_sends_captured_cotton_plus_command() {
        assert_start_program("Cotton+");
    }

    #[test]
    fn start_program_sends_captured_mix_command() {
        assert_start_program("Mix");
    }

    #[test]
    fn start_program_sends_captured_steam_refresh_command() {
        assert_start_program("Steam refresh");
    }

    #[test]
    fn start_program_sends_captured_quick_14_command() {
        assert_start_program("Quick 14");
    }

    #[test]
    fn staged_settings_and_delay_reach_both_program_commands() {
        // The expected frames come from upstream's command builders, not from a capture.
        let (ha, thinq, dev) = make();
        dev.set_property("stage_spin", "800");
        dev.set_property("start_program", "");
        assert!(thinq.outbox().is_empty());

        dev.set_property("stage_program", "Wool");
        expect(&ha, "stage_spin", "800");
        expect(&ha, "stage_temp", "30");
        expect(&ha, "stage_rinse", "Normal");
        expect(&ha, "stage_dry", "Off");
        expect(&ha, "stage_delay", "0");

        dev.set_property("stage_spin", "1200");
        dev.set_property("stage_temp", "Cold");
        dev.set_property("stage_rinse", "Rinse+");
        dev.set_property("stage_dry", "Auto");
        dev.set_property("stage_delay", "5");
        dev.set_property("stage_delay", "20");
        dev.set_property("stage_spin", "1600");
        expect(&ha, "stage_spin", "1200");
        expect(&ha, "stage_delay", "5");

        dev.set_property("set_program", "");
        dev.set_property("start_program", "");
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(
            sent,
            [
                "AA18F02503151B03090102020500000003000000000076BB",
                "AA16F0261B0309010202050000000300000000005FBB",
            ]
        );
    }

    #[test]
    fn start_sends_the_fixed_start_command_whatever_the_payload() {
        let (_, thinq, dev) = make();
        dev.set_property("start", "F024040100");
        assert_eq!(thinq.outbox(), vec![hex_decode("AA09F02405010098BB")]);
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory(MODEL_ID).is_some());
    }
}
