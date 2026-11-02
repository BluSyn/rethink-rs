//! F_V8_Y___W.B_2QEUK washer (AABB, class `0x20` status records).
//!
//! Most models send 39-byte records. The F4WV909P2TE sends 32-byte ones laid out
//! like the first 32 bytes of the long record. A `0xEB` frame carries one record
//! (the reply to the status query), a `0xEC` frame the previous and the current one.
//! Discovery waits for the first record: the 32-byte models also get remote start
//! (course, temperature, spin, options, Delay End), resume and smart-course
//! download. Port of upstream PR #243.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{
    course_name, fy_base_components, install_components, smart_course_name, soil_level,
    SMART_COURSES, SOIL_LABELS, SPIN_LABELS, TEMPERATURE_LABELS,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{DeviceDiscovery, HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const RECORD_LEN: usize = 39;
const SHORT_RECORD_LEN: usize = 32;
const STATUS_PAUSED: u8 = 2;

// F0 26 <course> <intensity> <spin> <temp> <rinse> 00 <delay h> 00 00 <options> <flag> 00 <smart id>,
// using the status record's codes. Requires Remote Start.
const CMD_START: u8 = 0x26;
const START_FLAG: u8 = 0x03;
const RESUME_FLAG: u8 = 0x01;
const START_DELAY: usize = 8; // Delay End, whole hours
const START_SMART_ID: usize = 14;

// F0 25 03 15 <base> <intensity> <spin> <temp> <rinse> 00 00 00 00 <options> 02 00 <smart id> 00 00 00
const CMD_DOWNLOAD: [u8; 4] = [0xf0, 0x25, 0x03, 0x15];
const DOWNLOAD_LEN: usize = 20;
const DOWNLOAD_FLAG: u8 = 0x02;

const PROGRAM_TEMPS: &[u8] = &[1, 2, 3, 4, 6, 7];
const PROGRAM_SPINS: &[u8] = &[1, 2, 5, 7, 9, 10];
const PROGRAM_TURBOWASH: u8 = 0x01;
const PROGRAM_STEAM: u8 = 0x80;
const PROGRAM_PREWASH: u8 = 0x40;
const INTENSITY_NORMAL: u8 = 3;
const INTENSITY_INTENSIVE: u8 = 4;
const DELAY_MIN: u8 = 3;
const DELAY_MAX: u8 = 19;
const PROGRAM_DOWNLOAD_CYCLE: &str = "Download Cycle";
const ACTIVE_SMART_OFFSET: usize = 22; // on the download position, else 0
const LOADED_SMART_OFFSET: usize = 25;

struct ProgramSettings {
    course: u8,
    temp: u8,
    temps: &'static [u8],
    spin: u8,
    spins: &'static [u8],
    rinse: bool,
    options: u8,
}

const fn program(
    course: u8,
    temp: u8,
    temps: &'static [u8],
    spin: u8,
    spins: &'static [u8],
    rinse: bool,
    options: u8,
) -> ProgramSettings {
    ProgramSettings { course, temp, temps, spin, spins, rinse, options }
}

// Per-course defaults and choices from the modelJSON; "Max" spin is 1400.
const ALL_SPINS: &[u8] = &[1, 2, 5, 7, 9, 10];
const PROGRAMS: &[ProgramSettings] = &[
    program(0x01, 4, &[1, 2, 3, 4, 6, 7], 10, ALL_SPINS, true, 0),
    program(0x02, 4, &[1, 2, 3, 4, 6], 10, ALL_SPINS, true, 0),
    program(0x04, 4, &[4, 6], 10, ALL_SPINS, true, 0),
    program(0x05, 1, &[1, 2, 3, 4], 7, &[1, 2, 5, 7], true, 0),
    program(0x07, 4, &[1, 2, 3, 4, 6], 7, ALL_SPINS, true, 0),
    program(0x08, 4, &[1, 2, 3, 4], 5, &[1, 2, 5], true, 0),
    program(0x09, 4, &[1, 2, 3, 4, 6], 5, &[1, 2, 5, 7], true, 0),
    program(0x0c, 2, &[2, 3, 4], 2, ALL_SPINS, true, PROGRAM_TURBOWASH),
    program(0x12, 6, &[], 1, &[], false, 0),
    program(0x1b, 3, &[1, 2, 3, 4], 5, &[1, 2, 5], true, 0),
    program(0x20, 2, &[1, 2, 3, 4], 5, &[1, 2, 5], true, 0),
    program(0x2d, 6, &[], 10, ALL_SPINS, true, PROGRAM_STEAM),
    program(0x31, 4, &[1, 2, 3, 4, 6], 9, &[2, 5, 7, 9, 10], true, PROGRAM_TURBOWASH),
];

#[derive(Clone, Copy)]
struct Settings {
    base: u8,
    intensity: u8,
    spin: u8,
    temp: u8,
    rinse: u8,
    options: u8,
}

const fn settings(base: u8, intensity: u8, spin: u8, temp: u8, rinse: u8, options: u8) -> Settings {
    Settings { base, intensity, spin, temp, rinse, options }
}

// This model's downloads; spin 0xff = Max, option 0x10 is not decoded.
const SMART_SETTINGS: &[(u8, Settings)] = &[
    (0x34, settings(0x01, 3, 0x09, 6, 1, 0x40)),
    (0x35, settings(0x02, 3, 0x07, 4, 1, 0)),
    (0x36, settings(0x1b, 3, 0x02, 2, 1, 0)),
    (0x37, settings(0x01, 3, 0xff, 4, 1, 0)),
    (0x38, settings(0x08, 3, 0x05, 1, 1, 0)),
    (0x39, settings(0x20, 3, 0xff, 2, 1, 0)),
    (0x3a, settings(0x05, 3, 0x02, 1, 1, 0)),
    (0x3b, settings(0x01, 3, 0xff, 4, 1, 0)),
    (0x3e, settings(0x0c, 3, 0x02, 2, 1, 0x01)),
    (0x3f, settings(0x20, 3, 0x05, 2, 1, 0)),
    (0x40, settings(0x09, 3, 0x07, 4, 1, 0)),
    (0x47, settings(0x01, 3, 0x07, 6, 2, 0x50)),
    (0x48, settings(0x0a, 3, 0xff, 6, 1, 0x80)),
    (0x49, settings(0x0c, 3, 0x02, 2, 1, 0x01)),
    (0x4a, settings(0x20, 3, 0x05, 2, 1, 0)),
    (0x4c, settings(0x01, 3, 0xff, 4, 2, 0x10)),
    (0x4d, settings(0x01, 3, 0xff, 1, 1, 0)),
    (0x64, settings(0x0e, 3, 0xff, 0, 1, 0)),
    (0x65, settings(0x1b, 3, 0x02, 2, 1, 0)),
    (0x66, settings(0x01, 3, 0xff, 4, 2, 0)),
    (0x6b, settings(0x01, 3, 0x07, 6, 1, 0x40)),
    (0x6c, settings(0x01, 3, 0x07, 4, 1, 0x40)),
    (0x6f, settings(0x01, 3, 0xff, 4, 1, 0)),
    (0x71, settings(0x30, 3, 0x02, 1, 0, 0)),
    (0x79, settings(0x17, 0, 0x01, 0, 0, 0)),
    (0x7a, settings(0x17, 0, 0xff, 0, 0, 0)),
];

fn smart_settings(id: u8) -> Option<Settings> {
    SMART_SETTINGS.iter().find(|(sid, _)| *sid == id).map(|(_, s)| *s)
}

/// The labels of `codes`, in the table's order.
fn labels_of(table: &[&'static str], codes: &[u8]) -> Vec<&'static str> {
    table
        .iter()
        .enumerate()
        .filter(|(code, _)| codes.contains(&(*code as u8)))
        .map(|(_, label)| *label)
        .collect()
}

fn program_courses() -> Vec<&'static str> {
    PROGRAMS
        .iter()
        .filter_map(|p| course_name(p.course as u32))
        .chain([PROGRAM_DOWNLOAD_CYCLE])
        .collect()
}

fn smart_options() -> Vec<&'static str> {
    SMART_COURSES
        .iter()
        .filter(|(id, _)| smart_settings(*id).is_some())
        .map(|(_, label)| *label)
        .collect()
}

fn start_command(len: usize, s: Settings, flag: u8, delay: u8, smart_id: u8) -> Vec<u8> {
    let mut cmd = vec![0u8; len];
    cmd[0] = 0xf0;
    cmd[1] = CMD_START;
    cmd[2] = s.base;
    cmd[3] = s.intensity;
    cmd[4] = s.spin;
    cmd[5] = s.temp;
    cmd[6] = s.rinse;
    cmd[START_DELAY] = delay;
    cmd[11] = s.options;
    cmd[12] = flag;
    cmd[START_SMART_ID] = smart_id;
    cmd
}

fn download_command(id: u8) -> Option<Vec<u8>> {
    let s = smart_settings(id)?;
    let mut cmd = vec![0u8; DOWNLOAD_LEN];
    cmd[..CMD_DOWNLOAD.len()].copy_from_slice(&CMD_DOWNLOAD);
    cmd[4] = s.base;
    cmd[5] = s.intensity;
    cmd[6] = s.spin;
    cmd[7] = s.temp;
    cmd[8] = s.rinse;
    cmd[13] = s.options;
    cmd[14] = DOWNLOAD_FLAG;
    cmd[16] = id;
    Some(cmd)
}

// The F4WV909P2TE refuses F024050100.
fn resume_command(rec: &[u8]) -> Vec<u8> {
    let s = settings(rec[7], rec[9], rec[10], rec[11], rec[12], rec[16]);
    start_command(19, s, RESUME_FLAG, 0, rec[ACTIVE_SMART_OFFSET])
}

/// JavaScript `Math.round`: the nearest integer, ties towards +∞.
fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

// Only for the 32-byte models (F4WV909P2TE).
fn remote_components() -> Vec<(&'static str, serde_json::Value)> {
    let smart_courses: Vec<&str> = SMART_COURSES.iter().map(|(_, label)| *label).collect();
    vec![
        ("start_program", json!({"platform":"button","unique_id":"$deviceid-start_program","command_topic":"$this/start_program/set","payload_press":"","name":"Start program","icon":"mdi:play-circle"})),
        ("program_course", json!({"platform":"select","unique_id":"$deviceid-program_course","state_topic":"$this/program_course","command_topic":"$this/program_course/set","name":"Program","icon":"mdi:pin-outline","options":program_courses()})),
        ("program_temp", json!({"platform":"select","unique_id":"$deviceid-program_temp","state_topic":"$this/program_temp","command_topic":"$this/program_temp/set","name":"Program temperature","icon":"mdi:thermometer","options":labels_of(TEMPERATURE_LABELS, PROGRAM_TEMPS)})),
        ("program_spin", json!({"platform":"select","unique_id":"$deviceid-program_spin","state_topic":"$this/program_spin","command_topic":"$this/program_spin/set","name":"Program spin","icon":"mdi:autorenew","options":labels_of(SPIN_LABELS, PROGRAM_SPINS)})),
        ("program_rinse_plus", json!({"platform":"switch","unique_id":"$deviceid-program_rinse_plus","state_topic":"$this/program_rinse_plus","command_topic":"$this/program_rinse_plus/set","name":"Program Rinse+","icon":"mdi:water-plus"})),
        ("program_turbowash", json!({"platform":"switch","unique_id":"$deviceid-program_turbowash","state_topic":"$this/program_turbowash","command_topic":"$this/program_turbowash/set","name":"Program TurboWash","icon":"mdi:rocket-launch"})),
        ("program_steam", json!({"platform":"switch","unique_id":"$deviceid-program_steam","state_topic":"$this/program_steam","command_topic":"$this/program_steam/set","name":"Program Steam","icon":"mdi:kettle-steam"})),
        ("program_prewash", json!({"platform":"switch","unique_id":"$deviceid-program_prewash","state_topic":"$this/program_prewash","command_topic":"$this/program_prewash/set","name":"Program Pre-wash","icon":"mdi:water-sync"})),
        ("program_intensive", json!({"platform":"switch","unique_id":"$deviceid-program_intensive","state_topic":"$this/program_intensive","command_topic":"$this/program_intensive/set","name":"Program Intensive","icon":"mdi:washing-machine-alert"})),
        ("program_delay", json!({"platform":"number","unique_id":"$deviceid-program_delay","state_topic":"$this/program_delay","command_topic":"$this/program_delay/set","name":"Program delay end","icon":"mdi:timer-sand","min":0,"max":DELAY_MAX,"step":1,"mode":"box","unit_of_measurement":"h"})),
        ("program_download", json!({"platform":"select","unique_id":"$deviceid-program_download","state_topic":"$this/program_download","command_topic":"$this/program_download/set","name":"Download cycle selection","icon":"mdi:cloud-download-outline","options":smart_options()})),
        ("download", json!({"platform":"button","unique_id":"$deviceid-download","command_topic":"$this/download/set","payload_press":"","name":"Download cycle","icon":"mdi:download"})),
        ("downloaded_course", json!({"platform":"sensor","unique_id":"$deviceid-downloaded_course","state_topic":"$this/downloaded_course","name":"Downloaded cycle","icon":"mdi:cloud-download-outline","device_class":"enum","options":smart_courses})),
    ]
}

/// The remote start selection, as option labels.
#[derive(Clone, Copy)]
struct Program {
    course: &'static str,
    temp: &'static str,
    spin: &'static str,
    rinse_plus: bool,
    turbowash: bool,
    steam: bool,
    prewash: bool,
    intensive: bool,
    delay: u8,
    download: &'static str,
}

impl Default for Program {
    fn default() -> Self {
        Self {
            course: "Cotton",
            temp: "40 °C",
            spin: "1400 rpm",
            rinse_plus: false,
            turbowash: false,
            steam: false,
            prewash: false,
            intensive: false,
            delay: 0,
            download: "Cold Wash",
        }
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    /// Published with the first record, whose length tells which model this is.
    base_config: Mutex<Option<DeviceDiscovery>>,
    short_record: Mutex<Option<Vec<u8>>>,
    program: Mutex<Program>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());

        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        install_components(&mut base, fy_base_components(crate::washer_common::FY_STEAM));
        install_components(
            &mut base,
            [
                ("cycles", json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycle count","icon":"mdi:counter"})),
                ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"})),
                ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
                ("reserve_time", json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","device_class":"duration","unit_of_measurement":"h","name":"Reserve time","icon":"mdi:timer-sand"})),
                ("extra_rinse", json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra rinse","icon":"mdi:water-plus"})),
                ("turbowash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"})),
                ("prewash", json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"})),
                ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:washing-machine","device_class":"enum","options":SOIL_LABELS})),
                ("intensive_wash", json!({"platform":"binary_sensor"})),
                ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
            ],
        );
        let this = Arc::new(Self {
            core: core.clone(),
            base_config: Mutex::new(Some(base)),
            short_record: Mutex::new(None),
            program: Mutex::new(Program::default()),
        });

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, 0x20, RECORD_LEN) {
            self.configure(false);
            self.process_record(rec, true);
        } else if let Some(rec) = process_common_status(buf, 0x20, SHORT_RECORD_LEN) {
            self.configure(true);
            *self.short_record.lock() = Some(rec.to_vec());
            self.process_record(rec, false);
        }
    }

    fn configure(&self, short: bool) {
        let Some(mut config) = self.base_config.lock().take() else {
            return;
        };
        if short {
            install_components(&mut config, remote_components());
        }
        self.core.set_config(config);
        if short {
            self.publish_program();
        }
    }

    fn process_record(&self, rec: &[u8], long: bool) {
        let status = rec[2];
        let time_remain = rec[3] as i64 * 60 + rec[4] as i64;
        let time_initial = rec[5] as i64 * 60 + rec[6] as i64;
        let course = rec[7];
        let error = rec[8];
        let soil = rec[9];
        let spin = rec[10];
        let temp = rec[11];
        let extra_rinse = rec[12];
        let time_reserve_hour = rec[14] as i64;
        let options = rec[16];
        let lock_status = rec[17];
        let cycles = rec[23] as i64;

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_steam_name);
        pub_temp_spin(&self.core, temp, spin);
        self.core.publish_property("cycles", cycles.into());
        self.core.publish_on_off("remote_start", lock_status & 2 != 0);
        let active = rec[ACTIVE_SMART_OFFSET];
        let course_label = if long || active == 0 {
            course_name(course as u32)
        } else {
            smart_course_name(active)
        };
        self.core
            .publish_property("course", course_label.unwrap_or("None").into());
        if !long {
            let loaded = smart_course_name(rec[LOADED_SMART_OFFSET]);
            self.core
                .publish_property("downloaded_course", loaded.unwrap_or("None").into());
        }
        // Lock device class: OFF = locked
        self.core.publish_on_off("door_lock", lock_status & 0x40 == 0);
        self.core.publish_on_off("child_lock", lock_status & 0x80 == 0);
        let energy = rec[30] as i64 * 256 + rec[31] as i64;
        self.core.publish_property("energy", energy.into());
        self.core
            .publish_property("initial_time", time_initial.into());
        self.core
            .publish_property("remaining_time", time_remain.into());
        self.core
            .publish_property("reserve_time", time_reserve_hour.into());
        self.core.publish_on_off("extra_rinse", extra_rinse >= 2);
        self.core.publish_on_off("turbowash", options & 0x01 != 0);
        self.core.publish_on_off("prewash", options & 0x40 != 0);
        self.core.publish_on_off("steam", options & 0x80 != 0);
        self.core.publish_property(
            "soil",
            soil_level(soil as usize).unwrap_or("None").into(),
        );
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let short = self.short_record.lock().clone();
        match (prop, short.as_deref()) {
            // The pause command toggles, so it would resume a paused cycle.
            ("pause", Some(rec)) if rec[2] == STATUS_PAUSED => {}
            ("start", Some(rec)) => self.core.send(&resume_command(rec)),
            ("start_program", rec) => {
                if let Some(cmd) = self.program_start_command(rec) {
                    self.core.send(&cmd);
                }
            }
            ("download", _) => {
                let download = self.program.lock().download;
                let id = SMART_COURSES.iter().find(|(_, label)| *label == download);
                if let Some(cmd) = id.and_then(|(id, _)| download_command(*id)) {
                    self.core.send(&cmd);
                }
            }
            _ => set_power_start_pause(&self.core, prop, mqtt_value),
        }
        self.set_program(prop, mqtt_value);
        if prop.starts_with("program_") {
            self.publish_program();
        }
    }

    fn set_program(&self, prop: &str, value: &str) {
        let pick = |options: Vec<&'static str>, current: &mut &'static str| {
            if let Some(option) = options.into_iter().find(|o| *o == value) {
                *current = option;
            }
        };
        let mut p = self.program.lock();
        match prop {
            "program_course" => pick(program_courses(), &mut p.course),
            "program_temp" => pick(labels_of(TEMPERATURE_LABELS, PROGRAM_TEMPS), &mut p.temp),
            "program_spin" => pick(labels_of(SPIN_LABELS, PROGRAM_SPINS), &mut p.spin),
            "program_download" => pick(smart_options(), &mut p.download),
            "program_rinse_plus" => p.rinse_plus = value == "ON",
            "program_turbowash" => p.turbowash = value == "ON",
            "program_steam" => p.steam = value == "ON",
            "program_prewash" => p.prewash = value == "ON",
            "program_intensive" => p.intensive = value == "ON",
            "program_delay" => {
                if let Some(hours) = crate::js::number(value).map(js_round) {
                    let offered = f64::from(DELAY_MIN)..=f64::from(DELAY_MAX);
                    if hours == 0.0 || offered.contains(&hours) {
                        p.delay = hours as u8;
                    }
                }
            }
            _ => {}
        }
    }

    fn publish_program(&self) {
        let p = *self.program.lock();
        self.core.publish_property("program_course", p.course.into());
        self.core.publish_property("program_temp", p.temp.into());
        self.core.publish_property("program_spin", p.spin.into());
        self.core.publish_on_off("program_rinse_plus", p.rinse_plus);
        self.core.publish_on_off("program_turbowash", p.turbowash);
        self.core.publish_on_off("program_steam", p.steam);
        self.core.publish_on_off("program_prewash", p.prewash);
        self.core.publish_on_off("program_intensive", p.intensive);
        self.core
            .publish_property("program_delay", (p.delay as i64).into());
        self.core
            .publish_property("program_download", p.download.into());
    }

    // An unsupported temperature or spin falls back to the course default.
    fn program_start_command(&self, short: Option<&[u8]>) -> Option<Vec<u8>> {
        let p = *self.program.lock();
        if p.course == PROGRAM_DOWNLOAD_CYCLE {
            let id = short?[LOADED_SMART_OFFSET];
            return Some(start_command(18, smart_settings(id)?, START_FLAG, p.delay, id));
        }

        let course = PROGRAMS
            .iter()
            .find(|s| course_name(s.course as u32) == Some(p.course))?;
        let temp = TEMPERATURE_LABELS.iter().position(|t| *t == p.temp)? as u8;
        let spin = SPIN_LABELS.iter().position(|s| *s == p.spin)? as u8;
        let mut options = course.options;
        if p.turbowash {
            options |= PROGRAM_TURBOWASH;
        }
        if p.steam {
            options |= PROGRAM_STEAM;
        }
        if p.prewash {
            options |= PROGRAM_PREWASH;
        }
        let s = Settings {
            base: course.course,
            intensity: if p.intensive { INTENSITY_INTENSIVE } else { INTENSITY_NORMAL },
            spin: if course.spins.contains(&spin) { spin } else { course.spin },
            temp: if course.temps.contains(&temp) { temp } else { course.temp },
            rinse: if course.rinse && p.rinse_plus { 2 } else { 1 },
            options,
        };
        Some(start_command(18, s, START_FLAG, p.delay, 0))
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        crate::devices::washer_ctrl::request_status(&self.core);
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
    use crate::device_trait::DeviceHandler;
    use crate::devices::washer_ctrl::STATUS_QUERY;
    use crate::test_support::{eb_from_ec, make_t2, outbox_has_inner, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("F_V8_Y___W.B_2QEUK", Device::new)
    }

    // Reuse 80-byte frame layout from F_V__F for basic decode of shared offsets
    const SAMPLE: &str = "AA5420EC00250101000100180000000000020000000000000600001000640000000000000000000000000000250104380438130003090401020000000000000400001000640000040000000000000000000053BB";

    // Captured status telegrams from upstream issue #75 (F6WV710P2S.ABLQPDG).
    const SAMPLE_COTTON_STD: &str = "aa5420ec002501000000000000000000000000000000000000000001006400000000000000000000000000002501033a033a0100030b04010000000000010002000001006400000400000000000000000000e0bb";
    const SAMPLE_ALLERGY_STEAM: &str = "aa5420ec002501021c021c0900030504010000000000000002000001006400000500000000000000000000002501023002302d00030b06010000008000000003000001006400000500000000000000000000f2bb";
    const SAMPLE_COTTON_STEAM: &str = "aa5420ec002501031c031c0100030b03010000000000010002000001006400000400000000000000000000002501041104110100030b0601000000800001000300000100640000040000000000000000000015bb";
    const SAMPLE_TURBOWASH: &str = "aa5420ec002501041104110100030b06010000008000010003000001006400000400000000000000000000002501031d031d010003070601000000010001000300000100640000040000000000000000000016bb";
    const SAMPLE_RINSE_PLUS: &str = "aa5420ec002501033703370100030706010000000000010004000001006400000400000000000000000000002501040c040c0100030706020000000000010004000001006400000500000000000000000000b9bb";
    const SAMPLE_PREWASH: &str = "aa5420ec002501033703370100030706010000000000010004000001006400000400000000000000000000002501040c040c01000307060100000040000100040000010064000005000000000000000000007ebb";
    const SAMPLE_INTENSIVE: &str = "aa5420ec002501033703370100030706010000000000010004000001006400000400000000000000000000002501041604160100040706010000000000010004000001006400000400000000000000000000aabb";

    // F4WV909P2TE (firmware 2.10.123): 32-byte records, live captures
    const EB_PAUSED: &str = "AA2620EB001F02000E000E0C0003020201000000014200000006003000640000010000005FBB";
    const EC_COTTON: &str = "AA4620EC001F01002D002D20000305020100000000000000010000300064000003000000001F01033B033B0100030A040100000000000100030000300064000004000000D1BB";
    const EC_COTTON_95: &str = "AA4620EC001F01033B033B0100030A060100000000000100040000300064000004000000001F01021D021D0100030A0701000000000001000400003000640000040000000EBB";
    const EC_NO_SPIN: &str = "AA4620EC001F01033B033B0100030A040100000000000100030000300064000004000000001F01032E032E010003010401000000000001000200003000640000040000003BBB";
    const EC_DELAY_3H: &str = "AA4620EC001F01033B033B0100030A040100000000000100030000300064000004000000001F01033B033B0100030A040100030000000100030000300064000004000000C0BB";
    const EC_STEAM: &str = "AA4620EC001F01033B033B0100030A040100000000000100030000300064000004000000001F01041204120100030A06010000008000010004000030006400000400000090BB";
    const EC_TURBOWASH: &str = "AA4620EC001F01033B033B0100030A060100000000000100040000300064000004000000001F01031E031E010003070601000000010001000300003000640000040000000EBB";
    const EC_RINSE_PLUS: &str = "AA4620EC001F010338033801000307060100000000000100040000300064000004000000001F01040D040D0100030706020000000000010004000030006400000500000061BB";
    const EC_PREWASH: &str = "AA4620EC001F010338033801000307060100000000000100040000300064000004000000001F01040D040D0100030706010000004000010004000030006400000500000026BB";
    const EC_INTENSIVE: &str = "AA4620EC001F010338033801000307060100000000000100040000300064000004000000001F01041704170100040706010000000000010004000030006400000400000012BB";
    const EC_REMOTE_START: &str = "AA4620EC001F010338033801000307060100000000000100040000300064000004000000001F010338033801000307060100000000020100040000300064000004000000DDBB";
    const EC_RUNNING: &str = "AA4620EC001F010338033801000307060100000000420100040000300064000004000000001F06000E000E0C00030202010000000142200001010030006400000400000080BB";
    // A full Cotton 60 / 1000 / Steam + TurboWash run
    const EC_RINSING: &str = "AA4620EC001F060015011A0100030706010000000142010004040030006400000400038A001F070014011A0100030700010000000142010004060030006400000400038AFDBB";
    const EC_RUN_END: &str = "AA4620EC001F080001011A010000070000000000004201000407003000640000040003AF001F0A0000011A010000000000000000004201000408003000640000040003AFE8BB";
    const EC_SPINNING: &str = "AA4620EC001F080001000101000001000000000000022000010100300064000000000000001F080001000101000001000000000000420000010100300064000000000000B7BB";
    const EC_END: &str = "AA4620EC001F080001000101000001000000000000420000010100300064000000000000001F0A000000010100000000000000000040000001080030006400000000000052BB";
    const EC_END_UNLOCKED: &str = "AA4620EC001F0A0000000101000000000000000000400000010800300064000000000000001F0A000000010100000000000000000000000001080030006400000000000099BB";
    // Started with the door left open
    const EC_DOOR_ERROR: &str = "AA4620EC001F01033B033B0100030A040100000000000100030000300064000004000000001F12033B033B0102030A040100000000020100030400300064000004000000FEBB";
    const EC_CHILD_LOCK_ON: &str = "AA4620EC001F01002D002D20000305020100000000000000010000310064000003000000001F01002D002D20000305020100000000800000010000310064000003000000ADBB";
    const EC_CHILD_LOCK_OFF: &str = "AA4620EC001F01002D002D20000305020100000000800000010000310064000003000000001F01002D002D20000305020100000000000000010000310064000003000000ADBB";
    // Quick 14 + Pre-wash + Intensive, accepted live
    const EC_STARTED_WITH_OPTIONS: &str = "AA4620EC001F010000000000000000000000000000020000000000310065000000000000001F06001100110C0004020201000000410220000101003100650000000000005EBB";
    // ThinQ app remote starts
    const APP_START_QUICK14: &str = "AA16F0260C030202010000000001030000000000BBBB";
    const APP_START_COTTON_30_400_RINSE_PLUS: &str = "AA16F02601030203020000000000030000000000B1BB";
    // Paused after a remote start, and what resumed it live
    const EB_PAUSED_REMOTE: &str = "AA2620EB001F02001700170C00030A03010000000142000001060030006400000100000073BB";
    const RESUME: &str = "AA17F0260C030A0301000000000101000000000000A3BB";
    const APP_START_DELAYED: &str = "AA16F0260103FF0401000A000000030000000000BEBB";
    const LIVE_START_WITH_OPTIONS: &str = "AA16F0260C0402020100000000410300000000007ABB";
    // Lightly Soiled Items started from the app
    const EC_SMART_STARTED: &str = "AA4620EC001F01002200221B000302020100000000420000010065310065000003000000001F06002200221B000302020100000000422000010165310065000003000000E7BB";
    const APP_START_SMART: &str = "AA16F0261B03020201000000000003006500000034BB";
    // Colour Protection (smart course 0x3f) started remotely, then paused
    const EC_PAUSED_SMART: &str = "AA4620EC001F06002D002D2000030502010000000042200001013F31003F000000000000001F02002D002D2000030502010000000042000001063F31003F0000030000000FBB";
    // ThinQ app downloads
    const APP_DOWNLOADS: &[(&str, &str)] = &[
        ("Kids Wear", "AA18F0250315010309060100000000400200340000002CBB"),
        ("School Uniform", "AA18F02503150203070401000000000002003500000062BB"),
        ("Swimming Wear", "AA18F02503151B0302020100000000000200360000001FBB"),
        ("Rainy Season", "AA18F02503150103FF0401000000000002003700000065BB"),
        ("Gym Clothes", "AA18F0250315080305010100000000000200380000006EBB"),
        ("Jeans", "AA18F02503152003FF020100000000000200390000001ABB"),
        ("Blanket", "AA18F02503150503020101000000000002003A00000062BB"),
        ("Sweat Stain", "AA18F02503150103FF0401000000000002003B00000061BB"),
        ("Single Garment", "AA18F02503150C03020201000000000102003E00000011BB"),
        ("Colour Protection", "AA18F02503152003050201000000000002003F0000000EBB"),
        ("Noise Minimize", "AA18F0250315090307040100000000000200400000001CBB"),
        ("Baby Care", "AA18F025031501030706020000000050020047000000CEBB"),
        ("Hygiene", "AA18F02503150A03FF0601000000008002004800000099BB"),
        ("Small Load", "AA18F02503150C0302020100000000010200490000001ABB"),
        ("Lingerie", "AA18F02503152003050201000000000002004A00000033BB"),
        ("Skin Care", "AA18F02503150103FF0402000000001002004C00000003BB"),
        ("Cold Wash", "AA18F02503150103FF0101000000000002004D00000016BB"),
        ("Rinse + Spin", "AA18F02503150E03FF0001000000000002006400000033BB"),
        ("Lightly Soiled Items", "AA18F02503151B0302020100000000000200650000002CBB"),
        ("Minimize Detergent Residue", "AA18F02503150103FF0402000000000002006600000035BB"),
        ("Sleeve Hems and Collars", "AA18F02503150103070601000000004002006B000000FBBB"),
        ("Juice and Food Stains", "AA18F02503150103070401000000004002006C000000F8BB"),
        ("Minimize Wrinkles", "AA18F02503150103FF0401000000000002006F0000003DBB"),
        ("Quick Tub Clean", "AA18F025031530030201000000000000020071000000CDBB"),
        ("Drain", "AA18F025031517000100000000000000020079000000D7BB"),
        ("Spin", "AA18F02503151700FF0000000000000002007A000000D4BB"),
    ];

    fn decode(frame: &str) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    fn values(ha: &MockHaConnection, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|n| prop(ha, n).unwrap_or_else(|| format!("<no {n}>")))
            .collect()
    }

    fn options(ha: &MockHaConnection) -> Vec<String> {
        values(ha, &["extra_rinse", "turbowash", "prewash", "steam", "soil"])
    }

    fn outbox(thinq: &MockThinq2Device) -> Vec<String> {
        thinq.outbox().iter().map(|p| hex_encode(p)).collect()
    }

    /// The first sent command without its AA..BB framing.
    fn first_command(thinq: &MockThinq2Device) -> Vec<u8> {
        let packet = &thinq.outbox()[0];
        packet[2..packet.len() - 2].to_vec()
    }

    fn remote(ha: &MockHaConnection) -> Vec<String> {
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let mut keys: Vec<String> = devinfo
            .config
            .unwrap()
            .components
            .into_keys()
            .filter(|c| c.starts_with("program_") || c == "start_program")
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn basic() {
        let (ha, thinq, dev) = make();
        assert!(ha.device(DEVICE_ID).is_none());
        thinq.emit_data(&hex_decode(SAMPLE));
        assert!(ha.device(DEVICE_ID).unwrap().config.is_some());
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        thinq.reset_recorder();
        dev.start();
        assert!(outbox_has_inner(&thinq, STATUS_QUERY));
    }

    #[test]
    fn cotton_40_1600_decodes_with_no_options_set() {
        let ha = decode(SAMPLE_COTTON_STD);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1600 rpm"));
        assert_eq!(options(&ha), ["OFF", "OFF", "OFF", "OFF", "Normal"]);
    }

    #[test]
    fn allergy_care_60_steam_1600_decodes_steam_on() {
        let ha = decode(SAMPLE_ALLERGY_STEAM);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergy Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1600 rpm"));
        assert_eq!(options(&ha), ["OFF", "OFF", "OFF", "ON", "Normal"]);
    }

    #[test]
    fn cotton_60_steam_decodes_steam_on() {
        let ha = decode(SAMPLE_COTTON_STEAM);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
    }

    #[test]
    fn turbowash_decodes_turbowash_on_and_1000_rpm() {
        let ha = decode(SAMPLE_TURBOWASH);
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1000 rpm"));
        assert_eq!(options(&ha), ["OFF", "ON", "OFF", "OFF", "Normal"]);
    }

    #[test]
    fn rinse_plus_decodes_extra_rinse_on() {
        let ha = decode(SAMPLE_RINSE_PLUS);
        assert_eq!(options(&ha), ["ON", "OFF", "OFF", "OFF", "Normal"]);
    }

    #[test]
    fn prewash_decodes_prewash_on() {
        let ha = decode(SAMPLE_PREWASH);
        assert_eq!(options(&ha), ["OFF", "OFF", "ON", "OFF", "Normal"]);
    }

    #[test]
    fn intensive_wash_decodes_soil_intensive() {
        let ha = decode(SAMPLE_INTENSIVE);
        assert_eq!(options(&ha), ["OFF", "OFF", "OFF", "OFF", "Intensive"]);
    }

    #[test]
    fn eb_status_reply_decodes_like_the_current_ec_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&eb_from_ec(&hex_decode(SAMPLE_ALLERGY_STEAM), RECORD_LEN));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergy Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
    }

    #[test]
    fn other_frame_types_and_unmapped_codes() {
        let (ha, thinq, _) = make();
        let mut frame = hex_decode(SAMPLE_COTTON_STD);
        frame[3] = 0x0a;
        thinq.emit_data(&frame);
        assert!(ha.device(DEVICE_ID).is_none());

        let mut frame = hex_decode(SAMPLE_COTTON_STD);
        frame[2 + 2 + RECORD_LEN + 7] = 0xff; // course
        frame[2 + 2 + RECORD_LEN + 8] = 0x0a; // error code without a label
        thinq.emit_data(&frame);
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
    }

    #[test]
    fn wv9_alias_uses_this_handler() {
        let factory = crate::registry::t2_factory("F_V__Y___W.B__QEUK").unwrap();
        let ha = MockHaConnection::new();
        let meta = Metadata::new("F_V__Y___W.B__QEUK", "WV9-1609B", "1");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let _dev = factory(ha.clone(), thinq.clone(), meta);
        thinq.emit_data(&hex_decode(SAMPLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
    }

    #[test]
    fn short_panel_settings_course_temperature_spin_time_and_options() {
        let (ha, thinq, _) = make();
        let emit = |f: &str| thinq.emit_data(&hex_decode(f));
        let ha = &*ha;

        emit(EC_COTTON);
        assert_eq!(
            values(ha, &["status", "course", "temp", "spin", "remaining_time", "cycles"]),
            ["Standby", "Cotton", "40 °C", "1400 rpm", "239", "48"]
        );
        emit(EC_COTTON_95);
        assert_eq!(values(ha, &["temp", "remaining_time"]), ["95 °C", "149"]);
        emit(EC_NO_SPIN);
        assert_eq!(prop(ha, "spin").as_deref(), Some("No spin"));
        emit(EC_DELAY_3H);
        assert_eq!(prop(ha, "reserve_time").as_deref(), Some("3"));

        let opts = || values(ha, &["steam", "turbowash", "extra_rinse", "prewash", "soil"]);
        emit(EC_STEAM);
        assert_eq!(opts(), ["ON", "OFF", "OFF", "OFF", "Normal"]);
        emit(EC_TURBOWASH);
        assert_eq!(opts(), ["OFF", "ON", "OFF", "OFF", "Normal"]);
        emit(EC_RINSE_PLUS);
        assert_eq!(opts(), ["OFF", "OFF", "ON", "OFF", "Normal"]);
        emit(EC_PREWASH);
        assert_eq!(opts(), ["OFF", "OFF", "OFF", "ON", "Normal"]);
        emit(EC_INTENSIVE);
        assert_eq!(opts(), ["OFF", "OFF", "OFF", "OFF", "Intensive"]);

        emit(EC_REMOTE_START);
        assert_eq!(prop(ha, "remote_start").as_deref(), Some("ON"));
    }

    #[test]
    fn short_quick_14_started_from_the_app_runs_and_pauses() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_RUNNING));
        assert_eq!(
            values(&ha, &["status", "course", "temp", "spin", "remaining_time"]),
            ["Washing", "Quick 14", "20 °C", "400 rpm", "14"]
        );
        thinq.emit_data(&hex_decode(EB_PAUSED));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
    }

    #[test]
    fn short_full_run_rinses_ends_and_reports_the_app_energy() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_RINSING));
        assert_eq!(
            values(&ha, &["status", "remaining_time", "energy"]),
            ["Rinsing", "20", "906"]
        );
        thinq.emit_data(&hex_decode(EC_RUN_END));
        assert_eq!(
            values(&ha, &["status", "remaining_time", "energy"]),
            ["End", "0", "943"]
        );
    }

    #[test]
    fn short_spin_only_run_ends_and_unlocks_the_door() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_SPINNING));
        assert_eq!(values(&ha, &["status", "door_lock"]), ["Spinning", "OFF"]);
        thinq.emit_data(&hex_decode(EC_END));
        assert_eq!(
            values(&ha, &["status", "remaining_time", "door_lock"]),
            ["End", "0", "OFF"]
        );
        thinq.emit_data(&hex_decode(EC_END_UNLOCKED));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn short_starting_with_the_door_open_reports_de1() {
        let ha = decode(EC_DOOR_ERROR);
        assert_eq!(
            values(&ha, &["status", "error", "error_message", "door_lock"]),
            ["Error", "ON", "Door open error (DE1)", "ON"]
        );
    }

    #[test]
    fn short_child_lock_follows_bit_0x80() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_CHILD_LOCK_ON));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(EC_CHILD_LOCK_OFF));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn short_reports_the_added_options() {
        let ha = decode(EC_STARTED_WITH_OPTIONS);
        assert_eq!(
            values(&ha, &["status", "turbowash", "prewash", "soil"]),
            ["Washing", "ON", "ON", "Intensive"]
        );
    }

    #[test]
    fn start_program_sends_the_same_frames_as_the_app() {
        let (_ha, thinq, dev) = make();
        dev.set_property("program_course", "Quick 14");
        dev.set_property("program_temp", "20 °C");
        dev.set_property("program_spin", "400 rpm");
        dev.set_property("start_program", "");

        dev.set_property("program_course", "Cotton");
        dev.set_property("program_temp", "30 °C");
        dev.set_property("program_rinse_plus", "ON");
        dev.set_property("start_program", "");

        assert_eq!(
            outbox(&thinq),
            [APP_START_QUICK14, APP_START_COTTON_30_400_RINSE_PLUS]
        );
    }

    #[test]
    fn unoffered_temperature_or_spin_falls_back_to_the_course_default() {
        let (_ha, thinq, dev) = make();
        dev.set_property("program_course", "Wool");
        dev.set_property("program_temp", "95 °C");
        dev.set_property("program_spin", "1400 rpm");
        dev.set_property("start_program", "");
        let cmd = first_command(&thinq);
        assert_eq!([cmd[2], cmd[4], cmd[5]], [0x1b, 5, 3]); // Wool, 800, 30
    }

    #[test]
    fn start_resumes_the_paused_cycle_with_its_own_selection() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EB_PAUSED_REMOTE));
        dev.set_property("start", "");
        assert_eq!(outbox(&thinq), [RESUME]);
    }

    #[test]
    fn without_a_short_record_start_keeps_the_original_command() {
        let (_ha, thinq, dev) = make();
        dev.set_property("start", "");
        assert_eq!(outbox(&thinq), ["AA09F02405010098BB"]);
    }

    #[test]
    fn option_switches_add_the_panel_bits_and_intensive_raises_the_intensity() {
        let (_ha, thinq, dev) = make();
        dev.set_property("program_course", "Cotton");
        dev.set_property("program_temp", "60 °C");
        dev.set_property("program_spin", "1000 rpm");
        dev.set_property("program_turbowash", "ON");
        dev.set_property("program_steam", "ON");
        dev.set_property("program_intensive", "ON");
        dev.set_property("start_program", "");
        let cmd = first_command(&thinq);
        // Cotton, intensive, 1000, 60, normal rinse, Steam + TurboWash
        assert_eq!(
            [cmd[2], cmd[3], cmd[4], cmd[5], cmd[6], cmd[11], cmd[12]],
            [0x01, 4, 7, 6, 1, 0x81, 3]
        );
    }

    #[test]
    fn start_program_carries_the_delay_where_the_app_put_it() {
        let (_ha, thinq, dev) = make();
        dev.set_property("program_course", "Cotton");
        dev.set_property("program_temp", "40 °C");
        dev.set_property("program_spin", "1400 rpm");
        dev.set_property("program_delay", "10");
        dev.set_property("start_program", "");
        let mut ours = first_command(&thinq);
        let app = hex_decode(APP_START_DELAYED);
        let app = &app[2..app.len() - 2];
        // the app sends Max (FF) for 1400 (0A)
        assert_eq!(ours[4], 0x0a);
        assert_eq!(app[4], 0xff);
        ours[4] = app[4];
        assert_eq!(hex_encode(&ours), hex_encode(app));
    }

    #[test]
    fn a_delay_the_washer_does_not_offer_is_ignored() {
        let (ha, _thinq, dev) = make();
        dev.set_property("program_delay", "2");
        dev.set_property("program_delay", "25");
        assert_eq!(prop(&ha, "program_delay").as_deref(), Some("0"));
    }

    #[test]
    fn program_delay_parses_like_number_and_math_round() {
        let (ha, _thinq, dev) = make();
        for (value, expected) in [
            ("2.5", "3"),
            ("18.5", "19"),
            ("19.5", "19"),
            (" 1e1 ", "10"),
            ("abc", "10"),
            ("-0.5", "0"),
            ("0x0c", "12"),
            ("", "0"),
        ] {
            dev.set_property("program_delay", value);
            assert_eq!(prop(&ha, "program_delay").as_deref(), Some(expected), "{value:?}");
        }
    }

    #[test]
    fn start_program_adds_the_switched_on_options_as_accepted_live() {
        let (_ha, thinq, dev) = make();
        dev.set_property("program_course", "Quick 14");
        dev.set_property("program_temp", "20 °C");
        dev.set_property("program_spin", "400 rpm");
        dev.set_property("program_prewash", "ON");
        dev.set_property("program_intensive", "ON");
        dev.set_property("start_program", "");
        assert_eq!(outbox(&thinq), [LIVE_START_WITH_OPTIONS]);
    }

    #[test]
    fn the_config_waits_for_the_first_record() {
        let (ha, thinq, _) = make();
        assert!(ha.device(DEVICE_ID).and_then(|d| d.config).is_none());
        thinq.emit_data(&hex_decode(SAMPLE_COTTON_STD)); // a 39-byte model
        let devinfo = ha.device(DEVICE_ID).unwrap();
        assert!(devinfo.config.as_ref().unwrap().components.contains_key("status"));
        assert!(remote(&ha).is_empty());
        assert_eq!(prop(&ha, "program_course"), None);
    }

    #[test]
    fn the_short_models_get_the_remote_controls_with_the_first_config() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EB_PAUSED_REMOTE));
        let controls = remote(&ha);
        assert!(controls.contains(&"start_program".to_string()));
        assert!(controls.contains(&"program_delay".to_string()));
        assert!(controls.contains(&"program_download".to_string()));
        let program = ["program_course", "program_temp", "program_spin", "program_delay"];
        assert_eq!(values(&ha, &program), ["Cotton", "40 °C", "1400 rpm", "0"]);
        assert_eq!(prop(&ha, "program_download").as_deref(), Some("Cold Wash"));
        // the config stays as the first record set it
        thinq.emit_data(&hex_decode(SAMPLE_COTTON_STD));
        assert_eq!(remote(&ha), controls);
    }

    #[test]
    fn download_sends_the_app_frame_for_every_smart_course() {
        let (_ha, thinq, dev) = make();
        for (name, _) in APP_DOWNLOADS {
            dev.set_property("program_download", name);
            dev.set_property("download", "");
        }
        let expected: Vec<&str> = APP_DOWNLOADS.iter().map(|(_, frame)| *frame).collect();
        assert_eq!(outbox(&thinq), expected);
        assert_eq!(smart_options().len(), APP_DOWNLOADS.len());
    }

    #[test]
    fn the_active_smart_course_is_the_course_and_download_cycle_starts_it() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EC_SMART_STARTED));
        assert_eq!(
            values(&ha, &["status", "course", "downloaded_course"]),
            ["Washing", "Lightly Soiled Items", "Lightly Soiled Items"]
        );

        thinq.reset_recorder();
        dev.set_property("program_course", "Download Cycle");
        dev.set_property("start_program", "");
        assert_eq!(outbox(&thinq), [APP_START_SMART]);
    }

    #[test]
    fn download_cycle_sends_nothing_before_the_washer_reports_what_it_loaded() {
        let (_ha, thinq, dev) = make();
        dev.set_property("program_course", "Download Cycle");
        dev.set_property("start_program", "");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn pause_does_not_toggle_a_paused_cycle_back_on() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EC_PAUSED_SMART));
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn start_resumes_the_smart_course_not_the_base_course() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(EC_PAUSED_SMART));
        thinq.reset_recorder();
        dev.set_property("start", "");
        let cmd = first_command(&thinq);
        assert_eq!([cmd[2], cmd[12], cmd[14]], [0x20, 0x01, 0x3f]);
    }

    #[test]
    fn remote_controls_publish_the_upstream_option_lists() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EC_COTTON));
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert_eq!(
            comps["program_course"]["options"],
            json!([
                "Cotton",
                "Ease Care",
                "Eco 40-60",
                "Duvet",
                "Mix",
                "Sports Wear",
                "Night Wash",
                "Quick 14",
                "Drum Clean",
                "Wool",
                "Delicate",
                "Allergy Care",
                "TurboWash 39",
                "Download Cycle"
            ])
        );
        assert_eq!(
            comps["program_temp"]["options"],
            json!(["Cold", "20 °C", "30 °C", "40 °C", "60 °C", "95 °C"])
        );
        assert_eq!(
            comps["program_spin"]["options"],
            json!(["No spin", "400 rpm", "800 rpm", "1000 rpm", "1200 rpm", "1400 rpm"])
        );
        let downloaded = comps["downloaded_course"]["options"].as_array().unwrap();
        assert_eq!(downloaded.len(), SMART_COURSES.len());
        assert_eq!(comps["program_delay"]["max"], 19);
    }
}
