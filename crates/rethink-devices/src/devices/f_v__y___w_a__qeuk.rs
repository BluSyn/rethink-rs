//! F_V__Y___W.A__QEUK — LG WV9-1409W front-load washer (Australia), AABB.
//!
//! `20 EC` carries the previous and current 30-byte records; `20 EB` carries one, the answer to the
//! status request sent on connecting. The record follows the WV5-1275W layout (upstream PR #222),
//! not F_C's: the course is rec[7] and rec[12] is the rinse. Settings chosen in HA are held until
//! Start, which sends them all in one `F0 26`; every command is the one the LG app sends. No
//! `auto_ack`: only the frame types the LG cloud acks are acked, never the BD/CD dumps. Port of
//! upstream PR #286.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use crate::washer_common::{
    error_options, fy_refresh_name, soil_level, spin_options, spin_value, temperature_value,
    washer_status_options, ERRORS, FY_REFRESH, SOIL_LABELS, SPIN_LABELS, TEMPERATURE_LABELS,
};
use rethink_core::device_base::{default_config, process_common_status, wrap_aabb, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const RECORD_LEN: usize = 30;

// The frames the LG cloud acks, as F0 00 <type> 04: pushes (72), the drum-clean count (D8), end of
// cycle (E2), the wake response (2B) and the hello (31). Not the status records, command results
// or BD/CD dumps.
const ACKED_TYPES: &[u8] = &[0x72, 0xd8, 0xe2, 0x2b, 0x31];

const PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];
const POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
const WAKE_UP: &[u8] = &[0xf0, 0x2a, 0x01, 0x00];
// F0 26's flags byte: 03 starts, 02 resumes a paused programme; Wrinkle care adds 0x20, as in
// rec[17].
const START: u8 = 0x03;
const RESUME: u8 = 0x02;
const WRINKLE_CARE: u8 = 0x20;
const STATE_PAUSED: u8 = 2;

const RINSES: &[&str] = &[
    "Not selected",
    "Normal",
    "Rinse+",
    "Rinse++",
    "Normal + Hold",
    "Rinse+ + Hold",
];

// The downloaded courses, as the other washers' smart course table.
const SMART_COURSES: &[(u8, &str)] = &[
    (0x34, "Kids Wear"),
    (0x35, "School Uniform"),
    (0x36, "Swimming Wear"),
    (0x37, "Rainy Season"),
    (0x38, "Gym Clothes"),
    (0x39, "Jeans"),
    (0x3a, "Blanket"),
    (0x3b, "Sweat Stain"),
    (0x3e, "Single Garment"),
    (0x3f, "Colour Protection"),
    (0x40, "Noise Minimize"),
    (0x47, "Baby Care"),
    (0x48, "Hygiene"),
    (0x49, "Small Load"),
    (0x4a, "Lingerie"),
    (0x4b, "Wool"),
    (0x4c, "Skin Care"),
    (0x4d, "Cold Wash"),
    (0x64, "Rinse + Spin"),
    (0x65, "Lightly Soiled Items"),
    (0x66, "Minimize Detergent Residue"),
    (0x67, "1hr. Wash"),
    (0x6b, "Sleeve Hems and Collars"),
    (0x6c, "Juice and Food Stains"),
    (0x6f, "Minimize Wrinkles"),
    (0x71, "Quick Tub Clean"),
    (0x79, "Drain"),
    (0x7a, "Spin"),
    (0x84, "Silent Wash"),
    (0x86, "MicroPlastic Care"),
];

/// A setting's default, and the values the LG app lets you choose instead (none: shown, not
/// changeable).
#[derive(Clone, Copy)]
struct Setting {
    initial: u8,
    choices: &'static [u8],
}

const fn choice(initial: u8, choices: &'static [u8]) -> Setting {
    Setting { initial, choices }
}

const fn fixed(initial: u8) -> Setting {
    Setting {
        initial,
        choices: &[],
    }
}

/// A course (rec[7]), named as on the panel and in the LG app, with its defaults and the choices
/// the LG app offers, from this model's modelJson (Course).
struct Programme {
    course: u8,
    name: &'static str,
    soil: u8,
    temp: Setting,
    spin: Setting,
    rinse: Setting,
    options: u8,
    delay: bool,
}

// Spin 255 is the app's "Max", which the washer reports as its top speed.
const PROGRAMMES: &[Programme] = &[
    Programme {
        course: 0x01,
        name: "Cotton",
        soil: 3,
        temp: choice(4, &[1, 2, 3, 4, 6, 7]),
        spin: choice(255, &[1, 2, 5, 7, 255]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x02,
        name: "Easy Care",
        soil: 3,
        temp: choice(4, &[1, 2, 3, 4, 6]),
        spin: choice(255, &[1, 2, 5, 7, 255]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x04,
        name: "Cotton Eco",
        soil: 3,
        temp: choice(6, &[1, 2, 3, 4, 6]),
        spin: choice(255, &[1, 2, 5, 7, 255]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x05,
        name: "Duvet",
        soil: 3,
        temp: choice(1, &[1, 2, 3, 4]),
        spin: choice(7, &[1, 2, 5, 7]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x07,
        name: "Mixed Fabric",
        soil: 3,
        temp: choice(4, &[1, 2, 3, 4, 6]),
        spin: choice(7, &[1, 2, 5, 7, 255]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x08,
        name: "Sportswear",
        soil: 3,
        temp: choice(4, &[1, 2, 3, 4]),
        spin: choice(5, &[1, 2, 5]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x09,
        name: "Quiet Wash",
        soil: 3,
        temp: choice(4, &[1, 2, 3, 4, 6]),
        spin: choice(5, &[1, 2, 5, 7]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x0c,
        name: "Quick 14",
        soil: 3,
        temp: choice(2, &[2, 3, 4]),
        spin: choice(2, &[1, 2, 5, 7, 255]),
        rinse: choice(1, &[1, 2]),
        options: 0x01,
        delay: true,
    },
    Programme {
        course: 0x12,
        name: "Tub Clean",
        soil: 3,
        temp: fixed(6),
        spin: fixed(1),
        rinse: fixed(1),
        options: 0x00,
        delay: false,
    },
    Programme {
        course: 0x1b,
        name: "Hand/Wool",
        soil: 3,
        temp: choice(3, &[1, 2, 3, 4]),
        spin: choice(5, &[1, 2, 5]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x20,
        name: "Delicates",
        soil: 3,
        temp: choice(2, &[1, 2, 3, 4]),
        spin: choice(5, &[1, 2, 5]),
        rinse: choice(1, &[1, 2]),
        options: 0x00,
        delay: true,
    },
    Programme {
        course: 0x2d,
        name: "Allergy Care",
        soil: 3,
        temp: fixed(6),
        spin: choice(255, &[1, 2, 5, 7, 255]),
        rinse: choice(1, &[1, 2]),
        options: 0x80,
        delay: true,
    },
    Programme {
        course: 0x31,
        name: "TurboClean 39",
        soil: 3,
        temp: choice(4, &[1, 2, 3, 4, 6]),
        spin: choice(9, &[2, 5, 7, 9, 255]),
        rinse: choice(1, &[1, 2]),
        options: 0x01,
        delay: true,
    },
];

fn programme(course: u8) -> Option<&'static Programme> {
    PROGRAMMES.iter().find(|p| p.course == course)
}

fn smart_course_name(code: u8) -> Option<&'static str> {
    SMART_COURSES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn rinse_name(code: u8) -> Option<&'static str> {
    RINSES.get(usize::from(code)).copied()
}

fn error_name(code: u8) -> Option<&'static str> {
    ERRORS.get(usize::from(code)).copied().flatten()
}

fn position(labels: &[&str], label: &str) -> Option<u8> {
    labels
        .iter()
        .position(|l| *l == label)
        .and_then(|i| u8::try_from(i).ok())
}

fn temperature_code(label: &str) -> Option<u8> {
    position(TEMPERATURE_LABELS, label)
}

fn spin_code(label: &str) -> Option<u8> {
    if label == "Max" {
        Some(255)
    } else {
        position(SPIN_LABELS, label)
    }
}

fn rinse_code(label: &str) -> Option<u8> {
    position(RINSES, label)
}

/// `code`, if the LG app offers it for the course about to start.
fn offered(code: Option<u8>, setting: Option<Setting>) -> Option<u8> {
    code.filter(|c| setting.is_some_and(|s| s.choices.contains(c)))
}

fn minutes(hour: u8, minute: u8) -> PropertyValue {
    (i64::from(hour) * 60 + i64::from(minute)).into()
}

fn label(name: Option<&'static str>) -> PropertyValue {
    name.unwrap_or("None").into()
}

/// The fields of an F0 26 command, as wire codes.
#[derive(Clone, Copy)]
struct Run {
    course: u8,
    soil: u8,
    spin: u8,
    temp: u8,
    rinse: u8,
    delay: [u8; 2],
    options: u8,
    wrinkle_care: u8,
}

impl Run {
    /// The washer's current settings, as an F0 26 would carry them.
    fn current(r: &[u8]) -> Self {
        Self {
            course: r[7],
            soil: r[9],
            spin: r[10],
            temp: r[11],
            rinse: r[12],
            delay: [r[14], r[15]],
            options: r[16],
            wrinkle_care: r[17] & WRINKLE_CARE,
        }
    }

    /// A course picked in HA starts from its defaults, as in the LG app.
    fn defaults(p: &Programme) -> Self {
        Self {
            course: p.course,
            soil: p.soil,
            spin: p.spin.initial,
            temp: p.temp.initial,
            rinse: p.rinse.initial,
            delay: [0, 0],
            options: p.options,
            wrinkle_care: 0,
        }
    }

    /// F0 26 <course> <soil> <spin> <temp> <rinse> 00 <delay h> <delay min> 00 <options> <flags>
    /// 00 00 00 00 00, as the LG app sends it (modelJson's WMStart in the same order).
    fn command(&self, flags: u8) -> [u8; 18] {
        let [delay_hours, delay_minutes] = self.delay;
        [
            0xf0,
            0x26,
            self.course,
            self.soil,
            self.spin,
            self.temp,
            self.rinse,
            0,
            delay_hours,
            delay_minutes,
            0,
            self.options,
            flags | self.wrinkle_care,
            0,
            0,
            0,
            0,
            0,
        ]
    }
}

#[derive(Clone, Copy, Default)]
struct Pending {
    course: Option<u8>,
    temp: Option<u8>,
    spin: Option<u8>,
    rinse: Option<u8>,
    delay: Option<u8>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    // The latest status record from this connection; Start builds its command from it.
    record: Mutex<Option<Vec<u8>>>,
    // Choices made in HA and not yet sent; the F0 26 command carries them all at once.
    pending: Mutex<Pending>,
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
            record: Mutex::new(None),
            pending: Mutex::new(Pending::default()),
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
        if buf.len() < 2 || buf[0] != CLASS_BYTE {
            return;
        }
        if ACKED_TYPES.contains(&buf[1]) {
            self.core
                .thinq
                .send_ack(&wrap_aabb(&[0xf0, 0x00, buf[1], 0x04]));
        }

        // 20 00 <command> <result>: the washer's answer to a command, 00 meaning done.
        if buf[1] == 0x00 && buf.len() == 4 {
            if buf[3] != 0 {
                tracing::debug!(
                    id = %self.core.id,
                    "command {:x} refused: {:x}",
                    buf[2],
                    buf[3]
                );
            }
            return;
        }

        // 20 D8 nn: washes since the last Tub Clean (LG's TCLCount). At some power-ons it comes as
        // 00, then the count.
        if buf[1] == 0xd8 && buf.len() == 3 {
            self.core
                .publish_property("tub_clean_count", i64::from(buf[2]).into());
            return;
        }

        if let Some(rec) = process_common_status(buf, CLASS_BYTE, RECORD_LEN) {
            self.process_record(rec);
        }
    }

    fn process_record(&self, rec: &[u8]) {
        if rec[0] != 0x00 || rec[1] != 0x1c {
            return;
        }
        let state = rec[2];

        self.core.publish_on_off("power", state != 0);
        self.core
            .publish_property("status", fy_refresh_name(state).into());
        self.core
            .publish_property("error_message", label(error_name(rec[8])));
        self.core.publish_on_off("error", rec[8] != 0);
        // rec[22] is the downloaded course while Download Cycle is selected, else 0.
        let course = if rec[22] != 0 {
            smart_course_name(rec[22])
        } else {
            programme(rec[7]).map(|p| p.name)
        };
        self.core.publish_property("course", label(course));
        self.core
            .publish_property("downloaded_course", label(smart_course_name(rec[25])));
        self.core
            .publish_property("soil", label(soil_level(rec[9].into())));
        self.core
            .publish_property("spin", label(spin_value(rec[10].into())));
        self.core
            .publish_property("temp", label(temperature_value(rec[11].into())));
        self.core
            .publish_property("rinse", label(rinse_name(rec[12])));
        self.core
            .publish_property("remaining_time", minutes(rec[3], rec[4]));
        self.core
            .publish_property("initial_time", minutes(rec[5], rec[6]));
        self.core
            .publish_property("delay_end", minutes(rec[14], rec[15]));
        self.core.publish_on_off("steam", rec[16] & 0x80 != 0);
        self.core.publish_on_off("pre_wash", rec[16] & 0x40 != 0);
        self.core.publish_on_off("turbo_wash", rec[16] & 0x01 != 0);
        self.core.publish_on_off("child_lock", rec[17] & 0x80 != 0);
        self.core.publish_on_off("door_lock", rec[17] & 0x40 != 0);
        self.core
            .publish_on_off("wrinkle_care", rec[17] & WRINKLE_CARE != 0);
        // Also ON for the whole of a running wash, as the LG cloud reports it.
        self.core
            .publish_on_off("remote_start", rec[17] & 0x02 != 0);
        self.core.publish_on_off("standby", rec[29] & 0x01 != 0);
        self.core
            .publish_property("tub_clean_count", i64::from(rec[23]).into());

        *self.record.lock() = Some(rec.to_vec());
        self.publish_settings();
    }

    /// What Start would send: a course picked in HA, else the washer's own settings, with the
    /// choices made in HA. None while there is no course to start.
    fn settings(&self) -> Option<(Run, &'static Programme)> {
        let r = self.record.lock().clone()?;
        // A downloaded course is not startable here.
        if r[22] != 0 {
            return None;
        }
        let pending = *self.pending.lock();
        let course = pending.course.unwrap_or(r[7]);
        let programme = programme(course)?;
        let base = if course == r[7] {
            Run::current(&r)
        } else {
            Run::defaults(programme)
        };
        let run = Run {
            temp: pending.temp.unwrap_or(base.temp),
            spin: pending.spin.unwrap_or(base.spin),
            rinse: pending.rinse.unwrap_or(base.rinse),
            delay: pending.delay.map_or(base.delay, |hours| [hours, 0]),
            ..base
        };
        Some((run, programme))
    }

    fn publish_settings(&self) {
        let s = self.settings().map(|(run, _)| run);
        self.core.publish_property(
            "set_course",
            label(s.and_then(|s| programme(s.course)).map(|p| p.name)),
        );
        self.core.publish_property(
            "set_temp",
            label(s.and_then(|s| temperature_value(s.temp.into()))),
        );
        self.core
            .publish_property("set_spin", label(s.and_then(|s| spin_value(s.spin.into()))));
        self.core
            .publish_property("set_rinse", label(s.and_then(|s| rinse_name(s.rinse))));
        self.core.publish_property(
            "set_delay_end",
            s.map_or("None".into(), |s| i64::from(s.delay[0]).into()),
        );
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        let programme = self.settings().map(|(_, p)| p);
        match prop {
            "set_course" => {
                if let Some(p) = PROGRAMMES.iter().find(|p| p.name == value) {
                    // A new course brings its own defaults.
                    let mut pending = self.pending.lock();
                    *pending = Pending {
                        course: Some(p.course),
                        delay: pending.delay.filter(|&hours| hours != 0 && p.delay),
                        ..Pending::default()
                    };
                }
            }
            // Only what the app offers for this course.
            "set_temp" => {
                if let Some(code) = offered(temperature_code(value), programme.map(|p| p.temp)) {
                    self.pending.lock().temp = Some(code);
                }
            }
            "set_spin" => {
                if let Some(code) = offered(spin_code(value), programme.map(|p| p.spin)) {
                    self.pending.lock().spin = Some(code);
                }
            }
            "set_rinse" => {
                if let Some(code) = offered(rinse_code(value), programme.map(|p| p.rinse)) {
                    self.pending.lock().rinse = Some(code);
                }
            }
            "set_delay_end" => {
                // 0 is none; modelJson's reserveTimeHour allows 3..19.
                if let Ok(hours) = value.trim().parse::<f64>() {
                    let allowed = hours == 0.0
                        || ((3.0..=19.0).contains(&hours) && programme.is_some_and(|p| p.delay));
                    if hours.fract() == 0.0 && allowed {
                        self.pending.lock().delay = Some(hours as u8);
                    }
                }
            }
            _ => {
                self.command(prop);
                return;
            }
        }
        self.publish_settings();
    }

    // Sent whatever the washer's state: it accepts or refuses (logged in process_aabb). Only a
    // command that can't be built isn't sent.
    fn command(&self, prop: &str) {
        match prop {
            "start" => {
                let record = self.record.lock().clone();
                match record {
                    Some(r) if r[2] == STATE_PAUSED => {
                        // Resume with the washer's own settings. A downloaded course's resume was
                        // never captured, so its bytes aren't known.
                        if r[22] != 0 {
                            tracing::debug!(
                                id = %self.core.id,
                                "start not sent: resuming a downloaded course"
                            );
                            return;
                        }
                        self.core.send(&Run::current(&r).command(RESUME));
                    }
                    _ => {
                        let Some((run, _)) = self.settings() else {
                            tracing::debug!(
                                id = %self.core.id,
                                "start not sent: no course on the washer or chosen"
                            );
                            return;
                        };
                        self.core.send(&run.command(START));
                    }
                }
                // HA's choices are used up; the settings follow the washer again.
                *self.pending.lock() = Pending::default();
                self.publish_settings();
            }
            "pause" => self.core.send(PAUSE),
            "power_off" => self.core.send(POWER_OFF),
            "wake_up" => self.core.send(WAKE_UP),
            _ => {}
        }
    }
}

/// The labels of every code the LG app offers for a setting in any course, plus `extra`, in code
/// order.
fn offered_labels(
    setting: fn(&Programme) -> Setting,
    extra: &[u8],
    name: fn(u8) -> Option<&'static str>,
) -> Vec<&'static str> {
    let mut codes: Vec<u8> = PROGRAMMES
        .iter()
        .map(setting)
        .flat_map(|s| std::iter::once(s.initial).chain(s.choices.iter().copied()))
        .chain(extra.iter().copied())
        .collect();
    codes.sort_unstable();
    codes.dedup();
    codes.into_iter().filter_map(name).collect()
}

fn with(mut component: Value, extra: Value) -> Value {
    if let (Value::Object(component), Value::Object(extra)) = (&mut component, extra) {
        component.extend(extra);
    }
    component
}

fn binary(key: &str, name: &str, icon: &str) -> Value {
    json!({
        "platform": "binary_sensor",
        "unique_id": format!("$deviceid-{key}"),
        "state_topic": format!("$this/{key}"),
        "name": name,
        "icon": icon,
    })
}

fn enum_sensor(key: &str, name: &str, icon: &str, options: &[&str]) -> Value {
    json!({
        "platform": "sensor",
        "unique_id": format!("$deviceid-{key}"),
        "state_topic": format!("$this/{key}"),
        "name": name,
        "icon": icon,
        "device_class": "enum",
        "options": options,
    })
}

fn duration(key: &str, name: &str, icon: &str) -> Value {
    json!({
        "platform": "sensor",
        "unique_id": format!("$deviceid-{key}"),
        "state_topic": format!("$this/{key}"),
        "name": name,
        "icon": icon,
        "device_class": "duration",
        "unit_of_measurement": "min",
    })
}

fn select(key: &str, name: &str, icon: &str, options: &[&str]) -> Value {
    json!({
        "platform": "select",
        "unique_id": format!("$deviceid-{key}"),
        "state_topic": format!("$this/{key}"),
        "command_topic": format!("$this/{key}/set"),
        "name": name,
        "icon": icon,
        "options": options,
    })
}

/// Greyed out in HA unless the washer reports `available_when` ON, as the LG app hides the command.
fn button(key: &str, name: &str, icon: &str, available_when: Option<&str>) -> Value {
    let mut button = json!({
        "platform": "button",
        "unique_id": format!("$deviceid-{key}"),
        "command_topic": format!("$this/{key}/set"),
        "payload_press": "",
        "name": name,
        "icon": icon,
    });
    if let Some(topic) = available_when {
        button["availability"] = json!([
            {"topic": "$this/availability"},
            {"topic": "$rethink/availability"},
            {
                "topic": format!("$this/{topic}"),
                "payload_available": "ON",
                "payload_not_available": "OFF",
            },
        ]);
        button["availability_mode"] = json!("all");
    }
    button
}

fn components() -> HashMap<String, Value> {
    let programmes: Vec<&str> = PROGRAMMES.iter().map(|p| p.name).collect();
    let smart_courses: Vec<&str> = SMART_COURSES.iter().map(|(_, name)| *name).collect();
    let mut courses = programmes.clone();
    for &name in &smart_courses {
        if !courses.contains(&name) {
            courses.push(name);
        }
    }
    let diagnostic = json!({"entity_category": "diagnostic"});
    [
        (
            "power",
            with(
                binary("power", "Power", "mdi:washing-machine"),
                json!({"device_class": "power"}),
            ),
        ),
        (
            "status",
            enum_sensor(
                "status",
                "Status",
                "mdi:state-machine",
                &washer_status_options(FY_REFRESH),
            ),
        ),
        (
            "error",
            with(
                binary("error", "Error", "mdi:check-circle"),
                json!({"device_class": "problem", "entity_category": "diagnostic"}),
            ),
        ),
        (
            "error_message",
            with(
                enum_sensor(
                    "error_message",
                    "Error message",
                    "mdi:alert-circle-outline",
                    &error_options(),
                ),
                diagnostic.clone(),
            ),
        ),
        (
            "course",
            enum_sensor("course", "Course", "mdi:pin-outline", &courses),
        ),
        (
            "downloaded_course",
            enum_sensor(
                "downloaded_course",
                "Downloaded course",
                "mdi:download",
                &smart_courses,
            ),
        ),
        (
            "temp",
            enum_sensor("temp", "Temperature", "mdi:thermometer", TEMPERATURE_LABELS),
        ),
        (
            "spin",
            enum_sensor("spin", "Spin", "mdi:autorenew", &spin_options()),
        ),
        (
            "rinse",
            enum_sensor("rinse", "Rinse", "mdi:water-sync", RINSES),
        ),
        (
            "soil",
            enum_sensor("soil", "Soil level", "mdi:liquid-spot", SOIL_LABELS),
        ),
        (
            "remaining_time",
            duration("remaining_time", "Remaining time", "mdi:timer-outline"),
        ),
        (
            "initial_time",
            duration("initial_time", "Initial time", "mdi:timer-sand"),
        ),
        (
            "delay_end",
            duration("delay_end", "Delay end", "mdi:timer-sand"),
        ),
        ("steam", binary("steam", "Steam", "mdi:kettle-steam")),
        (
            "turbo_wash",
            binary("turbo_wash", "TurboWash", "mdi:rocket-launch"),
        ),
        ("pre_wash", binary("pre_wash", "Pre-wash", "mdi:water-sync")),
        (
            "wrinkle_care",
            binary("wrinkle_care", "Wrinkle care", "mdi:tshirt-crew-outline"),
        ),
        (
            "remote_start",
            binary("remote_start", "Remote start", "mdi:play-circle-outline"),
        ),
        ("standby", binary("standby", "Standby", "mdi:sleep")),
        // Plain on/off, ON = locked, as the LG cloud reports them. Not HA's lock class, where ON
        // is unlocked.
        ("door_lock", binary("door_lock", "Door lock", "mdi:lock")),
        (
            "child_lock",
            with(binary("child_lock", "Child lock", "mdi:lock"), diagnostic),
        ),
        (
            // No state_class: Tub Clean resets the count.
            "tub_clean_count",
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-tub_clean_count",
                "state_topic": "$this/tub_clean_count",
                "name": "Washes since drum clean",
                "icon": "mdi:washing-machine-alert",
                "entity_category": "diagnostic",
            }),
        ),
        // What the LG app can set before Start.
        (
            "set_course",
            select("set_course", "Set course", "mdi:tune-vertical", &programmes),
        ),
        (
            "set_temp",
            select(
                "set_temp",
                "Set temperature",
                "mdi:thermometer",
                &offered_labels(|p| p.temp, &[], |c| temperature_value(c.into())),
            ),
        ),
        (
            // Plus 1400 rpm, which the washer reports for "Max".
            "set_spin",
            select(
                "set_spin",
                "Set spin",
                "mdi:autorenew",
                &offered_labels(|p| p.spin, &[10], |c| spin_value(c.into())),
            ),
        ),
        (
            "set_rinse",
            select(
                "set_rinse",
                "Set rinse",
                "mdi:water-sync",
                &offered_labels(|p| p.rinse, &[], rinse_name),
            ),
        ),
        (
            // 0 is none; modelJson's reserveTimeHour allows 3..19.
            "set_delay_end",
            json!({
                "platform": "number",
                "unique_id": "$deviceid-set_delay_end",
                "state_topic": "$this/set_delay_end",
                "command_topic": "$this/set_delay_end/set",
                "name": "Set delayed end",
                "icon": "mdi:timer-sand",
                "unit_of_measurement": "h",
                "min": 0,
                "max": 19,
                "step": 1,
            }),
        ),
        // Start also resumes a paused programme, as on rethink's other washers.
        (
            "start",
            button(
                "start",
                "Start",
                "mdi:play-circle-outline",
                Some("remote_start"),
            ),
        ),
        (
            "pause",
            button("pause", "Pause", "mdi:pause-circle-outline", None),
        ),
        // A washer that is off can't be powered on remotely, nor off again.
        (
            "power_off",
            button("power_off", "Power off", "mdi:power-off", Some("power")),
        ),
        // Wake up is for a sleeping washer only, as the app's "Sleep Mode OFF".
        (
            "wake_up",
            button("wake_up", "Wake up", "mdi:sleep-off", Some("standby")),
        ),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    // Ask for the state on connecting, as the LG cloud does, so a reconnect shows the washer at
    // once.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::DEVICE_ID;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const MODEL_ID: &str = "F_V__Y___W.A__QEUK";

    // All frames are real, captured from a WV9-1409W (firmware 2.10.71) bridged to the LG cloud,
    // and checked against the cloud's labels.

    // 19 Oct 2026, Ready, one setting changed at the panel per frame.
    const D8_ZERO: &str = "aa0720d800fcbb";
    const D8_36: &str = "aa0720d82498bb";
    const POWER_ON: &str =
        "aa4220ec001c00000000000000000000000000000000000000000024007a00000000001c01000000000000000000000000000000000000010024007a000000003bbb";
    const COTTON: &str =
        "aa4220ec001c01000000000000000000000000000000000000010024007a00000000001c01021502150100030a04010000000000010002010024007a00000400edbb";
    const COTTON_ECO: &str =
        "aa4220ec001c01021502150100030a04010000000000010002010024007a00000400001c01021202120400030a06010000000000000003010024007a00000200a8bb";
    const TURBOCLEAN_39: &str =
        "aa4220ec001c01021202120400030a06010000000000000003010024007a00000200001c01002700273100030904010000000100000001010024007a0000000012bb";
    const MIXED_FABRIC: &str =
        "aa4220ec001c01002700273100030904010000000100000001010024007a00000000001c01011501150700030704010000000000010002010024007a000003001fbb";
    const EASY_CARE: &str =
        "aa4220ec001c01011501150700030704010000000000010002010024007a00000300001c01020102010200030a04010000000000010002010024007a000003008dbb";
    const QUIET_WASH: &str =
        "aa4220ec001c01020102010200030a04010000000000010002010024007a00000300001c01021c021c0900030504010000000000000002010024007a00000500bcbb";
    const ALLERGY_CARE: &str =
        "aa4220ec001c01021c021c0900030504010000000000000002010024007a00000500001c01023002302d00030a06010000008000000004010024007a00000500a2bb";
    const DOWNLOAD_CYCLE: &str =
        "aa4220ec001c01023002302d00030a06010000008000000004010024007a00000500001c01000e000e1700000a00000000000000000001017a24007a0000000003bb";
    const TUB_CLEAN: &str =
        "aa4220ec001c01000e000e1700000a00000000000000000001017a24007a00000000001c01010c010c1200030106010000000000000003010024007a0000020031bb";
    const DUVET: &str =
        "aa4220ec001c01010c010c1200030106010000000000000003010024007a00000200001c01012101210500030701010000000000000001010024007a0000030050bb";
    const QUICK_14: &str =
        "aa4220ec001c01012101210500030701010000000000000001010024007a00000300001c01000e000e0c00030202010000000100000000010024007a00000100aebb";
    const SPORTSWEAR: &str =
        "aa4220ec001c01000e000e0c00030202010000000100000000010024007a00000100001c01003400340800030504010000000000000002010024007a0000020076bb";
    const HAND_WOOL: &str =
        "aa4220ec001c01003400340800030504010000000000000002010024007a00000200001c01002400241b00030503010000000000000001010024007a0000030031bb";
    const DELICATES: &str =
        "aa4220ec001c01002400241b00030503010000000000000001010024007a00000300001c01002d002d2000030502010000000000000001010024007a0000030039bb";
    const TEMP_60: &str =
        "aa4220ec001c01021502150100030a04010000000000010002010024007a00000400001c01022902290100030a06010000000000010004010024007a0000040079bb";
    const TEMP_95: &str =
        "aa4220ec001c01022902290100030a06010000000000010004010024007a00000400001c01023302330100030a07010000000000010004010024007a0000040038bb";
    const TEMP_COLD: &str =
        "aa4220ec001c01023302330100030a07010000000000010004010024007a00000400001c01033903390100030a01010000000000010002010024007a00000400ddbb";
    const TEMP_20: &str =
        "aa4220ec001c01033903390100030a01010000000000010002010024007a00000400001c01012901290100030a02010000000000010001010024007a000004003fbb";
    const TEMP_30: &str =
        "aa4220ec001c01012901290100030a02010000000000010001010024007a00000400001c01013301330100030a03010000000000010002010024007a0000040009bb";
    const SPIN_NO: &str =
        "aa4220ec001c01021502150100030a04010000000000010002010024007a00000400001c01020802080100030104010000000000010002010024007a0000040088bb";
    const SPIN_400: &str =
        "aa4220ec001c01020802080100030104010000000000010002010024007a00000400001c01020b020b0100030204010000000000010002010024007a0000040094bb";
    const SPIN_800: &str =
        "aa4220ec001c01020b020b0100030204010000000000010002010024007a00000400001c01020d020d0100030504010000000000010002010024007a000004009abb";
    const SPIN_1000: &str =
        "aa4220ec001c01020d020d0100030504010000000000010002010024007a00000400001c01021202120100030704010000000000010002010024007a00000400b7bb";
    const SPIN_1200: &str =
        "aa4220ec001c01021202120100030704010000000000010002010024007a00000400001c01021202120100030904010000000000010002010024007a00000400a5bb";
    const STEAM_ON: &str =
        "aa4220ec001c01021502150100030a04010000000000010002010024007a00000400001c01030003000100030a06010000008000010004010024007a0000040009bb";
    const TURBO_CLEAN_ON: &str =
        "aa4220ec001c01022902290100030a06010000000000010004010024007a00000400001c01020f020f0100030706010000000100010003010024007a0000040074bb";
    const PRE_WASH_ON: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c01023702370100030706010000004000010004010024007a00000500fcbb";
    const WRINKLE_CARE_ON: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c01030903090100030706010000000020010004010024007a000004007bbb";
    const RINSE_PLUS_ON: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c01023302330100030706020000000000010004010024007a0000050037bb";
    const INTENSIVE_ON: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c01031103110100040706010000000000010004010024007a000004004abb";
    const CHILD_LOCK_ON: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c01022602260100030706010000000080010004010024007a0000040093bb";
    const DELAY_3H: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c01022602260100030706010003000000010004010024007a000004001cbb";
    const DELAY_19H: &str =
        "aa4220ec001c01022602260100030706010012000000010004010024007a00000400001c01022602260100030706010013000000010004010024007a000004003ebb";
    const PUSH_REMOTE_START_ON: &str = "aa09207200c9005bbb";
    const REMOTE_START_ON: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c01022602260100030706010000000002010004010024007a000004001dbb";
    const REMOTE_START_DOOR_LOCKED: &str =
        "aa4220ec001c01022602260100030706010000000002010004010024007a00000400001c01022602260100030706010000000042010004010024007a00000400dfbb";
    const PUSH_REMOTE_START_OFF: &str = "aa09207200c80058bb";
    const POWER_OFF: &str =
        "aa4220ec001c01022602260100030706010000000000010004010024007a00000400001c00022602260100000000000000000000010004000024007a0000040066bb";

    // 21 Oct 2026, a Quick 14 wash with one pause, started at the panel.
    const WASH_STARTED: &str =
        "aa4220ec001c01000e000e0c00030202010000000100000000010024007a00000100001c06000e000e0c00030202010000000102200001060024007a0000000055bb";
    const BD_DUMP: &str =
        "aa0020bd00010190000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004cbb";
    const CD_DUMP: &str =
        "aa0020cd000190000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000007dbb";
    const WASH_DOOR_LOCKED: &str =
        "aa4220ec001c06000e000e0c00030202010000000102200001060024007a00000000001c06000e000e0c00030202010000000142200001060024007a0000000039bb";
    const WASH_13_MIN: &str =
        "aa4220ec001c06000e000e0c00030202010000000142200001060024007a00000000001c06000d000e0c00030202010000000142200001060024007a00000000febb";
    const PAUSED: &str =
        "aa4220ec001c06000c000e0c00030202010000000142200001060024007a00000000001c02000c000e0c00030202010000000140000000020024007a000001002bbb";
    const PAUSED_DOOR_UNLOCKED: &str =
        "aa4220ec001c02000c000e0c00030202010000000140000000020024007a00000100001c02000c000e0c00030202010000000100000000020024007a0000010041bb";
    const RESUMED: &str =
        "aa4220ec001c02000c000e0c00030202010000000100000000020024007a00000100001c06000c000e0c00030202010000000102200001020024007a00000000afbb";
    const RINSING: &str =
        "aa4220ec001c06000c000e0c00030202010000000142200001060024007a00000000001c07000b000e0c00030200010000000142000001070024007a00000000d2bb";
    const SPINNING: &str =
        "aa4220ec001c070006000e0c00030200010000000142000001070024007a00000100001c080005000e0c00000200000000000042000001080024007a000001000fbb";
    const PUSH_WASH_COMPLETE: &str = "aa09207200000010bb";
    const END: &str =
        "aa4220ec001c080001000e0c00000200000000000042000001080024007a00000100001c0a0000000e0c000000000000000000400000010a0024007a0000010018bb";
    const E2_END: &str = "aa2420e2091c06000e000e0c00030202010000000102200001060024007a00000100a1bb";
    const END_DOOR_UNLOCKED: &str =
        "aa4220ec001c0a0000000e0c000000000000000000400000010a0024007a00000100001c0a0000000e0c000000000000000000000000010a0024007a0000010059bb";
    const OFF_AFTER_END: &str =
        "aa4220ec001c0a0000000e0c000000000000000000000000010a0024007a00000100001c000000000e0c00000000000000000000000000000024007a00000100e2bb";
    const OFF_AFTER_POWER_OFF: &str =
        "AA4220EC001C01022602260100030706010000000000010004010024007A00000400001C00022602260100000000000000000000010004000024007A0000040066BB";

    // 21 and 22 Oct 2026: the LG app's own commands, sent through the bridge, and the washer's
    // state just before each. LG_* frames are what the LG cloud sent the washer.
    // Cotton on the panel, Remote Start armed
    const COTTON_REMOTE_START: &str =
        "AA4220EC001C01021502150100030A04010000000002010002010025007A00000400001C01021502150100030A04010000000042010002010025007A0000040013BB";

    // Start in the app, nothing changed
    const LG_START_COTTON: &str = "AA16F02601030A04010000000000030000000000B9BB";

    // The washer's answer: command 26 done
    const RESULT_START_OK: &str = "AA0820002600ADBB";

    // Just powered on, dial untouched
    const NO_COURSE_REMOTE_START: &str =
        "AA4220EC001C01000000000000000000000000000002000000010025007A00000000001C01000000000000000000000000000042000000010025007A00000000E3BB";

    // Quick 14 picked in the app, Start
    const LG_START_QUICK_14: &str = "AA16F0260C030202010000000001030000000000BBBB";
    const COTTON_STEAM_WRINKLE_REMOTE_START: &str =
        "AA4220EC001C01031F031F0100030A06010000008022010004010025007A00000400001C01031F031F0100030A06010000008062010004010025007A00000400EFBB";

    // Rinse+ picked in the app, Start
    const LG_START_RINSE_PLUS: &str = "AA16F02601030A06020000000080230000000000DABB";
    const NO_COURSE_REMOTE_START_2: &str =
        "AA4220EC001C01020E02100000000000000000000002010003010025007A00000100001C01020E02100000000000000000000042010003010025007A0000010051BB";

    // Cotton, "Set delayed cycle" 3 h, Start
    const LG_START_DELAYED: &str = "AA16F0260103FF04010003000000030000000000B1BB";

    // Waiting out the delay: 3 h 0, wash 0:59
    const DELAYED: &str =
        "AA4220EC001C04021502150100030A04010003000042210001040025007A00000000001C03003B003B0100030A04010003000042210001030025007A0000000041BB";
    const LG_POWER_OFF_DELAYED: &str = "AA09F0240101009CBB";
    const QUICK_14_WASHING: &str =
        "AA4220EC001C06000E000E0C00030202010000000142200001060025007A00000000001C06000D000E0C00030202010000000142200001060025007A00000000F8BB";
    const LG_PAUSE: &str = "AA09F02404010099BB";
    const QUICK_14_PAUSED: &str =
        "AA4220EC001C06000D000E0C00030202010000000142200001060025007A00000000001C02000D000E0C00030202010000000142000000020025007A00000100D1BB";
    const LG_RESUME: &str = "AA16F0260C030202010000000001020000000000B8BB";
    const RINSING_SESSION_1: &str =
        "AA4220EC001C07000B000E0C00030200010000000102200001070025007A00000000001C07000B000E0C00030200010000000142000001070025007A000000001DBB";
    const LG_POWER_OFF: &str = "AA09F0240101009CBB";

    // Asleep after 14 idle minutes with Remote Start armed
    const STANDBY: &str =
        "AA4220EC001C01000000000000000000000000000042000000010025007A00000000001C01000000000000000000000000000042000000010025007A00000001A2BB";

    // The app's "Sleep Mode OFF"
    const LG_WAKE_UP: &str = "AA08F02A010098BB";
    const WAKE_RESPONSE: &str = "AA08202B0100ABBB";
    const LG_ACK_WAKE_RESPONSE: &str = "AA08F0002B0484BB";
    const AWAKE: &str =
        "AA4220EC001C01000000000000000000000000000002000000010025007A00000000001C01000000000000000000000000000042000000010025007A00000000E3BB";

    // Remote Start armed with the door open
    const PUSH_ERROR: &str = "AA0C2072006403000000FABB";
    const ERROR_DE1: &str =
        "AA4220EC001C01000000000000000000000000000000000000010025007A00000000001C12000000000002000000000000000000000000120025007A00000000C3BB";

    // The washer connecting (22 Oct): its hello, LG's status request and the washer's EB answer.
    const HELLO: &str =
        "AA372031020153414134313035393332300146AD600000400000000000025341413431303539323230000074BA000040000000000047BB";
    const LG_ACK_HELLO: &str = "AA08F000310482BB";
    const LG_STATUS_REQUEST: &str = "AA0EF0ED1121010000001800B5BB";
    const EB_ON_CONNECT: &str =
        "AA2420EB001C01021502150100030A04010000000000010002010025007A000004008BBB";

    // The LG cloud's acks to the washer, byte for byte from the same captures.
    const LG_ACK_D8: &str = "AA08F000D8042BBB";
    const LG_ACK_72: &str = "AA08F00072044DBB";
    const LG_ACK_E2: &str = "AA08F000E204DDBB";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, MODEL_ID, "2.10.71")
    }

    fn get(ha: &MockHaConnection, name: &str) -> Option<PropertyValue> {
        ha.device(DEVICE_ID)?.properties.get(name).cloned()
    }

    fn text(value: &str) -> Option<PropertyValue> {
        Some(value.into())
    }

    fn int(value: i64) -> Option<PropertyValue> {
        Some(PropertyValue::Int(value))
    }

    // As upstream's hex(buf(...)): the frame's bytes in upper-case hex.
    fn hex(frame: &str) -> String {
        hex_encode(&hex_decode(frame))
    }

    /// An HA press: the property and its payload.
    type Press = (&'static str, &'static str);
    /// What is checked, the washer's state, the presses in HA, and the LG app's command.
    type CommandCase = (&'static str, &'static str, &'static [Press], &'static str);

    /// The washer connected through rethink's bridge, as it runs for real: the registered
    /// handler, started, and HA presses arriving as set_property.
    struct Washer {
        ha: Arc<MockHaConnection>,
        thinq: Arc<MockThinq2Device>,
        dev: Arc<dyn DeviceHandler>,
    }

    impl Washer {
        fn press(&self, actions: &[Press]) {
            for (prop, value) in actions {
                self.dev.set_property(prop, value);
            }
        }

        fn sent(&self) -> Vec<String> {
            self.thinq.outbox().iter().map(|p| hex_encode(p)).collect()
        }

        fn settings(&self) -> Vec<Option<PropertyValue>> {
            [
                "set_course",
                "set_temp",
                "set_spin",
                "set_rinse",
                "set_delay_end",
            ]
            .iter()
            .map(|name| get(&self.ha, name))
            .collect()
        }
    }

    fn connect(frames: &[&str]) -> Washer {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let factory = crate::t2_factory(MODEL_ID).expect("registered");
        let dev = factory(ha.clone(), thinq.clone(), meta());
        dev.start();
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        thinq.reset_recorder();
        Washer { ha, thinq, dev }
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

    fn decode(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _dev) = make();
        for frame in frames {
            emit(&thinq, frame);
        }
        ha
    }

    fn acks(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn every_dial_position_reports_its_panel_name_and_programme_time() {
        for (frame, course, time) in [
            (COTTON, "Cotton", 141),
            (COTTON_ECO, "Cotton Eco", 138),
            (TURBOCLEAN_39, "TurboClean 39", 39),
            (MIXED_FABRIC, "Mixed Fabric", 81),
            (EASY_CARE, "Easy Care", 121),
            (QUIET_WASH, "Quiet Wash", 148),
            (ALLERGY_CARE, "Allergy Care", 168),
            // The downloaded course, Spin.
            (DOWNLOAD_CYCLE, "Spin", 14),
            (TUB_CLEAN, "Tub Clean", 72),
            (DUVET, "Duvet", 93),
            (QUICK_14, "Quick 14", 14),
            (SPORTSWEAR, "Sportswear", 52),
            (HAND_WOOL, "Hand/Wool", 36),
            (DELICATES, "Delicates", 45),
        ] {
            let ha = decode(&[frame]);
            assert_eq!(get(&ha, "course"), text(course));
            assert_eq!(get(&ha, "remaining_time"), int(time), "{course}");
            assert_eq!(get(&ha, "initial_time"), int(time), "{course}");
        }
    }

    #[test]
    fn download_cycle_has_no_temperature_rinse_or_soil_level() {
        let ha = decode(&[DOWNLOAD_CYCLE]);
        assert_eq!(get(&ha, "downloaded_course"), text("Spin"));
        assert_eq!(get(&ha, "temp"), text("Not selected"));
        assert_eq!(get(&ha, "rinse"), text("Not selected"));
        assert_eq!(get(&ha, "soil"), text("Not selected"));
    }

    #[test]
    fn every_temperature_and_spin_speed_on_the_panel() {
        for (frame, temp) in [
            (TEMP_COLD, "Cold"),
            (TEMP_20, "20 °C"),
            (TEMP_30, "30 °C"),
            (COTTON, "40 °C"),
            (TEMP_60, "60 °C"),
            (TEMP_95, "95 °C"),
        ] {
            assert_eq!(get(&decode(&[frame]), "temp"), text(temp));
        }

        for (frame, spin) in [
            (SPIN_NO, "No spin"),
            (SPIN_400, "400 rpm"),
            (SPIN_800, "800 rpm"),
            (SPIN_1000, "1000 rpm"),
            (SPIN_1200, "1200 rpm"),
            (COTTON, "1400 rpm"),
        ] {
            assert_eq!(get(&decode(&[frame]), "spin"), text(spin));
        }
    }

    #[test]
    fn options_including_the_settings_the_washer_changes_with_them() {
        let plain = decode(&[COTTON]);
        for key in ["steam", "turbo_wash", "pre_wash", "wrinkle_care"] {
            assert_eq!(get(&plain, key), text("OFF"), "{key}");
        }
        assert_eq!(get(&plain, "rinse"), text("Normal"));
        assert_eq!(get(&plain, "soil"), text("Normal"));

        // Steam moves 40 °C to 60 °C, TurboWash (the panel's Turbo Clean) moves 1400 rpm to
        // 1000 rpm, in the same frame.
        let steam = decode(&[STEAM_ON]);
        assert_eq!(get(&steam, "steam"), text("ON"));
        assert_eq!(get(&steam, "temp"), text("60 °C"));
        let turbo = decode(&[TURBO_CLEAN_ON]);
        assert_eq!(get(&turbo, "turbo_wash"), text("ON"));
        assert_eq!(get(&turbo, "spin"), text("1000 rpm"));

        assert_eq!(get(&decode(&[PRE_WASH_ON]), "pre_wash"), text("ON"));
        assert_eq!(get(&decode(&[WRINKLE_CARE_ON]), "wrinkle_care"), text("ON"));
        assert_eq!(get(&decode(&[INTENSIVE_ON]), "soil"), text("Intensive"));

        // Rinse+ sets rec[12], which F_C's layout reads as the course.
        let rinse = decode(&[RINSE_PLUS_ON]);
        assert_eq!(get(&rinse, "rinse"), text("Rinse+"));
        assert_eq!(get(&rinse, "course"), text("Cotton"));
    }

    #[test]
    fn child_lock_delay_end_and_remote_start_locking_the_door() {
        assert_eq!(get(&decode(&[CHILD_LOCK_ON]), "child_lock"), text("ON"));
        assert_eq!(get(&decode(&[COTTON]), "child_lock"), text("OFF"));
        assert_eq!(get(&decode(&[DELAY_3H]), "delay_end"), int(180));
        assert_eq!(get(&decode(&[DELAY_19H]), "delay_end"), int(1140));

        let (ha, thinq, _) = make();
        emit(&thinq, REMOTE_START_ON);
        assert_eq!(get(&ha, "remote_start"), text("ON"));
        // Not locked yet.
        assert_eq!(get(&ha, "door_lock"), text("OFF"));
        emit(&thinq, REMOTE_START_DOOR_LOCKED);
        // Locked about 5 s later.
        assert_eq!(get(&ha, "door_lock"), text("ON"));
    }

    #[test]
    fn power_on_before_the_dial_moves_and_power_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, D8_ZERO);
        // As the LG cloud also reports it.
        assert_eq!(get(&ha, "tub_clean_count"), int(0));
        emit(&thinq, D8_36);
        assert_eq!(get(&ha, "tub_clean_count"), int(36));

        // The dial is on Cotton, but nothing is filled in until it is turned.
        emit(&thinq, POWER_ON);
        assert_eq!(get(&ha, "status"), text("Standby"));
        assert_eq!(get(&ha, "power"), text("ON"));
        assert_eq!(get(&ha, "course"), text("None"));
        assert_eq!(get(&ha, "remaining_time"), int(0));
        assert_eq!(get(&ha, "tub_clean_count"), int(36));

        emit(&thinq, POWER_OFF);
        assert_eq!(get(&ha, "status"), text("Off"));
        assert_eq!(get(&ha, "power"), text("OFF"));
    }

    #[test]
    fn a_quick_14_wash_with_a_pause_until_the_washer_switches_itself_off() {
        let (ha, thinq, _) = make();
        let step = |frame: &str, expected: &[(&str, Option<PropertyValue>)]| {
            emit(&thinq, frame);
            for (key, value) in expected {
                assert_eq!(&get(&ha, key), value, "{key}");
            }
        };

        step(
            WASH_STARTED,
            &[
                ("status", text("Washing")),
                ("course", text("Quick 14")),
                ("remaining_time", int(14)),
                ("remote_start", text("ON")),
            ],
        );
        step(WASH_DOOR_LOCKED, &[("door_lock", text("ON"))]);
        step(WASH_13_MIN, &[("remaining_time", int(13))]);
        step(
            PAUSED,
            &[("status", text("Paused")), ("remote_start", text("OFF"))],
        );
        step(PAUSED_DOOR_UNLOCKED, &[("door_lock", text("OFF"))]);
        step(
            RESUMED,
            &[("status", text("Washing")), ("remote_start", text("ON"))],
        );
        step(
            RINSING,
            &[
                ("status", text("Rinsing")),
                ("remaining_time", int(11)),
                ("temp", text("Not selected")),
            ],
        );
        step(
            SPINNING,
            &[
                ("status", text("Spinning")),
                ("remaining_time", int(5)),
                ("rinse", text("Not selected")),
                ("soil", text("Not selected")),
            ],
        );
        step(
            END,
            &[
                ("status", text("End")),
                ("remaining_time", int(0)),
                ("spin", text("Not selected")),
                ("remote_start", text("OFF")),
                ("door_lock", text("ON")),
            ],
        );
        step(END_DOOR_UNLOCKED, &[("door_lock", text("OFF"))]);
        step(
            OFF_AFTER_END,
            &[("status", text("Off")), ("power", text("OFF"))],
        );
    }

    #[test]
    fn acks_exactly_the_frames_the_lg_cloud_acks_with_its_bytes() {
        let (_, thinq, _) = make();
        for frame in [
            D8_ZERO,
            COTTON,
            PUSH_REMOTE_START_ON,
            PUSH_REMOTE_START_OFF,
            RESULT_START_OK,
            WASH_STARTED,
            BD_DUMP,
            CD_DUMP,
            PUSH_WASH_COMPLETE,
            END,
            E2_END,
            HELLO,
            WAKE_RESPONSE,
        ] {
            emit(&thinq, frame);
        }

        let acks = acks(&thinq);
        assert_eq!(
            acks,
            [
                LG_ACK_D8.to_string(),
                LG_ACK_72.to_string(),
                LG_ACK_72.to_string(),
                LG_ACK_72.to_string(),
                LG_ACK_E2.to_string(),
                hex(LG_ACK_HELLO),
                hex(LG_ACK_WAKE_RESPONSE),
            ]
        );
        assert_eq!(thinq.sent().len(), acks.len());
        assert!(
            thinq.outbox().is_empty(),
            "no packets in reply to anything the washer sends"
        );
    }

    #[test]
    fn connecting_asks_for_the_state_as_lg_does_and_reads_the_eb_answer() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev =
            crate::t2_factory(MODEL_ID).expect("registered")(ha.clone(), thinq.clone(), meta());
        dev.start();
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(sent, [hex(LG_STATUS_REQUEST)]);

        emit(&thinq, EB_ON_CONNECT);
        assert_eq!(get(&ha, "status"), text("Standby"));
        assert_eq!(get(&ha, "course"), text("Cotton"));
        assert_eq!(get(&ha, "remaining_time"), int(141));
        assert_eq!(get(&ha, "temp"), text("40 °C"));
        assert_eq!(get(&ha, "spin"), text("1400 rpm"));
    }

    #[test]
    fn each_command_is_byte_for_byte_the_lg_apps_from_the_same_washer_state() {
        let cases: &[CommandCase] = &[
            (
                "Start, as set on the panel",
                COTTON_REMOTE_START,
                &[("start", "")],
                LG_START_COTTON,
            ),
            (
                "Start, course picked",
                NO_COURSE_REMOTE_START,
                &[("set_course", "Quick 14"), ("start", "")],
                LG_START_QUICK_14,
            ),
            (
                "Start, rinse changed",
                COTTON_STEAM_WRINKLE_REMOTE_START,
                &[("set_rinse", "Rinse+"), ("start", "")],
                LG_START_RINSE_PLUS,
            ),
            (
                "Start, delayed",
                NO_COURSE_REMOTE_START_2,
                &[
                    ("set_course", "Cotton"),
                    ("set_delay_end", "3"),
                    ("start", ""),
                ],
                LG_START_DELAYED,
            ),
            ("Pause", QUICK_14_WASHING, &[("pause", "")], LG_PAUSE),
            (
                "Start while paused: resume",
                QUICK_14_PAUSED,
                &[("start", "")],
                LG_RESUME,
            ),
            (
                "Power off, rinsing",
                RINSING_SESSION_1,
                &[("power_off", "")],
                LG_POWER_OFF,
            ),
            (
                "Power off, delayed",
                DELAYED,
                &[("power_off", "")],
                LG_POWER_OFF_DELAYED,
            ),
            (
                "Wake up, in standby",
                STANDBY,
                &[("wake_up", "")],
                LG_WAKE_UP,
            ),
        ];
        for (name, state, actions, lg) in cases {
            let washer = connect(&[*state]);
            washer.press(actions);
            assert_eq!(washer.sent(), [hex(lg)], "{name}");
        }
    }

    #[test]
    fn start_power_off_and_wake_up_are_greyed_out_unless_the_washer_allows_them() {
        let (ha, _, _) = make();
        let config = ha.device(DEVICE_ID).unwrap().config.unwrap();
        let components = &config.components;
        let gate = |key: &str| -> Option<(Value, Value)> {
            let last = components[key].get("availability")?.as_array()?.last()?;
            Some((last["topic"].clone(), last["payload_available"].clone()))
        };
        assert_eq!(
            gate("start"),
            Some((json!("$this/remote_start"), json!("ON")))
        );
        assert_eq!(gate("power_off"), Some((json!("$this/power"), json!("ON"))));
        assert_eq!(gate("wake_up"), Some((json!("$this/standby"), json!("ON"))));
        assert_eq!(gate("pause"), None);
        assert!(
            !components.contains_key("resume"),
            "Start resumes; there is no Resume button"
        );
        assert_eq!(components["start"]["availability_mode"], "all");

        // What those topics read in the recorded states.
        assert_eq!(
            get(&decode(&[COTTON_REMOTE_START]), "remote_start"),
            text("ON")
        );
        assert_eq!(get(&decode(&[COTTON]), "remote_start"), text("OFF"));
        assert_eq!(get(&decode(&[OFF_AFTER_POWER_OFF]), "power"), text("OFF"));
        assert_eq!(get(&decode(&[STANDBY]), "standby"), text("ON"));
        assert_eq!(get(&decode(&[QUICK_14_WASHING]), "standby"), text("OFF"));
    }

    #[test]
    fn sends_nothing_only_when_there_is_nothing_valid_to_send() {
        let cases: &[(&str, &[&str], &[Press])] = &[
            (
                "no status from the washer yet: no settings to start or resume with",
                &[],
                &[("start", "")],
            ),
            (
                "no course on the washer or chosen",
                &[NO_COURSE_REMOTE_START],
                &[("start", "")],
            ),
        ];
        for (name, frames, actions) in cases {
            let washer = connect(frames);
            washer.press(actions);
            assert!(washer.sent().is_empty(), "{name}");
        }
    }

    #[test]
    fn any_other_press_is_sent_whatever_the_state_and_the_washer_decides() {
        let cases: &[CommandCase] = &[
            (
                "Pause while ready",
                COTTON_REMOTE_START,
                &[("pause", "")],
                LG_PAUSE,
            ),
            (
                "Start while washing: the running programme, start flags",
                QUICK_14_WASHING,
                &[("start", "")],
                LG_START_QUICK_14,
            ),
            // Washing again, though the previous-state byte still reads paused: Start, not resume.
            (
                "Start just after a resume",
                RESUMED,
                &[("start", "")],
                LG_START_QUICK_14,
            ),
            (
                "Power off while off",
                OFF_AFTER_POWER_OFF,
                &[("power_off", "")],
                LG_POWER_OFF,
            ),
            (
                "Wake up while awake",
                COTTON_REMOTE_START,
                &[("wake_up", "")],
                LG_WAKE_UP,
            ),
        ];
        for (name, state, actions, bytes) in cases {
            let washer = connect(&[*state]);
            washer.press(actions);
            assert_eq!(washer.sent(), [hex(bytes)], "{name}");
        }
    }

    #[test]
    fn a_course_picked_brings_its_defaults_and_only_the_choices_the_app_offers() {
        let washer = connect(&[NO_COURSE_REMOTE_START]);
        washer.press(&[("set_course", "Quick 14")]);
        let quick_14 = [
            text("Quick 14"),
            text("20 °C"),
            text("400 rpm"),
            text("Normal"),
            int(0),
        ];
        assert_eq!(washer.settings(), quick_14);

        // Not offered for Quick 14: 95 °C, 1400 rpm (the app offers "Max"), a 2 h delay.
        washer.press(&[
            ("set_temp", "95 °C"),
            ("set_spin", "1400 rpm"),
            ("set_delay_end", "2"),
        ]);
        assert_eq!(washer.settings(), quick_14);
        washer.press(&[("start", "")]);
        assert_eq!(washer.sent(), [hex(LG_START_QUICK_14)]);

        // Tub Clean: fixed settings, no Delay End.
        let tub = connect(&[NO_COURSE_REMOTE_START]);
        tub.press(&[
            ("set_course", "Quick 14"),
            ("set_delay_end", "3"),
            ("set_course", "Tub Clean"),
        ]);
        tub.press(&[("set_temp", "40 °C"), ("set_delay_end", "3")]);
        let p = |name| get(&tub.ha, name);
        assert_eq!(
            [
                p("set_course"),
                p("set_temp"),
                p("set_spin"),
                p("set_delay_end")
            ],
            [text("Tub Clean"), text("60 °C"), text("No spin"), int(0)]
        );
    }

    #[test]
    fn start_resumes_a_paused_programme_as_it_was_whatever_was_chosen_meanwhile() {
        let washer = connect(&[QUICK_14_PAUSED]);
        washer.press(&[("set_course", "Cotton"), ("start", "")]);
        assert_eq!(washer.sent(), [hex(LG_RESUME)]);
        assert_eq!(get(&washer.ha, "set_course"), text("Quick 14"));
    }

    #[test]
    fn the_settings_show_the_washers_own_until_changed_and_follow_it_again_after_start() {
        let washer = connect(&[COTTON_STEAM_WRINKLE_REMOTE_START]);
        let own = [
            text("Cotton"),
            text("60 °C"),
            text("1400 rpm"),
            text("Normal"),
            int(0),
        ];
        assert_eq!(washer.settings(), own);
        washer.press(&[("set_rinse", "Rinse+")]);
        assert_eq!(
            washer.settings(),
            [
                text("Cotton"),
                text("60 °C"),
                text("1400 rpm"),
                text("Rinse+"),
                int(0),
            ]
        );
        washer.press(&[("start", "")]);
        assert_eq!(washer.settings(), own);
    }

    #[test]
    fn the_door_error_status_code_and_message_and_its_push_acked() {
        let (ha, thinq, _) = make();
        emit(&thinq, PUSH_ERROR);
        emit(&thinq, ERROR_DE1);
        assert_eq!(get(&ha, "status"), text("Error"));
        assert_eq!(get(&ha, "error"), text("ON"));
        assert_eq!(get(&ha, "error_message"), text("Door open error (DE1)"));
        let sent: Vec<Value> = thinq.sent().into_iter().map(|m| m.data).collect();
        assert_eq!(sent, [json!(LG_ACK_72)]);
        emit(&thinq, COTTON);
        assert_eq!(get(&ha, "error"), text("OFF"));
        assert_eq!(get(&ha, "error_message"), text("OK"));
    }

    #[test]
    fn standby_the_delayed_start_and_its_countdown() {
        assert_eq!(get(&decode(&[STANDBY]), "standby"), text("ON"));
        assert_eq!(get(&decode(&[AWAKE]), "standby"), text("OFF"));
        let delayed = decode(&[DELAYED]);
        assert_eq!(get(&delayed, "status"), text("Delayed"));
        assert_eq!(get(&delayed, "delay_end"), int(180));
        assert_eq!(get(&delayed, "remaining_time"), int(59));
    }

    // Not an upstream test: pins the option lists upstream computes from the course table.
    #[test]
    fn the_set_selects_offer_every_choice_the_app_has_for_some_course() {
        let (ha, _, _) = make();
        let components = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        let options = |key: &str| components[key]["options"].clone();
        assert_eq!(
            options("set_course"),
            json!([
                "Cotton",
                "Easy Care",
                "Cotton Eco",
                "Duvet",
                "Mixed Fabric",
                "Sportswear",
                "Quiet Wash",
                "Quick 14",
                "Tub Clean",
                "Hand/Wool",
                "Delicates",
                "Allergy Care",
                "TurboClean 39",
            ])
        );
        assert_eq!(
            options("set_temp"),
            json!(["Cold", "20 °C", "30 °C", "40 °C", "60 °C", "95 °C"])
        );
        assert_eq!(
            options("set_spin"),
            json!(["No spin", "400 rpm", "800 rpm", "1000 rpm", "1200 rpm", "1400 rpm", "Max"])
        );
        assert_eq!(options("set_rinse"), json!(["Normal", "Rinse+"]));
    }
}
