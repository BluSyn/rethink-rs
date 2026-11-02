//! 2REBGLUB_2P__ — LG GBBSJ2CCEP bottom freezer.
//!
//! 96-byte class `0x10` status. The F017 write is the 118-byte frame shared
//! with 2REF11EIDG__4, including the fixed bytes. `auto_ack` is on.
//! Express Cool is Super Cool, ecoFriendly is Vacation, and byte 95 is the
//! writable Fresh Converter. `10AF` watt-hours are summed, `10C5` carries
//! door totals, and `1031` is the two firmware modules.
//!
//! Night view (the door light's night mode) is status byte 30 (0 off, 2 dusk
//! till dawn, 3 specific hours); byte 26 counts its changes. It is set with
//! `F010 02 <mode> <start: yy mm dd hh mm ss> <end> <preview> <brightness %>`:
//! mode 0 off (zero times), 1 dusk till dawn (sent only for the preview, with
//! preview 01), 2 specific hours. The times are the dated UTC window running
//! now or next, which the fridge then repeats every night. The hours and the
//! brightness are never reported, so they are known only from our own writes
//! and from LG's cloud commands passing through the bridge; until then they
//! are unknown and nothing that needs them is sent. Dusk till dawn is left to
//! the LG app, whose times come from LG's cloud.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use chrono::{
    DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeDelta,
    TimeZone, Timelike, Utc,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const STATUS_LENGTH: usize = 96;
const COMMAND_LENGTH: usize = 118;
const FRESH_CONVERTER: usize = 95;
const NIGHT_VIEW: usize = 30;
const DOOR_OPENINGS_RECORD: u8 = 0x01;
const DOOR_OPEN_TIME_RECORD: u8 = 0x11;
const FIRMWARE_RECORD_LENGTH: usize = 24;

const NIGHT_VIEW_MODES: &[(&str, u8)] = &[("Off", 0), ("Dusk till dawn", 2), ("Specific hours", 3)];
// the modes as the F010 command numbers them
const NIGHT_VIEW_OFF: u8 = 0;
const NIGHT_VIEW_DUSK_TILL_DAWN: u8 = 1;
const NIGHT_VIEW_SPECIFIC_HOURS: u8 = 2;
const NIGHT_VIEW_BRIGHTNESS: &[(&str, u8)] = &[("10%", 10), ("30%", 30), ("50%", 50), ("70%", 70)];
const NIGHT_VIEW_COMMAND_LENGTH: usize = 18;
const TIME_PATTERN: &str = "^([01][0-9]|2[0-3]):[0-5][0-9]$";
const DAY_MINUTES: i64 = 24 * 60;

const COMMAND_FIXED: &[(usize, u8)] = &[
    (21, 0x00),
    (22, 0x00),
    (23, 0x00),
    (26, 0x00),
    (31, 0x00),
    (41, 0x00),
    (45, 0x1e),
    (70, 0x0a),
    (102, 0x00),
];

const FRESH_OPTIONS: &[&str] = &["Meat", "Fish", "Cheese"];

fn fresh_name(code: u8) -> &'static str {
    match code {
        0 => "Cheese",
        1 => "Fish",
        2 => "Meat",
        _ => "None",
    }
}

fn fresh_code(name: &str) -> Option<u8> {
    match name {
        "Cheese" => Some(0),
        "Fish" => Some(1),
        "Meat" => Some(2),
        _ => None,
    }
}

fn label_of(table: &[(&'static str, u8)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(label, _)| *label)
}

fn code_of(table: &[(&str, u8)], label: &str) -> Option<u8> {
    table
        .iter()
        .find(|(l, _)| *l == label)
        .map(|(_, code)| *code)
}

fn labels_of(table: &[(&'static str, u8)]) -> Vec<&'static str> {
    table.iter().map(|(label, _)| *label).collect()
}

/// A `TIME_PATTERN` value as minutes of the day.
fn parse_time(value: &str) -> Option<u32> {
    let b = value.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    let field = |i: usize| u32::from(b[i] - b'0') * 10 + u32::from(b[i + 1] - b'0');
    let (hours, minutes) = (field(0), field(3));
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

fn format_time(minutes: Option<u32>) -> String {
    minutes.map_or_else(|| "None".into(), |m| format!("{:02}:{:02}", m / 60, m % 60))
}

/// The clock and zone the night view hours are kept in: the host's, or fixed ones in tests.
trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn local(&self, at: DateTime<Utc>) -> NaiveDateTime;
    fn instant(&self, local: NaiveDateTime) -> DateTime<Utc>;
}

struct HostClock;

impl Clock for HostClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        at.with_timezone(&Local).naive_local()
    }
    fn instant(&self, local: NaiveDateTime) -> DateTime<Utc> {
        js_local_instant(&Local, local)
    }
}

/// The instant JS `new Date(y, m, d, h, min)` gives a local time: the first of a repeated
/// time, and a skipped one read with the offset from before the gap.
fn js_local_instant<Tz: TimeZone>(zone: &Tz, local: NaiveDateTime) -> DateTime<Utc> {
    match zone.from_local_datetime(&local) {
        LocalResult::Single(at) => at.with_timezone(&Utc),
        // chrono orders the two by offset, not by instant
        LocalResult::Ambiguous(a, b) => a.min(b).with_timezone(&Utc),
        LocalResult::None => {
            let before = zone.offset_from_utc_datetime(&(local - TimeDelta::days(1)));
            (local - before.fix()).and_utc()
        }
    }
}

/// A `yy mm dd hh mm` UTC time; out-of-range fields roll over as in `Date.UTC`.
fn utc_time(at: &[u8]) -> Option<DateTime<Utc>> {
    let month = i32::from(at[1]) - 1;
    let year = 2000 + i32::from(at[0]) + month.div_euclid(12);
    let first = NaiveDate::from_ymd_opt(year, month.rem_euclid(12) as u32 + 1, 1)?;
    let time = first.and_time(NaiveTime::MIN)
        + TimeDelta::days(i64::from(at[2]) - 1)
        + TimeDelta::hours(i64::from(at[3]))
        + TimeDelta::minutes(i64::from(at[4]));
    Some(time.and_utc())
}

/// The reported mode, and the hours and brightness as last sent to the fridge, by us or by
/// LG's cloud; `None` until then.
#[derive(Clone, Copy, Default)]
struct NightView {
    mode: Option<&'static str>,
    /// local minutes of the day
    start: Option<u32>,
    end: Option<u32>,
    brightness: Option<u8>,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    energy_wh: Mutex<i64>,
    night_view: Mutex<NightView>,
    clock: Box<dyn Clock>,
    meta: Metadata,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        Self::with_clock(ha, thinq, meta, Box::new(HostClock))
    }

    fn with_clock(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        clock: Box<dyn Clock>,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            temperature_unit: Mutex::new(None),
            energy_wh: Mutex::new(0),
            night_view: Mutex::new(NightView::default()),
            clock,
            meta,
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        let dev = this.clone();
        thinq.on_send_packet(Box::new(move |frame| dev.learn_night_view(frame)));
        this
    }

    fn set_temperature_unit(&self, unit: TemperatureUnit) {
        {
            let mut current = self.temperature_unit.lock();
            if *current == Some(unit) {
                return;
            }
            *current = Some(unit);
        }
        let (fu, fmin, fmax) = fridge_range(unit);
        let (zu, zmin, zmax) = freezer_range(unit);
        let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        config.components = [
            ("fridge_setpoint".into(), json!({
                "platform": "number", "device_class": "temperature",
                "unique_id": "$deviceid-fridge_setpoint",
                "state_topic": "$this/fridge_setpoint",
                "command_topic": "$this/fridge_setpoint/set",
                "name": "Fridge temperature",
                "unit_of_measurement": fu, "min": fmin, "max": fmax,
            })),
            ("freezer_setpoint".into(), json!({
                "platform": "number", "device_class": "temperature",
                "unique_id": "$deviceid-freezer_setpoint",
                "state_topic": "$this/freezer_setpoint",
                "command_topic": "$this/freezer_setpoint/set",
                "name": "Freezer temperature",
                "unit_of_measurement": zu, "min": zmin, "max": zmax,
            })),
            ("door".into(), json!({
                "platform": "binary_sensor", "device_class": "door",
                "unique_id": "$deviceid-door",
                "state_topic": "$this/door",
                "name": "Door",
            })),
            ("express_freeze".into(), json!({
                "platform": "switch", "icon": "mdi:snowflake",
                "unique_id": "$deviceid-express_freeze",
                "state_topic": "$this/express_freeze",
                "command_topic": "$this/express_freeze/set",
                "name": "Express Freeze",
            })),
            ("super_cool".into(), json!({
                "platform": "switch", "icon": "mdi:snowflake-variant",
                "unique_id": "$deviceid-super_cool",
                "state_topic": "$this/super_cool",
                "command_topic": "$this/super_cool/set",
                "name": "Super Cool",
            })),
            ("smart_learner".into(), json!({
                "platform": "switch", "icon": "mdi:brain",
                "unique_id": "$deviceid-smart_learner",
                "state_topic": "$this/smart_learner",
                "command_topic": "$this/smart_learner/set",
                "name": "Smart Learner",
            })),
            ("vacation".into(), json!({
                "platform": "switch", "icon": "mdi:airplane",
                "unique_id": "$deviceid-vacation",
                "state_topic": "$this/vacation",
                "command_topic": "$this/vacation/set",
                "name": "Vacation",
            })),
            ("fresh_converter".into(), json!({
                "platform": "select", "icon": "mdi:fridge-bottom",
                "unique_id": "$deviceid-fresh_converter",
                "state_topic": "$this/fresh_converter",
                "command_topic": "$this/fresh_converter/set",
                "name": "Fresh Converter+",
                "options": FRESH_OPTIONS,
            })),
            ("night_view".into(), json!({
                "platform": "select", "icon": "mdi:weather-night",
                "unique_id": "$deviceid-night_view",
                "state_topic": "$this/night_view",
                "command_topic": "$this/night_view/set",
                "name": "Night view",
                "options": labels_of(NIGHT_VIEW_MODES),
            })),
            ("night_view_start".into(), json!({
                "platform": "text", "icon": "mdi:clock-start",
                "unique_id": "$deviceid-night_view_start",
                "state_topic": "$this/night_view_start",
                "command_topic": "$this/night_view_start/set",
                "name": "Night view start",
                "pattern": TIME_PATTERN, "min": 5, "max": 5,
            })),
            ("night_view_end".into(), json!({
                "platform": "text", "icon": "mdi:clock-end",
                "unique_id": "$deviceid-night_view_end",
                "state_topic": "$this/night_view_end",
                "command_topic": "$this/night_view_end/set",
                "name": "Night view end",
                "pattern": TIME_PATTERN, "min": 5, "max": 5,
            })),
            ("night_view_brightness".into(), json!({
                "platform": "select", "icon": "mdi:brightness-6",
                "unique_id": "$deviceid-night_view_brightness",
                "state_topic": "$this/night_view_brightness",
                "command_topic": "$this/night_view_brightness/set",
                "name": "Night view brightness",
                "options": labels_of(NIGHT_VIEW_BRIGHTNESS),
            })),
            ("night_view_preview".into(), json!({
                "platform": "button", "icon": "mdi:lightbulb-on-outline",
                "unique_id": "$deviceid-night_view_preview",
                "command_topic": "$this/night_view_preview/set",
                "payload_press": "PRESS",
                "name": "Night view preview",
            })),
            ("energy".into(), json!({
                "platform": "sensor", "device_class": "energy",
                "unique_id": "$deviceid-energy",
                "state_topic": "$this/energy",
                "unit_of_measurement": "Wh", "state_class": "total_increasing",
                "name": "Energy",
            })),
            ("door_openings".into(), json!({
                "platform": "sensor", "icon": "mdi:door-open",
                "unique_id": "$deviceid-door_openings",
                "state_topic": "$this/door_openings",
                "state_class": "total_increasing",
                "name": "Door openings",
            })),
            ("door_open_time".into(), json!({
                "platform": "sensor", "device_class": "duration",
                "unique_id": "$deviceid-door_open_time",
                "state_topic": "$this/door_open_time",
                "unit_of_measurement": "s", "state_class": "total_increasing",
                "name": "Door open time",
            })),
            ("firmware_1".into(), json!({
                "platform": "sensor", "entity_category": "diagnostic", "icon": "mdi:chip",
                "unique_id": "$deviceid-firmware_1",
                "state_topic": "$this/firmware_1",
                "name": "Firmware 1",
            })),
            ("firmware_2".into(), json!({
                "platform": "sensor", "entity_category": "diagnostic", "icon": "mdi:chip",
                "unique_id": "$deviceid-firmware_2",
                "state_topic": "$this/firmware_2",
                "name": "Firmware 2",
            })),
        ]
        .into_iter()
        .collect();
        self.core.set_config(config);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, 0x10, STATUS_LENGTH) {
            self.process_status(rec);
            return;
        }
        if buf.first() != Some(&0x10) || buf.len() < 2 {
            return;
        }
        if buf[1] == 0xaf && buf.len() >= 5 {
            let wh = u16::from_be_bytes([buf[3], buf[4]]);
            self.process_energy(wh);
        } else if buf[1] == 0xc5 && buf.len() >= 4 {
            self.process_usage(&buf[2..]);
        } else if buf[1] == 0x31 && buf.len() >= 3 {
            self.process_firmware(&buf[2..]);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        if rec.len() < STATUS_LENGTH {
            return;
        }
        let unit = if rec[8] != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        self.core.publish_property(
            "fridge_setpoint",
            convert_fridge_temperature(unit, i32::from(rec[1])).into(),
        );
        self.core.publish_property(
            "freezer_setpoint",
            convert_freezer_temperature(unit, i32::from(rec[2])).into(),
        );
        self.core.publish_on_off("door", rec[7] == 1);
        self.core.publish_on_off("express_freeze", rec[3] == 2);
        self.core.publish_on_off("super_cool", rec[16] == 1);
        self.core.publish_on_off("smart_learner", rec[17] == 1);
        self.core.publish_on_off("vacation", rec[12] == 1);
        self.core
            .publish_property("fresh_converter", fresh_name(rec[FRESH_CONVERTER]).into());
        let mode = label_of(NIGHT_VIEW_MODES, rec[NIGHT_VIEW]);
        self.night_view.lock().mode = mode;
        self.core
            .publish_property("night_view", mode.unwrap_or("None").into());
        self.publish_night_view_settings();
    }

    fn publish_night_view_settings(&self) {
        let night_view = *self.night_view.lock();
        self.core
            .publish_property("night_view_start", format_time(night_view.start).into());
        self.core
            .publish_property("night_view_end", format_time(night_view.end).into());
        let brightness = night_view
            .brightness
            .and_then(|code| label_of(NIGHT_VIEW_BRIGHTNESS, code));
        self.core
            .publish_property("night_view_brightness", brightness.unwrap_or("None").into());
    }

    /// Picks up the hours and brightness from an F010 frame on its way to the fridge.
    fn learn_night_view(&self, frame: &[u8]) {
        if frame.len() != 2 + NIGHT_VIEW_COMMAND_LENGTH + 2 || frame[0] != 0xaa {
            return;
        }
        let cmd = &frame[2..frame.len() - 2];
        // not F010, or a preview
        if cmd[..3] != [0xf0, 0x10, 0x02] || cmd[16] != 0 {
            return;
        }
        {
            let mut night_view = self.night_view.lock();
            if cmd[3] == NIGHT_VIEW_SPECIFIC_HOURS {
                night_view.start = self.local_minutes(&cmd[4..10]);
                night_view.end = self.local_minutes(&cmd[10..16]);
            }
            if label_of(NIGHT_VIEW_BRIGHTNESS, cmd[17]).is_some() {
                night_view.brightness = Some(cmd[17]);
            }
        }
        if self.temperature_unit.lock().is_some() {
            self.publish_night_view_settings();
        }
    }

    fn local_minutes(&self, at: &[u8]) -> Option<u32> {
        let local = self.clock.local(utc_time(at)?);
        Some(local.hour() * 60 + local.minute())
    }

    /// The window from `start` to `end` (local minutes of the day) that is running now, or
    /// else the next one.
    fn night_view_window(&self, start: u32, end: u32) -> [DateTime<Utc>; 2] {
        let now = self.clock.now();
        let today = self.clock.local(now).date().and_time(NaiveTime::MIN);
        let length = match (i64::from(end) - i64::from(start)).rem_euclid(DAY_MINUTES) {
            0 => DAY_MINUTES,
            minutes => minutes,
        };
        let window = |day_offset: i64| {
            let from = self.clock.instant(
                today + TimeDelta::days(day_offset) + TimeDelta::minutes(i64::from(start)),
            );
            [from, from + TimeDelta::minutes(length)]
        };
        let yesterday = window(-1);
        if now < yesterday[1] {
            yesterday
        } else {
            window(0)
        }
    }

    /// Sends the night view command; false when a setting it needs is still unknown.
    fn send_night_view(&self, mode: u8, preview: bool) -> bool {
        let night_view = *self.night_view.lock();
        if night_view.brightness.is_none() && mode != NIGHT_VIEW_OFF {
            tracing::debug!("Night view: set the brightness first");
            return false;
        }
        let mut cmd = [0u8; NIGHT_VIEW_COMMAND_LENGTH];
        cmd[..4].copy_from_slice(&[0xf0, 0x10, 0x02, mode]);
        if mode == NIGHT_VIEW_SPECIFIC_HOURS && !preview {
            let (Some(start), Some(end)) = (night_view.start, night_view.end) else {
                tracing::debug!("Night view: set the start and end time first");
                return false;
            };
            for (i, at) in self.night_view_window(start, end).iter().enumerate() {
                cmd[4 + i * 6..10 + i * 6].copy_from_slice(&[
                    (at.year() - 2000) as u8,
                    at.month() as u8,
                    at.day() as u8,
                    at.hour() as u8,
                    at.minute() as u8,
                    0,
                ]);
            }
        }
        cmd[16] = u8::from(preview);
        // the app sends a brightness with Off too; with none known yet, the app's default
        cmd[17] = night_view.brightness.unwrap_or(30);
        self.core.send(&cmd);
        true
    }

    /// After an hours or brightness write: publish it, and resend while the fridge runs them.
    fn night_view_setting_changed(&self) {
        self.publish_night_view_settings();
        let running = self.night_view.lock().mode == Some("Specific hours");
        if running {
            self.send_night_view(NIGHT_VIEW_SPECIFIC_HOURS, false);
        }
    }

    fn process_energy(&self, wh: u16) {
        let total = {
            let mut energy = self.energy_wh.lock();
            *energy += i64::from(wh);
            *energy
        };
        if self.temperature_unit.lock().is_some() {
            self.core
                .publish_property("energy", PropertyValue::Int(total));
        }
    }

    fn process_usage(&self, report: &[u8]) {
        if report.len() < 2 {
            return;
        }
        let count = report[1] as usize;
        for i in 0..count {
            let start = 2 + i * 6;
            let Some(record) = report.get(start..start + 6) else {
                return;
            };
            let total = u16::from_be_bytes([record[4], record[5]]) as i64;
            if record[0] == DOOR_OPENINGS_RECORD {
                self.core
                    .publish_property("door_openings", PropertyValue::Int(total));
            } else if record[0] == DOOR_OPEN_TIME_RECORD {
                self.core
                    .publish_property("door_open_time", PropertyValue::Int(total));
            }
        }
    }

    fn process_firmware(&self, report: &[u8]) {
        if report.is_empty() {
            return;
        }
        let count = report[0] as usize;
        for i in 0..count {
            let start = 1 + i * FIRMWARE_RECORD_LENGTH;
            let Some(record) = report.get(start..start + FIRMWARE_RECORD_LENGTH) else {
                return;
            };
            if record[0] != 1 && record[0] != 2 {
                continue;
            }
            let part: String = record[1..12].iter().copied().map(char::from).collect();
            let version = format!("{:02x}{:02x}.{}", record[14], record[15], record[16]);
            let name = format!("firmware_{}", record[0]);
            self.core
                .publish_property(&name, format!("{version} ({part})").into());
        }
    }

    fn command(&self, fields: &[(usize, u8)]) {
        let mut message = vec![0xffu8; 2 + COMMAND_LENGTH];
        message[0] = 0xf0;
        message[1] = 0x17;
        for &(index, value) in COMMAND_FIXED {
            message[2 + index] = value;
        }
        for &(index, value) in fields {
            message[2 + index] = value;
        }
        self.core.send(&message);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        request_fridge_status(&self.core);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        let unit_byte = if unit == TemperatureUnit::C { 1 } else { 0 };
        match prop {
            "fridge_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                self.command(&[
                    (1, convert_fridge_temperature(unit, display) as u8),
                    (8, unit_byte),
                ]);
            }
            "freezer_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                self.command(&[
                    (2, convert_freezer_temperature(unit, display) as u8),
                    (8, unit_byte),
                ]);
            }
            "express_freeze" => self.command(&[(3, if mqtt_value == "ON" { 2 } else { 1 })]),
            "vacation" => self.command(&[(12, if mqtt_value == "ON" { 1 } else { 0 })]),
            "super_cool" => self.command(&[(16, if mqtt_value == "ON" { 1 } else { 0 })]),
            "smart_learner" => self.command(&[(17, if mqtt_value == "ON" { 1 } else { 0 })]),
            "fresh_converter" => {
                if let Some(code) = fresh_code(mqtt_value) {
                    self.command(&[(FRESH_CONVERTER, code)]);
                }
            }
            "night_view" => {
                let sent = match mqtt_value {
                    "Off" => self.send_night_view(NIGHT_VIEW_OFF, false),
                    "Specific hours" => self.send_night_view(NIGHT_VIEW_SPECIFIC_HOURS, false),
                    _ => {
                        tracing::debug!(
                            "Night view: dusk till dawn can only be set from the LG app"
                        );
                        false
                    }
                };
                // HA keeps showing the option it asked for until it gets a state
                if !sent {
                    let mode = self.night_view.lock().mode;
                    self.core.ha.publish_property(
                        &self.core.id,
                        "night_view",
                        mode.unwrap_or("None").into(),
                    );
                }
            }
            "night_view_start" | "night_view_end" => {
                let Some(minutes) = parse_time(mqtt_value) else {
                    return;
                };
                {
                    let mut night_view = self.night_view.lock();
                    if prop == "night_view_start" {
                        night_view.start = Some(minutes);
                    } else {
                        night_view.end = Some(minutes);
                    }
                }
                self.night_view_setting_changed();
            }
            "night_view_brightness" => {
                let Some(brightness) = code_of(NIGHT_VIEW_BRIGHTNESS, mqtt_value) else {
                    return;
                };
                self.night_view.lock().brightness = Some(brightness);
                self.night_view_setting_changed();
            }
            "night_view_preview" => {
                self.send_night_view(NIGHT_VIEW_DUSK_TILL_DAWN, true);
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
    use chrono::FixedOffset;
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const STATUS_INITIAL: &str = "AA6610EB02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF00000DBB";
    const STATUS_DOOR_OPEN: &str = "AAC610EC02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF020101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000002040401FFFFFF0101FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF020101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF0000AABB";
    const STATUS_SUPER_COOL: &str = "AAC610EC02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000002040401FFFFFF0201FFFFFF00FFFFFF0101FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000052BB";
    const STATUS_FRESH_CONVERTER_2: &str = "AAC610EC02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000002040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF00025DBB";
    const ENERGY_8WH: &str = "AA0B10AF0000080404D1BB";
    const USAGE: &str = "AA2010C5150401000100001203000000000011000A0000A6130000000000F6BB";
    const FIRMWARE: &str = "AA37103102015341413330383830383031000068E0000018000003E8000253414133303631393030310000968F000000000000A8004CBB";
    const APP_FRIDGE_5C: &str = "AA7CF017FF03FFFFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBBBB";
    const APP_FREEZER_M19C: &str = "AA7CF017FFFF05FFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA5BB";
    const APP_EXPRESS_FREEZE_ON: &str = "AA7CF017FFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB";
    const APP_VACATION_ON: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB";
    const APP_SUPER_COOL_ON: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB";
    const APP_SMART_LEARNER_OFF: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBCBB";
    const APP_FRESH_CONVERTER_2: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF02FFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB";

    // Night view (F010) commands the LG app sent through the cloud, from a fridge in
    // Europe/Sofia (UTC+3 until 25 October).
    // Specific hours 21:00 to 06:00 local, 30% brightness, set on 2026-10-21 at 10:56 local
    const APP_SPECIFIC_21_06: &str = "AA16F01002021A0A151200001A0A16030000001E3FBB";
    // 21:00 to 07:00 local at 70%, 10% and 50%, set on 2026-10-22 at 19:59 local
    const APP_SPECIFIC_70: &str = "AA16F01002021A0A161200001A0A170400000046C0BB";
    const APP_SPECIFIC_10: &str = "AA16F01002021A0A161200001A0A17040000000A0CBB";
    const APP_SPECIFIC_50: &str = "AA16F01002021A0A161200001A0A170400000032D4BB";
    // the same hours at 30%, re-sent by the cloud at 00:11 local after rethink reconnected
    const CLOUD_SPECIFIC_HOURS_AT_0011: &str = "AA16F01002021A0A161200001A0A17040000001E38BB";
    // dusk till dawn at 70% on 2026-10-21: sunset 18:36, sunrise 07:47 local
    const APP_DUSK_70: &str = "AA16F01002011A0A150F24001A0A16042F000046B7BB";
    const APP_OFF: &str = "AA16F0100200000000000000000000000000001EB5BB";
    const APP_PREVIEW: &str = "AA16F0100201000000000000000000000000011EB7BB";

    /// Europe/Sofia's 2026 rules: UTC+2, and UTC+3 from 29 March to 25 October, 01:00 UTC.
    #[derive(Clone, Copy, Debug)]
    struct Sofia;

    impl Sofia {
        fn offset_at(utc: &NaiveDateTime) -> FixedOffset {
            let change = |month, day| {
                NaiveDate::from_ymd_opt(2026, month, day)
                    .and_then(|date| date.and_hms_opt(1, 0, 0))
                    .unwrap()
            };
            let summer = (change(3, 29)..change(10, 25)).contains(utc);
            FixedOffset::east_opt(if summer { 3 * 3600 } else { 2 * 3600 }).unwrap()
        }
    }

    impl TimeZone for Sofia {
        type Offset = FixedOffset;
        fn from_offset(_: &FixedOffset) -> Self {
            Sofia
        }
        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
            self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
        }
        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
            // a repeated time lists the smaller offset first, as chrono's Local does
            let fits: Vec<FixedOffset> = [2, 3]
                .into_iter()
                .filter_map(|hours| FixedOffset::east_opt(hours * 3600))
                .filter(|&offset| Self::offset_at(&(*local - offset)) == offset)
                .collect();
            match fits[..] {
                [offset] => LocalResult::Single(offset),
                [first, second] => LocalResult::Ambiguous(first, second),
                _ => LocalResult::None,
            }
        }
        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            Self::offset_at(&utc.and_time(NaiveTime::MIN))
        }
        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            Self::offset_at(utc)
        }
    }

    /// A stopped clock in Sofia, where the night view captures come from.
    struct SofiaClock(DateTime<Utc>);

    impl Clock for SofiaClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
        fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
            at.with_timezone(&Sofia).naive_local()
        }
        fn instant(&self, local: NaiveDateTime) -> DateTime<Utc> {
            js_local_instant(&Sofia, local)
        }
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2REBGLUB_2P__", "2REBGLUB_2P__", "");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    /// `ready()` with the clock stopped at `now` in Sofia.
    fn ready_at(now: &str) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2REBGLUB_2P__", "2REBGLUB_2P__", "");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let clock = Box::new(SofiaClock(now.parse().unwrap()));
        let dev = Device::with_clock(ha.clone(), thinq.clone(), meta, clock);
        thinq.emit_data(&hex_decode(STATUS_INITIAL));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn ready() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let made = make();
        made.1.emit_data(&hex_decode(STATUS_INITIAL));
        made.1.reset_recorder();
        made
    }

    #[test]
    fn config_not_published_until_a_status_frame() {
        let (ha, _, _) = make();
        assert!(ha.device(ID).is_none());
    }

    #[test]
    fn initial_status() {
        let (ha, _, _) = ready();
        let comps = ha.device(ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°C"));
        assert!(comps.get("drawer_mode").is_none());
        assert!(comps.get("express_cool").is_none());
        assert_eq!(
            comps["fresh_converter"]["options"],
            json!(["Meat", "Fish", "Cheese"])
        );
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "super_cool").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "smart_learner").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "vacation").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fresh_converter").as_deref(), Some("Cheese"));
    }

    #[test]
    fn status_changes() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(STATUS_DOOR_OPEN));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(STATUS_SUPER_COOL));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "super_cool").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(STATUS_FRESH_CONVERTER_2));
        assert_eq!(prop(&ha, "fresh_converter").as_deref(), Some("Meat"));
    }

    #[test]
    fn energy_accumulates() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("16"));
    }

    #[test]
    fn energy_before_the_unit_is_counted() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        assert!(prop(&ha, "energy").is_none());
        thinq.emit_data(&hex_decode(STATUS_INITIAL));
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("16"));
    }

    #[test]
    fn door_usage_totals() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(USAGE));
        assert_eq!(prop(&ha, "door_openings").as_deref(), Some("18"));
        assert_eq!(prop(&ha, "door_open_time").as_deref(), Some("166"));
    }

    #[test]
    fn firmware_versions() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(FIRMWARE));
        assert_eq!(
            prop(&ha, "firmware_1").as_deref(),
            Some("68e0.0 (SAA30880801)")
        );
        assert_eq!(
            prop(&ha, "firmware_2").as_deref(),
            Some("968f.0 (SAA30619001)")
        );
    }

    #[test]
    fn firmware_part_numbers_decode_as_latin1() {
        let (ha, thinq, _) = ready();
        let frame = hex_decode(FIRMWARE);
        let mut inner = frame[2..frame.len() - 2].to_vec();
        inner[2 + 1 + 11] = 0xe9;
        thinq.emit_data(&wrap_aabb(&inner));
        assert_eq!(
            prop(&ha, "firmware_1").as_deref(),
            Some("68e0.0 (SAA3088080é)")
        );
    }

    #[test]
    fn start_sends_the_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1211010000010400EBBB"
        );
    }

    #[test]
    fn writes_match_the_app_commands() {
        let cases = [
            ("fridge_setpoint", "5", APP_FRIDGE_5C),
            ("freezer_setpoint", "-19", APP_FREEZER_M19C),
            ("express_freeze", "ON", APP_EXPRESS_FREEZE_ON),
            ("vacation", "ON", APP_VACATION_ON),
            ("super_cool", "ON", APP_SUPER_COOL_ON),
            ("smart_learner", "OFF", APP_SMART_LEARNER_OFF),
            ("fresh_converter", "Meat", APP_FRESH_CONVERTER_2),
        ];
        for (prop_name, value, expected) in cases {
            let (_, thinq, dev) = ready();
            dev.set_property(prop_name, value);
            assert_eq!(
                hex_encode(&thinq.outbox()[0]),
                expected,
                "{prop_name}={value}"
            );
        }
    }

    #[test]
    fn night_view_entities() {
        let (ha, _, _) = ready();
        let comps = ha.device(ID).unwrap().config.unwrap().components;
        assert_eq!(comps["night_view"]["platform"], json!("select"));
        assert_eq!(comps["night_view_start"]["platform"], json!("text"));
        assert_eq!(comps["night_view_end"]["platform"], json!("text"));
        assert_eq!(comps["night_view_brightness"]["platform"], json!("select"));
        assert_eq!(comps["night_view_preview"]["platform"], json!("button"));
        assert_eq!(
            comps["night_view"]["options"],
            json!(["Off", "Dusk till dawn", "Specific hours"])
        );
        assert_eq!(
            comps["night_view_brightness"]["options"],
            json!(["10%", "30%", "50%", "70%"])
        );
        assert_eq!(prop(&ha, "night_view").as_deref(), Some("Dusk till dawn"));
        // the fridge does not report these
        assert_eq!(prop(&ha, "night_view_start").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "night_view_end").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "night_view_brightness").as_deref(), Some("None"));
    }

    #[test]
    fn night_view_settings_are_learned_from_the_cloud_commands() {
        let (ha, thinq, _) = ready_at("2026-10-22T21:11:43Z");
        thinq.emit_send_packet(&hex_decode(CLOUD_SPECIFIC_HOURS_AT_0011));
        assert_eq!(prop(&ha, "night_view_start").as_deref(), Some("21:00"));
        assert_eq!(prop(&ha, "night_view_end").as_deref(), Some("07:00"));
        assert_eq!(prop(&ha, "night_view_brightness").as_deref(), Some("30%"));
        // dusk till dawn carries LG's sun times, which are not the hours
        thinq.emit_send_packet(&hex_decode(APP_DUSK_70));
        assert_eq!(prop(&ha, "night_view_brightness").as_deref(), Some("70%"));
        assert_eq!(prop(&ha, "night_view_start").as_deref(), Some("21:00"));
    }

    #[test]
    fn night_view_writes_match_the_app_commands() {
        // (now, writes after the default hours and brightness, the last command sent)
        type Case = (
            &'static str,
            &'static [(&'static str, &'static str)],
            &'static str,
        );
        let cases: &[Case] = &[
            // specific hours set in the morning: tonight's window
            (
                "2026-10-21T07:56:54Z",
                &[("night_view_end", "06:00")],
                APP_SPECIFIC_21_06,
            ),
            (
                "2026-10-22T16:59:49Z",
                &[("night_view_brightness", "70%")],
                APP_SPECIFIC_70,
            ),
            (
                "2026-10-22T16:59:49Z",
                &[("night_view_brightness", "10%")],
                APP_SPECIFIC_10,
            ),
            (
                "2026-10-22T16:59:49Z",
                &[("night_view_brightness", "50%")],
                APP_SPECIFIC_50,
            ),
            // after midnight, the window that started the evening before
            ("2026-10-22T21:11:43Z", &[], CLOUD_SPECIFIC_HOURS_AT_0011),
            ("2026-10-21T07:56:48Z", &[("night_view", "Off")], APP_OFF),
            (
                "2026-10-21T07:56:48Z",
                &[("night_view_preview", "PRESS")],
                APP_PREVIEW,
            ),
        ];
        let defaults = [
            ("night_view_start", "21:00"),
            ("night_view_end", "07:00"),
            ("night_view_brightness", "30%"),
        ];
        for &(now, writes, expected) in cases {
            let (_, thinq, dev) = ready_at(now);
            for (prop_name, value) in defaults.iter().chain(writes) {
                dev.set_property(prop_name, value);
            }
            if !writes
                .iter()
                .any(|(p, _)| *p == "night_view" || *p == "night_view_preview")
            {
                dev.set_property("night_view", "Specific hours");
            }
            let outbox = thinq.outbox();
            assert_eq!(
                hex_encode(outbox.last().unwrap()),
                expected,
                "{now} {writes:?}"
            );
        }
    }

    #[test]
    fn night_view_sends_nothing_until_the_settings_it_needs_are_known() {
        let (ha, thinq, dev) = ready_at("2026-10-21T07:56:54Z");
        dev.set_property("night_view", "Specific hours");
        dev.set_property("night_view_preview", "PRESS");
        assert!(thinq.outbox().is_empty(), "no brightness yet");
        assert_eq!(
            prop(&ha, "night_view").as_deref(),
            Some("Dusk till dawn"),
            "HA is told the mode did not change"
        );

        dev.set_property("night_view_brightness", "30%");
        dev.set_property("night_view_end", "06:00");
        dev.set_property("night_view", "Specific hours");
        assert!(thinq.outbox().is_empty(), "no start time yet");

        dev.set_property("night_view_start", "9pm");
        assert!(thinq.outbox().is_empty(), "not a time");
        dev.set_property("night_view_start", "21:00");
        assert!(
            thinq.outbox().is_empty(),
            "the fridge still reports dusk till dawn"
        );
        dev.set_property("night_view", "Specific hours");
        assert_eq!(hex_encode(&thinq.outbox()[0]), APP_SPECIFIC_21_06);
    }

    #[test]
    fn night_view_off_needs_no_settings_and_dusk_till_dawn_is_not_sent() {
        let (_, thinq, dev) = ready();
        dev.set_property("night_view", "Dusk till dawn");
        assert!(thinq.outbox().is_empty());
        dev.set_property("night_view", "Off");
        assert_eq!(hex_encode(&thinq.outbox()[0]), APP_OFF);
    }

    #[test]
    fn night_view_hours_across_the_dst_changes_follow_js_dates() {
        // frames and read-back hours from the upstream handler under TZ=Europe/Sofia
        let cases = [
            // 25 October: 03:30 comes twice, and the window starts at the first
            (
                "2026-10-24T22:30:00Z",
                "03:30",
                "05:00",
                "AA16F01002021A0A19001E001A0A19020000001E29BB",
                "03:30",
                "04:00",
            ),
            // 29 March: 03:30 is skipped, and is read with the winter offset
            (
                "2026-03-28T22:00:00Z",
                "03:30",
                "07:00",
                "AA16F01002021A031D011E001A031D050000001E2FBB",
                "04:30",
                "08:00",
            ),
            // a window running across the autumn change keeps its length
            (
                "2026-10-24T22:00:00Z",
                "21:00",
                "07:00",
                "AA16F01002021A0A181200001A0A19040000001E24BB",
                "21:00",
                "06:00",
            ),
        ];
        for (now, start, end, expected, start_after, end_after) in cases {
            let (ha, thinq, dev) = ready_at(now);
            dev.set_property("night_view_start", start);
            dev.set_property("night_view_end", end);
            dev.set_property("night_view_brightness", "30%");
            dev.set_property("night_view", "Specific hours");
            assert_eq!(hex_encode(&thinq.outbox()[0]), expected, "{now}");
            // the hours are read back from the command just sent, as upstream does
            assert_eq!(prop(&ha, "night_view_start").as_deref(), Some(start_after));
            assert_eq!(prop(&ha, "night_view_end").as_deref(), Some(end_after));
        }
    }

    #[test]
    fn unknown_property_sends_nothing() {
        let (_, thinq, dev) = ready();
        dev.set_property("nonsense", "whatever");
        dev.set_property("drawer_mode", "Fish");
        assert!(thinq.outbox().is_empty());
    }
}
