//! F24VDD — LG front-load washer (ThinQ2 deviceType 201). Class `0x20` AABB.
//!
//! `20 EB 00` carries one record and `20 EC 00` the previous record, a `00`
//! byte, then the current one. A record is the `0x24` marker and 36 bytes. The
//! `20 D8` heartbeat reports power and `20 72` Remote Start (bit 0 of byte 3).
//! This cycle's energy in Wh only comes in the detailed telemetry: byte 36 of
//! the 405-byte `20 CD` frame, byte 37 of the 406- or 476-byte `20 BD` frame.
//! Frames with a bad checksum are dropped. `auto_ack` stays off.
//!
//! Start and Resume send the captured `F0 26` template of the selected course:
//! byte 4 is the spin, 5 the temperature, 6 the rinse count, 7 the reserve
//! hours and 11 the start flag (`0x20` start, `0x00` resume). Every other byte
//! is a per-course constant. Spin, temperature and rinse are only written
//! where a capture showed them varying for that course; otherwise the
//! template's value goes out. `F0 25` installs one of the 14 downloadable
//! courses, whose id record byte 21 reports. Each has its own `F0 26` template
//! (course 14) in which only the reserve hours and the flag vary. Power off
//! and Pause are `F0 24 01 01 00` and `F0 24 04 01 00`. Power on was never
//! captured and is refused.
//!
//! Port of upstream PR #180.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, Thinq2Device};
use rethink_util::packet_codec::aabb_checksum;
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
/// A `00` byte, the `0x24` marker, then 36 record bytes.
const STATUS_LENGTH: usize = 38;
const RECORD_MARKER: u8 = 0x24;
const POWER_HEARTBEAT: u8 = 0xd8;
const REMOTE_START_HEARTBEAT: u8 = 0x72;
const REMOTE_START_ON: u8 = 0x01;
const TELEMETRY_CD: u8 = 0xcd;
const TELEMETRY_BD: u8 = 0xbd;

const POWER_OFF_COMMAND: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
const PAUSE_COMMAND: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];

// Record offsets, counted from the marker.
const REC_STATE: usize = 1;
const REC_REMAIN_HOUR: usize = 2;
const REC_REMAIN_MINUTE: usize = 3;
const REC_INITIAL_HOUR: usize = 4;
const REC_INITIAL_MINUTE: usize = 5;
const REC_COURSE: usize = 6;
const REC_ERROR: usize = 7;
const REC_SPIN: usize = 9;
const REC_TEMPERATURE: usize = 10;
const REC_RINSE: usize = 11;
const REC_RESERVE_HOUR: usize = 13;
const REC_RESERVE_MINUTE: usize = 14;
const REC_FLAGS: usize = 15;
const REC_TURBO_SHOT: usize = 16;
const REC_DOWNLOADED_COURSE: usize = 21;

const CHILD_LOCK_FLAG: u8 = 0x08;
const STEAM_FLAG: u8 = 0x10;
const TURBO_SHOT_FLAG: u8 = 0x80;
const STATE_SMART_DIAGNOSIS: u8 = 101;

// `F0 26` template offsets.
const TEMPLATE_SPIN: usize = 4;
const TEMPLATE_TEMP: usize = 5;
const TEMPLATE_RINSE: usize = 6;
const TEMPLATE_RESERVE: usize = 7;
const TEMPLATE_FLAG: usize = 11;
const START_FLAG: u8 = 0x20;
const RESUME_FLAG: u8 = 0x00;

const ALL_RINSE: &[u8] = &[0, 1, 2, 3, 4, 5];
const ALL_SPIN: &[u8] = &[0, 1, 2, 3, 4, 5];
const ALL_TEMP: &[u8] = &[1, 2, 3, 4];

/// The modelJSON's own `state` indices.
const STATES: &[(u8, &str)] = &[
    (0, "Power off"),
    (5, "Standby"),
    (6, "Pause"),
    (7, "Error"),
    (10, "Reserved"),
    (20, "Detecting"),
    (21, "Add drain"),
    (22, "Detergent amount"),
    (23, "Running"),
    (24, "Prewash"),
    (30, "Rinsing"),
    (31, "Rinse hold"),
    (40, "Spinning"),
    (50, "Drying"),
    (60, "Complete"),
    (61, "Fresh care"),
    (83, "Freeze prevention standby"),
    (84, "Freeze prevention running"),
    (85, "Freeze prevention pause"),
    (101, "Smart diagnosis"),
];

const DOWNLOADED_COURSE: (u8, &str) = (14, "Downloaded Course");
const COURSES: &[(u8, &str)] = &[
    (0, "None"),
    (2, "Allergy Care"),
    (1, "Steam Refresh"),
    (4, "Speedboil"),
    (5, "Babywear"),
    (7, "Standard"),
    (8, "Speedwash"),
    (9, "Quiet"),
    (10, "Colorcare"),
    (13, "Rinse+Spin"),
    DOWNLOADED_COURSE,
    (6, "Heavy Duty"),
    (3, "Functional Wear"),
    (11, "Duvet"),
    (12, "Lingerie/Wool"),
    (15, "Tub Clean"),
];

/// Code 0 is the model's own no-spin setting, not an unknown reading.
const SPINS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Extra low"),
    (2, "Low"),
    (3, "Medium"),
    (4, "High"),
    (5, "Extra high"),
];

/// Off and Cold are labels; the other readings are °C.
const TEMPERATURES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Cold"),
    (2, "30"),
    (3, "40"),
    (4, "60"),
    (5, "95"),
];
const TEMPERATURE_OPTIONS: &[&str] = &["Cold", "30", "40", "60"];

const ERROR_MESSAGES: &[(u8, &str)] = &[
    (0, "Normal"),
    (1, "dE2 Door open"),
    (2, "IE Water inlet"),
    (3, "OE Drainage"),
    (4, "UE Unbalance"),
    (5, "FE Overflow"),
    (6, "PE Water pressure"),
    (7, "tE Thermistor"),
    (8, "LE Motor (BLDC)"),
    (9, "CE Communication"),
    (10, "dHE Service call"),
    (11, "PF Power failure"),
    (12, "FF Freeze protection"),
    (13, "dCE Door circuit"),
    (14, "VS Vibration sensor"),
    (15, "EE EEPROM"),
    (16, "PS Power supply"),
    (17, "DE1 Door lock"),
    (18, "LOE Sliding lid"),
    (19, "dE4 Door"),
];

/// A native course's `F0 26` start template, and the option codes a capture
/// showed varying for it.
struct CourseTemplate {
    id: u8,
    template: &'static str,
    rinse: &'static [u8],
    spin: &'static [u8],
    temp: &'static [u8],
}

/// In course order, which is also Course select's order.
const COURSE_TEMPLATES: &[CourseTemplate] = &[
    CourseTemplate {
        id: 1,
        template: "f026010200000000003000200100000000000000000000",
        rinse: &[],
        spin: &[],
        temp: &[],
    },
    CourseTemplate {
        id: 2,
        template: "f026020203000403003000200200000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: &[],
    },
    CourseTemplate {
        id: 3,
        template: "f026030201000303003000200300000000000000000000",
        rinse: ALL_RINSE,
        spin: &[0, 1],
        temp: &[],
    },
    CourseTemplate {
        id: 4,
        template: "f026040203050303002000200d00000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: &[],
    },
    CourseTemplate {
        id: 5,
        template: "f026050203000404003000200b00000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: &[],
    },
    CourseTemplate {
        id: 6,
        template: "f026060303040304002000200e00000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: &[3, 4],
    },
    CourseTemplate {
        id: 7,
        template: "f026070201040100002080200500000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: ALL_TEMP,
    },
    CourseTemplate {
        id: 8,
        template: "f026080203000103003080200400000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: &[],
    },
    CourseTemplate {
        id: 9,
        template: "f026090202030403002000200700000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: ALL_TEMP,
    },
    CourseTemplate {
        id: 10,
        template: "f0260a0202020303002000201000000000000000000000",
        rinse: ALL_RINSE,
        spin: ALL_SPIN,
        temp: &[1, 2, 3],
    },
    CourseTemplate {
        id: 11,
        template: "f0260b0203010403002000200900000000000000000000",
        rinse: ALL_RINSE,
        spin: &[0, 1, 2, 3],
        temp: &[1, 2, 3],
    },
    CourseTemplate {
        id: 12,
        template: "f0260c0202010304002000200800000000000000000000",
        rinse: ALL_RINSE,
        spin: &[0, 1, 2],
        temp: &[1, 2, 3],
    },
    CourseTemplate {
        id: 13,
        template: "f0260d0003000103002000201100000000000000000000",
        rinse: &[1, 2, 3, 4, 5],
        spin: &[1, 2, 3, 4, 5],
        temp: &[],
    },
    CourseTemplate {
        id: 15,
        template: "f0260f0203040203002000200a00000000000000000000",
        rinse: &[],
        spin: &[],
        temp: &[],
    },
];

/// A downloadable course: its id in record byte 21, the `F0 25` install and
/// the `F0 26` start template, each captured from the app.
struct SmartCourse {
    id: u8,
    name: &'static str,
    download: &'static str,
    start: &'static str,
}

const SMART_COURSES: &[SmartCourse] = &[
    SmartCourse {
        id: 51,
        name: "Cold Wash",
        download: "f02503150e0204010300000000000f33000000000000000000",
        start: "f0260e0204010300002000200f33000000000000000000",
    },
    SmartCourse {
        id: 52,
        name: "Small Load",
        download: "f02503150e0203000200001080000434000000000000000000",
        start: "f0260e0203000200003080200434000000000000000000",
    },
    SmartCourse {
        id: 53,
        name: "Skin Care",
        download: "f02503150e0204030400000080000535000000000000000000",
        start: "f0260e0204030400002080200535000000000000000000",
    },
    SmartCourse {
        id: 54,
        name: "Rainy Day",
        download: "f02503150e0205030200000080000536000000000000000000",
        start: "f0260e0205030200002080200536000000000000000000",
    },
    SmartCourse {
        id: 55,
        name: "Sweat Stain",
        download: "f02503150e0303030300000000000e37000000000000000000",
        start: "f0260e0303030300002000200e37000000000000000000",
    },
    SmartCourse {
        id: 56,
        name: "Single Garments",
        download: "f02503150e0203000100001080000438000000000000000000",
        start: "f0260e0203000100003080200438000000000000000000",
    },
    SmartCourse {
        id: 57,
        name: "Kids Wear",
        download: "f02503150e0303040400000000000e39000000000000000000",
        start: "f0260e0303040400002000200e39000000000000000000",
    },
    SmartCourse {
        id: 58,
        name: "Shirt",
        download: "f02503150e0302040300000000000e3a000000000000000000",
        start: "f0260e0302040300002000200e3a000000000000000000",
    },
    SmartCourse {
        id: 59,
        name: "School Uniform",
        download: "f02503150e020303020000008000053b000000000000000000",
        start: "f0260e020303020000208020053b000000000000000000",
    },
    SmartCourse {
        id: 60,
        name: "Static Reduce",
        download: "f02503150e020000000000100000013c000000000000000000",
        start: "f0260e020000000000300020013c000000000000000000",
    },
    SmartCourse {
        id: 63,
        name: "Spin Only",
        download: "f02503150e000300000000000000153f000000000000000000",
        start: "f0260e000300000000200020153f000000000000000000",
    },
    SmartCourse {
        id: 65,
        name: "Deodorization",
        download: "f02503150e0200000000001000000141000000000000000000",
        start: "f0260e0200000000003000200141000000000000000000",
    },
    SmartCourse {
        id: 67,
        name: "Cloth Care",
        download: "f02503150e0202030300000000000843000000000000000000",
        start: "f0260e0202030300002000200843000000000000000000",
    },
    SmartCourse {
        id: 68,
        name: "Smart Rinse",
        download: "f02503150e0204030400000080000544000000000000000000",
        start: "f0260e0204030400002080200544000000000000000000",
    },
];

/// What Start course and Resume send. Options apply to native courses only.
#[derive(Clone, Copy)]
struct Selection {
    course: u8,
    smart: u8,
    smart_selected: bool,
    reserve_hours: u8,
    spin: u8,
    temperature: u8,
    rinse: u8,
}

/// The first captured Standard start: Extra low, 60 °C, one rinse.
const DEFAULT_SELECTION: Selection = Selection {
    course: 7,
    smart: 51,
    smart_selected: false,
    reserve_hours: 0,
    spin: 1,
    temperature: 4,
    rinse: 1,
};

pub struct Device {
    core: Arc<AabbDeviceCore>,
    selection: Mutex<Selection>,
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
            selection: Mutex::new(DEFAULT_SELECTION),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.components = components();
        core.set_config(config);

        let s = DEFAULT_SELECTION;
        core.publish_property(
            "course_select",
            label(COURSES, s.course).unwrap_or("None").into(),
        );
        core.publish_property(
            "smart_course_select",
            smart_course(s.smart).map_or("None", |c| c.name).into(),
        );
        core.publish_property("reserve_hours", i64::from(s.reserve_hours).into());
        core.publish_property("spin_select", label(SPINS, s.spin).unwrap_or("None").into());
        core.publish_property(
            "temperature_select",
            label(TEMPERATURES, s.temperature).unwrap_or("None").into(),
        );
        core.publish_property("rinse_count", i64::from(s.rinse).into());

        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if !checksum_ok(data) {
                return;
            }
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        match *buf {
            [CLASS_BYTE, REMOTE_START_HEARTBEAT, _, flags, _] => {
                self.core
                    .publish_flag("remote_start", flags, REMOTE_START_ON)
            }
            [CLASS_BYTE, POWER_HEARTBEAT, power] => self.core.publish_on_off("power", power != 0),
            [CLASS_BYTE, TELEMETRY_CD, ..] if buf.len() == 405 => self.publish_energy(buf[36]),
            [CLASS_BYTE, TELEMETRY_BD, ..] if matches!(buf.len(), 406 | 476) => {
                self.publish_energy(buf[37])
            }
            _ => {
                if let Some(block) = process_common_status(buf, CLASS_BYTE, STATUS_LENGTH) {
                    if block[1] == RECORD_MARKER {
                        self.process_record(&block[1..]);
                    }
                }
            }
        }
    }

    fn publish_energy(&self, wh: u8) {
        self.core.publish_property("energy", i64::from(wh).into());
    }

    fn process_record(&self, rec: &[u8]) {
        let state = rec[REC_STATE];
        let error = rec[REC_ERROR];
        self.core.publish_on_off("power", state != 0);
        self.core
            .publish_property("status", label_or_code(STATES, state).into());
        let course = rec[REC_COURSE];
        self.core
            .publish_property("course", label_or_code(COURSES, course).into());
        if course == DOWNLOADED_COURSE.0 {
            self.selection.lock().smart_selected = true;
            self.core
                .publish_property("course_select", DOWNLOADED_COURSE.1.into());
        }
        if let Some(smart) = smart_course(rec[REC_DOWNLOADED_COURSE]) {
            self.selection.lock().smart = smart.id;
            self.core
                .publish_property("smart_course", smart.name.into());
            self.core
                .publish_property("smart_course_select", smart.name.into());
        }
        self.core
            .publish_property("spin", label_or_code(SPINS, rec[REC_SPIN]).into());
        self.core.publish_property(
            "temperature",
            label(TEMPERATURES, rec[REC_TEMPERATURE])
                .unwrap_or("None")
                .into(),
        );
        self.core
            .publish_property("rinse", i64::from(rec[REC_RINSE]).into());
        self.core.publish_property(
            "remaining_time",
            minutes(rec[REC_REMAIN_HOUR], rec[REC_REMAIN_MINUTE]).into(),
        );
        self.core.publish_property(
            "initial_time",
            minutes(rec[REC_INITIAL_HOUR], rec[REC_INITIAL_MINUTE]).into(),
        );
        self.core.publish_property(
            "reserve_time",
            minutes(rec[REC_RESERVE_HOUR], rec[REC_RESERVE_MINUTE]).into(),
        );
        self.core.publish_on_off("error", error != 0);
        self.core
            .publish_property("error_message", label_or_code(ERROR_MESSAGES, error).into());
        self.core
            .publish_on_off("smart_diagnosis", state == STATE_SMART_DIAGNOSIS);
        self.core
            .publish_flag("child_lock", rec[REC_FLAGS], CHILD_LOCK_FLAG);
        self.core
            .publish_flag("turboshot", rec[REC_TURBO_SHOT], TURBO_SHOT_FLAG);
        self.core.publish_flag("steam", rec[REC_FLAGS], STEAM_FLAG);
    }

    fn select_course(&self, value: &str) {
        let Some(id) = code_of(COURSES, value) else {
            return;
        };
        {
            let mut s = self.selection.lock();
            if id == DOWNLOADED_COURSE.0 {
                // Start course now runs whatever Smart course select holds.
                s.smart_selected = true;
            } else if COURSE_TEMPLATES.iter().any(|c| c.id == id) {
                s.course = id;
                s.smart_selected = false;
            } else {
                return;
            }
        }
        self.core.publish_property("course_select", value.into());
    }

    fn select_smart_course(&self, value: &str) {
        let Some(smart) = SMART_COURSES.iter().find(|c| c.name == value) else {
            return;
        };
        {
            let mut s = self.selection.lock();
            s.smart = smart.id;
            s.smart_selected = true;
        }
        self.core.send(&hex_decode(smart.download));
        self.core
            .publish_property("smart_course_select", value.into());
        self.core
            .publish_property("course_select", DOWNLOADED_COURSE.1.into());
    }

    fn start_course(&self, flag: u8) {
        let s = *self.selection.lock();
        if s.smart_selected {
            if let Some(smart) = smart_course(s.smart) {
                self.core
                    .send(&start_frame(smart.start, s.reserve_hours, flag));
                self.core
                    .publish_property("smart_course", smart.name.into());
            }
            return;
        }
        let Some(course) = COURSE_TEMPLATES.iter().find(|c| c.id == s.course) else {
            return;
        };
        let mut frame = start_frame(course.template, s.reserve_hours, flag);
        if course.spin.contains(&s.spin) {
            frame[TEMPLATE_SPIN] = s.spin;
        }
        if course.temp.contains(&s.temperature) {
            frame[TEMPLATE_TEMP] = s.temperature;
        }
        if course.rinse.contains(&s.rinse) {
            frame[TEMPLATE_RINSE] = s.rinse;
        }
        self.core.send(&frame);
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "power_off" => self.core.send(POWER_OFF_COMMAND),
            // Older automations write the power entity; ON has no captured command.
            "power" if value == "OFF" => self.core.send(POWER_OFF_COMMAND),
            "pause" => self.core.send(PAUSE_COMMAND),
            "course_select" => self.select_course(value),
            "smart_course_select" => self.select_smart_course(value),
            "reserve_hours" => {
                // LG declares 0 (start now) or 3 to 19 hours.
                let Some(hours) = whole_number(value).filter(|&h| h == 0 || (3..=19).contains(&h))
                else {
                    return;
                };
                self.selection.lock().reserve_hours = hours as u8;
                self.core.publish_property("reserve_hours", hours.into());
            }
            "spin_select" => {
                let Some(code) = code_of(SPINS, value) else {
                    return;
                };
                self.selection.lock().spin = code;
                self.core.publish_property("spin_select", value.into());
            }
            "temperature_select" => {
                let Some(code) = temperature_code(value) else {
                    return;
                };
                self.selection.lock().temperature = code;
                self.core
                    .publish_property("temperature_select", value.into());
            }
            "rinse_count" => {
                let Some(count) = whole_number(value).filter(|c| (0..=5).contains(c)) else {
                    return;
                };
                self.selection.lock().rinse = count as u8;
                self.core.publish_property("rinse_count", count.into());
            }
            "start_course" => self.start_course(START_FLAG),
            "resume" => self.start_course(RESUME_FLAG),
            _ => {}
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

fn checksum_ok(frame: &[u8]) -> bool {
    let n = frame.len();
    n >= 4 && frame[n - 2] == aabb_checksum(&frame[..n - 2])
}

/// A start template with the reserve hours and the start flag filled in.
fn start_frame(template: &str, reserve_hours: u8, flag: u8) -> Vec<u8> {
    let mut frame = hex_decode(template);
    frame[TEMPLATE_RESERVE] = reserve_hours;
    frame[TEMPLATE_FLAG] = flag;
    frame
}

fn smart_course(id: u8) -> Option<&'static SmartCourse> {
    SMART_COURSES.iter().find(|c| c.id == id)
}

fn label(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table.iter().find(|(c, _)| *c == code).map(|(_, l)| *l)
}

/// Unmapped codes read `Code <n>`, as upstream reports them.
fn label_or_code(table: &'static [(u8, &'static str)], code: u8) -> String {
    label(table, code).map_or_else(|| format!("Code {code}"), str::to_string)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, l)| *l).collect()
}

fn code_of(table: &'static [(u8, &'static str)], name: &str) -> Option<u8> {
    table.iter().find(|(_, l)| *l == name).map(|(c, _)| *c)
}

/// `Number(value)` when it is a whole number.
fn whole_number(value: &str) -> Option<i64> {
    crate::js::number(value)
        .filter(|n| n.is_finite() && n.fract() == 0.0)
        .map(|n| n as i64)
}

/// A numeric payload (`40`, `40.0`) matches the °C labels, anything else is
/// matched verbatim (`Cold`, `Off`).
fn temperature_code(value: &str) -> Option<u8> {
    let number = crate::js::number(value);
    TEMPERATURES
        .iter()
        .find(|(_, l)| match number {
            Some(n) => l.parse::<f64>().ok() == Some(n),
            None => *l == value,
        })
        .map(|(c, _)| *c)
}

fn minutes(hour: u8, minute: u8) -> i64 {
    i64::from(hour) * 60 + i64::from(minute)
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(base), Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
    base
}

fn sensor(id: &str, name: &str, extra: Value) -> Value {
    merge(
        json!({"platform":"sensor","unique_id":format!("$deviceid-{id}"),"state_topic":format!("$this/{id}"),"name":name}),
        extra,
    )
}

fn duration(id: &str, name: &str, icon: &str) -> Value {
    sensor(
        id,
        name,
        json!({"device_class":"duration","unit_of_measurement":"min","state_class":"measurement","icon":icon}),
    )
}

fn reading(id: &str, name: &str, options: &[&str], extra: Value) -> Value {
    sensor(
        id,
        name,
        merge(json!({"device_class":"enum","options":options}), extra),
    )
}

fn binary_sensor(id: &str, name: &str, extra: Value) -> Value {
    merge(
        json!({"platform":"binary_sensor","unique_id":format!("$deviceid-{id}"),"state_topic":format!("$this/{id}"),"name":name,"payload_on":"ON","payload_off":"OFF"}),
        extra,
    )
}

fn press(id: &str, name: &str, icon: &str) -> Value {
    json!({"platform":"button","unique_id":format!("$deviceid-{id}"),"command_topic":format!("$this/{id}/set"),"payload_press":"","name":name,"icon":icon})
}

fn choice(id: &str, name: &str, options: &[&str], extra: Value) -> Value {
    merge(
        json!({"platform":"select","unique_id":format!("$deviceid-{id}"),"state_topic":format!("$this/{id}"),"command_topic":format!("$this/{id}/set"),"name":name,"options":options}),
        extra,
    )
}

fn number(id: &str, name: &str, min: u8, max: u8, extra: Value) -> Value {
    merge(
        json!({"platform":"number","unique_id":format!("$deviceid-{id}"),"state_topic":format!("$this/{id}"),"command_topic":format!("$this/{id}/set"),"name":name,"min":min,"max":max,"step":1}),
        extra,
    )
}

fn components() -> HashMap<String, Value> {
    let course_select: Vec<&str> = COURSE_TEMPLATES
        .iter()
        .filter_map(|c| label(COURSES, c.id))
        .chain([DOWNLOADED_COURSE.1])
        .collect();
    let smart_courses: Vec<&str> = SMART_COURSES.iter().map(|c| c.name).collect();
    [
        ("power_off", press("power_off", "Power off", "mdi:power")),
        (
            "power",
            binary_sensor("power", "Power", json!({"icon":"mdi:power"})),
        ),
        (
            "status",
            reading(
                "status",
                "Status",
                &labels(STATES),
                json!({"icon":"mdi:washing-machine"}),
            ),
        ),
        (
            "course",
            reading(
                "course",
                "Course",
                &labels(COURSES),
                json!({"icon":"mdi:playlist-check"}),
            ),
        ),
        (
            "course_select",
            choice(
                "course_select",
                "Course select",
                &course_select,
                json!({"icon":"mdi:playlist-edit"}),
            ),
        ),
        (
            "smart_course",
            reading(
                "smart_course",
                "Smart course",
                &smart_courses,
                json!({"icon":"mdi:playlist-star"}),
            ),
        ),
        (
            "smart_course_select",
            choice(
                "smart_course_select",
                "Smart course select",
                &smart_courses,
                json!({"icon":"mdi:playlist-edit"}),
            ),
        ),
        // HA's number has no holes; set_property enforces 0 or 3 to 19.
        (
            "reserve_hours",
            number(
                "reserve_hours",
                "Reserve hours",
                0,
                19,
                json!({"icon":"mdi:calendar-clock","entity_category":"config"}),
            ),
        ),
        (
            "spin_select",
            choice(
                "spin_select",
                "Spin select",
                &labels(SPINS),
                json!({"icon":"mdi:rotate-3d-variant","entity_category":"config"}),
            ),
        ),
        (
            "temperature_select",
            choice(
                "temperature_select",
                "Temperature select",
                TEMPERATURE_OPTIONS,
                json!({"icon":"mdi:thermometer","entity_category":"config"}),
            ),
        ),
        (
            "rinse_count",
            number(
                "rinse_count",
                "Rinse count",
                0,
                5,
                json!({"icon":"mdi:water-sync","entity_category":"config"}),
            ),
        ),
        (
            "spin",
            reading(
                "spin",
                "Spin",
                &labels(SPINS),
                json!({"icon":"mdi:rotate-3d-variant"}),
            ),
        ),
        (
            "rinse",
            sensor("rinse", "Rinse", json!({"icon":"mdi:water-sync"})),
        ),
        (
            "start_course",
            press("start_course", "Start course", "mdi:play-circle-outline"),
        ),
        ("pause", press("pause", "Pause", "mdi:pause-circle-outline")),
        ("resume", press("resume", "Resume", "mdi:play-pause")),
        // Not a temperature device_class: it can read Off or Cold.
        (
            "temperature",
            sensor(
                "temperature",
                "Water temperature",
                json!({"icon":"mdi:thermometer"}),
            ),
        ),
        (
            "remaining_time",
            duration("remaining_time", "Remaining time", "mdi:timer-sand"),
        ),
        (
            "initial_time",
            duration("initial_time", "Initial time", "mdi:timer-outline"),
        ),
        (
            "reserve_time",
            duration("reserve_time", "Reserve time", "mdi:calendar-clock"),
        ),
        (
            "energy",
            sensor(
                "energy",
                "Energy",
                json!({"device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh","icon":"mdi:lightning-bolt"}),
            ),
        ),
        (
            "error",
            binary_sensor(
                "error",
                "Error",
                json!({"device_class":"problem","entity_category":"diagnostic"}),
            ),
        ),
        (
            "error_message",
            reading(
                "error_message",
                "Error message",
                &labels(ERROR_MESSAGES),
                json!({"icon":"mdi:alert-circle-outline","entity_category":"diagnostic"}),
            ),
        ),
        (
            "remote_start",
            binary_sensor(
                "remote_start",
                "Remote start",
                json!({"icon":"mdi:cellphone-check"}),
            ),
        ),
        // Read-only: child lock is panel-only; TurboShot and Steam are set on the panel or app.
        (
            "child_lock",
            binary_sensor(
                "child_lock",
                "Child lock",
                json!({"icon":"mdi:lock","entity_category":"diagnostic"}),
            ),
        ),
        (
            "turboshot",
            binary_sensor(
                "turboshot",
                "TurboShot",
                json!({"icon":"mdi:shower-head"}),
            ),
        ),
        (
            "steam",
            binary_sensor("steam", "Steam", json!({"icon":"mdi:cloud-outline"})),
        ),
        (
            "smart_diagnosis",
            binary_sensor(
                "smart_diagnosis",
                "Smart diagnosis",
                json!({"device_class":"problem","icon":"mdi:stethoscope","entity_category":"diagnostic"}),
            ),
        ),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{prop, DEVICE_ID};
    use rethink_core::{MockHaConnection, MockThinq2Device};

    // Standby reply to the status query, 2026-09-07.
    const CURRENT: &str =
        "aa2c20eb0024050000000000000000000000000000020000010033010f000000000002022d1e00000100f5bb";

    // Panel Child Lock pressed then released: only record byte 15 changes (0x00 <-> 0x08).
    const CHILD_LOCK_ON: &str = "aa5220ec0024050000000000000000000000000000020000000033040f000000000002022d1e000001000024050000000000000000000000000008020000000033040f000000000002022d1e00000100c7bb";
    const CHILD_LOCK_OFF: &str = "aa5220ec0024050000000000000000000000000008020000000033040f000000000002022d1e000001000024050000000000000000000000000000020000000033040f000000000002022d1e00000100c7bb";
    const CHILD_LOCK_ON_SINGLE: &str =
        "aa2c20eb0024050000000000000000000000000008020000000033040f000000000002022d1e00000100ffbb";

    // Timing out from Standby to Power off, as EC then EB.
    const POWERING_OFF: &str = "aa5220ec0024050000000000000000000000000000020000020033010f000000000002022d1e000001000024000000000000000000000000000000020000020533010f000000000002022d1e00000100ddbb";
    const OFF: &str =
        "aa2c20eb0024000000000000000000000000000000020000020533010f000000000002022d1e00000100f4bb";

    // An older standby snapshot whose unlabelled tail differs.
    const HISTORICAL_CURRENT: &str =
        "aa2c20eb0024050000000000000000000000000000020000000033310f000000000002022d1e000001009abb";

    // Standard picked on the panel (40 °C, 33 min), then raised to 60 °C from the app (55 min).
    const STANDARD: &str = "aa5220ec002405012f012f06000303040300000000020000000033010f000000000002022d1e000001000024050021002107000204030200000000824000000033010f000000000002022d1e000001005ebb";
    const TEMPERATURE_60: &str = "aa5220ec0024050021002107000204030200000000824000000033010f000000000002022d1e000001000024050037003707000204040200000000820000000033010f000000000002022d1e00000100ccbb";

    // D8 heartbeat before and after a physical power-on.
    const HEARTBEAT_OFF: &str = "aa0720d800fcbb";
    const HEARTBEAT_ON: &str = "aa0720d801ffbb";

    // Detailed telemetry from a full cycle the app reported as 221 Wh: BD (406) mid-cycle at 80,
    // CD (405) just before Complete at 218, BD (476) right after Complete at 221.
    const ENERGY_BD_80: &str = "aa0020bd000201900103011701280209000005000202000504000520c60000000400000000000050043d3e3f3a381d1d1d1c1ce8e7e6e6e7099609960996099609960aff0aff0aff0aff0aff00360036003600360036848484848498270000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000fd00000000000000070000010000017a02abe3e301000101000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a200000000000000000000000000000000000000000000000000000000000068bb";
    const ENERGY_CD_218: &str = "aa0020cd00019001030128000202090000050000000000040005208600000004000000000000da04474849494a1d1d1d1d1dffffffffff099609960996099609960aff0aff0aff0aff0aff00360036003600360036848484848497dd000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003012c0300000000030000000384017a02a1ffe1040001010000ffff010f3104045a0000000c2911006917310402e7000000081708002309310402ce0000000d2107003c06310402780000000c0c05002608310403070000000c0a06001f05310402440000000b180700370e32020264000000071d05005a0d32020288000000041c0600520a320202be000000050f030028073402009c000000021508004007340200aa00000011140c002001340200d4000000091c0700450932020260000000060c01001c07310402ab0000000718050025073104034d00000004140700240702ee02ee0900000000a2000000000000000000000000000000000000000000000000000000000000fabb";
    const ENERGY_BD_221: &str = "aa0020bd0003019001030128000102090000050000000000000005000600000004000000000000dd014b460000001d1d000000ffff000000099609960000000000000aff0aff0000000000000036003600000000000084840000009895000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003012c0300000000030000000384017a028affe1040001010000ffff010f3104045a0000000c2911006917310402e7000000081708002309310402ce0000000d2107003c06310402780000000c0c05002608310403070000000c0a06001f05310402440000000b180700370e32020264000000071d05005a0d32020288000000041c0600520a320202be000000050f030028073402009c000000021508004007340200aa00000011140c002001340200d4000000091c0700450932020260000000060c01001c07310402ab0000000718050025073104034d00000004140700240702ee02ee0a00000000a20000000000000000000000000000000000000000000000000000000000004503010100040506033900000100ff014b00000000011c1c01041d1c000f03fd4b4a0000fdfd000000fd00000806070700000f000550200002640000c0425288595f011a180059bb";

    // An E2 frame from the same run, with no energy field.
    const UNRELATED_E2: &str =
        "aa2c20e20324140106010607000204020500000020860000dd0533040f000000000002022d1e0000010006bb";

    // Steam Refresh started with a 19-hour reservation.
    const RESERVED_STEAM_REFRESH: &str = "aa5220ec002406002e002e07000201040100000020860000011733010f010000000002022d1e0000010000240a0014001401000200000000130030060000010633010f050000000000002d1e0000010079bb";

    // The same 16-hour Standard reservation with TurboShot on, then off: record byte 16 bit 0x80.
    const TURBOSHOT_ON_RESERVED: &str = "aa5220ec0024140032003207000201040100100020c60000000533050f000000000002022d1e0000010000240a002e002e07000201040100100020c60000001433050f010000000002022d1e00000100d5bb";
    const TURBOSHOT_OFF_RESERVED: &str = "aa5220ec00241400320032070002010401000f3920460000000633050f000000000002022d1e0000010000240a002e002e070002010401000f3920460000001433050f010000000002022d1e00000100a4bb";

    // That reservation with steam on (temperature forced Off): flags bit 0x10.
    const STEAM_ON_RESERVED: &str = "aa5220ec00241400310031070002010001000f3530460000000633050f000000000002022d1e0000010000240a002d002d070002010001000f3530460000001433050f010000000002022d1e00000100a8bb";

    // Standard started with 1 rinse, Extra low, 60 °C.
    const DETECTING_RUN: &str = "aa5220ec0024050021002107000204030200000020864000000033010f000000000002022d1e000001000024140032003207000201040100000020860000000533010f000000000002022d1e00000100b2bb";

    // Reserved Standard runs: 2 rinses/High/40 °C/3 h, 3/Low/30 °C/3 h, 4/Medium/Cold/3 h,
    // 5/Extra high/60 °C/3 h and no rinse/no spin/40 °C/4 h.
    const CUSTOM_OPTIONS_RUN: &str = "aa5220ec0024140021002107000204030200030020864000020633010f000000000002022d1e0000010000240a0134013407000204030200030020c64000001433010f050000000002022d1e00000100c2bb";
    const THIRTY_C_RUN: &str = "aa5220ec002414002e002e07000202020300030020c60000000633010f000000000002022d1e0000010000240a0125012507000202020300030020c60000001433010f050000000002022d1e0000010018bb";
    const COLD_WATER_RUN: &str = "aa5220ec0024060125012507000202020300023920860000000a33010f050000000002022d1e000001000024140039003907000203010400030020c60000000633010f000000000002022d1e000001001bbb";
    const MAX_RINSE_SPIN_RUN: &str = "aa5220ec00240a0229022907000205040500030020c60000001433010f050000000002022d1e000001000024170229022907000205040500000020c60000000a33010f050000000002022d1e000001003dbb";
    const OPTIONS_OFF_RUN: &str = "aa5220ec0024140010001007000200030000040020864000000633010f000000000002022d1e0000010000240a0109010907000200030000040020c64000001433010f050000000002022d1e0000010046bb";

    // Quiet with 4 rinses/Low/40 °C, then 1/Extra low/60 °C, both reserved 3 h.
    const QUIET_RUN: &str = "aa5220ec0024060109010907000200030000033b20864000000a33010f050000000002022d1e00000100002414003b003b09000202030400030020460000000633010f000000000002022d1e0000010089bb";
    const QUIET_RUN_2: &str = "aa5220ec0024060014001401000200000000000030060000001733010f050000000000002d1e000001000024140020002009000201040100030020060000000633010f000000000002022d1e00000100dcbb";

    // One captured run of each other course; the tests below name its options.
    const SPEEDWASH_RUN: &str = "aa5220ec0024060106010609000201040100030020060000000a33010f010000000002022d1e00000100002414000f000f08000203000100030030c60000010633010f000000000002022d1e000001005abb";
    const COLORCARE_RUN: &str = "aa5220ec002406000f000f08000203000100023a30860000000a33010f010000000002022d1e00000100002414010201020a000202020300030020060000010633010f000000000002022d1e0000010057bb";
    const RINSE_SPIN_RUN: &str = "aa5220ec002406003b003b0a000202020300023b20060000000a33010f010000000002022d1e0000010000240a001300130d000003000100030020460000000633010f050000000000022d1e0000010073bb";
    const SPEEDBOIL_RUN: &str = "aa5220ec002406001300130d000003000100023a20060000000a33010f050000000000022d1e000001000024140227022704000203050300030020060000000633010f000000000002022d1e000001009dbb";
    const BABYWEAR_RUN: &str = "aa5220ec0024060224022404000203050300000020060000001733010f010000000002022d1e000001000024140123012305000203000400040030060000000633010f000000000002022d1e0000010094bb";
    const ALLERGYCARE_RUN: &str = "aa5220ec0024060120012005000203000400040030060000000a33010f010000000002022d1e000001000024140123012302000203000400030030060000000633010f000000000002022d1e00000100e2bb";
    const HEAVYDUTY_RUN: &str = "aa5220ec0024060120012002000203000400030030060000000a33010f010000000002022d1e00000100002414012f012f06000303040300040020060000000633010f000000000002022d1e0000010091bb";
    const FUNCTIONALWEAR_RUN: &str = "aa5220ec002406012c012c06000303040300040020060000000a33010f010000000002022d1e000001000024140109010903000201000300030030060000000633010f000000000000002d1e00000100dfbb";
    const DUVET_RUN: &str = "aa5220ec0024060106010603000201000300030030060000000a33010f010000000000002d1e00000100002414013401340b000203010400030020060000000633010f000000000002022d1e00000100c0bb";
    const LINGERIE_WOOL_RUN: &str = "aa5220ec002406013401340b000203010400030020060000001433010f000000000002022d1e00000100002414003600360c000202010300040020060000000633010f000000000000002d1e00000100adbb";
    const TUB_CLEAN_RUN: &str = "aa5220ec002406003100310c000202010300033720060000010a33010f010000000000002d1e00000100002414020902090f000203040200030020060000020633010f000000000000002d1e000001009ebb";

    // F0 25 installs from the app, and records reporting each installed course id at byte 21.
    const SMALL_LOAD_DOWNLOAD: &str = "aa1df02503150e020300020000108000043400000000000000000084bb";
    const COLD_WASH_DOWNLOAD: &str = "aa1df02503150e0204010300000000000f330000000000000000001bbb";
    const SMALL_LOAD_DOWNLOADED: &str = "aa5220ec0024000000000000000000000000000000020000070633040f000000000002022d1e0000010000240000000000000000000000000000000200000706340404000000000002022d1e00000100c5bb";
    const COLD_WASH_DOWNLOADED: &str = "aa5220ec00240000000000000000000000000000000200000706340404000000000002022d1e000001000024000000000000000000000000000000020000000633040f000000000002022d1e00000100dcbb";
    const SWEAT_STAIN_DOWNLOADED: &str = "aa5220ec00240000000000000000000000000000000200000006340404000000000002022d1e000001000024000000000000000000000000000000020000000637040e000000000002022d1e00000100d0bb";
    const SINGLE_GARMENTS_DOWNLOADED: &str = "aa5220ec0024000000000000000000000000000000020000000637040e000000000002022d1e0000010000240000000000000000000000000000000200000006380404000000000002022d1e00000100dcbb";
    const KIDS_WEAR_DOWNLOADED: &str = "aa5220ec00240000000000000000000000000000000200000006380404000000000002022d1e000001000024000000000000000000000000000000020000000639040e000000000002022d1e00000100debb";
    const SHIRT_DOWNLOADED: &str = "aa5220ec0024000000000000000000000000000000020000000639040e000000000002022d1e00000100002400000000000000000000000000000002000000063a040e000000000002022d1e00000100c2bb";
    const SCHOOL_UNIFORM_DOWNLOADED: &str = "aa5220ec002400000000000000000000000000000002000000063a040e000000000002022d1e00000100002400000000000000000000000000000002000000063b0405000000000002022d1e00000100c5bb";
    const STATIC_REDUCE_DOWNLOADED: &str = "aa5220ec002400000000000000000000000000000002000000063b0405000000000002022d1e00000100002400000000000000000000000000000002000000063c0401000000000002022d1e00000100d0bb";
    const SPIN_ONLY_DOWNLOADED: &str = "aa5220ec002400000000000000000000000000000002000000063c0401000000000002022d1e00000100002400000000000000000000000000000002000000063f0415000000000002022d1e00000100ccbb";
    const DEODORIZATION_DOWNLOADED: &str = "aa5220ec002400000000000000000000000000000002000000063f0415000000000002022d1e0000010000240000000000000000000000000000000200000006410401000000000002022d1e00000100cbbb";
    const CLOTH_CARE_DOWNLOADED: &str = "aa5220ec00240000000000000000000000000000000200000006410401000000000002022d1e0000010000240000000000000000000000000000000200000006430408000000000002022d1e00000100c0bb";
    const SMART_RINSE_DOWNLOADED: &str = "aa5220ec00240000000000000000000000000000000200000006430408000000000002022d1e0000010000240000000000000000000000000000000200000006440405000000000002022d1e00000100c9bb";

    // Cold Wash starts: reserved 19 h, now, and 7 h.
    const COLD_WASH_RESERVED_START: &str = "aa1bf0260e0204010313002000200f33000000000000000000ddbb";
    const COLD_WASH_START_NOW: &str = "aa1bf0260e0204010300002000200f3300000000000000000020bb";
    const COLD_WASH_START_7H: &str = "aa1bf0260e0204010307002000200f3300000000000000000029bb";

    // The 19-hour Cold Wash reservation.
    const COLD_WASH_RESERVED: &str = "aa5220ec002414011401140e000204010300130020460000000533040f000000000002022d1e0000010000240a011001100e000204010300130020460000001433040f010000000002022d1e0000010077bb";

    // Live Rinsing at spin High, temperature Off, rinse 2 then 1 (owner-labelled in the app).
    const LIVE_RINSE_2: &str = "aa5220ec00241e000c0104070000040002000000208600007b1733010f010000000002022d1e0000010000241e000b010407000004000200000020860000841733010f010000000002022d1e000001003dbb";
    const LIVE_RINSE_1: &str = "aa5220ec00241e000b010407000004000200000020860000841733010f010000000002022d1e0000010000241e000a010407000004000100000020860000871733010f010000000002022d1e0000010024bb";

    const STATUS_REQUEST: &str = "aa0ef0ed1121010000001800b5bb";

    fn meta() -> Metadata {
        Metadata::new("F24VDD", "F24VDD", "2.10.97")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    /// A fresh device after `frames`.
    fn decode(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        for frame in frames {
            emit(&thinq, frame);
        }
        ha
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn properties(ha: &MockHaConnection) -> HashMap<String, String> {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .iter()
            .map(|(name, value)| (name.clone(), value.as_string()))
            .collect()
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    /// Every packet sent, as lowercase hex.
    fn outbox(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .outbox()
            .iter()
            .map(rethink_util::hex::encode)
            .collect()
    }

    /// What a fresh device sends for `writes`, applied in order.
    fn sent_after(writes: &[(&str, &str)]) -> Vec<String> {
        let (_, thinq, dev) = make();
        for (name, value) in writes {
            dev.set_property(name, value);
        }
        outbox(&thinq)
    }

    /// `frame` with its checksum recomputed after an edit.
    fn reframe(mut frame: Vec<u8>) -> Vec<u8> {
        let n = frame.len();
        frame[n - 2] = aabb_checksum(&frame[..n - 2]);
        frame
    }

    #[test]
    fn real_capture_fixtures_have_intact_aabb_envelopes() {
        for fixture in [
            CURRENT,
            CHILD_LOCK_ON,
            CHILD_LOCK_OFF,
            CHILD_LOCK_ON_SINGLE,
            POWERING_OFF,
            OFF,
            HISTORICAL_CURRENT,
            STANDARD,
            TEMPERATURE_60,
            DETECTING_RUN,
            RESERVED_STEAM_REFRESH,
            TURBOSHOT_ON_RESERVED,
            TURBOSHOT_OFF_RESERVED,
            STEAM_ON_RESERVED,
            CUSTOM_OPTIONS_RUN,
            THIRTY_C_RUN,
            COLD_WATER_RUN,
            MAX_RINSE_SPIN_RUN,
            OPTIONS_OFF_RUN,
            QUIET_RUN,
            QUIET_RUN_2,
            SPEEDWASH_RUN,
            COLORCARE_RUN,
            RINSE_SPIN_RUN,
            SPEEDBOIL_RUN,
            BABYWEAR_RUN,
            ALLERGYCARE_RUN,
            HEAVYDUTY_RUN,
            FUNCTIONALWEAR_RUN,
            DUVET_RUN,
            LINGERIE_WOOL_RUN,
            TUB_CLEAN_RUN,
            COLD_WASH_RESERVED,
            SMALL_LOAD_DOWNLOADED,
            COLD_WASH_DOWNLOADED,
            SWEAT_STAIN_DOWNLOADED,
            SINGLE_GARMENTS_DOWNLOADED,
            KIDS_WEAR_DOWNLOADED,
            SHIRT_DOWNLOADED,
            SCHOOL_UNIFORM_DOWNLOADED,
            STATIC_REDUCE_DOWNLOADED,
            SPIN_ONLY_DOWNLOADED,
            DEODORIZATION_DOWNLOADED,
            CLOTH_CARE_DOWNLOADED,
            SMART_RINSE_DOWNLOADED,
            SMALL_LOAD_DOWNLOAD,
            COLD_WASH_DOWNLOAD,
            COLD_WASH_RESERVED_START,
            COLD_WASH_START_NOW,
            COLD_WASH_START_7H,
            LIVE_RINSE_2,
            LIVE_RINSE_1,
        ] {
            let frame = hex_decode(fixture);
            let n = frame.len();
            assert_eq!(usize::from(frame[1]), n, "{fixture}");
            assert_eq!(frame[n - 2], aabb_checksum(&frame[..n - 2]), "{fixture}");
        }
    }

    #[test]
    fn publishes_the_currently_grounded_entities_with_power_off_writable() {
        let (ha, _, _) = make();
        let components = components(&ha);
        let mut keys: Vec<&str> = components.keys().map(String::as_str).collect();
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
                "rinse",
                "rinse_count",
                "smart_course",
                "smart_course_select",
                "smart_diagnosis",
                "spin",
                "spin_select",
                "start_course",
                "status",
                "steam",
                "temperature",
                "temperature_select",
                "turboshot",
            ]
        );
        assert_eq!(components["power"]["platform"], "binary_sensor");
        assert_eq!(components["power"]["icon"], "mdi:power");
        for read_only in ["turboshot", "steam"] {
            assert_eq!(components[read_only]["platform"], "binary_sensor");
            assert!(components[read_only].get("command_topic").is_none());
            assert!(components[read_only].get("entity_category").is_none());
        }
        assert_eq!(components["smart_diagnosis"]["device_class"], "problem");
        let status = components["status"]["options"].as_array().unwrap();
        assert!(status.contains(&json!("Error")));
        assert!(status.contains(&json!("Smart diagnosis")));
        assert!(!status.contains(&json!("Error auto off")));
        assert!(!status.contains(&json!("Audible diagnosis")));
        assert_eq!(
            components["smart_course_select"]["options"],
            json!([
                "Cold Wash",
                "Small Load",
                "Skin Care",
                "Rainy Day",
                "Sweat Stain",
                "Single Garments",
                "Kids Wear",
                "Shirt",
                "School Uniform",
                "Static Reduce",
                "Spin Only",
                "Deodorization",
                "Cloth Care",
                "Smart Rinse",
            ])
        );
        let writable = [
            "power_off",
            "pause",
            "resume",
            "course_select",
            "smart_course_select",
            "reserve_hours",
            "start_course",
            "spin_select",
            "temperature_select",
            "rinse_count",
        ];
        for (id, component) in &components {
            if writable.contains(&id.as_str()) {
                assert_eq!(
                    component["command_topic"],
                    format!("$this/{id}/set"),
                    "{id}"
                );
            } else {
                assert!(component.get("command_topic").is_none(), "{id}");
            }
        }
    }

    #[test]
    fn decodes_the_current_captured_standby_record() {
        let ha = decode(&[CURRENT]);
        let expected: HashMap<String, String> = [
            ("course_select", "Standard"),
            ("smart_course_select", "Cold Wash"),
            ("reserve_hours", "0"),
            ("spin_select", "Extra low"),
            ("temperature_select", "60"),
            ("rinse_count", "1"),
            ("power", "ON"),
            ("status", "Standby"),
            ("course", "None"),
            ("smart_course", "Cold Wash"),
            ("spin", "Off"),
            ("temperature", "Off"),
            ("rinse", "0"),
            ("remaining_time", "0"),
            ("initial_time", "0"),
            ("reserve_time", "0"),
            ("error", "OFF"),
            ("error_message", "Normal"),
            ("smart_diagnosis", "OFF"),
            ("child_lock", "OFF"),
            ("turboshot", "OFF"),
            ("steam", "OFF"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        assert_eq!(properties(&ha), expected);
    }

    #[test]
    fn tracks_the_actually_downloaded_course_from_record_byte_21() {
        let (ha, thinq, _) = make();
        for (frame, label) in [
            (SMALL_LOAD_DOWNLOADED, "Small Load"),
            (COLD_WASH_DOWNLOADED, "Cold Wash"),
            (SWEAT_STAIN_DOWNLOADED, "Sweat Stain"),
            (SINGLE_GARMENTS_DOWNLOADED, "Single Garments"),
            (KIDS_WEAR_DOWNLOADED, "Kids Wear"),
            (SHIRT_DOWNLOADED, "Shirt"),
            (SCHOOL_UNIFORM_DOWNLOADED, "School Uniform"),
            (STATIC_REDUCE_DOWNLOADED, "Static Reduce"),
            (SPIN_ONLY_DOWNLOADED, "Spin Only"),
            (DEODORIZATION_DOWNLOADED, "Deodorization"),
            (CLOTH_CARE_DOWNLOADED, "Cloth Care"),
            (SMART_RINSE_DOWNLOADED, "Smart Rinse"),
        ] {
            emit(&thinq, frame);
            expect(&ha, "smart_course", label);
            expect(&ha, "smart_course_select", label);
        }

        // An unknown id leaves the last reading in place.
        emit(&thinq, COLD_WASH_DOWNLOADED);
        let mut unknown = hex_decode(COLD_WASH_DOWNLOADED);
        unknown[64] = 99; // AA/len + current record at 41 + record byte 21
        thinq.emit_data(&reframe(unknown));
        expect(&ha, "smart_course", "Cold Wash");
        expect(&ha, "smart_course_select", "Cold Wash");
    }

    #[test]
    fn an_out_of_range_spin_code_is_reported_as_code_n_not_folded_into_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, CURRENT);
        expect(&ha, "spin", "Off");

        let mut unknown_spin = hex_decode(CURRENT);
        unknown_spin[14] = 9; // AA/len + record at 3 + spin at 9
        thinq.emit_data(&reframe(unknown_spin));
        expect(&ha, "spin", "Code 9");
    }

    #[test]
    fn a_frame_with_a_corrupted_checksum_is_dropped_not_decoded() {
        let (ha, thinq, _) = make();
        emit(&thinq, CURRENT);
        expect(&ha, "spin", "Off");

        let mut corrupted = hex_decode(CURRENT);
        corrupted[14] = 9;
        thinq.emit_data(&corrupted);
        expect(&ha, "spin", "Off");
    }

    #[test]
    fn decodes_the_captured_child_lock_button_press_and_release() {
        let (ha, thinq, _) = make();
        emit(&thinq, CHILD_LOCK_ON);
        expect(&ha, "child_lock", "ON");
        emit(&thinq, CHILD_LOCK_OFF);
        expect(&ha, "child_lock", "OFF");
        emit(&thinq, CHILD_LOCK_ON_SINGLE);
        expect(&ha, "child_lock", "ON");
    }

    #[test]
    fn decodes_the_owner_labelled_turboshot_on_off_pair_on_the_same_reservation() {
        let (ha, thinq, _) = make();
        emit(&thinq, TURBOSHOT_ON_RESERVED);
        expect(&ha, "course", "Standard");
        expect(&ha, "turboshot", "ON");
        emit(&thinq, TURBOSHOT_OFF_RESERVED);
        expect(&ha, "turboshot", "OFF");
    }

    #[test]
    fn decodes_the_owner_labelled_steam_on_frame_off_on_the_same_settings_run() {
        let (ha, thinq, _) = make();
        emit(&thinq, STEAM_ON_RESERVED);
        expect(&ha, "course", "Standard");
        expect(&ha, "temperature", "Off");
        expect(&ha, "steam", "ON");
        expect(&ha, "turboshot", "OFF");
        emit(&thinq, TURBOSHOT_OFF_RESERVED);
        expect(&ha, "steam", "OFF");
    }

    #[test]
    fn decodes_the_user_labelled_standard_course_with_its_captured_33_minute_estimate() {
        let ha = decode(&[STANDARD]);
        expect(&ha, "course", "Standard");
        expect(&ha, "remaining_time", "33");
        expect(&ha, "initial_time", "33");
        expect(&ha, "temperature", "40");
    }

    #[test]
    fn maps_the_isolated_standard_course_40c_to_60c_transition_made_in_the_app() {
        let ha = decode(&[STANDARD, TEMPERATURE_60]);
        expect(&ha, "temperature", "60");
        expect(&ha, "remaining_time", "55");
        expect(&ha, "initial_time", "55");
    }

    #[test]
    fn decodes_owner_labelled_live_rinse_spin_temperature_status_and_rinse_countdown() {
        let (ha, thinq, _) = make();
        emit(&thinq, LIVE_RINSE_2);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "spin", "High");
        expect(&ha, "temperature", "Off");
        expect(&ha, "rinse", "2");

        emit(&thinq, LIVE_RINSE_1);
        expect(&ha, "rinse", "1");
    }

    #[test]
    fn physical_power_on_changes_the_captured_d8_heartbeat_from_0_to_1() {
        let (ha, thinq, _) = make();
        emit(&thinq, HEARTBEAT_OFF);
        expect(&ha, "power", "OFF");
        emit(&thinq, HEARTBEAT_ON);
        expect(&ha, "power", "ON");
    }

    #[test]
    fn uses_the_current_record_in_a_real_eb_to_ec_power_off_transition() {
        let (ha, thinq, _) = make();
        emit(&thinq, POWERING_OFF);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Power off");

        emit(&thinq, OFF);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Power off");
    }

    #[test]
    fn unknown_changing_tail_bytes_do_not_corrupt_the_grounded_baseline() {
        let ha = decode(&[HISTORICAL_CURRENT]);
        expect(&ha, "status", "Standby");
        expect(&ha, "error", "OFF");
    }

    #[test]
    fn ignores_other_aabb_payload_shapes() {
        let (ha, thinq, _) = make();
        emit(&thinq, CURRENT);
        let before = properties(&ha);
        emit(&thinq, "aa083100240052bb");
        assert_eq!(properties(&ha), before);
    }

    #[test]
    fn start_sends_only_the_captured_read_only_status_query() {
        let (_, thinq, dev) = make();
        dev.start();
        assert_eq!(outbox(&thinq), [STATUS_REQUEST]);
    }

    #[test]
    fn power_off_reproduces_the_frame_captured_from_a_real_remote_power_off() {
        assert_eq!(sent_after(&[("power_off", "OFF")]), ["aa09f0240101009cbb"]);
    }

    #[test]
    fn remote_start_heartbeat_c8_is_off_and_c9_is_on() {
        let (ha, thinq, _) = make();
        emit(&thinq, "aa09207200c80058bb");
        expect(&ha, "remote_start", "OFF");
        emit(&thinq, "aa09207200c9005bbb");
        expect(&ha, "remote_start", "ON");
    }

    #[test]
    fn power_on_is_refused_because_no_on_command_was_ever_captured() {
        assert!(sent_after(&[("power", "ON")]).is_empty());
    }

    #[test]
    fn writing_an_unknown_property_emits_no_packet() {
        assert!(sent_after(&[("nonsense", "ON")]).is_empty());
    }

    #[test]
    fn decodes_a_real_start_with_1_rinse_extra_low_and_60c_into_detecting() {
        let ha = decode(&[DETECTING_RUN]);
        expect(&ha, "status", "Detecting");
        expect(&ha, "course", "Standard");
        expect(&ha, "temperature", "60");
    }

    #[test]
    fn pause_reproduces_the_checksum_verified_frame_captured_mid_run() {
        assert_eq!(sent_after(&[("pause", "")]), ["aa09f02404010099bb"]);
    }

    #[test]
    fn resume_with_the_default_selection_reproduces_the_captured_standard_resume_frame() {
        assert_eq!(
            sent_after(&[("resume", "")]),
            ["aa1bf026070201040100002080000500000000000000000000dabb"]
        );
    }

    #[test]
    fn start_course_with_the_default_selection_reproduces_the_captured_standard_start_frame() {
        assert_eq!(
            sent_after(&[("start_course", "")]),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
    }

    #[test]
    fn smart_course_selection_and_start_reproduce_the_separately_captured_cold_wash_commands() {
        let (ha, thinq, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        assert_eq!(outbox(&thinq), [COLD_WASH_DOWNLOAD]);
        expect(&ha, "smart_course_select", "Cold Wash");

        thinq.reset_recorder();
        dev.set_property("reserve_hours", "19");
        dev.set_property("start_course", "");
        assert_eq!(outbox(&thinq), [COLD_WASH_RESERVED_START]);
        expect(&ha, "smart_course", "Cold Wash");
    }

    #[test]
    fn every_captured_smart_course_can_be_selected_downloaded_and_started() {
        for (label, download, start) in [
            (
                "Cold Wash",
                "aa1df02503150e0204010300000000000f330000000000000000001bbb",
                "aa1bf0260e0204010300002000200f3300000000000000000020bb",
            ),
            (
                "Small Load",
                "aa1df02503150e020300020000108000043400000000000000000084bb",
                "aa1bf0260e0203000200003080200434000000000000000000adbb",
            ),
            (
                "Skin Care",
                "aa1df02503150e02040304000000800005350000000000000000009cbb",
                "aa1bf0260e0204030400002080200535000000000000000000a5bb",
            ),
            (
                "Rainy Day",
                "aa1df02503150e02050302000000800005360000000000000000009cbb",
                "aa1bf0260e0205030200002080200536000000000000000000a5bb",
            ),
            (
                "Sweat Stain",
                "aa1df02503150e0303030300000000000e3700000000000000000006bb",
                "aa1bf0260e0303030300002000200e370000000000000000002fbb",
            ),
            (
                "Single Garments",
                "aa1df02503150e020300010000108000043800000000000000000081bb",
                "aa1bf0260e0203000100003080200438000000000000000000aebb",
            ),
            (
                "Kids Wear",
                "aa1df02503150e0303040400000000000e3900000000000000000002bb",
                "aa1bf0260e0303040400002000200e390000000000000000002bbb",
            ),
            (
                "Shirt",
                "aa1df02503150e0302040300000000000e3a00000000000000000003bb",
                "aa1bf0260e0302040300002000200e3a00000000000000000028bb",
            ),
            (
                "School Uniform",
                "aa1df02503150e020303020000008000053b00000000000000000099bb",
                "aa1bf0260e020303020000208020053b000000000000000000a6bb",
            ),
            (
                "Static Reduce",
                "aa1df02503150e020000000000100000013c00000000000000000004bb",
                "aa1bf0260e020000000000300020013c0000000000000000002dbb",
            ),
            (
                "Spin Only",
                "aa1df02503150e000300000000000000153f0000000000000000000cbb",
                "aa1bf0260e000300000000200020153f000000000000000000d5bb",
            ),
            (
                "Deodorization",
                "aa1df02503150e020000000000100000014100000000000000000003bb",
                "aa1bf0260e020000000000300020014100000000000000000028bb",
            ),
            (
                "Cloth Care",
                "aa1df02503150e020203030000000000084300000000000000000002bb",
                "aa1bf0260e02020303000020002008430000000000000000002bbb",
            ),
            (
                "Smart Rinse",
                "aa1df02503150e02040304000000800005440000000000000000008dbb",
                "aa1bf0260e0204030400002080200544000000000000000000aabb",
            ),
        ] {
            let (ha, thinq, dev) = make();
            dev.set_property("smart_course_select", label);
            assert_eq!(outbox(&thinq), [download], "{label}");
            expect(&ha, "smart_course_select", label);

            thinq.reset_recorder();
            dev.set_property("start_course", "");
            assert_eq!(outbox(&thinq), [start], "{label}");
            expect(&ha, "smart_course", label);
        }
    }

    #[test]
    fn smart_course_start_with_a_3_hour_reservation_reproduces_the_captured_cold_wash_frame() {
        let (_, thinq, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        dev.set_property("reserve_hours", "3");
        thinq.reset_recorder();
        dev.set_property("start_course", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf0260e0204010303002000200f330000000000000000002dbb"]
        );
    }

    #[test]
    fn smart_course_resume_reproduces_the_captured_cold_wash_resume_frame() {
        let (_, thinq, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        thinq.reset_recorder();
        dev.set_property("resume", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf0260e0204010300002000000f3300000000000000000000bb"]
        );
    }

    #[test]
    fn smart_course_resume_flips_the_start_flag_for_every_captured_course() {
        for (label, resume) in [
            (
                "Small Load",
                "aa1bf0260e02030002000030800004340000000000000000008dbb",
            ),
            (
                "Skin Care",
                "aa1bf0260e020403040000208000053500000000000000000085bb",
            ),
            (
                "Rainy Day",
                "aa1bf0260e020503020000208000053600000000000000000085bb",
            ),
            (
                "Sweat Stain",
                "aa1bf0260e0303030300002000000e370000000000000000000fbb",
            ),
            (
                "Single Garments",
                "aa1bf0260e02030001000030800004380000000000000000008ebb",
            ),
            (
                "Kids Wear",
                "aa1bf0260e0303040400002000000e390000000000000000000bbb",
            ),
            (
                "Shirt",
                "aa1bf0260e0302040300002000000e3a00000000000000000008bb",
            ),
            (
                "School Uniform",
                "aa1bf0260e020303020000208000053b00000000000000000086bb",
            ),
            (
                "Static Reduce",
                "aa1bf0260e020000000000300000013c0000000000000000000dbb",
            ),
            (
                "Spin Only",
                "aa1bf0260e000300000000200000153f00000000000000000035bb",
            ),
            (
                "Deodorization",
                "aa1bf0260e020000000000300000014100000000000000000008bb",
            ),
            (
                "Cloth Care",
                "aa1bf0260e02020303000020000008430000000000000000000bbb",
            ),
            (
                "Smart Rinse",
                "aa1bf0260e02040304000020800005440000000000000000008abb",
            ),
        ] {
            let (_, thinq, dev) = make();
            dev.set_property("smart_course_select", label);
            thinq.reset_recorder();
            dev.set_property("resume", "");
            assert_eq!(outbox(&thinq), [resume], "{label}");
        }
    }

    #[test]
    fn selecting_a_normal_course_after_a_smart_course_makes_normal_start_win_again() {
        let (_, thinq, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        thinq.reset_recorder();
        dev.set_property("course_select", "Standard");
        dev.set_property("start_course", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
    }

    #[test]
    fn downloaded_course_routes_start_course_to_the_pre_selected_smart_course() {
        let (ha, thinq, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        // Re-selecting the placeholder keeps the smart selection.
        dev.set_property("course_select", "Downloaded Course");
        expect(&ha, "course_select", "Downloaded Course");
        thinq.reset_recorder();
        dev.set_property("start_course", "");
        assert_eq!(outbox(&thinq), [COLD_WASH_START_NOW]);
    }

    #[test]
    fn selecting_a_smart_course_syncs_course_select_to_downloaded_course() {
        let (ha, _, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        expect(&ha, "course_select", "Downloaded Course");
    }

    #[test]
    fn cold_wash_start_with_no_reservation_preserves_the_captured_zero_reserve_byte() {
        let (_, thinq, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        thinq.reset_recorder();
        dev.set_property("start_course", "");
        assert_eq!(outbox(&thinq), [COLD_WASH_START_NOW]);
    }

    #[test]
    fn cold_wash_start_at_an_intermediate_reservation_changes_only_the_reserve_byte() {
        let (_, thinq, dev) = make();
        dev.set_property("smart_course_select", "Cold Wash");
        thinq.reset_recorder();
        dev.set_property("reserve_hours", "7");
        dev.set_property("start_course", "");
        assert_eq!(outbox(&thinq), [COLD_WASH_START_7H]);
    }

    #[test]
    fn unknown_smart_course_input_sends_nothing_and_does_not_take_over_the_normal_course() {
        let (_, thinq, dev) = make();
        dev.set_property("smart_course_select", "Not captured");
        assert!(thinq.outbox().is_empty());

        dev.set_property("start_course", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
    }

    #[test]
    fn course_select_then_start_course_reproduces_the_captured_19_hour_steam_refresh_start() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Steam Refresh"),
                ("reserve_hours", "19"),
                ("start_course", ""),
            ]),
            ["aa1bf02601020000001300300020010000000000000000000017bb"]
        );
    }

    #[test]
    fn reserve_hours_then_resume_reproduces_the_captured_5_hour_steam_refresh_resume() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Steam Refresh"),
                ("reserve_hours", "5"),
                ("resume", ""),
            ]),
            ["aa1bf02601020000000500300000010000000000000000000041bb"]
        );
    }

    #[test]
    fn course_select_with_an_unknown_course_does_not_change_the_selection() {
        // The default Standard still starts.
        assert_eq!(
            sent_after(&[("course_select", "Download"), ("start_course", "")]),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
    }

    #[test]
    fn reserve_hours_outside_the_model_declared_range_is_refused() {
        assert_eq!(
            sent_after(&[("reserve_hours", "20"), ("start_course", "")]),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
    }

    #[test]
    fn reserve_hours_of_1_or_2_is_refused_because_lg_declares_0_or_3_to_19() {
        let (_, thinq, dev) = make();
        dev.set_property("reserve_hours", "1");
        dev.set_property("start_course", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
        thinq.reset_recorder();
        dev.set_property("reserve_hours", "2");
        dev.set_property("start_course", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
        thinq.reset_recorder();
        // 3 is the lower bound of the declared range.
        dev.set_property("reserve_hours", "3");
        dev.set_property("start_course", "");
        assert_eq!(
            outbox(&thinq),
            ["aa1bf026070201040103002080200500000000000000000000e7bb"]
        );
    }

    #[test]
    fn decodes_the_captured_cold_wash_reservation_as_a_downloaded_course() {
        let ha = decode(&[COLD_WASH_RESERVED]);
        expect(&ha, "status", "Reserved");
        expect(&ha, "course", "Downloaded Course");
        expect(&ha, "temperature", "Cold");
        expect(&ha, "rinse", "3");
        expect(&ha, "reserve_time", "1140");
    }

    #[test]
    fn decodes_a_real_19_hour_steam_refresh_reservation() {
        let ha = decode(&[RESERVED_STEAM_REFRESH]);
        expect(&ha, "status", "Reserved");
        expect(&ha, "course", "Steam Refresh");
        expect(&ha, "reserve_time", "1140");
    }

    #[test]
    fn decodes_a_real_3_hour_reservation_with_40c_after_a_full_options_change() {
        let ha = decode(&[CUSTOM_OPTIONS_RUN]);
        expect(&ha, "temperature", "40");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn spin_temperature_and_rinse_writes_then_start_course_reproduce_the_captured_frame() {
        assert_eq!(
            sent_after(&[
                ("spin_select", "High"),
                ("temperature_select", "40"),
                ("rinse_count", "2"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf026070204030203002080200500000000000000000000e0bb"]
        );
    }

    #[test]
    fn options_are_refused_for_a_course_without_a_captured_options_frame() {
        // Tracked, but Steam Refresh keeps its template's bytes.
        assert_eq!(
            sent_after(&[
                ("course_select", "Steam Refresh"),
                ("spin_select", "High"),
                ("temperature_select", "40"),
                ("rinse_count", "2"),
                ("start_course", ""),
            ]),
            ["aa1bf0260102000000000030002001000000000000000000007abb"]
        );
    }

    #[test]
    fn an_unrecognised_spin_select_is_refused() {
        assert_eq!(
            sent_after(&[("spin_select", "Ultra Spin"), ("start_course", "")]),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
    }

    #[test]
    fn rinse_count_outside_the_captured_0_to_5_range_is_refused() {
        assert_eq!(
            sent_after(&[("rinse_count", "6"), ("start_course", "")]),
            ["aa1bf026070201040100002080200500000000000000000000fabb"]
        );
    }

    #[test]
    fn decodes_a_real_30c_run() {
        let ha = decode(&[THIRTY_C_RUN]);
        expect(&ha, "temperature", "30");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn temperature_select_30_then_start_course_reproduces_the_captured_30c_frame() {
        assert_eq!(
            sent_after(&[
                ("spin_select", "Low"),
                ("temperature_select", "30"),
                ("rinse_count", "3"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf026070202020303002080200500000000000000000000e6bb"]
        );
    }

    #[test]
    fn decodes_a_real_cold_water_run() {
        let ha = decode(&[COLD_WATER_RUN]);
        expect(&ha, "temperature", "Cold");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn temperature_select_cold_then_start_course_reproduces_the_captured_cold_water_frame() {
        assert_eq!(
            sent_after(&[
                ("spin_select", "Medium"),
                ("temperature_select", "Cold"),
                ("rinse_count", "4"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf026070203010403002080200500000000000000000000e1bb"]
        );
    }

    #[test]
    fn decodes_a_real_5_rinse_extra_high_spin_run() {
        let ha = decode(&[MAX_RINSE_SPIN_RUN]);
        expect(&ha, "temperature", "60");
        expect(&ha, "status", "Running");
    }

    #[test]
    fn extra_high_spin_and_5_rinses_then_start_course_reproduce_the_captured_frame() {
        assert_eq!(
            sent_after(&[
                ("spin_select", "Extra high"),
                ("temperature_select", "60"),
                ("rinse_count", "5"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf026070205040503002080200500000000000000000000efbb"]
        );
    }

    #[test]
    fn decodes_a_real_run_with_rinse_and_spin_off() {
        let ha = decode(&[OPTIONS_OFF_RUN]);
        expect(&ha, "temperature", "40");
        expect(&ha, "reserve_time", "240");
    }

    #[test]
    fn spin_off_and_no_rinse_then_start_course_reproduce_the_captured_frame() {
        assert_eq!(
            sent_after(&[
                ("spin_select", "Off"),
                ("temperature_select", "40"),
                ("rinse_count", "0"),
                ("reserve_hours", "4"),
                ("start_course", ""),
            ]),
            ["aa1bf026070200030004002080200500000000000000000000e5bb"]
        );
    }

    #[test]
    fn decodes_a_real_quiet_run() {
        let ha = decode(&[QUIET_RUN]);
        expect(&ha, "course", "Quiet");
        expect(&ha, "temperature", "40");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn quiet_with_rinse_spin_and_temperature_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Quiet"),
                ("spin_select", "Low"),
                ("temperature_select", "40"),
                ("rinse_count", "4"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260902020304030020002007000000000000000000006cbb"]
        );
    }

    #[test]
    fn decodes_a_second_real_quiet_run_with_other_options() {
        let ha = decode(&[QUIET_RUN_2]);
        expect(&ha, "course", "Quiet");
        expect(&ha, "temperature", "60");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn quiet_with_other_options_reproduces_the_second_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Quiet"),
                ("spin_select", "Extra low"),
                ("temperature_select", "60"),
                ("rinse_count", "1"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf02609020104010300200020070000000000000000000063bb"]
        );
    }

    #[test]
    fn decodes_a_real_speedwash_run_with_temperature_unselectable() {
        let ha = decode(&[SPEEDWASH_RUN]);
        expect(&ha, "course", "Speedwash");
        expect(&ha, "temperature", "Off");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn speedwash_with_rinse_and_spin_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Speedwash"),
                ("spin_select", "Medium"),
                ("rinse_count", "1"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf02608020300010300308020040000000000000000000095bb"]
        );
    }

    #[test]
    fn speedwash_ignores_temperature_because_it_was_never_captured_varying() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Speedwash"),
                ("temperature_select", "60"),
                ("spin_select", "Medium"),
                ("rinse_count", "1"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf02608020300010300308020040000000000000000000095bb"]
        );
    }

    #[test]
    fn decodes_a_real_colorcare_run() {
        let ha = decode(&[COLORCARE_RUN]);
        expect(&ha, "course", "Colorcare");
        expect(&ha, "temperature", "30");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn colorcare_with_rinse_spin_and_temperature_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Colorcare"),
                ("spin_select", "Low"),
                ("temperature_select", "30"),
                ("rinse_count", "3"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260a020202030300200020100000000000000000000014bb"]
        );
    }

    #[test]
    fn colorcare_ignores_60c_outside_its_cold_30_40_whitelist() {
        // The template's own 30 °C goes out.
        assert_eq!(
            sent_after(&[
                ("course_select", "Colorcare"),
                ("temperature_select", "60"),
                ("spin_select", "Low"),
                ("rinse_count", "3"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260a020202030300200020100000000000000000000014bb"]
        );
    }

    #[test]
    fn decodes_a_real_rinse_spin_run_with_temperature_fixed_off() {
        let ha = decode(&[RINSE_SPIN_RUN]);
        expect(&ha, "course", "Rinse+Spin");
        expect(&ha, "temperature", "Off");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn rinse_spin_with_rinse_and_spin_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Rinse+Spin"),
                ("spin_select", "Medium"),
                ("rinse_count", "1"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260d000300010300200020110000000000000000000015bb"]
        );
    }

    #[test]
    fn rinse_spin_ignores_no_rinse_and_spin_off_because_it_has_no_off_value() {
        // The template's own 1 rinse and Medium go out.
        assert_eq!(
            sent_after(&[
                ("course_select", "Rinse+Spin"),
                ("rinse_count", "0"),
                ("spin_select", "Off"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260d000300010300200020110000000000000000000015bb"]
        );
    }

    #[test]
    fn decodes_a_real_speedboil_run_with_a_new_95c_temperature_point() {
        let ha = decode(&[SPEEDBOIL_RUN]);
        expect(&ha, "course", "Speedboil");
        expect(&ha, "temperature", "95");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn speedboil_with_rinse_and_spin_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Speedboil"),
                ("spin_select", "Medium"),
                ("rinse_count", "3"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf026040203050303002000200d0000000000000000000069bb"]
        );
    }

    #[test]
    fn speedboil_ignores_temperature_because_95c_is_fixed() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Speedboil"),
                ("temperature_select", "30"),
                ("spin_select", "Medium"),
                ("rinse_count", "3"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf026040203050303002000200d0000000000000000000069bb"]
        );
    }

    #[test]
    fn decodes_a_real_babywear_run_with_temperature_fixed_off() {
        let ha = decode(&[BABYWEAR_RUN]);
        expect(&ha, "course", "Babywear");
        expect(&ha, "temperature", "Off");
        expect(&ha, "reserve_time", "240");
    }

    #[test]
    fn babywear_with_rinse_and_spin_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Babywear"),
                ("spin_select", "Medium"),
                ("rinse_count", "4"),
                ("reserve_hours", "4"),
                ("start_course", ""),
            ]),
            ["aa1bf026050203000404003000200b000000000000000000001dbb"]
        );
    }

    #[test]
    fn babywear_ignores_temperature_because_it_is_fixed_off() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Babywear"),
                ("temperature_select", "30"),
                ("spin_select", "Medium"),
                ("rinse_count", "4"),
                ("reserve_hours", "4"),
                ("start_course", ""),
            ]),
            ["aa1bf026050203000404003000200b000000000000000000001dbb"]
        );
    }

    #[test]
    fn decodes_a_real_allergy_care_run_with_temperature_fixed_off() {
        let ha = decode(&[ALLERGYCARE_RUN]);
        expect(&ha, "course", "Allergy Care");
        expect(&ha, "temperature", "Off");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn allergy_care_with_rinse_and_spin_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Allergy Care"),
                ("spin_select", "Medium"),
                ("rinse_count", "4"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260202030004030030002002000000000000000000006ebb"]
        );
    }

    #[test]
    fn allergy_care_ignores_temperature_because_it_is_fixed_off() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Allergy Care"),
                ("temperature_select", "30"),
                ("spin_select", "Medium"),
                ("rinse_count", "4"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260202030004030030002002000000000000000000006ebb"]
        );
    }

    #[test]
    fn decodes_a_real_heavy_duty_run() {
        let ha = decode(&[HEAVYDUTY_RUN]);
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "temperature", "60");
        expect(&ha, "reserve_time", "240");
    }

    #[test]
    fn heavy_duty_with_rinse_spin_and_temperature_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Heavy Duty"),
                ("spin_select", "Medium"),
                ("temperature_select", "60"),
                ("rinse_count", "3"),
                ("reserve_hours", "4"),
                ("start_course", ""),
            ]),
            ["aa1bf026060303040304002000200e0000000000000000000015bb"]
        );
    }

    #[test]
    fn heavy_duty_ignores_cold_outside_its_40_60_whitelist() {
        // The template's own 60 °C goes out.
        assert_eq!(
            sent_after(&[
                ("course_select", "Heavy Duty"),
                ("temperature_select", "Cold"),
                ("spin_select", "Medium"),
                ("rinse_count", "3"),
                ("reserve_hours", "4"),
                ("start_course", ""),
            ]),
            ["aa1bf026060303040304002000200e0000000000000000000015bb"]
        );
    }

    #[test]
    fn decodes_a_real_functional_wear_run_with_restricted_spin_options() {
        let ha = decode(&[FUNCTIONALWEAR_RUN]);
        expect(&ha, "course", "Functional Wear");
        expect(&ha, "temperature", "Off");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn functional_wear_with_rinse_and_spin_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Functional Wear"),
                ("spin_select", "Extra low"),
                ("rinse_count", "3"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260302010003030030002003000000000000000000006fbb"]
        );
    }

    #[test]
    fn functional_wear_ignores_medium_spin_and_any_temperature() {
        // The template's own Extra low and Off go out.
        assert_eq!(
            sent_after(&[
                ("course_select", "Functional Wear"),
                ("spin_select", "Medium"),
                ("temperature_select", "30"),
                ("rinse_count", "3"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260302010003030030002003000000000000000000006fbb"]
        );
    }

    #[test]
    fn decodes_a_real_duvet_run_with_restricted_spin_and_temperature_options() {
        let ha = decode(&[DUVET_RUN]);
        expect(&ha, "course", "Duvet");
        expect(&ha, "temperature", "Cold");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn duvet_with_rinse_spin_and_temperature_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Duvet"),
                ("spin_select", "Medium"),
                ("temperature_select", "Cold"),
                ("rinse_count", "4"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260b020301040300200020090000000000000000000069bb"]
        );
    }

    #[test]
    fn duvet_ignores_high_spin_and_60c_outside_its_whitelists() {
        // The template's own Medium and Cold go out.
        assert_eq!(
            sent_after(&[
                ("course_select", "Duvet"),
                ("spin_select", "High"),
                ("temperature_select", "60"),
                ("rinse_count", "4"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260b020301040300200020090000000000000000000069bb"]
        );
    }

    #[test]
    fn decodes_a_real_lingerie_wool_run_with_restricted_spin_and_temperature_options() {
        let ha = decode(&[LINGERIE_WOOL_RUN]);
        expect(&ha, "course", "Lingerie/Wool");
        expect(&ha, "temperature", "Cold");
        expect(&ha, "reserve_time", "240");
    }

    #[test]
    fn lingerie_wool_with_rinse_spin_and_temperature_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Lingerie/Wool"),
                ("spin_select", "Low"),
                ("temperature_select", "Cold"),
                ("rinse_count", "3"),
                ("reserve_hours", "4"),
                ("start_course", ""),
            ]),
            ["aa1bf0260c02020103040020002008000000000000000000006ebb"]
        );
    }

    #[test]
    fn lingerie_wool_ignores_medium_spin_and_60c_outside_its_whitelists() {
        // The modelJSON allows spin 0-2 only and temperature Cold to 40 °C.
        assert_eq!(
            sent_after(&[
                ("course_select", "Lingerie/Wool"),
                ("spin_select", "Medium"),
                ("temperature_select", "60"),
                ("rinse_count", "3"),
                ("reserve_hours", "4"),
                ("start_course", ""),
            ]),
            ["aa1bf0260c02020103040020002008000000000000000000006ebb"]
        );
    }

    #[test]
    fn decodes_a_real_tub_clean_run_with_all_options_fixed() {
        let ha = decode(&[TUB_CLEAN_RUN]);
        expect(&ha, "course", "Tub Clean");
        expect(&ha, "temperature", "60");
        expect(&ha, "reserve_time", "180");
    }

    #[test]
    fn tub_clean_reproduces_the_captured_start_frame() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Tub Clean"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260f0203040203002000200a0000000000000000000017bb"]
        );
    }

    #[test]
    fn tub_clean_ignores_rinse_spin_and_temperature_because_all_are_fixed() {
        assert_eq!(
            sent_after(&[
                ("course_select", "Tub Clean"),
                ("rinse_count", "5"),
                ("spin_select", "Extra high"),
                ("temperature_select", "Cold"),
                ("reserve_hours", "3"),
                ("start_course", ""),
            ]),
            ["aa1bf0260f0203040203002000200a0000000000000000000017bb"]
        );
    }

    #[test]
    fn publishes_this_cycle_energy_in_wh_from_detailed_telemetry_frames() {
        let (ha, thinq, _) = make();
        assert_eq!(
            components(&ha)["energy"],
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-energy",
                "state_topic": "$this/energy",
                "name": "Energy",
                "device_class": "energy",
                "state_class": "total_increasing",
                "unit_of_measurement": "Wh",
                "icon": "mdi:lightning-bolt",
            })
        );
        emit(&thinq, ENERGY_BD_80);
        expect(&ha, "energy", "80");
        emit(&thinq, ENERGY_CD_218);
        expect(&ha, "energy", "218");
        emit(&thinq, ENERGY_BD_221);
        expect(&ha, "energy", "221");
    }

    #[test]
    fn leaves_energy_untouched_on_frames_without_a_validated_energy_layout() {
        let (ha, thinq, _) = make();
        emit(&thinq, ENERGY_BD_80);
        expect(&ha, "energy", "80");
        emit(&thinq, UNRELATED_E2);
        expect(&ha, "energy", "80");
    }

    #[test]
    fn course_select_offers_the_templated_courses_in_course_order_then_downloaded_course() {
        let (ha, _, _) = make();
        assert_eq!(
            components(&ha)["course_select"]["options"],
            json!([
                "Steam Refresh",
                "Allergy Care",
                "Functional Wear",
                "Speedboil",
                "Babywear",
                "Heavy Duty",
                "Standard",
                "Speedwash",
                "Quiet",
                "Colorcare",
                "Duvet",
                "Lingerie/Wool",
                "Rinse+Spin",
                "Tub Clean",
                "Downloaded Course",
            ])
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("F24VDD").is_some());
    }
}
