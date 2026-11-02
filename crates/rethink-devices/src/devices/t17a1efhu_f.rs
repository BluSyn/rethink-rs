//! T17A1EFHU_F — LG WT7305CV / WT7305CW top-load washer.
//!
//! The codes agree with T1789EFH_F (WT7300CW), whose course tables are shared,
//! but status arrives in class `0x20` long envelopes holding one 29-byte record
//! (`EB` snapshot) or two (`EC` old then current). Only those are decoded: `E2`
//! settings echoes can replay the cycle's starting state after End. `start()`
//! sends the WT7300CW status query, which this model answers with an EB
//! snapshot. `auto_ack` is on: unacked envelopes repeat about every 1.6 s.
//!
//! Wash settings are staged in HA and sent in the `F0 26` start/resume command;
//! untouched fields follow the latest record and the washer enforces its own
//! interlocks. `F0 25 03 0F` writes a downloaded course preset without starting
//! it. Power-on is omitted (no verified command), and bytes 5/6 are not an
//! initial time (they hold 04 15 through a whole cycle).
//!
//! Port of upstream PR #292.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use crate::tl_us_washer_courses::{COURSES, SMART_COURSES};
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const RECORD_LENGTH: usize = 29;
const PHASE_OFFSET: usize = 2;
const PHASE_OFF: u8 = 0x00;
const PHASE_PAUSED: u8 = 0x02;
const TIME_HOUR_OFFSET: usize = 3;
const TIME_MIN_OFFSET: usize = 4;
const COURSE_OFFSET: usize = 7;
const SOIL_OFFSET: usize = 9;
const SPIN_OFFSET: usize = 10;
const TEMP_OFFSET: usize = 11;
/// The command's rinse count shifted left one bit: downloads of 1 and 2 reported 2 and 4.
const RINSE_OFFSET: usize = 12;
const DELAY_HOUR_OFFSET: usize = 14;
const DELAY_MIN_OFFSET: usize = 15;
const FLAGS_OFFSET: usize = 16;
const OPTIONS_OFFSET: usize = 17;
const REMOTE_START_OFFSET: usize = 18;
const DEEP_FILL_MAX_OFFSET: usize = 18;
/// The active Smart Course clears when the dial leaves Downloaded; the stored slot remains.
const SMART_COURSE_OFFSET: usize = 22;
const DOWNLOADED_COURSE_OFFSET: usize = 25;

const FLAG_CONTROL_LOCK: u8 = 0x01;
const FLAG_DOOR_LOCK: u8 = 0x04;
const FLAG_COLD_WASH: u8 = 0x20;
const FLAG_TURBO_WASH: u8 = 0x40;
const OPTION_EXTRA_RINSE: u8 = 0x01;
const OPTION_DEEP_FILL: u8 = 0x02;
const OPTION_STAIN_CARE: u8 = 0x08;
const OPTION_SOAK: u8 = 0x20;
const REMOTE_START_ON: u8 = 0x01;
const DEEP_FILL_MAX_ON: u8 = 0x08;

/// `F0 26 course soil spin temp rinse 00 00 00 soak delayH delayM smartCourse flags options verb`
const CMD_START: &[u8] = &[0xf0, 0x26];
const VERB_START: u8 = 0x03;
const VERB_RESUME: u8 = 0x01;
const COMMAND_MAX_FILL: u8 = 0x08;
const CMD_PAUSE: &[u8] = &[0xf0, 0x24, 0x04, 0x01, 0x00];
const CMD_POWER_OFF: &[u8] = &[0xf0, 0x24, 0x01, 0x01, 0x00];
/// Writes a preset to the Downloaded slot, in the start layout with a zero verb.
const CMD_DOWNLOAD: &[u8] = &[0xf0, 0x25, 0x03, 0x0f];

const STATUS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Standby"),
    (2, "Paused"),
    (3, "Detecting"),
    (4, "Soaking"),
    (5, "Washing"),
    (6, "Rinsing"),
    (7, "Spinning"),
    (8, "End"),
    (9, "Delayed"),
    (10, "Updating firmware"),
    (11, "Smart Diagnosis"),
];
const SOIL: &[(u8, &str)] = &[
    (0, "Not selected"),
    (1, "Light"),
    (2, "Light-Normal"),
    (3, "Normal"),
    (4, "Normal-Heavy"),
    (5, "Heavy"),
];
const SPIN: &[(u8, &str)] = &[
    (0, "No Spin"),
    (1, "Low"),
    (2, "Medium"),
    (3, "High"),
    (4, "Extra High"),
];
/// No course on this model offers Extra Hot (6).
const TEMP: &[(u8, &str)] = &[
    (0, "Not selected"),
    (1, "Tap Cold"),
    (2, "Cold"),
    (3, "Semi Warm"),
    (4, "Warm"),
    (5, "Hot"),
];
const TEMP_COLD: u8 = 2;

const NOT_SELECTED: &str = "Not selected";
const USE_WASHER_SETTING: &str = "Use washer setting";

type Labels = &'static [(u8, &'static str)];

/// Staged selects in command order: HA property, labels, record offset.
const SELECTS: [(&str, Labels, usize); 4] = [
    ("set_course", COURSES, COURSE_OFFSET),
    ("set_soil", SOIL, SOIL_OFFSET),
    ("set_spin", SPIN, SPIN_OFFSET),
    ("set_temp", TEMP, TEMP_OFFSET),
];
const SELECT_COURSE: usize = 0;
const SELECT_TEMP: usize = 3;

/// Course, soil, spin, temp, rinse (command units), Turbo Wash, Extra Rinse, Soak, Stain Care.
type Preset = (u8, u8, u8, u8, u8, bool, bool, bool, bool);

/// Defaults from LG's T17A1EFHU_F modelJSON v0.7, `SmartCourse.*.function`.
const SMART_COURSE_PRESETS: &[(&str, Preset)] = &[
    ("Small Load", (0x01, 1, 2, 4, 0, true, false, false, false)),
    ("Single Item", (0x07, 1, 3, 2, 0, true, false, false, false)),
    (
        "Wrinkle Stop/Gentle Wave",
        (0x04, 1, 1, 4, 0, false, false, false, false),
    ),
    ("Rapid Clean", (0x07, 1, 3, 1, 0, true, false, false, false)),
    ("Sweat Stains", (0x02, 3, 3, 4, 0, true, false, true, false)),
    ("Swimwear", (0x04, 1, 1, 2, 1, false, true, false, false)),
    ("Curtains", (0x08, 1, 2, 4, 0, false, false, false, false)),
    ("Dress Shirts", (0x02, 5, 3, 5, 0, true, false, true, false)),
    (
        "Collar Stains",
        (0x02, 5, 3, 4, 0, true, false, false, true),
    ),
    (
        "Pillow Covers",
        (0x01, 3, 3, 5, 0, true, false, false, false),
    ),
    ("Lingerie", (0x04, 1, 1, 4, 1, false, true, false, false)),
    ("Baby Wear", (0x01, 3, 3, 4, 1, true, true, false, false)),
    (
        "Children's Wear",
        (0x01, 5, 3, 4, 0, true, false, true, false),
    ),
    (
        "Juice & Food Stains",
        (0x02, 5, 3, 5, 1, true, true, true, false),
    ),
    ("Baby Bibs", (0x01, 3, 2, 5, 1, true, true, true, false)),
    (
        "Pet Hair and Odor",
        (0x07, 1, 4, 4, 2, true, true, false, false),
    ),
    ("EconoWash", (0x01, 3, 3, 2, 0, true, false, false, false)),
    (
        "Collars and Cuffs",
        (0x02, 5, 2, 5, 0, true, false, true, false),
    ),
    (
        "Delicate Dresses",
        (0x04, 1, 1, 2, 0, false, false, false, false),
    ),
    (
        "Half Load Wash",
        (0x01, 1, 3, 5, 0, true, false, false, false),
    ),
    (
        "Full Load Wash",
        (0x01, 3, 4, 4, 1, true, true, false, false),
    ),
    ("Whites", (0x01, 5, 2, 5, 1, true, true, false, false)),
];

/// HA choices waiting for Start. `None` follows the latest record.
#[derive(Default)]
struct Pending {
    selects: [Option<u8>; 4],
    turbo_wash: Option<bool>,
    cold_wash: Option<bool>,
    extra_rinse: Option<bool>,
    stain_care: Option<bool>,
    deep_fill: Option<bool>,
    deep_fill_max: Option<bool>,
    soak: Option<bool>,
    delay_wash: Option<u8>,
    rinse_count: Option<u8>,
}

impl Pending {
    fn switch(&mut self, prop: &str) -> Option<&mut Option<bool>> {
        Some(match prop {
            "set_turbo_wash" => &mut self.turbo_wash,
            "set_cold_wash" => &mut self.cold_wash,
            "set_extra_rinse" => &mut self.extra_rinse,
            "set_stain_care" => &mut self.stain_care,
            "set_deep_fill" => &mut self.deep_fill,
            "set_deep_fill_max" => &mut self.deep_fill_max,
            "set_soak" => &mut self.soak,
            _ => return None,
        })
    }
}

#[derive(Default)]
struct State {
    record: Option<[u8; RECORD_LENGTH]>,
    pending: Pending,
}

impl State {
    /// The staged option, else the record's bit; `None` before the first record.
    fn option(&self, staged: Option<bool>, offset: usize, bit: u8) -> Option<bool> {
        staged.or_else(|| self.record.map(|rec| rec[offset] & bit != 0))
    }

    /// The rinse count in command units and the Extra Rinse switch. The base course's count
    /// is kept even when Extra Rinse is off.
    fn rinse(&self) -> Option<(u8, bool)> {
        let mut count = self.record.map(|rec| rec[RINSE_OFFSET] >> 1);
        let mut extra = self
            .record
            .map(|rec| rec[OPTIONS_OFFSET] & OPTION_EXTRA_RINSE != 0);
        if let Some(n) = self.pending.rinse_count {
            count = Some(n);
            extra = Some(n > 0);
        } else if let Some(on) = self.pending.extra_rinse {
            if extra != Some(on) {
                count = Some(u8::from(on));
            }
            extra = Some(on);
        }
        count.zip(extra)
    }

    fn select_label(&self, i: usize) -> &'static str {
        let (_, labels, offset) = SELECTS[i];
        self.pending.selects[i]
            .or(self.record.map(|rec| rec[offset]))
            .and_then(|code| named(labels, code))
            .filter(|label| *label != NOT_SELECTED)
            .unwrap_or(USE_WASHER_SETTING)
    }

    /// The setting controls: staged choices over the latest record.
    fn settings(&self) -> Vec<(&'static str, PropertyValue)> {
        let p = &self.pending;
        let rinse = self.rinse();
        let mut settings: Vec<(&'static str, PropertyValue)> = (0..SELECTS.len())
            .map(|i| (SELECTS[i].0, self.select_label(i).into()))
            .collect();
        settings.extend([
            (
                "set_turbo_wash",
                on_off(self.option(p.turbo_wash, FLAGS_OFFSET, FLAG_TURBO_WASH)),
            ),
            (
                "set_cold_wash",
                on_off(self.option(p.cold_wash, FLAGS_OFFSET, FLAG_COLD_WASH)),
            ),
            ("set_extra_rinse", on_off(rinse.map(|(_, extra)| extra))),
            (
                "set_extra_rinses",
                number(rinse.map(|(count, extra)| if extra { count } else { 0 })),
            ),
            (
                "set_stain_care",
                on_off(self.option(p.stain_care, OPTIONS_OFFSET, OPTION_STAIN_CARE)),
            ),
            (
                "set_deep_fill",
                on_off(self.option(p.deep_fill, OPTIONS_OFFSET, OPTION_DEEP_FILL)),
            ),
            (
                "set_deep_fill_max",
                on_off(self.option(p.deep_fill_max, DEEP_FILL_MAX_OFFSET, DEEP_FILL_MAX_ON)),
            ),
            (
                "set_soak",
                on_off(self.option(p.soak, OPTIONS_OFFSET, OPTION_SOAK)),
            ),
            (
                "set_delay_wash",
                number(
                    p.delay_wash
                        .or(self.record.map(|rec| rec[DELAY_HOUR_OFFSET])),
                ),
            ),
        ]);
        settings
    }

    /// The start/resume command, or `None` before the first record.
    fn start_command(&self) -> Option<Vec<u8>> {
        let rec = self.record?;
        let p = &self.pending;
        let (rinse, extra_rinse) = self.rinse()?;
        // Options apply to the latest record: other bits can change while a choice is pending.
        let mut flags = rec[FLAGS_OFFSET] & !FLAG_DOOR_LOCK;
        flags = with_bit(flags, FLAG_TURBO_WASH, p.turbo_wash);
        flags = with_bit(flags, FLAG_COLD_WASH, p.cold_wash);
        // Soak needs its own data byte as well as the options bit; the bit alone clears it.
        let soak = self.option(p.soak, OPTIONS_OFFSET, OPTION_SOAK)?;
        let mut options = with_bit(rec[OPTIONS_OFFSET], OPTION_SOAK, Some(soak));
        options = with_bit(options, OPTION_STAIN_CARE, p.stain_care);
        options = with_bit(options, OPTION_DEEP_FILL, p.deep_fill);
        options = with_bit(options, OPTION_EXTRA_RINSE, Some(extra_rinse));
        let max_fill = self.option(p.deep_fill_max, DEEP_FILL_MAX_OFFSET, DEEP_FILL_MAX_ON)?;
        let [course, soil, spin, temp]: [u8; 4] =
            std::array::from_fn(|i| p.selects[i].unwrap_or(rec[SELECTS[i].2]));
        let verb = if rec[PHASE_OFFSET] == PHASE_PAUSED {
            VERB_RESUME
        } else {
            VERB_START
        };
        let data = [
            course,
            soil,
            spin,
            temp,
            rinse,
            0,
            0,
            0,
            u8::from(soak),
            p.delay_wash.unwrap_or(rec[DELAY_HOUR_OFFSET]),
            if p.delay_wash.is_none() {
                rec[DELAY_MIN_OFFSET]
            } else {
                0
            },
            if p.selects[SELECT_COURSE].is_none() {
                rec[SMART_COURSE_OFFSET]
            } else {
                0
            },
            flags,
            options,
            verb | bit(max_fill, COMMAND_MAX_FILL),
        ];
        Some([CMD_START, &data].concat())
    }
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
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(rec) = process_enveloped_status(inner, CLASS_BYTE, RECORD_LENGTH) {
                    dev.process_status(rec);
                }
            }
        }));
        this
    }

    fn process_status(&self, record: &[u8]) {
        let Ok(rec) = <[u8; RECORD_LENGTH]>::try_from(record) else {
            return;
        };
        let settings = {
            let mut state = self.state.lock();
            state.record = Some(rec);
            state.settings()
        };
        let core = &self.core;
        let phase = rec[PHASE_OFFSET];
        let flags = rec[FLAGS_OFFSET];
        let options = rec[OPTIONS_OFFSET];
        core.publish_on_off("power", phase != PHASE_OFF);
        core.publish_property("status", label(STATUS, phase));
        core.publish_property(
            "remaining_time",
            minutes(rec[TIME_HOUR_OFFSET], rec[TIME_MIN_OFFSET]),
        );
        core.publish_property("course", label(COURSES, rec[COURSE_OFFSET]));
        core.publish_property(
            "smart_course",
            label(SMART_COURSES, rec[SMART_COURSE_OFFSET]),
        );
        let downloaded = named(SMART_COURSES, rec[DOWNLOADED_COURSE_OFFSET])
            .filter(|course| *course != NOT_SELECTED);
        core.publish_property("downloaded_course", downloaded.unwrap_or("None").into());
        core.publish_property("soil", label(SOIL, rec[SOIL_OFFSET]));
        core.publish_property("spin", label(SPIN, rec[SPIN_OFFSET]));
        core.publish_property("temp", label(TEMP, rec[TEMP_OFFSET]));
        core.publish_flag("turbo_wash", flags, FLAG_TURBO_WASH);
        core.publish_flag("cold_wash", flags, FLAG_COLD_WASH);
        core.publish_flag("extra_rinse", options, OPTION_EXTRA_RINSE);
        core.publish_flag("soak", options, OPTION_SOAK);
        core.publish_flag("stain_care", options, OPTION_STAIN_CARE);
        core.publish_flag("deep_fill", options, OPTION_DEEP_FILL);
        core.publish_flag("deep_fill_max", rec[DEEP_FILL_MAX_OFFSET], DEEP_FILL_MAX_ON);
        core.publish_flag("child_lock", flags, FLAG_CONTROL_LOCK);
        core.publish_property(
            "reserve_time",
            minutes(rec[DELAY_HOUR_OFFSET], rec[DELAY_MIN_OFFSET]),
        );
        // HA's lock device class reads ON as unlocked.
        core.publish_on_off("door_lock", flags & FLAG_DOOR_LOCK == 0);
        core.publish_flag("remote_start", rec[REMOTE_START_OFFSET], REMOTE_START_ON);
        self.publish(settings);
    }

    fn publish(&self, settings: Vec<(&str, PropertyValue)>) {
        for (prop, value) in settings {
            self.core.publish_property(prop, value);
        }
    }

    /// Applies `change` to the staged choices and republishes the setting controls.
    fn stage(&self, change: impl FnOnce(&mut Pending)) {
        let settings = {
            let mut state = self.state.lock();
            change(&mut state.pending);
            state.settings()
        };
        self.publish(settings);
    }

    fn reset_settings(&self) {
        self.stage(|pending| *pending = Pending::default());
    }

    fn set_select(&self, i: usize, value: &str) {
        let code = if value == USE_WASHER_SETTING {
            None
        } else {
            match code_of(SELECTS[i].1, value) {
                Some(code) if value != NOT_SELECTED => Some(code),
                _ => return,
            }
        };
        self.stage(|pending| pending.selects[i] = code);
    }

    fn set_switch(&self, prop: &str, value: &str) {
        let on = match value {
            "ON" => true,
            "OFF" => false,
            _ => return,
        };
        let settings = {
            let mut state = self.state.lock();
            let pending = &mut state.pending;
            let Some(switch) = pending.switch(prop) else {
                return;
            };
            *switch = Some(on);
            if prop == "set_cold_wash" && on {
                // As the panel and LG's captured Start do: Cold, with Turbo Wash off.
                pending.selects[SELECT_TEMP] = Some(TEMP_COLD);
                pending.turbo_wash = Some(false);
            }
            if prop == "set_extra_rinse" && (!on || pending.rinse_count == Some(0)) {
                pending.rinse_count = None;
            }
            state.settings()
        };
        self.publish(settings);
    }

    fn start_wash(&self) {
        let (command, settings) = {
            let mut state = self.state.lock();
            let Some(command) = state.start_command() else {
                return;
            };
            state.pending = Pending::default();
            (command, state.settings())
        };
        self.core.send(&command);
        self.publish(settings);
    }

    fn download_course(&self, name: &str) {
        let Some((_, preset)) = SMART_COURSE_PRESETS.iter().find(|(n, _)| *n == name) else {
            return;
        };
        let Some(smart_course) = code_of(SMART_COURSES, name) else {
            return;
        };
        let (course, soil, spin, temp, rinse, turbo_wash, extra_rinse, soak, stain_care) = *preset;
        let options = bit(extra_rinse, OPTION_EXTRA_RINSE)
            | bit(soak, OPTION_SOAK)
            | bit(stain_care, OPTION_STAIN_CARE);
        let data = [
            course,
            soil,
            spin,
            temp,
            rinse,
            0,
            0,
            0,
            u8::from(soak),
            0,
            0,
            smart_course,
            bit(turbo_wash, FLAG_TURBO_WASH),
            options,
            0,
        ];
        self.core.send(&[CMD_DOWNLOAD, &data].concat());
        // Only the appliance's report confirms the slot. Staged choices were for the old course.
        self.reset_settings();
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
        match prop {
            "downloaded_course" => self.download_course(value),
            "reset_settings" => self.reset_settings(),
            "set_delay_wash" => {
                if let Some(hours) = whole_number(value, 19) {
                    self.stage(|pending| pending.delay_wash = Some(hours));
                }
            }
            "set_extra_rinses" => {
                if let Some(count) = whole_number(value, 2) {
                    self.stage(|pending| {
                        pending.rinse_count = Some(count);
                        pending.extra_rinse = None;
                    });
                }
            }
            "pause" => self.core.send(CMD_PAUSE),
            "power_off" => {
                self.core.send(CMD_POWER_OFF);
                self.reset_settings();
            }
            "start" => self.start_wash(),
            _ => match SELECTS.iter().position(|(select, _, _)| *select == prop) {
                Some(i) => self.set_select(i, value),
                None => self.set_switch(prop, value),
            },
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

fn named(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn code_of(table: &[(u8, &str)], name: &str) -> Option<u8> {
    table
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(code, _)| *code)
}

fn label(table: &[(u8, &'static str)], code: u8) -> PropertyValue {
    named(table, code).unwrap_or("None").into()
}

fn labels(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

/// A staged select's options: the washer's own setting, then every real choice.
fn setting_options(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    let choices = labels(table)
        .into_iter()
        .filter(|name| *name != NOT_SELECTED);
    std::iter::once(USE_WASHER_SETTING).chain(choices).collect()
}

fn minutes(hours: u8, minutes: u8) -> PropertyValue {
    (i64::from(hours) * 60 + i64::from(minutes)).into()
}

fn on_off(value: Option<bool>) -> PropertyValue {
    match value {
        Some(true) => "ON",
        Some(false) => "OFF",
        None => "None",
    }
    .into()
}

fn number(value: Option<u8>) -> PropertyValue {
    value.map_or_else(|| "None".into(), |n| i64::from(n).into())
}

fn bit(on: bool, mask: u8) -> u8 {
    if on {
        mask
    } else {
        0
    }
}

/// Sets or clears `mask` as staged; `None` keeps the byte's own bit.
fn with_bit(byte: u8, mask: u8, on: Option<bool>) -> u8 {
    match on {
        Some(true) => byte | mask,
        Some(false) => byte & !mask,
        None => byte,
    }
}

/// `Number(value)` when it is a whole number in `0..=max`. Blank input is rejected; like
/// JavaScript's `trim()`, U+FEFF counts as blank.
fn whole_number(value: &str, max: u8) -> Option<u8> {
    if value.chars().all(|c| c.is_whitespace() || c == '\u{feff}') {
        return None;
    }
    let n = crate::js::number(value)?;
    (n.fract() == 0.0 && (0.0..=f64::from(max)).contains(&n)).then_some(n as u8)
}

fn components() -> HashMap<String, Value> {
    let presets: Vec<&str> = SMART_COURSE_PRESETS.iter().map(|(name, _)| *name).collect();
    [
        (
            "power",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:washing-machine"}),
        ),
        (
            "status",
            json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":labels(STATUS)}),
        ),
        (
            "remaining_time",
            json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"}),
        ),
        (
            "start",
            json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","name":"Start","icon":"mdi:play"}),
        ),
        (
            "pause",
            json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","name":"Pause","icon":"mdi:pause"}),
        ),
        (
            "power_off",
            json!({"platform":"button","unique_id":"$deviceid-power_off","command_topic":"$this/power_off/set","name":"Power off","icon":"mdi:power"}),
        ),
        (
            "course",
            json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":labels(COURSES)}),
        ),
        (
            "smart_course",
            json!({"platform":"sensor","unique_id":"$deviceid-smart_course","state_topic":"$this/smart_course","name":"Smart course","icon":"mdi:download-circle-outline","device_class":"enum","options":labels(SMART_COURSES)}),
        ),
        (
            "downloaded_course",
            json!({"platform":"select","unique_id":"$deviceid-downloaded_course","state_topic":"$this/downloaded_course","command_topic":"$this/downloaded_course/set","name":"Downloaded course","icon":"mdi:download","options":presets}),
        ),
        (
            "door_lock",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","device_class":"lock"}),
        ),
        (
            "remote_start",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"}),
        ),
        (
            "turbo_wash",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_wash","state_topic":"$this/turbo_wash","name":"Turbo Wash","icon":"mdi:speedometer"}),
        ),
        (
            "cold_wash",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-cold_wash","state_topic":"$this/cold_wash","name":"Cold Wash","icon":"mdi:snowflake"}),
        ),
        (
            "extra_rinse",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra Rinse","icon":"mdi:water-sync"}),
        ),
        (
            "soak",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-soak","state_topic":"$this/soak","name":"Soak","icon":"mdi:water-outline"}),
        ),
        (
            "stain_care",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-stain_care","state_topic":"$this/stain_care","name":"Stain Care","icon":"mdi:liquid-spot"}),
        ),
        (
            "deep_fill",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-deep_fill","state_topic":"$this/deep_fill","name":"Deep Fill","icon":"mdi:water-plus"}),
        ),
        (
            "deep_fill_max",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-deep_fill_max","state_topic":"$this/deep_fill_max","name":"Maximum Deep Fill","icon":"mdi:water-plus"}),
        ),
        (
            "child_lock",
            json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Control Lock","icon":"mdi:lock-alert","entity_category":"diagnostic"}),
        ),
        (
            "reserve_time",
            json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","name":"Delay Wash","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"}),
        ),
        (
            "soil",
            json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot","device_class":"enum","options":labels(SOIL)}),
        ),
        (
            "spin",
            json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin speed","icon":"mdi:rotate-3d-variant","device_class":"enum","options":labels(SPIN)}),
        ),
        (
            "temp",
            json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMP)}),
        ),
        (
            "set_course",
            json!({"platform":"select","unique_id":"$deviceid-set_course","state_topic":"$this/set_course","command_topic":"$this/set_course/set","name":"Set course","icon":"mdi:pin-outline","options":setting_options(COURSES)}),
        ),
        (
            "set_soil",
            json!({"platform":"select","unique_id":"$deviceid-set_soil","state_topic":"$this/set_soil","command_topic":"$this/set_soil/set","name":"Set soil level","icon":"mdi:liquid-spot","options":setting_options(SOIL)}),
        ),
        (
            "set_spin",
            json!({"platform":"select","unique_id":"$deviceid-set_spin","state_topic":"$this/set_spin","command_topic":"$this/set_spin/set","name":"Set spin speed","icon":"mdi:rotate-3d-variant","options":setting_options(SPIN)}),
        ),
        (
            "set_temp",
            json!({"platform":"select","unique_id":"$deviceid-set_temp","state_topic":"$this/set_temp","command_topic":"$this/set_temp/set","name":"Set temperature","icon":"mdi:thermometer","options":setting_options(TEMP)}),
        ),
        (
            "set_turbo_wash",
            json!({"platform":"switch","unique_id":"$deviceid-set_turbo_wash","state_topic":"$this/set_turbo_wash","command_topic":"$this/set_turbo_wash/set","name":"Set Turbo Wash","icon":"mdi:speedometer"}),
        ),
        (
            "set_cold_wash",
            json!({"platform":"switch","unique_id":"$deviceid-set_cold_wash","state_topic":"$this/set_cold_wash","command_topic":"$this/set_cold_wash/set","name":"Set Cold Wash","icon":"mdi:snowflake"}),
        ),
        (
            "set_extra_rinse",
            json!({"platform":"switch","unique_id":"$deviceid-set_extra_rinse","state_topic":"$this/set_extra_rinse","command_topic":"$this/set_extra_rinse/set","name":"Set Extra Rinse","icon":"mdi:water-sync"}),
        ),
        (
            "set_extra_rinses",
            json!({"platform":"number","unique_id":"$deviceid-set_extra_rinses","state_topic":"$this/set_extra_rinses","command_topic":"$this/set_extra_rinses/set","name":"Set extra rinses","icon":"mdi:water-sync","min":0,"max":2,"step":1,"mode":"box"}),
        ),
        (
            "set_stain_care",
            json!({"platform":"switch","unique_id":"$deviceid-set_stain_care","state_topic":"$this/set_stain_care","command_topic":"$this/set_stain_care/set","name":"Set Stain Care","icon":"mdi:liquid-spot"}),
        ),
        (
            "set_deep_fill",
            json!({"platform":"switch","unique_id":"$deviceid-set_deep_fill","state_topic":"$this/set_deep_fill","command_topic":"$this/set_deep_fill/set","name":"Set Deep Fill","icon":"mdi:water-plus"}),
        ),
        (
            "set_soak",
            json!({"platform":"switch","unique_id":"$deviceid-set_soak","state_topic":"$this/set_soak","command_topic":"$this/set_soak/set","name":"Set Soak","icon":"mdi:water-outline"}),
        ),
        (
            "set_deep_fill_max",
            json!({"platform":"switch","unique_id":"$deviceid-set_deep_fill_max","state_topic":"$this/set_deep_fill_max","command_topic":"$this/set_deep_fill_max/set","name":"Set Maximum Deep Fill","icon":"mdi:water-plus"}),
        ),
        (
            "set_delay_wash",
            json!({"platform":"number","unique_id":"$deviceid-set_delay_wash","state_topic":"$this/set_delay_wash","command_topic":"$this/set_delay_wash/set","name":"Set Delay Wash","icon":"mdi:timer-sand","unit_of_measurement":"h","min":0,"max":19,"step":1,"mode":"box"}),
        ),
        (
            "reset_settings",
            json!({"platform":"button","unique_id":"$deviceid-reset_settings","command_topic":"$this/reset_settings/set","name":"Reset settings","icon":"mdi:restore"}),
        ),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const MODEL_ID: &str = "T17A1EFHU_F";
    const ID: &str = "test-id";
    const STATUS_REQUEST: &str = "AA0EF0ED1121010000001800B5BB";

    // Reply to the status query: standby on Heavy Duty, 1:37, Heavy/High/Warm.
    const STATUS_REPLY: &str = "aaff200a002f006dc1000100eb001d000001012504350200050304000000000000000000000000007b0000005a57bb";
    // Dial on Pre Wash+Normal at its defaults: 1:24, Normal/High/Warm, Turbo Wash on.
    const PREWASH_NORMAL: &str = "AAFF200A004C0067D7000100EC003A000001003B003B0000000000000000000080000000000000007B000000000001011804282900030304000000004000000000000000007B000000DFEDBB";
    // Dial turned to Heavy Duty: 1:37, Heavy/High/Warm, Turbo Wash off.
    const HEAVY_DUTY: &str = "AAFF200A004C0067F7000100EC003A000001011804282900030304000000004000000000000000007B000000000001012504350200050304000000000000000000000000007B000000512ABB";
    // Remote Start held on Heavy Duty: record byte 18 changes first, the lid locks a frame
    // later. Remote Start is then switched off again.
    const REMOTE_ARMED: &str = "AAFF200A004C0067FA000100EC003A000001012504350200050304000000000000000000000000007B000000000001012504350200050304000000000000010000000000007B0000002463BB";
    const REMOTE_LOCKED: &str = "AAFF200A004C0067FB000100EC003A000001012504350200050304000000000000010000000000007B000000000001012504350200050304000000000400010000000000007B000000B7D9BB";
    const REMOTE_DISARMED: &str = "AAFF200A004C006806000100EC003A000001012504350200050304000000000400010000000000007B000000000001012504350200050304000000000000000000000000007B000000E0C9BB";
    // Dial turned to Normal: 0:59, Normal/High/Warm, Turbo Wash on. The old record is Heavy Duty.
    const NORMAL: &str = "AAFF200A004C006818000100EC003A000001012504350200050304000000000000000000000000007B000000000001003B003B0100030304000000004000000000000000007B000000C405BB";
    // Start pressed on Normal (Detecting, lid locked), then Pause, then the cycle is cancelled.
    const DETECTING: &str = "AAFF200A004C006821000100EC003A000001003B003B0100030304000000004000000000000000007B000000000003003B003B0100030304000000004400000000010000007B0000008A2ABB";
    const PAUSED: &str = "AAFF200A004C00682D000100EC003A000003003B003B0100030304000000004400000000010000007B000001000002003B003B0100030304000000004400000000030000007B0000015228BB";
    const CANCELLED: &str = "AAFF200A004C006833000100EC003A000002003B003B0100030304000000004400000000030000007B000001000001003B003B0100030304000000004000000000020000007B0000019B2BBB";
    // Normal changed from Warm to Hot.
    const NORMAL_HOT: &str = "AAFF200A004C006856000100EC003A000001003B003B0100030304000000004000000000020000007B000001000001003B003B0100030305000000004000000000020000007B000001C4D9BB";
    // One full Deep Wash cycle with Extra Rinse, Medium spin and Cold.
    const DEEP_WASH_STARTED: &str = "AAFF200A004C00685B000100EC003A000001003B003B0100030305000000004000000000020000007B000001000003011204221900030202020000000401000000010000007B0000007610BB";
    const WASHING: &str = "AAFF200A004C00688A000100EC003A000005010304151900030202020000000405000000030000007B000001000005010204151900030202020000000405000000030000007B000001DED9BB";
    const RINSING: &str = "AAFF200A004C006A02000100EC003A000005002A04151900030202020000000401000000030000007B00001F000006002A04151900000202020000000401000000050000007B00001F0D03BB";
    const SPINNING: &str = "AAFF200A004C006BAF000100EC003A000006000B04151900000202020000000401000000050000007B00002F000007000B04151900000200020000000401000000060000007B00002F820EBB";
    const END: &str = "AAFF200A004C006C69000100EC003A000007000104151900000200020000000401000000060000007B00003C000008000104151900000200020000000001000002070000007B00003C2186BB";
    // E2 settings echo after END. Its record is the cycle's starting state (Detecting, 1:18).
    const E2_AFTER_END: &str = "AAFF200A002F006C6A00010AE2001D000003011204221900030202020000000001000002010000007B00003C2442BB";
    // No status: the settings sync after a dial change, periodic telemetry (envelope type 02,
    // not EC), the Remote Start echo and the heartbeat.
    const SETTINGS_SYNC: &str = "AAFF200A005B006815000101030049010A010301F80103018A1800000800020003030104000002003B003B0000007B21010201040103010301030103010301030103010301030103013901030139010401040104010401033F6EBB";
    const PERIODIC_DATA: &str = "AAFF200A004C00689C00010102003A0A010365FD0503018B33000401010501020000222A29272522290F0F0F0F0F0F0F472E3D45474735012C001E01580037000000010000000000019771BB";
    const REMOTE_ECHO: &str = "AA09207200C9005BBB";
    const HEARTBEAT: &str = "AA0720D82D83BB";
    // Local command test: this physically started Heavy Duty with Heavy soil, High spin and
    // Warm water. The ack and settings sync are the captured replies.
    const START_COMMAND: &str = "AA15F026020503040000000000000000000003B3BB";
    const START_ACK: &str = "aa0820002600adbb";
    const START_SETTINGS_SYNC: &str = "aaff200a005b006de4000101030049010a0103011a0103018c1800000800020203040104000000012501250000007b30023b01040104010100000000000000000000000000000000000000000000000000000000000000009fd3bb";
    // Status after AA09F02404010099BB paused that cycle.
    const LOCAL_PAUSED: &str = "aaff200a002f006e54000100eb001d000002011204260200050304000000000000000000050000007b000002b0d5bb";
    // Sent from LOCAL_PAUSED; the washer resumed. The reply was START_ACK, then telemetry.
    const RESUME_COMMAND: &str = "aa15f026020503040000000000000000000001b1bb";
    const RESUME_TELEMETRY: &str = "aaff200a004c006e7400010102003a0a0103651f0203018c3300050201160112000028322f30302f2e0f0f0f0f0f0f0f47343737363937019400000158003700000001000000000002d5ccbb";
    // This powered the washer off. Neither reply contains a status record.
    const POWER_OFF_COMMAND: &str = "aa09f0240101009cbb";
    const POWER_OFF_ACK: &str = "aa0820002400a3bb";
    const POWER_OFF_SETTINGS_SYNC: &str =
        "aaff200a0022006e9e0001010300100e0a01030e210503018c0800070000046dd9bb";
    // The panel was set to Heavy Duty / Warm; this started it with Cold instead.
    const COLD_START_COMMAND: &str = "AA15F026020503020000000000000000000003B1BB";
    const COLD_START_SETTINGS_SYNC: &str = "aaff200a005b006f44000101030049010a010301250103018d1800000800020202040104000000012e012e0000007b3a023b0139010401040101023f00000000000000000000000000000000000000000000000000000000a120bb";
    const COLD_START_STATUS: &str = "aaff200a002f006f51000100eb001d000003012e05020200050302000000000400010000010000007b000000db2fbb";
    // Remote Start armed on Normal / High / Warm / Turbo Wash.
    const LOCAL_NORMAL_ARMED: &str = "aaff200a002f006f75000100eb001d000001003b003b0100030304000000004400010000000000007b0000011823bb";
    // This changed that selection to Deep Wash / Medium / Cold / two extra rinses.
    const DEEP_WASH_COMMAND: &str = "AA15F026190302020200000000000000000103AEBB";
    const DEEP_WASH_SETTINGS_SYNC: &str = "aaff200a005b006f7a000101030049010a010301270103018e1800000800020a02030303000040012501250000007b3f023b013901040101010100000000000000000000000000000000000000000000000000000000000067babb";
    const LOCAL_DEEP_WASH: &str = "aaff200a002f006f89000100eb001d000005011104211900030202040000000405010000030000007b000000acf9bb";
    // Sent with Heavy Duty at the panel: Normal soil, High, Warm, Turbo Wash on, Extra Rinse off.
    const NORMAL_TURBO_COMMAND: &str = "AA15F02601030304000000000000000040000376BB";
    const NORMAL_TURBO_SETTINGS_SYNC: &str = "aaff200a005b006fc2000101030049010a010301290103018f1800000800020003030104000002003b003b0000007b4b023b013901040101010101020102010201030102010201020000000000000000000000000000000009a9bb";
    const NORMAL_TURBO_STATUS: &str = "aaff200a002f006fc3000100eb001d000003003b003b0100030304000000004400010000010000007b000001bc58bb";
    // Downloads while idle on Downloaded. F0 25 writes the slot without starting a wash.
    const DOWNLOAD_ACK: &str = "AA0820002500A2BB";
    const SMALL_LOAD_COMMAND: &str = "AA17F025030F010102040000000000000064400000C1BB";
    const SMALL_LOAD_UPDATE: &str = "aaff200a004c0072f5000100ec003a000001011e042e0100030404020000004001000000007b00007b000000000001003b003b010001020400000000400000000000640000640000008c35bb";
    const SMALL_LOAD_STATUS: &str = "aaff200a002f0072fc000100eb001d000001003b003b010001020400000000400000000000640000640000002eb3bb";
    // Command rinse values 2 and 1 reported 4 and 2. Options 0x21 without data[8] left Soak off.
    const JUICE_TWO_RINSES: &str = "aaff200a002f007327000100eb001d000001020608260200050305040000004001000000007400007400000025aebb";
    const JUICE_ONE_RINSE: &str = "aaff200a002f007347000100eb001d000001012b043b02000503050200000040010000000074000074000000eebebb";
    // Soak at the panel: options 0x01 -> 0x21 and byte 26 from 0 to 1.
    const PANEL_SOAK_UPDATE: &str = "aaff200a004c007381000100ec003a000001012b043b02000503050200000040010000000074000074000000000001020d082d0200050305020000004021000000007400007401000090b5bb";
    const PANEL_SOAK_STATUS: &str = "aaff200a002f007386000100eb001d000001020d082d02000503050200000040210000000074000074010000c99bbb";
    // Re-sending the download with options 0x21 but data[8] = 0 clears panel-selected Soak.
    const DOWNLOAD_CLEARS_SOAK: &str = "aaff200a004c0073d2000100ec003a000001020d082d02000503050200000040210000000074000074010000000001012b043b02000503050200000040010000000074000074000000400fbb";
    // F0 26 with options 0x20 was accepted without enabling Soak.
    const START_IGNORES_SOAK: &str = "aaff200a002f007438000100eb001d000003003b003b01000303040000000044000100000100000074000000fd89bb";
    // Collar Stains enabled Stain Care. The washer overrode Turbo Wash / Warm / no rinse with
    // Turbo off / Semi Warm / one rinse.
    const COLLAR_STAINS_COMMAND: &str = "AA17F025030F02050304000000000000006E400800F9BB";
    const COLLAR_STAINS_UPDATE: &str = "aaff200a004c00745d000100ec003a000001003b003b01000303040000000040000000000000000074000000000001011504250200050303020000000008000000006e00006e0000006a7abb";
    const COLLAR_STAINS_STATUS: &str = "aaff200a002f007463000100eb001d000001011504250200050303020000000008000000006e00006e000000e13dbb";
    // Labelled panel captures: Deep Fill from Normal selects Deep Wash; Delay Wash 1 h then 2 h.
    const DEEP_FILL_FROM_NORMAL: &str = "aaff200a004c0074a8000100ec003a000001003b003b010003030400000000400000000000000000740000000000010039003919000302040000000000020000000000000074000000655fbb";
    const DELAY_ONE_HOUR: &str = "aaff200a004c0074ba000100ec003a0000010039003919000302040000000000020000000000000074000000000001003900391900030204000001000002000000000000007400000002a1bb";
    const DELAY_TWO_HOURS: &str = "aaff200a004c0074bf000100ec003a000001003900391900030204000001000002000000000000007400000000000100390039190003020400000200000200000000000000740000002c36bb";
    // F0 25 with Deep Fill and reserveHour 2: Deep Fill took effect, the delay did not.
    const DOWNLOAD_DEEP_FILL_STATUS: &str = "aaff200a002f0074e1000100eb001d000001003b003b01000102040000000040020000000064000064000000539fbb";
    // F0 26 with that delay field started immediately.
    const START_IGNORES_DELAY: &str = "aaff200a002f007512000100eb001d000003003b003b0100030304000000004400010000010000007400000079ecbb";
    // LG's Sweat Stains download sets data[8] as well as options bit 0x20, which enables Soak.
    const SWEAT_STAINS_COMMAND: &str = "aa17f025030f020303040000000001000069402000ebbb";
    const SWEAT_STAINS_UPDATE: &str = "aaff200a004c00759d000100ec003a000001011804282900030304000000004000000000000000007400000100000101300504020003030400000000402000000000690000690100015180bb";
    // LG's Start with Sweat Stains, Soak and a two-hour Delay Wash; the reply enters phase 9.
    const SOAK_DELAY_ARMED: &str = "aaff200a004c0075ae000100ec003a00000101300504020003030400000200402001000000690000690100010000010130050402000303040000020044200100000069000069010001cc01bb";
    const SOAK_DELAY_START_COMMAND: &str = "aa15f026020303040000000001020069402003e5bb";
    const SOAK_DELAY_STARTED: &str = "aaff200a004c0075ba000100ec003a0000010130050402000303040000020044200100000069000069010001000009013005040200030304000002004420010000016900006901000102f7bb";
    // Deep Wash: Deep Fill held for Max, then Signal held to enable and disable Control Lock.
    const MAX_FILL_ON: &str = "aaff200a004c0075fe000100ec003a00000100390039190003020400000000000000000002000000690000000000010039003919000302040000000000000800000200000069000000b5e7bb";
    const CONTROL_LOCK_ON: &str = "aaff200a004c007605000100ec003a0000010039003919000302040000000000000800000200000069000000000001003900391900030204000000000100080000020000006900000031f4bb";
    const CONTROL_LOCK_OFF: &str = "aaff200a004c007607000100ec003a000001003900391900030204000000000100080000020000006900000000000100390039190003020400000000000008000002000000690000004322bb";
    // LG's Start with Max adds 0x08 to the verb; the washer clears its Max flag as it starts.
    const MAX_FILL_ARMED: &str = "aaff200a004c00761a000100ec003a0000010039003919000302040000000000000900000200000069000000000001003900391900030204000000000400090000020000006900000032c3bb";
    const MAX_FILL_START_COMMAND: &str = "aa15f02619030204000000000000000000000b57bb";
    const MAX_FILL_STARTED: &str = "aaff200a004c007622000100ec003a00000100390039190003020400000000040009000002000000690000000000030039003919000302040000000004000100000100000069000000b078bb";
    // Cold Wash pressed on Normal.
    const COLD_WASH_ON: &str = "aaff200a004c007635000100ec003a000001003b003b01000303040000000040000000000000000069000000000001011c042c01000303020000000020000000000000000069000000df63bb";
    // LG's Normal + Cold Wash Start: Cold, Turbo Wash off, flag 0x20.
    const COLD_WASH_ARMED: &str = "aaff200a004c007648000100ec003a000001011c042c01000303020000000020000100000000000069000000000001011c042c01000303020000000024000100000000000069000000c261bb";
    const COLD_WASH_COMMAND: &str = "aa15f02601030302000000000000000020000354bb";
    const COLD_WASH_STARTED: &str = "aaff200a004c00765c000100ec003a000001011c042c01000303020000000024000100000000000069000000000003011c042c010003030200000000240001000001000000690000008d6bbb";
    // Labelled Special Care sweep: Soak, Stain Care, Speed Wash, Rinse+Spin, Off.
    const SPECIAL_CARE_SWEEP: [&str; 5] = [
        "aaff200a004c00740c000100ec003a000001003b003b01000303040000000040000000000000000074000000000001011d042d01000303040000000040200000000000000074010000135dbb",
        "aaff200a004c007410000100ec003a000001011d042d01000303040000000040200000000000000074010000000001011f042f0100030303020000000008000000000000007400000062c9bb",
        "aaff200a004c007412000100ec003a000001011f042f01000303030200000000080000000000000074000000000001001e001e07000103040000000040000000000000000074000000e8cebb",
        "aaff200a004c007414000100ec003a000001001e001e070001030400000000400000000000000000740000000000010021002111000003010000000000000000000000000074000000a957bb",
        "aaff200a004c007415000100ec003a0000010021002111000003010000000000000000000000000074000000000001003b003b0100030304000000004000000000000000007400000004cfbb",
    ];

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, "WT7305CV", "")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    /// Upstream's `Bridge.newDevice`: the registry picks the handler, starts it, and HA
    /// commands are routed to it.
    fn bridged() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let factory = crate::registry::t2_factory(MODEL_ID).expect("registered");
        let handler = factory(ha.clone(), thinq.clone(), meta());
        handler.start();
        ha.on_set_property(move |_, prop, value| handler.set_property(prop, value));
        (ha, thinq)
    }

    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    fn feed(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        for frame in frames {
            emit(&thinq, frame);
        }
        ha
    }

    fn config(ha: &MockHaConnection) -> rethink_core::ha::DeviceDiscovery {
        ha.device(ID).unwrap().config.unwrap()
    }

    fn props(ha: &MockHaConnection) -> HashMap<String, PropertyValue> {
        ha.device(ID).unwrap().properties
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn packets(hexes: &[&str]) -> Vec<Vec<u8>> {
        hexes.iter().map(|hex| hex_decode(hex)).collect()
    }

    /// The payload between `AA <len>` and `<checksum> BB`.
    fn inner(packet: &[u8]) -> Vec<u8> {
        packet[2..packet.len() - 2].to_vec()
    }

    fn acks(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .sent()
            .iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_bridge_requests_a_snapshot_on_connect_and_decodes_the_captured_reply() {
        let (ha, thinq) = bridged();
        // Upstream checks `instanceof DUT`: the registered handler publishes this discovery.
        let (direct, _, _) = make();
        assert_eq!(config(&ha).components, config(&direct).components);
        assert_eq!(thinq.outbox(), packets(&[STATUS_REQUEST]));
        emit(&thinq, STATUS_REPLY);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "remaining_time", "97");
        expect(&ha, "soil", "Heavy");
        expect(&ha, "spin", "High");
        expect(&ha, "temp", "Warm");
        expect(&ha, "turbo_wash", "OFF");
        expect(&ha, "extra_rinse", "OFF");
        expect(&ha, "door_lock", "ON");
        expect(&ha, "remote_start", "OFF");
        assert_eq!(config(&ha).device.model.as_deref(), Some("WT7305CV"));
    }

    #[test]
    fn the_current_state_is_the_last_record_of_the_envelope() {
        // old record Heavy Duty, current record Normal
        let ha = feed(&[NORMAL]);
        expect(&ha, "course", "Normal");
        expect(&ha, "remaining_time", "59");
    }

    #[test]
    fn status_retransmissions_and_settings_sync_are_acknowledged_by_sequence_number() {
        // unacked, this washer repeats each envelope every ~1.6 s
        let (_, thinq, _) = make();
        emit(&thinq, NORMAL);
        emit(&thinq, NORMAL);
        emit(&thinq, SETTINGS_SYNC);
        let ack = |body: &str| hex_encode(&wrap_aabb(&hex_decode(body)));
        assert_eq!(
            acks(&thinq),
            [
                ack("f0000a046818"),
                ack("f0000a046818"),
                ack("f0000a046815")
            ]
        );
    }

    #[test]
    fn dial_selections_decode_their_captured_settings() {
        let prewash = feed(&[PREWASH_NORMAL]);
        expect(&prewash, "course", "Pre Wash+Normal");
        expect(&prewash, "remaining_time", "84");
        expect(&prewash, "turbo_wash", "ON");

        let heavy = feed(&[HEAVY_DUTY]);
        expect(&heavy, "course", "Heavy Duty");
        expect(&heavy, "remaining_time", "97");
        expect(&heavy, "soil", "Heavy");
        expect(&heavy, "spin", "High");
        expect(&heavy, "temp", "Warm");
        expect(&heavy, "turbo_wash", "OFF");

        let normal = feed(&[NORMAL]);
        expect(&normal, "power", "ON");
        expect(&normal, "status", "Standby");
        expect(&normal, "soil", "Normal");
        expect(&normal, "spin", "High");
        expect(&normal, "temp", "Warm");
        expect(&normal, "turbo_wash", "ON");
        expect(&normal, "extra_rinse", "OFF");

        expect(&feed(&[NORMAL_HOT]), "temp", "Hot");
    }

    #[test]
    fn deep_wash_with_extra_rinse_medium_spin_and_cold() {
        let ha = feed(&[DEEP_WASH_STARTED]);
        expect(&ha, "status", "Detecting");
        expect(&ha, "course", "Deep Wash");
        expect(&ha, "remaining_time", "78");
        expect(&ha, "soil", "Normal");
        expect(&ha, "spin", "Medium");
        expect(&ha, "temp", "Cold");
        expect(&ha, "extra_rinse", "ON");
        expect(&ha, "turbo_wash", "OFF");
    }

    #[test]
    fn remote_start_arms_first_the_lid_locks_a_frame_later_and_both_clear_together() {
        let armed = feed(&[REMOTE_ARMED]);
        expect(&armed, "remote_start", "ON");
        expect(&armed, "door_lock", "ON"); // device_class lock: ON = unlocked

        let locked = feed(&[REMOTE_LOCKED]);
        expect(&locked, "remote_start", "ON");
        expect(&locked, "door_lock", "OFF");

        let disarmed = feed(&[REMOTE_DISARMED]);
        expect(&disarmed, "remote_start", "OFF");
        expect(&disarmed, "door_lock", "ON");
    }

    #[test]
    fn start_pause_and_cancel() {
        let detecting = feed(&[DETECTING]);
        expect(&detecting, "status", "Detecting");
        expect(&detecting, "door_lock", "OFF");

        expect(&feed(&[PAUSED]), "status", "Paused");

        let cancelled = feed(&[CANCELLED]);
        expect(&cancelled, "status", "Standby");
        expect(&cancelled, "door_lock", "ON");
    }

    #[test]
    fn a_whole_cycle_walks_through_every_phase_with_a_falling_countdown() {
        let (ha, thinq, _) = make();
        for (frame, status, minutes) in [
            (DEEP_WASH_STARTED, "Detecting", "78"),
            (WASHING, "Washing", "62"),
            (RINSING, "Rinsing", "42"),
            (SPINNING, "Spinning", "11"),
            (END, "End", "1"),
        ] {
            emit(&thinq, frame);
            expect(&ha, "status", status);
            expect(&ha, "remaining_time", minutes);
            expect(&ha, "extra_rinse", "ON");
            expect(&ha, "turbo_wash", "OFF");
            expect(&ha, "remote_start", "OFF");
        }
        expect(&ha, "soil", "Not selected");
        expect(&ha, "temp", "Not selected");
        expect(&ha, "door_lock", "ON");
        expect(&ha, "power", "ON");
    }

    #[test]
    fn the_settings_echo_after_end_does_not_throw_the_machine_back_into_its_wash() {
        assert!(prop(&feed(&[E2_AFTER_END]), "power").is_none());

        let ha = feed(&[END, E2_AFTER_END]);
        expect(&ha, "status", "End");
        expect(&ha, "remaining_time", "1");
    }

    #[test]
    fn other_packet_types_do_not_publish_or_overwrite_status() {
        for junk in [
            SETTINGS_SYNC,
            PERIODIC_DATA,
            REMOTE_ECHO,
            HEARTBEAT,
            START_ACK,
            START_SETTINGS_SYNC,
            COLD_START_SETTINGS_SYNC,
            DEEP_WASH_SETTINGS_SYNC,
            NORMAL_TURBO_SETTINGS_SYNC,
            DOWNLOAD_ACK,
            POWER_OFF_ACK,
            POWER_OFF_SETTINGS_SYNC,
        ] {
            let (ha, thinq, _) = make();
            emit(&thinq, junk);
            assert!(props(&ha).is_empty(), "{junk}");
            emit(&thinq, WASHING);
            let before = props(&ha);
            emit(&thinq, junk);
            assert_eq!(props(&ha), before, "{junk}");
        }
    }

    #[test]
    fn the_fixed_04_15_field_is_not_published_as_an_initial_time() {
        // it read 04 15 through this whole 62-minute cycle
        let ha = feed(&[DEEP_WASH_STARTED, WASHING, RINSING, SPINNING, END]);
        assert!(prop(&ha, "initial_time").is_none());
        assert!(!config(&ha).components.contains_key("initial_time"));
    }

    #[test]
    fn start_reproduces_the_command_that_physically_started_heavy_duty() {
        let (ha, thinq, dev) = make();
        // A captured armed Heavy Duty snapshot with the same settings as the local command test.
        // Its lid-lock bit must be cleared in the command, leaving the course options untouched.
        emit(&thinq, REMOTE_LOCKED);
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[START_COMMAND]));

        emit(&thinq, START_ACK);
        emit(&thinq, START_SETTINGS_SYNC);
        emit(&thinq, START_SETTINGS_SYNC);
        let acks = acks(&thinq);
        assert_eq!(
            acks[acks.len() - 2..],
            ["AA0AF0000A046DE456BB", "AA0AF0000A046DE456BB"]
        );
        // The acknowledgement and settings sync do not report a phase; wait for an EB/EC update.
        expect(&ha, "status", "Standby");
        assert_eq!(thinq.outbox(), packets(&[START_COMMAND]));
    }

    #[test]
    fn start_waits_for_actual_settings_before_constructing_a_command() {
        let (_, thinq, dev) = make();
        dev.start();
        dev.set_property("set_course", "Heavy Duty");
        dev.set_property("set_temp", "Cold");
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[STATUS_REQUEST]));

        emit(&thinq, STATUS_REPLY);
        dev.set_property("start", "PRESS");
        assert_eq!(
            thinq.outbox(),
            packets(&[STATUS_REQUEST, COLD_START_COMMAND])
        );
    }

    #[test]
    fn the_ha_temperature_control_stages_a_change_and_reproduces_the_verified_cold_start() {
        let (ha, thinq) = bridged();
        emit(&thinq, REMOTE_LOCKED);
        thinq.reset_recorder();

        ha.set_property(ID, "set_temp", "command", "Cold");
        expect(&ha, "set_temp", "Cold");
        assert_eq!(
            prop(&ha, "temp").as_deref(),
            Some("Warm"),
            "the sensor still reports the appliance, not the pending choice"
        );
        assert!(
            thinq.outbox().is_empty(),
            "selecting a setting does not start a wash"
        );

        ha.set_property(ID, "start", "command", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[COLD_START_COMMAND]));
        emit(&thinq, START_ACK);
        emit(&thinq, COLD_START_SETTINGS_SYNC);
        expect(&ha, "status", "Standby");
        assert_eq!(
            prop(&ha, "temp").as_deref(),
            Some("Warm"),
            "acknowledgements do not pretend the requested setting took effect"
        );

        emit(&thinq, COLD_START_STATUS);
        expect(&ha, "status", "Detecting");
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "soil", "Heavy");
        expect(&ha, "spin", "High");
        expect(&ha, "temp", "Cold");
        expect(&ha, "set_temp", "Cold");
        expect(&ha, "remaining_time", "106");

        // After submitting the selection, the controls follow later panel settings again.
        emit(&thinq, NORMAL_HOT);
        expect(&ha, "set_temp", "Hot");
    }

    #[test]
    fn all_setting_controls_can_reconstruct_the_captured_heavy_duty_start_from_different_settings()
    {
        let (ha, thinq) = bridged();
        emit(&thinq, DEEP_WASH_STARTED); // Normal soil, Medium spin, Cold, Extra Rinse on
        thinq.reset_recorder();

        for (setting, value) in [
            ("set_course", "Heavy Duty"),
            ("set_soil", "Heavy"),
            ("set_spin", "High"),
            ("set_temp", "Warm"),
            ("set_turbo_wash", "OFF"),
            ("set_extra_rinse", "OFF"),
        ] {
            ha.set_property(ID, setting, "command", value);
        }

        assert!(thinq.outbox().is_empty());
        expect(&ha, "course", "Deep Wash");
        ha.set_property(ID, "start", "command", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[START_COMMAND]));
    }

    #[test]
    fn changing_normal_to_deep_wash_enables_one_extra_rinse_and_reads_back_actual_settings() {
        let (ha, thinq, dev) = make();
        emit(&thinq, LOCAL_NORMAL_ARMED);
        for (setting, value) in [
            ("set_course", "Deep Wash"),
            ("set_soil", "Normal"),
            ("set_spin", "Medium"),
            ("set_temp", "Cold"),
            ("set_turbo_wash", "OFF"),
            ("set_extra_rinse", "ON"),
        ] {
            dev.set_property(setting, value);
        }
        assert!(thinq.outbox().is_empty());
        dev.set_property("start", "PRESS");
        // The physical test sent two extra rinses. The switch now enables one; every other
        // field still matches that captured command.
        let mut one_rinse = inner(&hex_decode(DEEP_WASH_COMMAND));
        one_rinse[6] = 1;
        assert_eq!(thinq.outbox(), [wrap_aabb(&one_rinse)]);

        emit(&thinq, START_ACK);
        emit(&thinq, DEEP_WASH_SETTINGS_SYNC);
        expect(&ha, "course", "Normal");
        emit(&thinq, LOCAL_DEEP_WASH);
        expect(&ha, "status", "Washing");
        expect(&ha, "course", "Deep Wash");
        expect(&ha, "soil", "Normal");
        expect(&ha, "spin", "Medium");
        expect(&ha, "temp", "Cold");
        expect(&ha, "turbo_wash", "OFF");
        expect(&ha, "extra_rinse", "ON");
        expect(&ha, "remaining_time", "77");

        // Re-selecting ON preserves the reported two-rinse selection, converting status 4 to command 2.
        thinq.reset_recorder();
        dev.set_property("set_extra_rinse", "ON");
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox()[0][8], 2);
    }

    #[test]
    fn pending_choices_survive_reports_while_untouched_settings_follow_the_latest_record() {
        let (ha, thinq, dev) = make();
        emit(&thinq, NORMAL);
        dev.set_property("set_soil", "Heavy");
        emit(&thinq, NORMAL_HOT);
        expect(&ha, "soil", "Normal");
        expect(&ha, "set_soil", "Heavy");
        expect(&ha, "temp", "Hot");
        expect(&ha, "set_temp", "Hot");

        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[0]);
        assert_eq!(command[3], 5, "pending Heavy soil");
        assert_eq!(command[5], 5, "latest reported Hot temperature");
    }

    #[test]
    fn soil_and_turbo_wash_selections_reproduce_the_verified_normal_start() {
        let (ha, thinq, dev) = make();
        emit(&thinq, REMOTE_LOCKED); // Heavy Duty, Heavy soil, Turbo Wash off
        for (setting, value) in [
            ("set_course", "Normal"),
            ("set_soil", "Normal"),
            ("set_spin", "High"),
            ("set_temp", "Warm"),
            ("set_turbo_wash", "ON"),
            ("set_extra_rinse", "OFF"),
        ] {
            dev.set_property(setting, value);
        }
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[NORMAL_TURBO_COMMAND]));

        emit(&thinq, START_ACK);
        emit(&thinq, NORMAL_TURBO_SETTINGS_SYNC);
        expect(&ha, "turbo_wash", "OFF");
        emit(&thinq, NORMAL_TURBO_STATUS);
        expect(&ha, "status", "Detecting");
        expect(&ha, "course", "Normal");
        expect(&ha, "soil", "Normal");
        expect(&ha, "spin", "High");
        expect(&ha, "temp", "Warm");
        expect(&ha, "turbo_wash", "ON");
        expect(&ha, "extra_rinse", "OFF");
        expect(&ha, "remaining_time", "59");
    }

    #[test]
    fn option_edits_preserve_other_live_bits_and_keep_the_extra_rinse_field_consistent() {
        let (_, thinq, dev) = make();
        emit(&thinq, DEEP_WASH_STARTED);
        dev.set_property("set_turbo_wash", "ON");
        dev.set_property("set_extra_rinse", "OFF");
        // An otherwise unexplained 0x04 bit appears in the options after these choices were made.
        emit(&thinq, WASHING);
        dev.set_property("start", "PRESS");
        let off = inner(&thinq.outbox()[0]);
        assert_eq!(off[6], 0, "Extra Rinse selection cleared");
        assert_eq!(
            off[14], 0x40,
            "Turbo Wash enabled and the door-lock bit excluded"
        );
        assert_eq!(
            off[15], 0x04,
            "only the Extra Rinse bit cleared; live unknown bit preserved"
        );

        thinq.reset_recorder();
        emit(&thinq, STATUS_REPLY);
        dev.set_property("set_extra_rinse", "ON");
        dev.set_property("start", "PRESS");
        let on = inner(&thinq.outbox()[0]);
        assert_eq!(on[6], 1, "one Extra Rinse in command units");
        assert_eq!(on[15], 0x01, "matching Extra Rinse option bit");
    }

    #[test]
    fn use_washer_setting_releases_one_selection_and_reset_releases_every_pending_choice() {
        let (ha, thinq, dev) = make();
        emit(&thinq, STATUS_REPLY);
        dev.set_property("set_temp", "Cold");
        dev.set_property("set_soil", "Normal");
        dev.set_property("set_turbo_wash", "ON");
        dev.set_property("set_extra_rinse", "ON");
        dev.set_property("set_temp", "Use washer setting");
        expect(&ha, "set_temp", "Warm");
        expect(&ha, "set_soil", "Normal");
        expect(&ha, "set_extra_rinse", "ON");

        dev.set_property("reset_settings", "PRESS");
        expect(&ha, "set_soil", "Heavy");
        expect(&ha, "set_turbo_wash", "OFF");
        expect(&ha, "set_extra_rinse", "OFF");
        assert!(
            thinq.outbox().is_empty(),
            "reset affects only the staged choices"
        );
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[START_COMMAND]));
    }

    #[test]
    fn an_explicit_no_spin_selection_stays_zero_when_a_later_report_has_high_spin() {
        let (ha, thinq, dev) = make();
        emit(&thinq, STATUS_REPLY);
        // This model's MonitoringValue.spin table defines NO_SPIN as index 0, a valid selection.
        dev.set_property("set_spin", "No Spin");
        emit(&thinq, NORMAL_HOT);
        expect(&ha, "spin", "High");
        expect(&ha, "set_spin", "No Spin");
        assert!(thinq.outbox().is_empty());

        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[0]);
        assert_eq!(
            command[4], 0,
            "zero is sent instead of falling back to the reported High spin"
        );
        assert_eq!(
            command[5], 5,
            "untouched temperature follows the latest Hot report"
        );
        assert_eq!(
            prop(&ha, "set_spin").as_deref(),
            Some("High"),
            "after sending, the selector follows the last actual report"
        );
    }

    #[test]
    fn invalid_settings_cannot_overwrite_a_pending_choice_or_turn_an_option_off() {
        let (ha, thinq, dev) = make();
        emit(&thinq, STATUS_REPLY);
        dev.set_property("set_temp", "Cold");
        dev.set_property("set_extra_rinse", "ON");
        for (setting, value) in [
            ("set_temp", "Boiling"),
            ("set_temp", "Not selected"),
            ("set_course", "Not selected"),
            ("set_spin", "0"),
            ("set_turbo_wash", "yes"),
            ("set_extra_rinse", "2"),
            ("set_constructor", "ON"),
            ("set_toString", "ON"),
        ] {
            dev.set_property(setting, value);
        }
        assert!(thinq.outbox().is_empty());
        expect(&ha, "set_extra_rinse", "ON");
        dev.set_property("set_extra_rinse", "OFF");
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[COLD_START_COMMAND]));
    }

    #[test]
    fn pause_reproduces_the_command_that_physically_paused_the_washer() {
        let (ha, thinq, dev) = make();
        emit(&thinq, WASHING);
        dev.set_property("pause", "PRESS");
        assert_eq!(thinq.outbox(), packets(&["AA09F02404010099BB"]));
        emit(&thinq, LOCAL_PAUSED);
        expect(&ha, "status", "Paused");
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "remaining_time", "78");
        expect(&ha, "soil", "Heavy");
        expect(&ha, "spin", "High");
        expect(&ha, "temp", "Warm");
    }

    #[test]
    fn start_while_paused_reproduces_the_verified_resume_command() {
        let (ha, thinq, dev) = make();
        emit(&thinq, LOCAL_PAUSED);
        // This real paused snapshot has Remote Start off; resume still worked on the appliance.
        expect(&ha, "remote_start", "OFF");
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[RESUME_COMMAND]));

        emit(&thinq, START_ACK);
        emit(&thinq, RESUME_TELEMETRY);
        expect(&ha, "status", "Paused");
        expect(&ha, "remaining_time", "78");
    }

    #[test]
    fn the_ha_power_off_button_sends_the_command_that_physically_powered_off_the_washer() {
        let (ha, thinq) = bridged();
        thinq.reset_recorder();

        ha.set_property(ID, "set_temp", "command", "Cold");
        ha.set_property(ID, "power_off", "command", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[POWER_OFF_COMMAND]));
        expect(&ha, "set_temp", "Use washer setting");

        emit(&thinq, POWER_OFF_ACK);
        emit(&thinq, POWER_OFF_SETTINGS_SYNC);
        emit(&thinq, POWER_OFF_SETTINGS_SYNC);
        assert_eq!(
            acks(&thinq),
            ["AA0AF0000A046E9EEBBB", "AA0AF0000A046E9EEBBB"]
        );
        // A successful command acknowledgement is not a status snapshot.
        assert!(prop(&ha, "power").is_none());
        assert!(prop(&ha, "status").is_none());
    }

    #[test]
    fn the_ha_download_selector_writes_small_load_and_waits_for_the_actual_slot_report() {
        let (ha, thinq) = bridged();
        emit(&thinq, STATUS_REPLY);
        thinq.reset_recorder();
        expect(&ha, "smart_course", "Not selected");
        expect(&ha, "downloaded_course", "Full Load Wash");
        ha.set_property(ID, "set_temp", "command", "Hot");
        ha.set_property(ID, "downloaded_course", "command", "Small Load");
        assert_eq!(thinq.outbox(), packets(&[SMALL_LOAD_COMMAND]));
        expect(&ha, "downloaded_course", "Full Load Wash");
        emit(&thinq, DOWNLOAD_ACK);
        assert_eq!(
            prop(&ha, "downloaded_course").as_deref(),
            Some("Full Load Wash"),
            "ACK alone is not a setting confirmation"
        );
        emit(&thinq, SMALL_LOAD_UPDATE);
        assert_eq!(
            prop(&ha, "status").as_deref(),
            Some("Standby"),
            "downloading did not start the wash"
        );
        expect(&ha, "course", "Normal");
        expect(&ha, "smart_course", "Small Load");
        expect(&ha, "downloaded_course", "Small Load");
        expect(&ha, "soil", "Light");
        expect(&ha, "spin", "Medium");
        expect(&ha, "temp", "Warm");
        assert_eq!(
            prop(&ha, "set_temp").as_deref(),
            Some("Warm"),
            "download cleared the old pending temperature"
        );
        expect(&ha, "turbo_wash", "ON");
        expect(&ha, "extra_rinse", "OFF");
        let before = props(&ha);
        emit(&thinq, SMALL_LOAD_STATUS);
        assert_eq!(
            props(&ha),
            before,
            "EB and EC agree on the downloaded preset"
        );
    }

    #[test]
    fn a_download_can_be_sent_before_a_status_report_but_invalid_names_cannot_change_the_slot() {
        let (ha, thinq, dev) = make();
        dev.set_property("set_temp", "Cold");
        for name in [
            "Not selected",
            "Unknown course",
            "constructor",
            "toString",
            "__proto__",
        ] {
            dev.set_property("downloaded_course", name);
        }
        assert!(thinq.outbox().is_empty());
        expect(&ha, "set_temp", "Cold");
        dev.set_property("downloaded_course", "Small Load");
        assert_eq!(thinq.outbox(), packets(&[SMALL_LOAD_COMMAND]));
    }

    #[test]
    fn start_preserves_the_active_smart_course_and_a_base_course_override_clears_it() {
        let (_, thinq, dev) = make();
        emit(&thinq, SMALL_LOAD_STATUS);
        dev.set_property("set_temp", "Cold");
        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[0]);
        assert_eq!(command[13], 0x64);
        assert_eq!(command[5], 2);
        thinq.reset_recorder();
        dev.set_property("set_course", "Normal");
        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[0]);
        assert_eq!(command[2], 1);
        assert_eq!(
            command[13], 0,
            "explicit base-course choice leaves the downloaded programme"
        );
    }

    #[test]
    fn downloaded_rinse_counts_are_converted_back_to_command_units_without_doubling() {
        for (report, rinse) in [(JUICE_ONE_RINSE, 1), (JUICE_TWO_RINSES, 2)] {
            let (ha, thinq, dev) = make();
            emit(&thinq, report);
            expect(&ha, "course", "Heavy Duty");
            expect(&ha, "smart_course", "Juice & Food Stains");
            expect(&ha, "downloaded_course", "Juice & Food Stains");
            expect(&ha, "soak", "OFF");
            dev.set_property("set_extra_rinse", "ON");
            dev.set_property("start", "PRESS");
            assert_eq!(thinq.outbox()[0][8], rinse);
        }
    }

    #[test]
    fn soak_follows_panel_and_download_reports_instead_of_assuming_a_preset_enabled_it() {
        let (ha, thinq, _) = make();
        emit(&thinq, JUICE_ONE_RINSE);
        expect(&ha, "soak", "OFF");
        for report in [PANEL_SOAK_UPDATE, PANEL_SOAK_STATUS] {
            emit(&thinq, report);
            expect(&ha, "soak", "ON");
            expect(&ha, "remaining_time", "133");
        }
        emit(&thinq, DOWNLOAD_CLEARS_SOAK);
        expect(&ha, "soak", "OFF");
        expect(&ha, "remaining_time", "103");
        emit(&thinq, START_IGNORES_SOAK);
        expect(&ha, "status", "Detecting");
        expect(&ha, "soak", "OFF");
        expect(&ha, "remaining_time", "59");
    }

    #[test]
    fn the_labelled_special_care_sweep_separates_options_from_base_courses() {
        let (ha, thinq, _) = make();
        let expected = [
            ("Normal", "ON", "OFF", "89"),
            ("Normal", "OFF", "ON", "91"),
            ("Speed Wash", "OFF", "OFF", "30"),
            ("Rinse+Spin", "OFF", "OFF", "33"),
            ("Normal", "OFF", "OFF", "59"),
        ];
        for (frame, (course, soak, stain_care, minutes)) in
            SPECIAL_CARE_SWEEP.into_iter().zip(expected)
        {
            emit(&thinq, frame);
            let decoded = ["course", "soak", "stain_care", "remaining_time"].map(|p| prop(&ha, p));
            assert_eq!(
                decoded,
                [course, soak, stain_care, minutes].map(|v| Some(v.to_string()))
            );
            expect(&ha, "smart_course", "Not selected");
            assert_eq!(
                prop(&ha, "downloaded_course").as_deref(),
                Some("Juice & Food Stains"),
                "leaving Downloaded preserves its stored slot"
            );
        }
    }

    #[test]
    fn collar_stains_sends_the_verified_download_and_reports_appliance_adjusted_settings() {
        let (ha, thinq, dev) = make();
        emit(&thinq, NORMAL);
        dev.set_property("downloaded_course", "Collar Stains");
        assert_eq!(thinq.outbox(), packets(&[COLLAR_STAINS_COMMAND]));
        emit(&thinq, DOWNLOAD_ACK);
        expect(&ha, "stain_care", "OFF");
        for report in [COLLAR_STAINS_UPDATE, COLLAR_STAINS_STATUS] {
            emit(&thinq, report);
            expect(&ha, "downloaded_course", "Collar Stains");
            expect(&ha, "smart_course", "Collar Stains");
            expect(&ha, "course", "Heavy Duty");
            expect(&ha, "stain_care", "ON");
            expect(&ha, "set_stain_care", "ON");
            expect(&ha, "turbo_wash", "OFF");
            expect(&ha, "temp", "Semi Warm");
            expect(&ha, "remaining_time", "81");
        }
    }

    #[test]
    fn stain_care_stages_independently_and_follows_reports_again_after_reset() {
        let (ha, thinq, dev) = make();
        emit(&thinq, NORMAL);
        dev.set_property("set_stain_care", "ON");
        dev.set_property("set_stain_care", "invalid");
        emit(&thinq, NORMAL_HOT);
        expect(&ha, "stain_care", "OFF");
        expect(&ha, "set_stain_care", "ON");
        assert!(thinq.outbox().is_empty());
        dev.set_property("start", "PRESS");
        assert_eq!(inner(&thinq.outbox()[0])[15], 0x08);
        assert_eq!(
            prop(&ha, "set_stain_care").as_deref(),
            Some("OFF"),
            "Start clears the submitted override"
        );
        emit(&thinq, COLLAR_STAINS_STATUS);
        dev.set_property("set_stain_care", "OFF");
        expect(&ha, "stain_care", "ON");
        expect(&ha, "set_stain_care", "OFF");
        dev.set_property("reset_settings", "PRESS");
        expect(&ha, "set_stain_care", "ON");
    }

    #[test]
    fn deep_fill_and_delay_wash_follow_the_labelled_panel_settings() {
        let (ha, thinq, dev) = make();
        emit(&thinq, NORMAL);
        expect(&ha, "deep_fill", "OFF");
        emit(&thinq, DEEP_FILL_FROM_NORMAL);
        expect(&ha, "course", "Deep Wash");
        expect(&ha, "deep_fill", "ON");
        expect(&ha, "reserve_time", "0");
        emit(&thinq, DELAY_ONE_HOUR);
        expect(&ha, "reserve_time", "60");
        emit(&thinq, DELAY_TWO_HOURS);
        expect(&ha, "reserve_time", "120");
        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[0]);
        assert_eq!(
            command[15] & 2,
            2,
            "Start preserves panel-selected Deep Fill"
        );
    }

    #[test]
    fn deep_fill_control_uses_the_verified_bit_and_delay_monitoring_reports_what_took_effect() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SMALL_LOAD_STATUS);
        dev.set_property("set_deep_fill", "ON");
        assert!(thinq.outbox().is_empty());
        dev.set_property("start", "PRESS");
        assert_eq!(inner(&thinq.outbox()[0])[15], 2);
        emit(&thinq, DOWNLOAD_DEEP_FILL_STATUS);
        expect(&ha, "deep_fill", "ON");
        expect(&ha, "set_deep_fill", "ON");
        expect(&ha, "reserve_time", "0");
        dev.set_property("set_deep_fill", "OFF");
        expect(&ha, "deep_fill", "ON");
        dev.set_property("start", "PRESS");
        assert_eq!(inner(&thinq.outbox()[1])[15], 0);
        emit(&thinq, START_IGNORES_DELAY);
        expect(&ha, "status", "Detecting");
        expect(&ha, "reserve_time", "0");
    }

    #[test]
    fn sweat_stains_download_matches_lg_exactly_and_confirms_soak_from_the_appliance() {
        let (ha, thinq) = bridged();
        emit(&thinq, SMALL_LOAD_STATUS);
        thinq.reset_recorder();
        ha.set_property(ID, "downloaded_course", "command", "Sweat Stains");
        assert_eq!(thinq.outbox(), packets(&[SWEAT_STAINS_COMMAND]));
        emit(&thinq, DOWNLOAD_ACK);
        assert_eq!(
            prop(&ha, "soak").as_deref(),
            Some("OFF"),
            "wait for the actual setting even with a known command"
        );
        emit(&thinq, SWEAT_STAINS_UPDATE);
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "smart_course", "Sweat Stains");
        expect(&ha, "downloaded_course", "Sweat Stains");
        expect(&ha, "soak", "ON");
        expect(&ha, "set_soak", "ON");
        expect(&ha, "soil", "Normal");
        expect(&ha, "spin", "High");
        expect(&ha, "temp", "Warm");
        expect(&ha, "turbo_wash", "ON");
        expect(&ha, "extra_rinse", "OFF");
        expect(&ha, "remaining_time", "108");
    }

    #[test]
    fn soak_overrides_and_panel_selections_encode_both_command_fields_together() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SMALL_LOAD_STATUS);
        dev.set_property("set_soak", "ON");
        dev.set_property("set_soak", "invalid");
        emit(&thinq, SMALL_LOAD_STATUS);
        expect(&ha, "soak", "OFF");
        expect(&ha, "set_soak", "ON");
        assert!(thinq.outbox().is_empty());
        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[0]);
        assert_eq!(command[10], 1, "Soak data byte");
        assert_eq!(command[15], 0x20, "matching Soak options bit");
        assert_eq!(
            prop(&ha, "set_soak").as_deref(),
            Some("OFF"),
            "submitted override is reset until confirmed"
        );

        emit(&thinq, PANEL_SOAK_STATUS); // Soak + Extra Rinse, selected at the panel
        thinq.reset_recorder();
        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[0]);
        assert_eq!(command[10], 1, "untouched panel-selected Soak is preserved");
        assert_eq!(command[15], 0x21);
        dev.set_property("set_soak", "OFF");
        dev.set_property("start", "PRESS");
        let command = inner(&thinq.outbox()[1]);
        assert_eq!(command[10], 0);
        assert_eq!(command[15], 1, "disabling Soak preserves Extra Rinse");
        dev.set_property("set_soak", "OFF");
        dev.set_property("reset_settings", "PRESS");
        assert_eq!(
            prop(&ha, "set_soak").as_deref(),
            Some("ON"),
            "Reset follows the latest actual report"
        );
    }

    #[test]
    fn start_preserves_soak_and_delay_wash_exactly_as_captured_from_lg() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SOAK_DELAY_ARMED);
        expect(&ha, "reserve_time", "120");
        expect(&ha, "set_delay_wash", "2");
        expect(&ha, "soak", "ON");
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[SOAK_DELAY_START_COMMAND]));
        emit(&thinq, START_ACK);
        expect(&ha, "status", "Standby");
        emit(&thinq, SOAK_DELAY_STARTED);
        expect(&ha, "status", "Delayed");
        expect(&ha, "reserve_time", "120");
        expect(&ha, "soak", "ON");
        expect(&ha, "smart_course", "Sweat Stains");
    }

    #[test]
    fn staged_soak_and_delay_reconstruct_the_captured_lg_start_from_an_unmodified_preset() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SWEAT_STAINS_UPDATE);
        dev.set_property("set_soak", "ON");
        dev.set_property("set_delay_wash", "2");
        for value in ["", " ", "-1", "20", "1.5", "NaN", "Infinity", "2 hours"] {
            dev.set_property("set_delay_wash", value);
        }
        emit(&thinq, SWEAT_STAINS_UPDATE);
        expect(&ha, "reserve_time", "0");
        expect(&ha, "set_delay_wash", "2");
        assert!(thinq.outbox().is_empty());
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[SOAK_DELAY_START_COMMAND]));
        assert_eq!(
            prop(&ha, "set_delay_wash").as_deref(),
            Some("0"),
            "submitted override clears until confirmed"
        );

        emit(&thinq, SOAK_DELAY_ARMED);
        thinq.reset_recorder();
        dev.set_property("set_delay_wash", "0");
        dev.set_property("start", "PRESS");
        let data = &inner(&thinq.outbox()[0])[2..];
        assert_eq!(data[8], 1, "clearing delay preserves Soak");
        assert_eq!(
            data[9], 0,
            "zero overrides the panel delay instead of falling back"
        );
        assert_eq!(data[10], 0);
        dev.set_property("set_delay_wash", "3");
        dev.set_property("reset_settings", "PRESS");
        expect(&ha, "set_delay_wash", "2");
    }

    #[test]
    fn choosing_two_extra_rinses_reconstructs_the_physically_verified_deep_wash_start() {
        let (ha, thinq, dev) = make();
        emit(&thinq, LOCAL_NORMAL_ARMED);
        for (setting, value) in [
            ("set_course", "Deep Wash"),
            ("set_soil", "Normal"),
            ("set_spin", "Medium"),
            ("set_temp", "Cold"),
            ("set_turbo_wash", "OFF"),
            ("set_extra_rinses", "2"),
        ] {
            dev.set_property(setting, value);
        }
        expect(&ha, "extra_rinse", "OFF");
        expect(&ha, "set_extra_rinse", "ON");
        expect(&ha, "set_extra_rinses", "2");
        for value in ["", " ", "-1", "3", "1.5", "NaN", "Infinity", "two"] {
            dev.set_property("set_extra_rinses", value);
        }
        dev.set_property("set_extra_rinse", "ON"); // Re-selecting ON preserves a staged count of 2.
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[DEEP_WASH_COMMAND]));
        emit(&thinq, LOCAL_DEEP_WASH);
        expect(&ha, "set_extra_rinses", "2");
    }

    #[test]
    fn the_rinse_switch_and_count_control_stay_consistent_and_preserve_automatic_base_rinses() {
        let (ha, thinq, dev) = make();
        emit(&thinq, COLLAR_STAINS_STATUS); // Automatic rinse, Extra Rinse off.
        expect(&ha, "set_extra_rinses", "0");
        dev.set_property("start", "PRESS");
        assert_eq!(
            thinq.outbox()[0][8],
            1,
            "untouched automatic rinse remains in the command"
        );
        dev.set_property("set_extra_rinses", "2");
        dev.set_property("set_extra_rinse", "OFF");
        expect(&ha, "set_extra_rinses", "0");
        expect(&ha, "set_extra_rinse", "OFF");
        dev.set_property("start", "PRESS");
        assert_eq!(
            thinq.outbox()[1][8],
            1,
            "OFF when already off preserves the base rinse"
        );
        dev.set_property("set_extra_rinses", "0");
        dev.set_property("set_extra_rinse", "ON");
        expect(&ha, "set_extra_rinses", "1");
        dev.set_property("set_extra_rinses", "0");
        dev.set_property("start", "PRESS");
        assert_eq!(
            thinq.outbox()[2][8],
            0,
            "explicit zero clears the command override"
        );
        dev.set_property("set_extra_rinses", "2");
        dev.set_property("reset_settings", "PRESS");
        expect(&ha, "set_extra_rinses", "0");
        expect(&ha, "set_extra_rinse", "OFF");
    }

    #[test]
    fn max_and_control_lock_are_independent_of_deep_fill_remote_start_and_the_lid_lock() {
        let (ha, thinq, _) = make();
        emit(&thinq, MAX_FILL_ON);
        expect(&ha, "course", "Deep Wash");
        expect(&ha, "deep_fill", "OFF");
        expect(&ha, "deep_fill_max", "ON");
        expect(&ha, "remote_start", "OFF");
        expect(&ha, "child_lock", "OFF");
        emit(&thinq, CONTROL_LOCK_ON);
        expect(&ha, "child_lock", "ON");
        assert_eq!(
            prop(&ha, "door_lock").as_deref(),
            Some("ON"),
            "control lock does not lock the lid"
        );
        expect(&ha, "deep_fill_max", "ON");
        emit(&thinq, CONTROL_LOCK_OFF);
        expect(&ha, "child_lock", "OFF");
        expect(&ha, "deep_fill_max", "ON");
    }

    #[test]
    fn max_start_matches_lg_and_follows_the_consumed_flag_in_the_actual_reply() {
        let (ha, thinq, dev) = make();
        emit(&thinq, MAX_FILL_ARMED);
        expect(&ha, "deep_fill_max", "ON");
        expect(&ha, "remote_start", "ON");
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[MAX_FILL_START_COMMAND]));
        emit(&thinq, MAX_FILL_STARTED);
        expect(&ha, "status", "Detecting");
        expect(&ha, "deep_fill_max", "OFF");
        expect(&ha, "set_deep_fill_max", "OFF");
        expect(&ha, "remote_start", "ON");
    }

    #[test]
    fn staging_max_changes_the_final_byte_without_changing_other_options() {
        let (ha, thinq, dev) = make();
        emit(&thinq, MAX_FILL_STARTED);
        dev.set_property("set_deep_fill_max", "ON");
        dev.set_property("set_deep_fill_max", "invalid");
        expect(&ha, "deep_fill_max", "OFF");
        assert!(thinq.outbox().is_empty());
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[MAX_FILL_START_COMMAND]));
        emit(&thinq, MAX_FILL_ARMED);
        dev.set_property("set_deep_fill_max", "OFF");
        thinq.reset_recorder();
        dev.set_property("start", "PRESS");
        let mut without_max = inner(&hex_decode(MAX_FILL_START_COMMAND));
        *without_max.last_mut().unwrap() = 3;
        assert_eq!(thinq.outbox(), [wrap_aabb(&without_max)]);
    }

    #[test]
    fn cold_wash_is_a_separate_option_from_selecting_cold_temperature() {
        let (ha, thinq, _) = make();
        emit(&thinq, COLD_START_STATUS);
        expect(&ha, "temp", "Cold");
        expect(&ha, "cold_wash", "OFF");
        emit(&thinq, COLD_WASH_ON);
        expect(&ha, "course", "Normal");
        expect(&ha, "cold_wash", "ON");
        expect(&ha, "temp", "Cold");
        expect(&ha, "turbo_wash", "OFF");
        expect(&ha, "remaining_time", "88");
    }

    #[test]
    fn cold_wash_selected_at_the_panel_reproduces_lg_start_and_reads_back_the_running_state() {
        let (ha, thinq, dev) = make();
        emit(&thinq, COLD_WASH_ARMED);
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[COLD_WASH_COMMAND]));
        emit(&thinq, COLD_WASH_STARTED);
        expect(&ha, "status", "Detecting");
        expect(&ha, "cold_wash", "ON");
        expect(&ha, "turbo_wash", "OFF");
        expect(&ha, "temp", "Cold");
    }

    #[test]
    fn enabling_cold_wash_stages_the_panel_combination_and_reconstructs_lg_start() {
        let (ha, thinq, dev) = make();
        emit(&thinq, NORMAL);
        dev.set_property("set_cold_wash", "ON");
        dev.set_property("set_cold_wash", "invalid");
        emit(&thinq, NORMAL_HOT);
        expect(&ha, "cold_wash", "OFF");
        expect(&ha, "turbo_wash", "ON");
        expect(&ha, "temp", "Hot");
        expect(&ha, "set_cold_wash", "ON");
        expect(&ha, "set_turbo_wash", "OFF");
        expect(&ha, "set_temp", "Cold");
        assert!(thinq.outbox().is_empty());
        dev.set_property("start", "PRESS");
        assert_eq!(thinq.outbox(), packets(&[COLD_WASH_COMMAND]));
        assert_eq!(
            prop(&ha, "set_cold_wash").as_deref(),
            Some("OFF"),
            "submitted choice resets until confirmed"
        );
        emit(&thinq, COLD_WASH_STARTED);
        dev.set_property("set_cold_wash", "OFF");
        thinq.reset_recorder();
        dev.set_property("start", "PRESS");
        let mut without_cold_wash = inner(&hex_decode(COLD_WASH_COMMAND));
        without_cold_wash[14] = 0;
        assert_eq!(thinq.outbox(), [wrap_aabb(&without_cold_wash)]);
    }

    #[test]
    fn unverified_controls_send_nothing() {
        let (_, thinq, dev) = make();
        emit(&thinq, PAUSED);
        for prop in ["power_on", "nonsense"] {
            dev.set_property(prop, "PRESS");
        }
        assert!(thinq.outbox().is_empty());
    }

    /// Upstream types the preset table by Smart Course name, so every downloadable course has
    /// exactly one preset and the select lists them in catalog order.
    #[test]
    fn every_smart_course_has_a_preset() {
        let catalog: Vec<&str> = labels(SMART_COURSES)
            .into_iter()
            .filter(|name| *name != NOT_SELECTED)
            .collect();
        let presets: Vec<&str> = SMART_COURSE_PRESETS.iter().map(|(name, _)| *name).collect();
        assert_eq!(presets, catalog);
        let (ha, _, _) = make();
        assert_eq!(
            config(&ha).components["downloaded_course"]["options"],
            json!(catalog)
        );
    }

    /// The #132-based handler this replaces published only these, so no removal tombstone
    /// is needed: each is still published, on the same platform.
    #[test]
    fn components_of_the_replaced_handler_are_still_published() {
        let (ha, _, _) = make();
        let components = config(&ha).components;
        for (key, platform) in [
            ("power", "binary_sensor"),
            ("status", "sensor"),
            ("remaining_time", "sensor"),
        ] {
            assert_eq!(components[key]["platform"], platform, "{key}");
            assert!(components[key].get("unique_id").is_some(), "{key}");
        }
    }
}
