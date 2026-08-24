//! WFV474PGV dual oven + cooktop (AABB class 0x40).
//!
//! Port of unmerged upstream PR #144. Offsets, opcodes, and command bytes come
//! from 2026-08-22 live captures on a WFV474PGV (deviceType 301):
//!   0x40 0xEB  62-byte current status
//!   0x40 0xEC  previous+current 62-byte blocks (publish current only)
//!   0x40 0x72  one-shot events (surfaced as undecoded diagnostics)
//! Cooktop: five redundant 5-byte slots at 36/41/46/51/56.
//! Timer: seconds/minutes/hours at 30/31/32 (plain binary, not BCD).
//! Cavities: upper at 0, lower at 18.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::PropertyValue;
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

const CLASS_BYTE: u8 = 0x40;
const SINGLE_STATUS: u8 = 0xeb;
const DUAL_STATUS: u8 = 0xec;
const STATUS_RECORD_LEN: usize = 62;

const COOKTOP_SLOT_OFFSETS: [usize; 5] = [36, 41, 46, 51, 56];
const COOKTOP_SLOT_LEN: usize = 5;
const TIMER_SECONDS: usize = 30;
const TIMER_MINUTES: usize = 31;
const TIMER_HOURS: usize = 32;

const UPPER_CAVITY: usize = 0;
const LOWER_CAVITY: usize = 18;
const CAVITY_STATE: usize = 0;
const CAVITY_MODE: usize = 1;
const CAVITY_COOK_S: usize = 2;
const CAVITY_COOK_M: usize = 3;
const CAVITY_COOK_H: usize = 4;
const CAVITY_SET_TEMP: usize = 5;
const CAVITY_CUR_TEMP: usize = 7;
const CAVITY_REMOTE_START: usize = 15;
const REMOTE_START_READY: u8 = 0x01;
const UPPER_BAKE_OP: u8 = 0x01;

const MIN_REMOTE_TEMP: i32 = 170;
const MAX_REMOTE_TEMP: i32 = 550;
const REMOTE_TEMP_STEP: i32 = 5;
const MAX_REMOTE_COOK_MIN: i32 = 24 * 60;

const STATUS_QUERY: &str =
    "F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CavityId {
    Upper,
    Lower,
}

impl CavityId {
    fn as_str(self) -> &'static str {
        match self {
            CavityId::Upper => "upper",
            CavityId::Lower => "lower",
        }
    }
    fn title(self) -> &'static str {
        match self {
            CavityId::Upper => "Upper",
            CavityId::Lower => "Lower",
        }
    }
    fn offset(self) -> usize {
        match self {
            CavityId::Upper => UPPER_CAVITY,
            CavityId::Lower => LOWER_CAVITY,
        }
    }
    fn cancel_sel(self) -> u8 {
        match self {
            CavityId::Upper => 0x00,
            CavityId::Lower => 0x01,
        }
    }
}

#[derive(Clone)]
struct CavityRemote {
    temperature: i32,
    cook_time: i32,
}

#[derive(Clone)]
struct RemoteParams {
    upper: CavityRemote,
    lower: CavityRemote,
    lower_mode: String,
}

impl RemoteParams {
    fn defaults() -> Self {
        Self {
            upper: CavityRemote {
                temperature: 350,
                cook_time: 10,
            },
            lower: CavityRemote {
                temperature: 350,
                cook_time: 10,
            },
            lower_mode: "Bake".into(),
        }
    }
    fn cavity(&self, id: CavityId) -> &CavityRemote {
        match id {
            CavityId::Upper => &self.upper,
            CavityId::Lower => &self.lower,
        }
    }
    fn cavity_mut(&mut self, id: CavityId) -> &mut CavityRemote {
        match id {
            CavityId::Upper => &mut self.upper,
            CavityId::Lower => &mut self.lower,
        }
    }
}

fn saved_remote() -> &'static Mutex<HashMap<String, Arc<Mutex<RemoteParams>>>> {
    static MAP: OnceLock<Mutex<HashMap<String, Arc<Mutex<RemoteParams>>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remote_for(id: &str) -> Arc<Mutex<RemoteParams>> {
    saved_remote()
        .lock()
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(RemoteParams::defaults())))
        .clone()
}

fn lower_op(mode: &str) -> Option<u8> {
    match mode {
        "Bake" => Some(0x15),
        "Convection Bake" => Some(0x17),
        "Convection Roast" => Some(0x18),
        _ => None,
    }
}

fn is_lower_mode(value: &str) -> bool {
    lower_op(value).is_some()
}

fn format_time(hours: u8, minutes: u8, seconds: u8) -> String {
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn parse_time(value: &str) -> Option<(u8, u8, u8)> {
    let b = value.as_bytes();
    if b.len() != 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let hours: u32 = std::str::from_utf8(&b[0..2]).ok()?.parse().ok()?;
    let minutes: u32 = std::str::from_utf8(&b[3..5]).ok()?.parse().ok()?;
    let seconds: u32 = std::str::from_utf8(&b[6..8]).ok()?.parse().ok()?;
    if hours > 0xff || minutes > 59 || seconds > 59 {
        return None;
    }
    // Pattern ^\d{2}:[0-5]\d:[0-5]\d$ — first minute/second digit must be 0-5.
    if b[3] < b'0' || b[3] > b'5' || b[6] < b'0' || b[6] > b'5' {
        return None;
    }
    if !b.iter().enumerate().all(|(i, c)| {
        if i == 2 || i == 5 {
            *c == b':'
        } else {
            c.is_ascii_digit()
        }
    }) {
        return None;
    }
    Some((hours as u8, minutes as u8, seconds as u8))
}

fn format_oven_state(value: u8) -> String {
    match value {
        0x00 => "Idle".into(),
        0x01 => "Preheating".into(),
        0x02 => "Active".into(),
        0x03 => "Complete".into(),
        n => format!("Unknown (0x{n:02x})"),
    }
}

fn format_oven_mode(value: u8) -> String {
    if value == 0 {
        return "None".into();
    }
    match value & 0x7f {
        0x01 | 0x15 => "Bake".into(),
        0x07 => "Broil".into(),
        0x17 => "Convection Bake".into(),
        0x18 => "Convection Roast".into(),
        // Unknown string uses the raw (possibly 0x80-flagged) byte, matching the TS handler.
        _ => format!("Unknown (0x{value:02x})"),
    }
}

fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

fn hex_lower(buf: &[u8]) -> String {
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn redact_decoded(current: &[u8]) -> Vec<u8> {
    let mut out = current.to_vec();
    for cavity in [UPPER_CAVITY, LOWER_CAVITY] {
        let end = (cavity + CAVITY_CUR_TEMP + 2).min(out.len());
        if cavity < out.len() {
            for b in &mut out[cavity..end] {
                *b = 0;
            }
        }
        let rs = cavity + CAVITY_REMOTE_START;
        if rs < out.len() {
            out[rs] = 0;
        }
    }
    if out.len() > TIMER_HOURS {
        out[TIMER_SECONDS..=TIMER_HOURS].fill(0);
    }
    for off in COOKTOP_SLOT_OFFSETS {
        let end = (off + COOKTOP_SLOT_LEN).min(out.len());
        if off < out.len() {
            out[off..end].fill(0);
        }
    }
    out
}

fn cavity_sensor_components(cavity: CavityId, comps: &mut Map<String, serde_json::Value>) {
    let oven = format!("{}_oven", cavity.as_str());
    let title = cavity.title();
    comps.insert(
        format!("{oven}_temperature"),
        json!({
            "platform": "sensor",
            "device_class": "temperature",
            "icon": "mdi:thermometer",
            "unique_id": format!("$deviceid-{oven}_temperature"),
            "state_topic": format!("$this/{oven}_temperature"),
            "name": format!("{title} Oven Current Temperature"),
            "unit_of_measurement": "°F",
        }),
    );
    comps.insert(
        format!("{oven}_state"),
        json!({
            "platform": "sensor",
            "icon": "mdi:stove",
            "unique_id": format!("$deviceid-{oven}_state"),
            "state_topic": format!("$this/{oven}_state"),
            "name": format!("{title} Oven State"),
        }),
    );
    comps.insert(
        format!("{oven}_mode"),
        json!({
            "platform": "sensor",
            "icon": "mdi:chef-hat",
            "unique_id": format!("$deviceid-{oven}_mode"),
            "state_topic": format!("$this/{oven}_mode"),
            "name": format!("{title} Oven Mode"),
        }),
    );
    comps.insert(
        format!("{oven}_set_temperature"),
        json!({
            "platform": "sensor",
            "device_class": "temperature",
            "icon": "mdi:thermometer-chevron-up",
            "unique_id": format!("$deviceid-{oven}_set_temperature"),
            "state_topic": format!("$this/{oven}_set_temperature"),
            "name": format!("{title} Oven Set Temperature"),
            "unit_of_measurement": "°F",
        }),
    );
    comps.insert(
        format!("{oven}_cook_time"),
        json!({
            "platform": "sensor",
            "icon": "mdi:timer-outline",
            "unique_id": format!("$deviceid-{oven}_cook_time"),
            "state_topic": format!("$this/{oven}_cook_time"),
            "name": format!("{title} Oven Cook Time"),
        }),
    );
    comps.insert(
        format!("{oven}_remote_start"),
        json!({
            "platform": "binary_sensor",
            "icon": "mdi:remote",
            "unique_id": format!("$deviceid-{oven}_remote_start"),
            "state_topic": format!("$this/{oven}_remote_start"),
            "name": format!("{title} Oven Remote Start Ready"),
            "payload_on": "ON",
            "payload_off": "OFF",
        }),
    );
}

fn cavity_remote_components(cavity: CavityId, comps: &mut Map<String, serde_json::Value>) {
    let id = cavity.as_str();
    let title = cavity.title();
    comps.insert(
        format!("{id}_remote_temperature"),
        json!({
            "platform": "number",
            "device_class": "temperature",
            "icon": "mdi:thermometer-chevron-up",
            "unique_id": format!("$deviceid-{id}-remote-temperature"),
            "state_topic": format!("$this/{id}_remote_temperature"),
            "command_topic": format!("$this/{id}_remote_temperature/set"),
            "unit_of_measurement": "°F",
            "min": MIN_REMOTE_TEMP,
            "max": MAX_REMOTE_TEMP,
            "step": REMOTE_TEMP_STEP,
            "mode": "box",
            "name": format!("{title} Remote Temperature"),
        }),
    );
    comps.insert(
        format!("{id}_remote_cook_time"),
        json!({
            "platform": "number",
            "device_class": "duration",
            "icon": "mdi:timer-outline",
            "unique_id": format!("$deviceid-{id}-remote-cook-time"),
            "state_topic": format!("$this/{id}_remote_cook_time"),
            "command_topic": format!("$this/{id}_remote_cook_time/set"),
            "unit_of_measurement": "min",
            "min": 0,
            "max": MAX_REMOTE_COOK_MIN,
            "step": 1,
            "mode": "box",
            "name": format!("{title} Remote Cook Time"),
        }),
    );
    comps.insert(
        format!("{id}_start"),
        json!({
            "platform": "button",
            "icon": "mdi:play-circle-outline",
            "unique_id": format!("$deviceid-{id}-start"),
            "command_topic": format!("$this/{id}_start/set"),
            "payload_press": "PRESS",
            "name": if cavity == CavityId::Upper { "Start Upper Bake" } else { "Start Lower" },
            "availability": [
                { "topic": "$this/availability" },
                { "topic": "$rethink/availability" },
                {
                    "topic": format!("$this/{id}_oven_remote_start"),
                    "payload_available": "ON",
                    "payload_not_available": "OFF",
                }
            ],
            "availability_mode": "all",
        }),
    );
    comps.insert(
        format!("{id}_cancel"),
        json!({
            "platform": "button",
            "icon": "mdi:stop-circle-outline",
            "unique_id": format!("$deviceid-{id}-cancel"),
            "command_topic": format!("$this/{id}_cancel/set"),
            "payload_press": "PRESS",
            "name": format!("Cancel {title}"),
        }),
    );
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    remote: Arc<Mutex<RemoteParams>>,
    start_ready: Mutex<(bool, bool)>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let remote = remote_for(thinq.id());
        let mut comps = Map::new();
        comps.insert(
            "cooktop_status".into(),
            json!({
                "platform": "binary_sensor",
                "icon": "mdi:stove",
                "unique_id": "$deviceid-cooktop_status",
                "state_topic": "$this/cooktop_status",
                "name": "Cooktop",
                "payload_on": "ON",
                "payload_off": "OFF",
            }),
        );
        cavity_sensor_components(CavityId::Upper, &mut comps);
        cavity_sensor_components(CavityId::Lower, &mut comps);
        comps.insert(
            "timer".into(),
            json!({
                "platform": "text",
                "icon": "mdi:timer",
                "unique_id": "$deviceid-timer",
                "state_topic": "$this/timer",
                "command_topic": "$this/timer/set",
                "pattern": "^\\d{2}:[0-5]\\d:[0-5]\\d$",
                "name": "Timer",
            }),
        );
        comps.insert(
            "timer_stop".into(),
            json!({
                "platform": "button",
                "icon": "mdi:timer-stop-outline",
                "unique_id": "$deviceid-timer-stop",
                "command_topic": "$this/timer_stop/set",
                "payload_press": "PRESS",
                "name": "Stop / Acknowledge Timer",
            }),
        );
        cavity_remote_components(CavityId::Upper, &mut comps);
        cavity_remote_components(CavityId::Lower, &mut comps);
        comps.insert(
            "lower_remote_mode".into(),
            json!({
                "platform": "select",
                "icon": "mdi:chef-hat",
                "unique_id": "$deviceid-lower-remote-mode",
                "state_topic": "$this/lower_remote_mode",
                "command_topic": "$this/lower_remote_mode/set",
                "options": ["Bake", "Convection Bake", "Convection Roast"],
                "name": "Lower Remote Mode",
            }),
        );
        comps.insert(
            "raw_status".into(),
            json!({
                "platform": "sensor",
                "icon": "mdi:stove",
                "entity_category": "diagnostic",
                "unique_id": "$deviceid-raw_status",
                "state_topic": "$this/raw_status",
                "name": "Raw Status",
            }),
        );
        comps.insert(
            "undecoded_frame".into(),
            json!({
                "platform": "sensor",
                "icon": "mdi:help-box",
                "entity_category": "diagnostic",
                "unique_id": "$deviceid-undecoded_frame",
                "state_topic": "$this/undecoded_frame",
                "name": "Undecoded Frame",
            }),
        );
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Oven"})));
        cfg.components = comps.into_iter().collect();
        core.set_config(cfg);

        {
            let r = remote.lock();
            core.publish_property("upper_remote_temperature", r.upper.temperature.into());
            core.publish_property("upper_remote_cook_time", r.upper.cook_time.into());
            core.publish_property("lower_remote_temperature", r.lower.temperature.into());
            core.publish_property("lower_remote_cook_time", r.lower.cook_time.into());
            core.publish_property("lower_remote_mode", r.lower_mode.clone().into());
        }
        core.publish_property("upper_oven_remote_start", "OFF".into());
        core.publish_property("lower_oven_remote_start", "OFF".into());

        let this = Arc::new(Self {
            core: core.clone(),
            remote,
            start_ready: Mutex::new((false, false)),
        });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() >= 2 && buf[0] == CLASS_BYTE {
            if buf.len() == 4 && buf[1] == 0x00 && (buf[2] == 0x43 || buf[2] == 0x44) && buf[3] == 0x00
            {
                return;
            }
            if buf[1] == SINGLE_STATUS && buf.len() == 2 + STATUS_RECORD_LEN {
                self.publish_status(SINGLE_STATUS, &buf[2..]);
                return;
            }
            if buf[1] == DUAL_STATUS && buf.len() == 2 + STATUS_RECORD_LEN * 2 {
                self.publish_status(DUAL_STATUS, &buf[2 + STATUS_RECORD_LEN..]);
                return;
            }
        }
        self.core
            .publish_property("undecoded_frame", hex_lower(buf).into());
    }

    fn publish_status(&self, frame_type: u8, current: &[u8]) {
        if current.len() < STATUS_RECORD_LEN {
            return;
        }
        self.core.publish_property(
            "timer",
            format_time(current[TIMER_HOURS], current[TIMER_MINUTES], current[TIMER_SECONDS]).into(),
        );
        self.publish_cavity(CavityId::Upper, current);
        self.publish_cavity(CavityId::Lower, current);
        let cooktop_on = COOKTOP_SLOT_OFFSETS
            .iter()
            .any(|&off| current.get(off).copied().unwrap_or(0) != 0);
        self.core
            .publish_on_off("cooktop_status", cooktop_on);
        let redacted = redact_decoded(current);
        self.core.publish_property(
            "raw_status",
            format!("{frame_type:02x}/{}", hex_lower(&redacted)).into(),
        );
    }

    fn publish_cavity(&self, cavity: CavityId, current: &[u8]) {
        let offset = cavity.offset();
        let oven = format!("{}_oven", cavity.as_str());
        self.core.publish_property(
            &format!("{oven}_state"),
            format_oven_state(current[offset + CAVITY_STATE]).into(),
        );
        self.core.publish_property(
            &format!("{oven}_mode"),
            format_oven_mode(current[offset + CAVITY_MODE]).into(),
        );
        self.core.publish_property(
            &format!("{oven}_temperature"),
            i64::from(be16(current, offset + CAVITY_CUR_TEMP)).into(),
        );
        self.core.publish_property(
            &format!("{oven}_set_temperature"),
            i64::from(be16(current, offset + CAVITY_SET_TEMP)).into(),
        );
        self.core.publish_property(
            &format!("{oven}_cook_time"),
            format_time(
                current[offset + CAVITY_COOK_H],
                current[offset + CAVITY_COOK_M],
                current[offset + CAVITY_COOK_S],
            )
            .into(),
        );
        let ready = current[offset + CAVITY_REMOTE_START] == REMOTE_START_READY;
        self.core.publish_on_off(&format!("{oven}_remote_start"), ready);
        let mut sr = self.start_ready.lock();
        match cavity {
            CavityId::Upper => sr.0 = ready,
            CavityId::Lower => sr.1 = ready,
        }
    }

    fn ready(&self, cavity: CavityId) -> bool {
        let sr = self.start_ready.lock();
        match cavity {
            CavityId::Upper => sr.0,
            CavityId::Lower => sr.1,
        }
    }

    fn set_ready(&self, cavity: CavityId, ready: bool) {
        let mut sr = self.start_ready.lock();
        match cavity {
            CavityId::Upper => sr.0 = ready,
            CavityId::Lower => sr.1 = ready,
        }
    }

    fn republish(&self, prop: &str, value: PropertyValue) {
        self.core.ha.publish_property(&self.core.id, prop, value);
    }

    fn update_remote_number(&self, prop: &str, mqtt_value: &str, min: i32, max: i32, step: i32, previous: i32) -> i32 {
        let trimmed = mqtt_value.trim();
        let ok_digits = !trimmed.is_empty() && trimmed.bytes().all(|c| c.is_ascii_digit());
        let value = if ok_digits {
            trimmed.parse::<i32>().ok()
        } else {
            None
        };
        match value {
            Some(v) if v >= min && v <= max && (v - min) % step == 0 => {
                self.core.publish_property(prop, v.into());
                v
            }
            _ => {
                self.republish(prop, previous.into());
                previous
            }
        }
    }

    fn send_timer(&self, hours: u8, minutes: u8, seconds: u8) {
        self.core.send(&[
            0xf0, 0x43, 0x23, 0x06, 0x80, 0x80, 0x80, seconds, minutes, hours,
        ]);
    }

    fn send_remote_start(&self, operation: u8, temperature: i32, cook_time_min: i32) {
        let hours = (cook_time_min / 60) as u8;
        let minutes = (cook_time_min % 60) as u8;
        self.core.send(&[
            0xf0,
            0x43,
            0x20,
            0x0b,
            operation,
            0x00,
            0x00,
            0x00,
            0x00,
            (temperature >> 8) as u8,
            (temperature & 0xff) as u8,
            hours,
            minutes,
            0x00,
            0x00,
        ]);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if prop == "timer" {
            if let Some((h, m, s)) = parse_time(mqtt_value) {
                self.send_timer(h, m, s);
            }
            return;
        }
        if prop == "timer_stop" {
            if mqtt_value == "PRESS" {
                self.send_timer(0, 0, 0);
            }
            return;
        }
        if prop == "lower_remote_mode" {
            if is_lower_mode(mqtt_value) {
                self.remote.lock().lower_mode = mqtt_value.to_string();
                self.core.publish_property(prop, mqtt_value.into());
            } else {
                let cur = self.remote.lock().lower_mode.clone();
                self.republish(prop, cur.into());
            }
            return;
        }

        let (cavity, action) = match prop {
            "upper_remote_temperature" => (CavityId::Upper, "remote_temperature"),
            "upper_remote_cook_time" => (CavityId::Upper, "remote_cook_time"),
            "upper_start" => (CavityId::Upper, "start"),
            "upper_cancel" => (CavityId::Upper, "cancel"),
            "lower_remote_temperature" => (CavityId::Lower, "remote_temperature"),
            "lower_remote_cook_time" => (CavityId::Lower, "remote_cook_time"),
            "lower_start" => (CavityId::Lower, "start"),
            "lower_cancel" => (CavityId::Lower, "cancel"),
            _ => return,
        };

        if action == "remote_temperature" {
            let prev = self.remote.lock().cavity(cavity).temperature;
            let v = self.update_remote_number(
                prop,
                mqtt_value,
                MIN_REMOTE_TEMP,
                MAX_REMOTE_TEMP,
                REMOTE_TEMP_STEP,
                prev,
            );
            self.remote.lock().cavity_mut(cavity).temperature = v;
            return;
        }
        if action == "remote_cook_time" {
            let prev = self.remote.lock().cavity(cavity).cook_time;
            let v = self.update_remote_number(prop, mqtt_value, 0, MAX_REMOTE_COOK_MIN, 1, prev);
            self.remote.lock().cavity_mut(cavity).cook_time = v;
            return;
        }
        if mqtt_value != "PRESS" {
            return;
        }
        if action == "cancel" {
            self.core.send(&[0xf0, 0x44, cavity.cancel_sel()]);
            return;
        }
        if !self.ready(cavity) {
            return;
        }
        self.set_ready(cavity, false);
        self.core
            .publish_on_off(&format!("{}_oven_remote_start", cavity.as_str()), false);
        let (op, temp, cook) = {
            let r = self.remote.lock();
            let op = if cavity == CavityId::Upper {
                UPPER_BAKE_OP
            } else {
                lower_op(&r.lower_mode).unwrap_or(0x15)
            };
            let c = r.cavity(cavity);
            (op, c.temperature, c.cook_time)
        };
        self.send_remote_start(op, temp, cook);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_QUERY));
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
    use crate::test_support::meta;
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockThinq2Device, Thinq2Device};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Process-global `saved_remote` is keyed by appliance id (TS
    /// `savedRemoteParams`). Node tests are serial and can `Map.clear()`;
    /// `cargo test` is parallel, so each case must use a unique thinq id
    /// and must not wipe the map.
    fn next_id() -> String {
        static N: AtomicU64 = AtomicU64::new(1);
        format!("wfv474pgv-{}", N.fetch_add(1, Ordering::Relaxed))
    }

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        let id = next_id();
        let ha = rethink_core::MockHaConnection::new();
        let m = meta("WFV474PGV");
        let thinq = MockThinq2Device::new(id, m.clone());
        let ha_dyn: Arc<dyn HaConnection> = ha.clone();
        let tq_dyn: Arc<dyn Thinq2Device> = thinq.clone();
        let dev = Device::new(ha_dyn, tq_dyn, m);
        (ha, thinq, dev)
    }

    fn only_dev(ha: &rethink_core::MockHaConnection) -> rethink_core::ha::MockDeviceInfo {
        let devices = ha.devices();
        assert_eq!(devices.len(), 1, "expected one HA device, got {}", devices.len());
        devices.into_values().next().unwrap()
    }

    fn prop(ha: &rethink_core::MockHaConnection, name: &str) -> Option<String> {
        only_dev(ha).properties.get(name).map(|p| p.as_string())
    }

    fn inner_hex(packet: &[u8]) -> String {
        hex_lower(&packet[2..packet.len() - 2])
    }

    fn frame(inner: &[u8]) -> Vec<u8> {
        wrap_aabb(inner)
    }

    const COOKTOP_OFF: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000000000000000000004D0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000090BB";
    const COOKTOP_OFF_TO_ON: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000000000000000000004E00000000000006000000000000000000004D00000000000006000001000300000100030000010003000001000300000100030000009FBB";
    const COOKTOP_ON_TO_OFF: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004D0000000000000E00000100190E000100190E000100190E000100190E000100190E000000000000000000004E0000000000000E000000000000000000004C0000000000000E00000000000000000000000000000000000000000000000000000000D8BB";
    const TIMER_09_01_13: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004C0000000000000E0000000000000000000000000000000000000000000000000000000000000000000000004E00000000000006000000000000000000004C0000000D010906000000000000000000000000000000000000000000000000000000009EBB";
    const TIMER_ROLLOVER_09_00_59: &str = "AA8240EC00000000000000004E00000000000006000000000000000000004C000000010109060000000000000000000000000000000000000000000000000000000000000000000000004E00000000000006000000000000000000004C0000003B00090600000000000000000000000000000000000000000000000000000000A6BB";
    const TIMER_STOPPED: &str = "AA8240EC00000000000000004E00000000000006000000000000000000004C0000001A3A08060000000000000000000000000000000000000000000000000000000000000000000000004E0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000044BB";
    const TIMER_EXPIRED_ALARM: &str = "AA13407203F50A0000000000000000000024BB";
    const BOTH_OVENS_REMOTE_START_READY: &str = "AA8240EC00000000000000004E00000000000006000000000000000000004D000008000000060000000000000000000000000000000000000000000000000000000000000000000000004E00000000000001000000000000000000004C0000000000000100000000000000000000000000000000000000000000000000000000F6BB";
    const LOWER_ONLY_REMOTE_START_READY: &str = "AA8240EC0000000000000000850000000000000E000000000000000000004E0000000000000E0000000000000000000000000000000000000000000000000000000000000000000000008500000000000000000000000000000000004E00000000000001000000000000000000000000000000000000000000000000000000004EBB";
    const LOWER_CONVECTION_ROAST_350F_TIME_OFF: &str = "AA8240EC00000000000000008300000000000000000000000000000000004E00000000000001000000000000000000000000000000000000000000000000000000000000000000000000830000000000000200000198000000014500000000000000000200000000000000000000000000000000000000000000000000000000C5BB";
    const LOWER_BAKE_285C_FIVE_MINUTES: &str = "AA8240EC0000000000000000FE0000000000000000000000000000000000AF00000000000001000000000000000000000000000000000000000000000000000000000000000000000000FE000000000000020000019500050002210000000000000000020000000000000000000000000000000000000000000000000000000093BB";
    const LOWER_BAKE_285C_FOUR_MINUTES_59: &str = "AA8240EC0000000000000000FE00000000000002000001950005000221000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000FE00000000000002000001953B040002210000000000000000020000000000000000000000000000000000000000000000000000000047BB";
    const LOWER_BAKE_CANCELLED_AT_252F: &str = "AA8240EC0000000000000000F400000000000002000001952702000221000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000F40000000000000600000000000000000000FC00000000000006000000000000000000000000000000000000000000000000000000007BBB";
    const LOWER_CONVECTION_BAKE_300F_TEN_MINUTES: &str = "AA8240EC0000000000000000EC0000000000000000000000000000000000F200000000000001000000000000000000000000000000000000000000000000000000000000000000000000EC0000000000000200000197000A0001130000000000000000020000000000000000000000000000000000000000000000000000000088BB";
    const LOWER_CONVECTION_BAKE_300F_NINE_MINUTES_59: &str = "AA8240EC0000000000000000EC0000000000000200000197000A000113000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000EC00000000000002000001973B09000113000000000000000002000000000000000000000000000000000000000000000000000000008BBB";
    const LOWER_CONVECTION_BAKE_PREHEAT_COMPLETE: &str = "AA8240EC0000000000000000EB00000000000002000001973009000113000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000E9000000000000020000029705090001130000000000000000020000000000000000000000000000000000000000000000000000000081BB";
    const UPPER_BROIL_HIGH_WITH_LOWER_PREHEATING: &str = "AA8240EC00000000000000007F0000000000000000000198000000014500000000000000000000000000000000000000000000000000000000000000000000000000020700000001900000000000000000020000019800000001450000000000000000020000000000000000000000000000000000000000000000000000000066BB";
    const LOWER_PREHEAT_COMPLETE_EVENT: &str = "AA13407203E90A0000000000000000000030BB";
    const LOWER_PREHEAT_COMPLETE_WITH_UPPER_BROIL: &str = "AA8240EC0207000000019000000000000000000200000198000000014500000000000000000200000000000000000000000000000000000000000000000000000000020700000001900000000000000000020000029800000001450000000000000000020000000000000000000000000000000000000000000000000000000006BB";
    const UPPER_CANCELLED_WITH_LOWER_ACTIVE: &str = "AA8240EC02070000000190000000000000000002000002980000000145000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000CE0000000000000200000298000000014500000000000000000200000000000000000000000000000000000000000000000000000000DDBB";
    const LOWER_CANCELLED: &str = "AA8240EC0000000000000000CC00000000000002000002980000000145000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000CC000000000000060000000000000000000164000000000000060000000000000000000000000000000000000000000000000000000010BB";
    const UPPER_OVEN_170F_THREE_MINUTES: &str = "AA8240EC00000000000000004E00000000000001000000000000000000004D0000000000000100000000000000000000000000000000000000000000000000000000018100030000AA000000000000000002000000000000000000004D000000000000020000000000000000000000000000000000000000000000000000000020BB";
    const UPPER_OVEN_170F_TWO_MINUTES_59: &str = "AA8240EC018100030000AA000000000000000002000000000000000000004D000000000000020000000000000000000000000000000000000000000000000000000001813B020000AA000000000000000002000000000000000000004D0000000000000200000000000000000000000000000000000000000000000000000000C7BB";
    const UPPER_BAKE_125C_ONE_HOUR_THREE_MINUTES: &str = "AA8240EC00000000000000009F0000000000000100000000000000000000D100000000000000000000000000000000000000000000000000000000000000000000000181000301010100000000000000000200000000000000000000D0000000000000020000000000000000000000000000000000000000000000000000000070BB";
    const UPPER_BAKE_125C_ONE_HOUR_TWO_MINUTES_59: &str = "AA8240EC0181000301010100000000000000000200000000000000000000D0000000000000020000000000000000000000000000000000000000000000000000000001813B0201010100000000000000000200000000000000000000D100000000000002000000000000000000000000000000000000000000000000000000001EBB";
    const UPPER_BAKE_PREHEAT_COMPLETE_EVENT: &str = "AA13407200010A000000000000000000002FBB";
    const UPPER_BAKE_PREHEAT_COMPLETE: &str = "AA8240EC01813B3A00010100000000000000000200000000000000000000C4000000000000020000000000000000000000000000000000000000000000000000000002811F3A00010100000000000000000200000000000000000000C30000000000000200000000000000000000000000000000000000000000000000000000EBBB";
    const UPPER_BAKE_CANCELLED_AT_266F: &str = "AA8240EC02813A3500010100000000000000000200000000000000000000B9000000000000020000000000000000000000000000000000000000000000000000000000000000000000010A0000000000000600000000000000000000B6000000000000060000000000000000000000000000000000000000000000000000000083BB";
    const UPPER_OVEN_FINISHED_AT_124F: &str = "AA8240EC018102000000AA000000000000000002000000000000000000004C000000000000020000000000000000000000000000000000000000000000000000000003000000000000007C00000000000000000000000000000000004C0000000000000200000000000000000000000000000000000000000000000000000000F6BB";
    const UPPER_OVEN_FINISHED_EVENT: &str = "AA13407200040A0000000000000000000028BB";
    const UPPER_OVEN_CLEARED_AT_149F: &str = "AA8240EC03000000000000009500000000000000000000000000000000004D000000000000020000000000000000000000000000000000000000000000000000000000000000000000009500000000000006000000000000000000004D000000000000060000000000000000000000000000000000000000000000000000000078BB";

    #[test]
    fn publishes_status_controls_and_diagnostics() {
        let (ha, _, _) = make();
        let device = only_dev(&ha);
        let comps = &device.config.as_ref().unwrap().components;
        assert_eq!(comps["cooktop_status"]["platform"], "binary_sensor");
        assert_eq!(comps["cooktop_status"]["payload_on"], "ON");
        assert!(comps["cooktop_status"].get("command_topic").is_none());
        assert!(comps["upper_oven_temperature"].get("command_topic").is_none());
        assert_eq!(comps["upper_oven_temperature"]["unit_of_measurement"], "°F");
        assert_eq!(comps["timer"]["platform"], "text");
        assert_eq!(comps["timer"]["command_topic"], "$this/timer/set");
        assert_eq!(comps["timer_stop"]["platform"], "button");
        assert_eq!(comps["upper_remote_temperature"]["platform"], "number");
        assert_eq!(comps["upper_remote_temperature"]["min"], 170);
        assert_eq!(comps["upper_remote_temperature"]["max"], 550);
        assert_eq!(comps["upper_start"]["platform"], "button");
        assert_eq!(comps["upper_start"]["name"], "Start Upper Bake");
        assert_eq!(comps["lower_start"]["name"], "Start Lower");
        assert_eq!(
            comps["lower_remote_mode"]["options"],
            json!(["Bake", "Convection Bake", "Convection Roast"])
        );
        assert_eq!(comps["raw_status"]["entity_category"], "diagnostic");
        assert_eq!(prop(&ha, "upper_remote_temperature").as_deref(), Some("350"));
        assert_eq!(prop(&ha, "upper_remote_cook_time").as_deref(), Some("10"));
        assert_eq!(prop(&ha, "lower_remote_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "lower_oven_remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn start_sends_captured_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA28F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000F3BB"
        );
    }

    #[test]
    fn eb_and_ec_publish_redacted_current_block() {
        let (ha, thinq, _) = make();
        let current = vec![0x22u8; 62];
        let mut inner = vec![0x40, 0xeb];
        inner.extend_from_slice(&current);
        thinq.emit_data(&frame(&inner));
        let red = redact_decoded(&current);
        assert_eq!(
            prop(&ha, "raw_status"),
            Some(format!("eb/{}", hex_lower(&red)))
        );

        let (ha, thinq, _) = make();
        let previous = vec![0x11u8; 62];
        let current = vec![0x22u8; 62];
        let mut inner = vec![0x40, 0xec];
        inner.extend_from_slice(&previous);
        inner.extend_from_slice(&current);
        thinq.emit_data(&frame(&inner));
        let red = redact_decoded(&current);
        assert_eq!(
            prop(&ha, "raw_status"),
            Some(format!("ec/{}", hex_lower(&red)))
        );
    }

    #[test]
    fn cooktop_transitions() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(COOKTOP_OFF));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(COOKTOP_OFF_TO_ON));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(COOKTOP_ON_TO_OFF));
        assert_eq!(prop(&ha, "cooktop_status").as_deref(), Some("OFF"));
    }

    #[test]
    fn timer_set_rollover_and_stop() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(TIMER_09_01_13));
        assert_eq!(prop(&ha, "timer").as_deref(), Some("09:01:13"));
        thinq.emit_data(&hex_decode(TIMER_ROLLOVER_09_00_59));
        assert_eq!(prop(&ha, "timer").as_deref(), Some("09:00:59"));
        thinq.emit_data(&hex_decode(TIMER_STOPPED));
        assert_eq!(prop(&ha, "timer").as_deref(), Some("00:00:00"));
    }

    #[test]
    fn timer_expiry_is_undecoded_diagnostic() {
        let (ha, thinq, _) = make();
        let pkt = hex_decode(TIMER_EXPIRED_ALARM);
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "undecoded_frame"), Some(inner_hex(&pkt)));
    }

    #[test]
    fn set_and_cancel_acks_ignored() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA084000430060BB"));
        thinq.emit_data(&hex_decode("AA084000440063BB"));
        assert!(prop(&ha, "undecoded_frame").is_none());
    }

    #[test]
    fn timer_set_and_stop_commands() {
        let (_, thinq, dev) = make();
        dev.set_property("timer", "09:01:13");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF04323068080800D0109FEBB");
        thinq.reset_recorder();
        dev.set_property("timer_stop", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0432306808080000000C1BB");
    }

    #[test]
    fn invalid_timer_and_non_press_send_nothing() {
        let (_, thinq, dev) = make();
        dev.set_property("timer", "9:01:13");
        dev.set_property("timer", "09:61:13");
        dev.set_property("timer_stop", "not-a-press");
        assert_eq!(thinq.outbox().len(), 0);
    }

    #[test]
    fn cancel_selectors() {
        let (_, thinq, dev) = make();
        dev.set_property("upper_cancel", "PRESS");
        dev.set_property("lower_cancel", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA07F04400B0BB");
        assert_eq!(hex_encode(&thinq.outbox()[1]), "AA07F04401B3BB");
    }

    #[test]
    fn remote_start_requires_readiness_and_matches_captures() {
        let (_, thinq, dev) = make();
        dev.set_property("upper_start", "PRESS");
        dev.set_property("lower_start", "PRESS");
        assert_eq!(thinq.outbox().len(), 0);

        dev.set_property("upper_remote_temperature", "170");
        dev.set_property("upper_remote_cook_time", "3");
        dev.set_property("lower_remote_temperature", "350");
        dev.set_property("lower_remote_cook_time", "0");
        dev.set_property("lower_remote_mode", "Convection Roast");
        thinq.emit_data(&hex_decode(BOTH_OVENS_REMOTE_START_READY));
        thinq.reset_recorder();
        dev.set_property("upper_start", "PRESS");
        dev.set_property("upper_start", "PRESS");
        dev.set_property("lower_start", "PRESS");
        assert_eq!(thinq.outbox().len(), 2);
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA13F043200B010000000000AA000300009CBB");
        assert_eq!(hex_encode(&thinq.outbox()[1]), "AA13F043200B1800000000015E00000000C7BB");
    }

    #[test]
    fn remote_inputs_validate_and_encode() {
        let (ha, thinq, dev) = make();
        dev.set_property("upper_remote_temperature", "400");
        dev.set_property("upper_remote_cook_time", "90");
        dev.set_property("upper_remote_temperature", "401");
        dev.set_property("upper_remote_cook_time", "1441");
        assert_eq!(prop(&ha, "upper_remote_temperature").as_deref(), Some("400"));
        assert_eq!(prop(&ha, "upper_remote_cook_time").as_deref(), Some("90"));
        assert_eq!(thinq.outbox().len(), 0);
        thinq.emit_data(&hex_decode(BOTH_OVENS_REMOTE_START_READY));
        thinq.reset_recorder();
        dev.set_property("upper_start", "PRESS");
        let command = thinq.outbox()[0].clone();
        assert_eq!(
            &command[2..command.len() - 2],
            &[0xf0, 0x43, 0x20, 0x0b, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x90, 0x01, 0x1e, 0x00, 0x00]
        );
    }

    #[test]
    fn lower_mode_encodes_every_captured_operation() {
        let (ha, thinq, dev) = make();
        dev.set_property("lower_remote_mode", "Convection Roast");
        dev.set_property("lower_remote_mode", "Steam Bake");
        assert_eq!(prop(&ha, "lower_remote_mode").as_deref(), Some("Convection Roast"));
        assert_eq!(thinq.outbox().len(), 0);
        dev.set_property("lower_remote_temperature", "300");
        dev.set_property("lower_remote_cook_time", "10");
        dev.set_property("lower_remote_mode", "Bake");
        assert_eq!(prop(&ha, "lower_remote_mode").as_deref(), Some("Bake"));
        thinq.emit_data(&hex_decode(LOWER_ONLY_REMOTE_START_READY));
        thinq.reset_recorder();
        dev.set_property("lower_start", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA13F043200B1500000000012C000A000032BB");

        dev.set_property("lower_remote_mode", "Convection Bake");
        thinq.emit_data(&hex_decode(LOWER_ONLY_REMOTE_START_READY));
        dev.set_property("lower_start", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[1]), "AA13F043200B1700000000012C000A00003CBB");

        dev.set_property("lower_remote_mode", "Convection Roast");
        thinq.emit_data(&hex_decode(LOWER_ONLY_REMOTE_START_READY));
        dev.set_property("lower_start", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[2]), "AA13F043200B1800000000012C000A00003FBB");
    }

    #[test]
    fn upper_remote_start_pins_cavity_order() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(BOTH_OVENS_REMOTE_START_READY));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "lower_oven_remote_start").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(UPPER_OVEN_170F_THREE_MINUTES));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "upper_oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "upper_oven_temperature").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "upper_oven_set_temperature").as_deref(), Some("170"));
        assert_eq!(prop(&ha, "upper_oven_cook_time").as_deref(), Some("00:03:00"));
        assert_eq!(prop(&ha, "lower_oven_temperature").as_deref(), Some("77"));
        assert_eq!(prop(&ha, "lower_oven_set_temperature").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "lower_oven_cook_time").as_deref(), Some("00:00:00"));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "lower_oven_remote_start").as_deref(), Some("OFF"));

        let raw_during = prop(&ha, "raw_status");
        thinq.emit_data(&hex_decode(UPPER_OVEN_170F_TWO_MINUTES_59));
        assert_eq!(prop(&ha, "upper_oven_cook_time").as_deref(), Some("00:02:59"));
        assert_eq!(prop(&ha, "raw_status"), raw_during);

        thinq.emit_data(&hex_decode(UPPER_OVEN_FINISHED_AT_124F));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Complete"));
        assert_eq!(prop(&ha, "upper_oven_temperature").as_deref(), Some("124"));
        assert_eq!(prop(&ha, "upper_oven_set_temperature").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "upper_oven_cook_time").as_deref(), Some("00:00:00"));

        let evt = hex_decode(UPPER_OVEN_FINISHED_EVENT);
        thinq.emit_data(&evt);
        assert_eq!(prop(&ha, "undecoded_frame"), Some(inner_hex(&evt)));

        thinq.emit_data(&hex_decode(UPPER_OVEN_CLEARED_AT_149F));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "upper_oven_temperature").as_deref(), Some("149"));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "lower_oven_remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn upper_bake_125c() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(UPPER_BAKE_125C_ONE_HOUR_THREE_MINUTES));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "upper_oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "upper_oven_set_temperature").as_deref(), Some("257"));
        assert_eq!(prop(&ha, "upper_oven_cook_time").as_deref(), Some("01:03:00"));
        thinq.emit_data(&hex_decode(UPPER_BAKE_125C_ONE_HOUR_TWO_MINUTES_59));
        assert_eq!(prop(&ha, "upper_oven_cook_time").as_deref(), Some("01:02:59"));
        let evt = hex_decode(UPPER_BAKE_PREHEAT_COMPLETE_EVENT);
        thinq.emit_data(&evt);
        assert_eq!(prop(&ha, "undecoded_frame"), Some(inner_hex(&evt)));
        thinq.emit_data(&hex_decode(UPPER_BAKE_PREHEAT_COMPLETE));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Active"));
        assert_eq!(prop(&ha, "upper_oven_cook_time").as_deref(), Some("00:58:31"));
        thinq.emit_data(&hex_decode(UPPER_BAKE_CANCELLED_AT_266F));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "upper_oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "upper_oven_temperature").as_deref(), Some("266"));
        assert_eq!(prop(&ha, "upper_oven_set_temperature").as_deref(), Some("0"));
    }

    #[test]
    fn lower_only_ready_and_bake_285c() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(LOWER_ONLY_REMOTE_START_READY));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "lower_oven_remote_start").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(LOWER_BAKE_285C_FIVE_MINUTES));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "lower_oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "lower_oven_set_temperature").as_deref(), Some("545"));
        assert_eq!(prop(&ha, "lower_oven_cook_time").as_deref(), Some("00:05:00"));
        thinq.emit_data(&hex_decode(LOWER_BAKE_285C_FOUR_MINUTES_59));
        assert_eq!(prop(&ha, "lower_oven_cook_time").as_deref(), Some("00:04:59"));
        thinq.emit_data(&hex_decode(LOWER_BAKE_CANCELLED_AT_252F));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "lower_oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "lower_oven_temperature").as_deref(), Some("252"));
        assert_eq!(prop(&ha, "lower_oven_set_temperature").as_deref(), Some("0"));
    }

    #[test]
    fn lower_convection_bake_and_roast() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(LOWER_CONVECTION_BAKE_300F_TEN_MINUTES));
        assert_eq!(prop(&ha, "lower_oven_mode").as_deref(), Some("Convection Bake"));
        assert_eq!(prop(&ha, "lower_oven_set_temperature").as_deref(), Some("275"));
        assert_eq!(prop(&ha, "lower_oven_cook_time").as_deref(), Some("00:10:00"));
        thinq.emit_data(&hex_decode(LOWER_CONVECTION_BAKE_300F_NINE_MINUTES_59));
        assert_eq!(prop(&ha, "lower_oven_cook_time").as_deref(), Some("00:09:59"));
        let evt = hex_decode(LOWER_PREHEAT_COMPLETE_EVENT);
        thinq.emit_data(&evt);
        assert_eq!(prop(&ha, "undecoded_frame"), Some(inner_hex(&evt)));
        thinq.emit_data(&hex_decode(LOWER_CONVECTION_BAKE_PREHEAT_COMPLETE));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Active"));
        assert_eq!(prop(&ha, "lower_oven_cook_time").as_deref(), Some("00:09:05"));

        thinq.emit_data(&hex_decode(LOWER_CONVECTION_ROAST_350F_TIME_OFF));
        assert_eq!(prop(&ha, "lower_oven_set_temperature").as_deref(), Some("325"));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "lower_oven_mode").as_deref(), Some("Convection Roast"));
        assert_eq!(prop(&ha, "lower_oven_cook_time").as_deref(), Some("00:00:00"));
    }

    #[test]
    fn dual_cavity_broil_and_cancels() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(UPPER_BROIL_HIGH_WITH_LOWER_PREHEATING));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Active"));
        assert_eq!(prop(&ha, "upper_oven_mode").as_deref(), Some("Broil"));
        assert_eq!(prop(&ha, "upper_oven_set_temperature").as_deref(), Some("400"));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "lower_oven_mode").as_deref(), Some("Convection Roast"));
        assert_eq!(prop(&ha, "lower_oven_set_temperature").as_deref(), Some("325"));

        let evt = hex_decode(LOWER_PREHEAT_COMPLETE_EVENT);
        thinq.emit_data(&evt);
        thinq.emit_data(&hex_decode(LOWER_PREHEAT_COMPLETE_WITH_UPPER_BROIL));
        assert_eq!(prop(&ha, "upper_oven_mode").as_deref(), Some("Broil"));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Active"));
        assert_eq!(prop(&ha, "lower_oven_temperature").as_deref(), Some("0"));

        thinq.emit_data(&hex_decode(UPPER_CANCELLED_WITH_LOWER_ACTIVE));
        assert_eq!(prop(&ha, "upper_oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "upper_oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "upper_oven_temperature").as_deref(), Some("206"));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Active"));
        assert_eq!(prop(&ha, "lower_oven_set_temperature").as_deref(), Some("325"));

        thinq.emit_data(&hex_decode(LOWER_CANCELLED));
        assert_eq!(prop(&ha, "upper_oven_temperature").as_deref(), Some("204"));
        assert_eq!(prop(&ha, "lower_oven_state").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "lower_oven_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "lower_oven_temperature").as_deref(), Some("356"));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "lower_oven_remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn other_opcodes_and_malformed_are_diagnostics() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&frame(&[0x40, 0x31, 0x01, 0x02]));
        assert_eq!(prop(&ha, "undecoded_frame").as_deref(), Some("40310102"));
        thinq.emit_data(&frame(&[0x32, 0xeb, 0x01]));
        assert_eq!(prop(&ha, "undecoded_frame").as_deref(), Some("32eb01"));
        thinq.emit_data(&frame(&[0x40, 0xeb, 0x01]));
        assert_eq!(prop(&ha, "undecoded_frame").as_deref(), Some("40eb01"));
        thinq.emit_data(&frame(&[0x40, 0xec, 0x01]));
        assert_eq!(prop(&ha, "undecoded_frame").as_deref(), Some("40ec01"));
        thinq.emit_data(&frame(&[0x40, 0x00, 0x45, 0x00]));
        assert_eq!(prop(&ha, "undecoded_frame").as_deref(), Some("40004500"));
        assert!(prop(&ha, "raw_status").is_none());
    }

    #[test]
    fn blank_remote_numbers_rejected() {
        let (ha, thinq, dev) = make();
        dev.set_property("upper_remote_temperature", "170");
        dev.set_property("upper_remote_cook_time", "3");
        dev.set_property("upper_remote_cook_time", "");
        dev.set_property("upper_remote_cook_time", "  ");
        assert_eq!(prop(&ha, "upper_remote_cook_time").as_deref(), Some("3"));
        thinq.emit_data(&hex_decode(BOTH_OVENS_REMOTE_START_READY));
        thinq.reset_recorder();
        dev.set_property("upper_start", "PRESS");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA13F043200B010000000000AA000300009CBB");
    }

    #[test]
    fn reconnect_keeps_staged_params_resets_ready() {
        let (ha, thinq, dev) = make();
        dev.set_property("upper_remote_temperature", "425");
        dev.set_property("lower_remote_mode", "Convection Bake");
        thinq.emit_data(&hex_decode(BOTH_OVENS_REMOTE_START_READY));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("ON"));

        let ha_dyn: Arc<dyn HaConnection> = ha.clone();
        let thinq2 = MockThinq2Device::new(thinq.id(), meta("WFV474PGV"));
        let tq_dyn: Arc<dyn Thinq2Device> = thinq2;
        let _dev2 = Device::new(ha_dyn, tq_dyn, meta("WFV474PGV"));
        assert_eq!(prop(&ha, "upper_remote_temperature").as_deref(), Some("425"));
        assert_eq!(prop(&ha, "lower_remote_mode").as_deref(), Some("Convection Bake"));
        assert_eq!(prop(&ha, "upper_oven_remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "lower_oven_remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn unflagged_panel_modes_decode() {
        let (ha, thinq, _) = make();
        let mut current = vec![0u8; 62];
        current[1] = 0x01;
        current[19] = 0x17;
        let mut inner = vec![0x40, 0xeb];
        inner.extend_from_slice(&current);
        thinq.emit_data(&frame(&inner));
        assert_eq!(prop(&ha, "upper_oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "lower_oven_mode").as_deref(), Some("Convection Bake"));
    }
}
