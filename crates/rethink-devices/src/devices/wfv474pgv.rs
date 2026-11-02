//! LG WFV474PGV range: a cooktop plus upper and lower ovens (deviceType 301, AABB class 0x40).
//!
//! Status arrives as `40 EB` (one 62-byte record, the reply to the status query sent on start) or
//! `40 EC` (previous then current record). The `40 31` component manifest, the `40 72` one-shot
//! events (timer expiry, cook completion) and anything else not understood are logged, never
//! published. Writes are limited to captured LG-app frames: `F0 43 20` cavity start, `F0 43 21`
//! Preference/clock write, `F0 43 23` timer and `F0 44` cavity cancel.
//!
//! The oven enforces Remote Start arming itself (an unarmed start is answered `40 00 43 40` and
//! ignored), so Start is gated only through HA availability on the readiness topic. The
//! Preference rows cannot be read back from the appliance, so they are write-only and optimistic.
//!
//! Port of upstream PR #144.

use crate::device_trait::DeviceHandler;
use chrono::Timelike;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::ha::PropertyValue;
use rethink_core::{hex_decode, HaConnection, Metadata, Thinq2Device};
use rethink_util::hex;
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x40;
const STATUS_RECORD_LENGTH: usize = 62;

// Redundant copies of one global cooktop state, not independent burners.
const COOKTOP_SLOT_STATE_OFFSETS: [usize; 5] = [36, 41, 46, 51, 56];
// Plain binary, not BCD.
const TIMER_SECONDS_OFFSET: usize = 30;
const TIMER_MINUTES_OFFSET: usize = 31;
const TIMER_HOURS_OFFSET: usize = 32;

// One frame carries the whole Preference screen with every untouched row marked "no change". The
// 0x0E length byte does not match the 13-byte payload; it is reproduced from the captures.
const PREFERENCE_COMMAND_PREFIX: [u8; 4] = [0xf0, 0x43, 0x21, 0x0e];
const PREFERENCE_PAYLOAD_LENGTH: usize = 13;
const PREFERENCE_NO_CHANGE: u8 = 0x80;
// Index 11 has a sentinel of its own.
const PREFERENCE_FF_INDEX: usize = 11;
const PREFERENCE_FF_NO_CHANGE: u8 = 0xff;
const PREFERENCE_OFF: u8 = 0x00;
const PREFERENCE_ON: u8 = 0x01;
const CLOCK_HOURS_INDEX: usize = 0;
const CLOCK_MINUTES_INDEX: usize = 1;
const CLOCK_HOUR_FORMAT_INDEX: usize = 2;
// Byte 2 was 0x01 with the hour sent as 11 and 0x00 with the same time sent as 23. The 0x00 shape
// carries the full 0-23 hour, so it is unambiguous whichever meaning byte 2 has.
const CLOCK_HOUR_FORMAT_24H: u8 = 0x00;
const AUTO_CONVERSION_INDEX: usize = 3;
const TEMPERATURE_ADJUSTMENT_UNIT_INDEX: usize = 4;
const TEMPERATURE_ADJUSTMENT_FAHRENHEIT: u8 = 0x00;
const PREHEAT_ALARM_LIGHT_INDEX: usize = 7;
const BEEPER_VOLUME_INDEX: usize = 8;
// Only the oven's own display: status temperatures stay Fahrenheit either way.
const TEMPERATURE_UNIT_INDEX: usize = 9;
// The LG app's own picker bounds in °F.
const MIN_TEMPERATURE_ADJUSTMENT: i32 = -35;
const MAX_TEMPERATURE_ADJUSTMENT: i32 = 35;

/// Labels with their codes, in option order; the first code is the one writes use.
type Table = &'static [(&'static str, &'static [u8])];

const BEEPER_VOLUMES: Table = &[("High", &[0x02]), ("Low", &[0x01]), ("Mute", &[0x00])];
const TEMPERATURE_UNITS: Table = &[("°F", &[0x00]), ("°C", &[0x01])];

const CAVITY_BLOCK_LENGTH: usize = 18;
const CAVITY_STATE_OFFSET: usize = 0;
const CAVITY_MODE_OFFSET: usize = 1;
const CAVITY_COOK_SECONDS_OFFSET: usize = 2;
const CAVITY_COOK_MINUTES_OFFSET: usize = 3;
const CAVITY_COOK_HOURS_OFFSET: usize = 4;
// Unsigned 16-bit big-endian °F.
const CAVITY_SET_TEMPERATURE_OFFSET: usize = 5;
const CAVITY_CURRENT_TEMPERATURE_OFFSET: usize = 7;
// A small status enum (0x0E idle, 0x08 arming, 0x02 other cavity active, ...), not a bitfield;
// 0x01 is the only value ever seen while Start was accepted.
const CAVITY_REMOTE_START_OFFSET: usize = 15;
const REMOTE_START_READY: u8 = 0x01;

const UPPER_BAKE_OPERATION: u8 = 0x01;
const LOWER_BAKE_OPERATION: u8 = 0x15;
const LOWER_CONVECTION_BAKE_OPERATION: u8 = 0x17;
const LOWER_CONVECTION_ROAST_OPERATION: u8 = 0x18;
const LOWER_REMOTE_MODES: Table = &[
    ("Bake", &[LOWER_BAKE_OPERATION]),
    ("Convection Bake", &[LOWER_CONVECTION_BAKE_OPERATION]),
    ("Convection Roast", &[LOWER_CONVECTION_ROAST_OPERATION]),
];

const OVEN_STATES: Table = &[
    ("Idle", &[0x00]),
    ("Preheating", &[0x01]),
    ("Active", &[0x02]),
    ("Complete", &[0x03]),
];
// Remote starts report mode = operation | 0x80; masking the flag lets panel-started cooks decode.
const REMOTE_START_MODE_FLAG: u8 = 0x80;
const OVEN_MODES: Table = &[
    ("None", &[0x00]),
    ("Bake", &[LOWER_BAKE_OPERATION, UPPER_BAKE_OPERATION]),
    ("Broil", &[0x07]),
    ("Convection Bake", &[LOWER_CONVECTION_BAKE_OPERATION]),
    ("Convection Roast", &[LOWER_CONVECTION_ROAST_OPERATION]),
];

const MIN_REMOTE_TEMPERATURE: i64 = 170;
const MAX_REMOTE_TEMPERATURE: i64 = 550;
const REMOTE_TEMPERATURE_STEP: i64 = 5;
const MAX_REMOTE_COOK_TIME_MINUTES: i64 = 24 * 60;

// Observed from LG's cloud, not inferred.
const STATUS_QUERY: &str =
    "F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CavityId {
    Upper,
    Lower,
}

const CAVITY_IDS: [CavityId; 2] = [CavityId::Upper, CavityId::Lower];

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

    // An app command explicitly labelled Upper populated only the first block.
    fn offset(self) -> usize {
        match self {
            CavityId::Upper => 0,
            CavityId::Lower => 18,
        }
    }

    fn cancel_selector(self) -> u8 {
        match self {
            CavityId::Upper => 0x00,
            CavityId::Lower => 0x01,
        }
    }

    fn temperature_adjustment_index(self) -> usize {
        match self {
            CavityId::Upper => 5,
            CavityId::Lower => 6,
        }
    }
}

struct CavityRemote {
    temperature: i64,
    cook_time: i64,
}

struct RemoteParams {
    upper: CavityRemote,
    lower: CavityRemote,
    lower_mode: String,
}

impl Default for RemoteParams {
    fn default() -> Self {
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
}

impl RemoteParams {
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

fn label(table: Table, code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(_, codes)| codes.contains(&code))
        .map(|&(label, _)| label)
}

fn code(table: Table, label: &str) -> Option<u8> {
    table
        .iter()
        .find(|&&(l, _)| l == label)
        .map(|(_, codes)| codes[0])
}

fn options(table: Table) -> Vec<&'static str> {
    table.iter().map(|&(label, _)| label).collect()
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

fn total_seconds(hours: u8, minutes: u8, seconds: u8) -> i64 {
    i64::from(hours) * 3600 + i64::from(minutes) * 60 + i64::from(seconds)
}

/// Zero is the oven declining to report: the current temperature reads 0 for a whole active cycle
/// and the setpoint whenever no cook is programmed.
fn temperature(block: &[u8], offset: usize) -> PropertyValue {
    match u16::from_be_bytes([block[offset], block[offset + 1]]) {
        0 => "None".into(),
        value => i64::from(value).into(),
    }
}

/// Digits only, so a non-numeric payload never reaches the wire (an empty cook time would read as
/// 0, "no time limit"). The range is left to the HA component's own bounds.
fn parse_staged_number(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    trimmed.parse().ok()
}

/// Signed, and range-checked so a payload that bypassed the HA bounds stays off the wire.
fn parse_temperature_adjustment(value: &str) -> Option<i32> {
    let trimmed = value.trim();
    let digits = trimmed.strip_prefix('-').unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let degrees: i32 = trimmed.parse().ok()?;
    (MIN_TEMPERATURE_ADJUSTMENT..=MAX_TEMPERATURE_ADJUSTMENT)
        .contains(&degrees)
        .then_some(degrees)
}

fn preference_payload() -> [u8; PREFERENCE_PAYLOAD_LENGTH] {
    let mut payload = [PREFERENCE_NO_CHANGE; PREFERENCE_PAYLOAD_LENGTH];
    payload[PREFERENCE_FF_INDEX] = PREFERENCE_FF_NO_CHANGE;
    payload
}

fn on_off_code(mqtt_value: &str) -> Option<u8> {
    match mqtt_value {
        "ON" => Some(PREFERENCE_ON),
        "OFF" => Some(PREFERENCE_OFF),
        _ => None,
    }
}

fn split_cavity(prop: &str) -> Option<(CavityId, &str)> {
    CAVITY_IDS.into_iter().find_map(|cavity| {
        let action = prop.strip_prefix(cavity.as_str())?.strip_prefix('_')?;
        Some((cavity, action))
    })
}

fn cavity_sensor_components(cavity: CavityId, comps: &mut Map<String, Value>) {
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
        format!("{oven}_status"),
        json!({
            "platform": "sensor",
            "device_class": "enum",
            "icon": "mdi:stove",
            "unique_id": format!("$deviceid-{oven}_status"),
            "state_topic": format!("$this/{oven}_status"),
            "name": format!("{title} Oven State"),
            "options": options(OVEN_STATES),
        }),
    );
    comps.insert(
        format!("{oven}_mode"),
        json!({
            "platform": "sensor",
            "device_class": "enum",
            "icon": "mdi:chef-hat",
            "unique_id": format!("$deviceid-{oven}_mode"),
            "state_topic": format!("$this/{oven}_mode"),
            "name": format!("{title} Oven Mode"),
            "options": options(OVEN_MODES),
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
            "device_class": "duration",
            "icon": "mdi:timer-outline",
            "unique_id": format!("$deviceid-{oven}_cook_time"),
            "state_topic": format!("$this/{oven}_cook_time"),
            "name": format!("{title} Oven Cook Time"),
            "unit_of_measurement": "s",
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

fn cavity_temperature_adjustment_components(cavity: CavityId, comps: &mut Map<String, Value>) {
    let id = cavity.as_str();
    let title = cavity.title();
    comps.insert(
        format!("{id}_temperature_adjustment"),
        json!({
            "platform": "number",
            "device_class": "temperature",
            "icon": "mdi:thermometer-plus",
            "unique_id": format!("$deviceid-{id}_temperature_adjustment"),
            "command_topic": format!("$this/{id}_temperature_adjustment/set"),
            "unit_of_measurement": "°F",
            "min": MIN_TEMPERATURE_ADJUSTMENT,
            "max": MAX_TEMPERATURE_ADJUSTMENT,
            "step": 1,
            "mode": "box",
            "optimistic": true,
            "name": format!("{title} Temperature Adjustment"),
        }),
    );
}

fn cavity_remote_components(cavity: CavityId, comps: &mut Map<String, Value>) {
    let id = cavity.as_str();
    let title = cavity.title();
    comps.insert(
        format!("{id}_remote_temperature"),
        json!({
            "platform": "number",
            "device_class": "temperature",
            "icon": "mdi:thermometer-chevron-up",
            "unique_id": format!("$deviceid-{id}_remote_temperature"),
            "state_topic": format!("$this/{id}_remote_temperature"),
            "command_topic": format!("$this/{id}_remote_temperature/set"),
            "unit_of_measurement": "°F",
            "min": MIN_REMOTE_TEMPERATURE,
            "max": MAX_REMOTE_TEMPERATURE,
            "step": REMOTE_TEMPERATURE_STEP,
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
            "unique_id": format!("$deviceid-{id}_remote_cook_time"),
            "state_topic": format!("$this/{id}_remote_cook_time"),
            "command_topic": format!("$this/{id}_remote_cook_time/set"),
            "unit_of_measurement": "min",
            "min": 0,
            "max": MAX_REMOTE_COOK_TIME_MINUTES,
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
            "unique_id": format!("$deviceid-{id}_start"),
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
            "unique_id": format!("$deviceid-{id}_cancel"),
            "command_topic": format!("$this/{id}_cancel/set"),
            "payload_press": "PRESS",
            "name": format!("Cancel {title}"),
        }),
    );
}

fn components() -> Map<String, Value> {
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
    for cavity in CAVITY_IDS {
        cavity_sensor_components(cavity, &mut comps);
    }
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
            "unique_id": "$deviceid-timer_stop",
            "command_topic": "$this/timer_stop/set",
            "payload_press": "PRESS",
            "name": "Stop / Acknowledge Timer",
        }),
    );
    comps.insert(
        "clock_sync".into(),
        json!({
            "platform": "button",
            "icon": "mdi:clock-check-outline",
            "unique_id": "$deviceid-clock_sync",
            "command_topic": "$this/clock_sync/set",
            "payload_press": "PRESS",
            "name": "Sync Clock",
        }),
    );
    comps.insert(
        "beeper_volume".into(),
        json!({
            "platform": "select",
            "icon": "mdi:volume-high",
            "unique_id": "$deviceid-beeper_volume",
            "command_topic": "$this/beeper_volume/set",
            "options": options(BEEPER_VOLUMES),
            "optimistic": true,
            "name": "Beeper Volume",
        }),
    );
    comps.insert(
        "preheat_alarm_light".into(),
        json!({
            "platform": "switch",
            "icon": "mdi:lightbulb-on-outline",
            "unique_id": "$deviceid-preheat_alarm_light",
            "command_topic": "$this/preheat_alarm_light/set",
            "payload_on": "ON",
            "payload_off": "OFF",
            "optimistic": true,
            "name": "Preheating Alarm Light",
        }),
    );
    comps.insert(
        "temperature_unit".into(),
        json!({
            "platform": "select",
            "icon": "mdi:thermometer",
            "unique_id": "$deviceid-temperature_unit",
            "command_topic": "$this/temperature_unit/set",
            "options": options(TEMPERATURE_UNITS),
            "optimistic": true,
            "name": "Temperature Unit",
        }),
    );
    comps.insert(
        "auto_conversion".into(),
        json!({
            "platform": "switch",
            "icon": "mdi:thermometer-auto",
            "unique_id": "$deviceid-auto_conversion",
            "command_topic": "$this/auto_conversion/set",
            "payload_on": "ON",
            "payload_off": "OFF",
            "optimistic": true,
            "name": "Auto Conversion",
        }),
    );
    for cavity in CAVITY_IDS {
        cavity_temperature_adjustment_components(cavity, &mut comps);
    }
    for cavity in CAVITY_IDS {
        cavity_remote_components(cavity, &mut comps);
    }
    comps.insert(
        "lower_remote_mode".into(),
        json!({
            "platform": "select",
            "icon": "mdi:chef-hat",
            "unique_id": "$deviceid-lower_remote_mode",
            "state_topic": "$this/lower_remote_mode",
            "command_topic": "$this/lower_remote_mode/set",
            "options": options(LOWER_REMOTE_MODES),
            "name": "Lower Remote Mode",
        }),
    );
    comps
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    remote: Mutex<RemoteParams>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut config = default_config(&meta, Some(json!({"name": "LG Oven"})));
        config.components = components().into_iter().collect();
        core.set_config(config);

        let remote = RemoteParams::default();
        for cavity in CAVITY_IDS {
            let id = cavity.as_str();
            let staged = remote.cavity(cavity);
            core.publish_property(
                &format!("{id}_remote_temperature"),
                staged.temperature.into(),
            );
            core.publish_property(&format!("{id}_remote_cook_time"), staged.cook_time.into());
            // Readiness is trusted only from live status: a stale retained ON must not enable
            // Start before the first status reply.
            core.publish_on_off(&format!("{id}_oven_remote_start"), false);
        }
        core.publish_property("lower_remote_mode", remote.lower_mode.as_str().into());

        let this = Arc::new(Self {
            core,
            remote: Mutex::new(remote),
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() >= 2 && buf[0] == CLASS_BYTE {
            // Acks echo the command family (0x43 set/start, 0x44 cancel) and a result byte; 0x40
            // answered a start aimed at an unarmed cavity.
            if buf.len() == 4 && buf[1] == 0x00 && (buf[2] == 0x43 || buf[2] == 0x44) {
                if buf[3] != 0x00 {
                    tracing::debug!(
                        "WFV474PGV {}: command rejected by the oven {}",
                        self.core.id,
                        hex::encode(buf)
                    );
                }
                return;
            }
            if let Some(current) = process_common_status(buf, CLASS_BYTE, STATUS_RECORD_LENGTH) {
                self.publish_status(current);
                return;
            }
        }
        tracing::debug!(
            "WFV474PGV {}: undecoded frame {}",
            self.core.id,
            hex::encode(buf)
        );
    }

    fn publish_status(&self, current: &[u8]) {
        self.core.publish_property(
            "timer",
            format_time(
                current[TIMER_HOURS_OFFSET],
                current[TIMER_MINUTES_OFFSET],
                current[TIMER_SECONDS_OFFSET],
            )
            .into(),
        );
        for cavity in CAVITY_IDS {
            let offset = cavity.offset();
            self.publish_cavity(cavity, &current[offset..offset + CAVITY_BLOCK_LENGTH]);
        }
        let cooktop_on = COOKTOP_SLOT_STATE_OFFSETS
            .iter()
            .any(|&offset| current[offset] != 0);
        self.core.publish_on_off("cooktop_status", cooktop_on);
    }

    fn publish_cavity(&self, cavity: CavityId, block: &[u8]) {
        let oven = format!("{}_oven", cavity.as_str());
        self.core.publish_property(
            &format!("{oven}_status"),
            self.oven_state(block[CAVITY_STATE_OFFSET]).into(),
        );
        self.core.publish_property(
            &format!("{oven}_mode"),
            self.oven_mode(block[CAVITY_MODE_OFFSET]).into(),
        );
        self.core.publish_property(
            &format!("{oven}_temperature"),
            temperature(block, CAVITY_CURRENT_TEMPERATURE_OFFSET),
        );
        self.core.publish_property(
            &format!("{oven}_set_temperature"),
            temperature(block, CAVITY_SET_TEMPERATURE_OFFSET),
        );
        self.core.publish_property(
            &format!("{oven}_cook_time"),
            total_seconds(
                block[CAVITY_COOK_HOURS_OFFSET],
                block[CAVITY_COOK_MINUTES_OFFSET],
                block[CAVITY_COOK_SECONDS_OFFSET],
            )
            .into(),
        );
        let ready = block[CAVITY_REMOTE_START_OFFSET] == REMOTE_START_READY;
        self.core
            .publish_on_off(&format!("{oven}_remote_start"), ready);
    }

    /// HA rejects a value outside the declared options, so an undecoded byte reads as unknown.
    fn oven_state(&self, value: u8) -> &'static str {
        label(OVEN_STATES, value).unwrap_or_else(|| {
            tracing::debug!("WFV474PGV {}: undecoded oven state {value:x}", self.core.id);
            "None"
        })
    }

    fn oven_mode(&self, value: u8) -> &'static str {
        label(OVEN_MODES, value & !REMOTE_START_MODE_FLAG).unwrap_or_else(|| {
            tracing::debug!("WFV474PGV {}: undecoded oven mode {value:x}", self.core.id);
            "None"
        })
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "timer" => {
                if let Some((hours, minutes, seconds)) = parse_time(mqtt_value) {
                    self.send_timer(hours, minutes, seconds);
                }
            }
            "timer_stop" => {
                if mqtt_value == "PRESS" {
                    self.send_timer(0, 0, 0);
                }
            }
            "clock_sync" => {
                if mqtt_value == "PRESS" {
                    let now = chrono::Local::now();
                    self.send_clock(now.hour() as u8, now.minute() as u8);
                }
            }
            "beeper_volume" => {
                if let Some(volume) = code(BEEPER_VOLUMES, mqtt_value) {
                    self.send_preference_field(BEEPER_VOLUME_INDEX, volume);
                }
            }
            "preheat_alarm_light" => {
                if let Some(value) = on_off_code(mqtt_value) {
                    self.send_preference_field(PREHEAT_ALARM_LIGHT_INDEX, value);
                }
            }
            "temperature_unit" => {
                if let Some(unit) = code(TEMPERATURE_UNITS, mqtt_value) {
                    self.send_preference_field(TEMPERATURE_UNIT_INDEX, unit);
                }
            }
            "auto_conversion" => {
                if let Some(value) = on_off_code(mqtt_value) {
                    self.send_preference_field(AUTO_CONVERSION_INDEX, value);
                }
            }
            "lower_remote_mode" => {
                // Only a mode with an operation code is staged; Start reads it back the same way.
                if code(LOWER_REMOTE_MODES, mqtt_value).is_some() {
                    self.remote.lock().lower_mode = mqtt_value.to_string();
                    self.core.publish_property(prop, mqtt_value.into());
                }
            }
            _ => {
                if let Some((cavity, action)) = split_cavity(prop) {
                    self.set_cavity_property(cavity, action, prop, mqtt_value);
                }
            }
        }
    }

    fn set_cavity_property(&self, cavity: CavityId, action: &str, prop: &str, mqtt_value: &str) {
        match action {
            "temperature_adjustment" => {
                if let Some(degrees) = parse_temperature_adjustment(mqtt_value) {
                    self.send_temperature_adjustment(&[(cavity, degrees)]);
                }
            }
            "remote_temperature" => {
                if let Some(value) = self.stage_number(prop, mqtt_value) {
                    self.remote.lock().cavity_mut(cavity).temperature = value;
                }
            }
            "remote_cook_time" => {
                if let Some(value) = self.stage_number(prop, mqtt_value) {
                    self.remote.lock().cavity_mut(cavity).cook_time = value;
                }
            }
            "cancel" if mqtt_value == "PRESS" => {
                self.core.send(&[0xf0, 0x44, cavity.cancel_selector()]);
            }
            "start" if mqtt_value == "PRESS" => self.start_cavity(cavity),
            _ => {}
        }
    }

    fn stage_number(&self, prop: &str, mqtt_value: &str) -> Option<i64> {
        let value = parse_staged_number(mqtt_value)?;
        self.core.publish_property(prop, value.into());
        Some(value)
    }

    fn start_cavity(&self, cavity: CavityId) {
        let (operation, temperature, cook_time) = {
            let remote = self.remote.lock();
            let operation = match cavity {
                CavityId::Upper => Some(UPPER_BAKE_OPERATION),
                CavityId::Lower => code(LOWER_REMOTE_MODES, &remote.lower_mode),
            };
            let staged = remote.cavity(cavity);
            (operation, staged.temperature, staged.cook_time)
        };
        if let Some(operation) = operation {
            self.send_remote_start(operation, temperature, cook_time);
        }
    }

    fn send_timer(&self, hours: u8, minutes: u8, seconds: u8) {
        self.core.send(&[
            0xf0, 0x43, 0x23, 0x06, 0x80, 0x80, 0x80, seconds, minutes, hours,
        ]);
    }

    fn send_preference(&self, payload: &[u8; PREFERENCE_PAYLOAD_LENGTH]) {
        self.core
            .send(&[PREFERENCE_COMMAND_PREFIX.as_slice(), payload].concat());
    }

    fn send_preference_field(&self, index: usize, value: u8) {
        let mut payload = preference_payload();
        payload[index] = value;
        self.send_preference(&payload);
    }

    /// No status record carries a clock, so this is a one-shot write like the app's "Sync with
    /// smartphone".
    fn send_clock(&self, hours: u8, minutes: u8) {
        let mut payload = preference_payload();
        payload[CLOCK_HOURS_INDEX] = hours;
        payload[CLOCK_MINUTES_INDEX] = minutes;
        payload[CLOCK_HOUR_FORMAT_INDEX] = CLOCK_HOUR_FORMAT_24H;
        self.send_preference(&payload);
    }

    /// Takes both cavities so the captured two-cavity frame can be reproduced; HA writes one at a
    /// time and leaves the other at "no change".
    fn send_temperature_adjustment(&self, adjustments: &[(CavityId, i32)]) {
        let mut payload = preference_payload();
        payload[TEMPERATURE_ADJUSTMENT_UNIT_INDEX] = TEMPERATURE_ADJUSTMENT_FAHRENHEIT;
        for &(cavity, degrees) in adjustments {
            payload[cavity.temperature_adjustment_index()] = degrees as u8;
        }
        self.send_preference(&payload);
    }

    fn send_remote_start(&self, operation: u8, temperature: i64, cook_time_minutes: i64) {
        let hours = cook_time_minutes / 60;
        let minutes = cook_time_minutes % 60;
        // Unlike status records, the command carries hours/minutes/seconds.
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
            temperature as u8,
            hours as u8,
            minutes as u8,
            0x00,
            0x00,
        ]);
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
    use crate::test_support::{make_t2, meta, prop, DEVICE_ID};
    use rethink_core::{hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};
    use std::collections::HashMap;

    const COOKTOP_OFF: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000000000000000000004D0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000090BB";
    const COOKTOP_OFF_TO_ON: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000000000000000000004E00000000000006000000000000000000004D00000000000006000001000300000100030000010003000001000300000100030000009FBB";
    const COOKTOP_ON_TO_OFF: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004D0000000000000E00000100190E000100190E000100190E000100190E000100190E000000000000000000004E0000000000000E000000000000000000004C0000000000000E00000000000000000000000000000000000000000000000000000000D8BB";
    const TIMER_09_01_13: &str = "AA8240EC00000000000000004E0000000000000E000000000000000000004C0000000000000E0000000000000000000000000000000000000000000000000000000000000000000000004E00000000000006000000000000000000004C0000000D010906000000000000000000000000000000000000000000000000000000009EBB";
    const TIMER_ROLLOVER_09_00_59: &str = "AA8240EC00000000000000004E00000000000006000000000000000000004C000000010109060000000000000000000000000000000000000000000000000000000000000000000000004E00000000000006000000000000000000004C0000003B00090600000000000000000000000000000000000000000000000000000000A6BB";
    const TIMER_STOPPED: &str = "AA8240EC00000000000000004E00000000000006000000000000000000004C0000001A3A08060000000000000000000000000000000000000000000000000000000000000000000000004E0000000000000E000000000000000000004D0000000000000E0000000000000000000000000000000000000000000000000000000044BB";
    const START_ACCEPTED_ACK: &str = "AA084000430060BB";
    const START_REJECTED_ACK: &str = "AA084000434020BB";
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

    /// Every real frame above, for assertions that must hold across the whole capture set.
    const ALL_CAPTURES: &[&str] = &[
        COOKTOP_OFF,
        COOKTOP_OFF_TO_ON,
        COOKTOP_ON_TO_OFF,
        TIMER_09_01_13,
        TIMER_ROLLOVER_09_00_59,
        TIMER_STOPPED,
        TIMER_EXPIRED_ALARM,
        BOTH_OVENS_REMOTE_START_READY,
        LOWER_ONLY_REMOTE_START_READY,
        LOWER_CONVECTION_ROAST_350F_TIME_OFF,
        LOWER_BAKE_285C_FIVE_MINUTES,
        LOWER_BAKE_285C_FOUR_MINUTES_59,
        LOWER_BAKE_CANCELLED_AT_252F,
        LOWER_CONVECTION_BAKE_300F_TEN_MINUTES,
        LOWER_CONVECTION_BAKE_300F_NINE_MINUTES_59,
        LOWER_CONVECTION_BAKE_PREHEAT_COMPLETE,
        UPPER_BROIL_HIGH_WITH_LOWER_PREHEATING,
        LOWER_PREHEAT_COMPLETE_EVENT,
        LOWER_PREHEAT_COMPLETE_WITH_UPPER_BROIL,
        UPPER_CANCELLED_WITH_LOWER_ACTIVE,
        LOWER_CANCELLED,
        UPPER_OVEN_170F_THREE_MINUTES,
        UPPER_OVEN_170F_TWO_MINUTES_59,
        UPPER_BAKE_125C_ONE_HOUR_THREE_MINUTES,
        UPPER_BAKE_125C_ONE_HOUR_TWO_MINUTES_59,
        UPPER_BAKE_PREHEAT_COMPLETE_EVENT,
        UPPER_BAKE_PREHEAT_COMPLETE,
        UPPER_BAKE_CANCELLED_AT_266F,
        UPPER_OVEN_FINISHED_AT_124F,
        UPPER_OVEN_FINISHED_EVENT,
        UPPER_OVEN_CLEARED_AT_149F,
    ];

    const STATUS_QUERY_PACKET: &str =
        "AA28F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000F3BB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("WFV474PGV", Device::new)
    }

    fn emit(thinq: &MockThinq2Device, packet: &str) {
        thinq.emit_data(&hex_decode(packet));
    }

    fn p(ha: &MockHaConnection, name: &str) -> String {
        prop(ha, name).unwrap_or_else(|| panic!("{name} was never published"))
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    /// Frames the handler does not decode must not reach HA at all, so "ignored" means no
    /// published property moves.
    fn snapshot(ha: &MockHaConnection) -> HashMap<String, PropertyValue> {
        ha.device(DEVICE_ID).unwrap().properties
    }

    fn hex(packet: &[u8]) -> String {
        hex_encode(packet)
    }

    /// A 62-byte record carrying only the Upper cavity fields named here.
    fn record(temperature: u16, state: u8, mode: u8) -> Vec<u8> {
        let mut out = vec![0u8; STATUS_RECORD_LENGTH];
        out[7..9].copy_from_slice(&temperature.to_be_bytes());
        out[0] = state;
        out[1] = mode;
        out
    }

    /// Synthetic framing around known oven frame shapes.
    fn frame(inner: &[u8]) -> Vec<u8> {
        wrap_aabb(inner)
    }

    fn status_frame(frame_type: u8, records: &[Vec<u8>]) -> Vec<u8> {
        frame(&[vec![CLASS_BYTE, frame_type], records.concat()].concat())
    }

    /// The preference payload sits after `AA <len> F0 43 21 0E` and before `<checksum> BB`.
    fn payload(packet: &[u8]) -> Vec<u8> {
        packet[6..packet.len() - 2].to_vec()
    }

    fn untouched_tail() -> Vec<u8> {
        [[0x80; 8].as_slice(), &[0xff, 0x80]].concat()
    }

    #[test]
    fn is_selected_by_the_bridge_and_queried_on_connect() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta("WFV474PGV"));
        let factory = crate::t2_factory("WFV474PGV").expect("WFV474PGV is registered");

        let handler = factory(ha.clone(), thinq.clone(), meta("WFV474PGV"));
        handler.start();

        assert!(ha.device(DEVICE_ID).unwrap().config.is_some());
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex(&thinq.outbox()[0]), STATUS_QUERY_PACKET);
    }

    #[test]
    fn publishes_status_confirmed_write_controls_and_diagnostics() {
        let (ha, _, _) = make();
        let c = components(&ha);

        assert_eq!(c["cooktop_status"]["platform"], "binary_sensor");
        assert_eq!(c["cooktop_status"]["payload_on"], "ON");
        assert_eq!(c["cooktop_status"]["payload_off"], "OFF");
        assert!(c["cooktop_status"].get("command_topic").is_none());
        assert!(c["upper_oven_temperature"].get("command_topic").is_none());
        assert_eq!(c["upper_oven_temperature"]["unit_of_measurement"], "°F");
        assert!(c["upper_oven_status"].get("command_topic").is_none());
        assert!(c["upper_oven_mode"].get("command_topic").is_none());
        assert!(c["upper_oven_set_temperature"]
            .get("command_topic")
            .is_none());
        assert_eq!(c["upper_oven_set_temperature"]["unit_of_measurement"], "°F");
        assert!(c["upper_oven_cook_time"].get("command_topic").is_none());
        assert!(c["lower_oven_temperature"].get("command_topic").is_none());
        assert_eq!(c["lower_oven_temperature"]["unit_of_measurement"], "°F");
        assert!(c["lower_oven_status"].get("command_topic").is_none());
        assert!(c["lower_oven_mode"].get("command_topic").is_none());
        assert!(c["lower_oven_set_temperature"]
            .get("command_topic")
            .is_none());
        assert_eq!(c["lower_oven_set_temperature"]["unit_of_measurement"], "°F");
        assert!(c["lower_oven_cook_time"].get("command_topic").is_none());
        assert!(c["upper_oven_remote_start"].get("command_topic").is_none());
        assert!(c["lower_oven_remote_start"].get("command_topic").is_none());
        assert_eq!(c["timer"]["platform"], "text");
        assert_eq!(c["timer"]["command_topic"], "$this/timer/set");
        assert_eq!(c["timer_stop"]["platform"], "button");
        assert_eq!(c["timer_stop"]["command_topic"], "$this/timer_stop/set");
        assert_eq!(c["clock_sync"]["platform"], "button");
        assert_eq!(c["clock_sync"]["command_topic"], "$this/clock_sync/set");
        assert!(c["clock_sync"].get("state_topic").is_none());
        assert_eq!(c["beeper_volume"]["platform"], "select");
        assert_eq!(
            c["beeper_volume"]["command_topic"],
            "$this/beeper_volume/set"
        );
        assert_eq!(
            c["beeper_volume"]["options"],
            json!(["High", "Low", "Mute"])
        );
        // Write-only: the oven never reports it, so HA must track the selection optimistically.
        assert_eq!(c["beeper_volume"]["optimistic"], true);
        assert!(c["beeper_volume"].get("state_topic").is_none());
        assert_eq!(c["preheat_alarm_light"]["platform"], "switch");
        assert_eq!(
            c["preheat_alarm_light"]["command_topic"],
            "$this/preheat_alarm_light/set"
        );
        assert_eq!(c["preheat_alarm_light"]["optimistic"], true);
        assert!(c["preheat_alarm_light"].get("state_topic").is_none());
        assert_eq!(c["temperature_unit"]["platform"], "select");
        assert_eq!(
            c["temperature_unit"]["command_topic"],
            "$this/temperature_unit/set"
        );
        assert_eq!(c["temperature_unit"]["options"], json!(["°F", "°C"]));
        assert_eq!(c["temperature_unit"]["optimistic"], true);
        assert!(c["temperature_unit"].get("state_topic").is_none());
        assert_eq!(c["auto_conversion"]["platform"], "switch");
        assert_eq!(
            c["auto_conversion"]["command_topic"],
            "$this/auto_conversion/set"
        );
        assert_eq!(c["auto_conversion"]["optimistic"], true);
        assert!(c["auto_conversion"].get("state_topic").is_none());
        assert_eq!(c["upper_temperature_adjustment"]["platform"], "number");
        assert_eq!(c["upper_temperature_adjustment"]["min"], -35);
        assert_eq!(c["upper_temperature_adjustment"]["max"], 35);
        assert_eq!(
            c["upper_temperature_adjustment"]["unit_of_measurement"],
            "°F"
        );
        assert_eq!(c["upper_temperature_adjustment"]["optimistic"], true);
        assert!(c["upper_temperature_adjustment"]
            .get("state_topic")
            .is_none());
        assert_eq!(c["lower_temperature_adjustment"]["platform"], "number");
        assert!(c["lower_temperature_adjustment"]
            .get("state_topic")
            .is_none());
        assert_eq!(c["upper_remote_temperature"]["platform"], "number");
        assert_eq!(c["upper_remote_temperature"]["min"], 170);
        assert_eq!(c["upper_remote_temperature"]["max"], 550);
        assert_eq!(c["upper_remote_cook_time"]["platform"], "number");
        assert_eq!(c["upper_start"]["platform"], "button");
        assert_eq!(c["upper_start"]["name"], "Start Upper Bake");
        assert_eq!(c["upper_cancel"]["platform"], "button");
        assert_eq!(c["lower_remote_temperature"]["platform"], "number");
        assert_eq!(c["lower_remote_temperature"]["min"], 170);
        assert_eq!(c["lower_remote_temperature"]["max"], 550);
        assert_eq!(c["lower_remote_cook_time"]["platform"], "number");
        assert_eq!(c["lower_remote_mode"]["platform"], "select");
        assert_eq!(
            c["lower_remote_mode"]["options"],
            json!(["Bake", "Convection Bake", "Convection Roast"])
        );
        assert_eq!(c["lower_start"]["platform"], "button");
        assert_eq!(c["lower_start"]["name"], "Start Lower");
        assert_eq!(c["lower_cancel"]["platform"], "button");
        assert_eq!(p(&ha, "upper_remote_temperature"), "350");
        assert_eq!(p(&ha, "upper_remote_cook_time"), "10");
        assert_eq!(p(&ha, "lower_remote_temperature"), "350");
        assert_eq!(p(&ha, "lower_remote_cook_time"), "10");
        assert_eq!(p(&ha, "lower_remote_mode"), "Bake");
        // Construction forces readiness OFF so a stale retained ON from a previous run cannot
        // enable the Start buttons before the first status reply.
        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "OFF");
        assert_eq!(c["upper_oven_status"]["device_class"], "enum");
        assert_eq!(
            c["upper_oven_status"]["options"],
            json!(["Idle", "Preheating", "Active", "Complete"])
        );
        assert_eq!(c["upper_oven_mode"]["device_class"], "enum");
        assert_eq!(
            c["upper_oven_mode"]["options"],
            json!([
                "None",
                "Bake",
                "Broil",
                "Convection Bake",
                "Convection Roast"
            ])
        );
        // Undecoded bytes and frames are logged rather than published, so the discovery payload
        // must carry no diagnostic dump entity.
        assert!(!c.contains_key("raw_status"));
        assert!(!c.contains_key("undecoded_frame"));
    }

    #[test]
    fn components_use_upstream_keys_unique_ids_and_cook_time_units() {
        let (ha, _, _) = make();
        let c = components(&ha);
        let mut keys: Vec<&str> = c.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut want = vec![
            "cooktop_status",
            "timer",
            "timer_stop",
            "clock_sync",
            "beeper_volume",
            "preheat_alarm_light",
            "temperature_unit",
            "auto_conversion",
            "lower_remote_mode",
        ];
        let per_cavity = [
            "oven_temperature",
            "oven_status",
            "oven_mode",
            "oven_set_temperature",
            "oven_cook_time",
            "oven_remote_start",
            "temperature_adjustment",
            "remote_temperature",
            "remote_cook_time",
            "start",
            "cancel",
        ];
        let cavity_keys: Vec<String> = ["upper", "lower"]
            .iter()
            .flat_map(|cavity| per_cavity.iter().map(move |key| format!("{cavity}_{key}")))
            .collect();
        want.extend(cavity_keys.iter().map(String::as_str));
        want.sort_unstable();
        assert_eq!(keys, want);
        for (key, comp) in &c {
            assert_eq!(comp["unique_id"], format!("$deviceid-{key}"), "{key}");
        }
        for cavity in ["upper", "lower"] {
            let cook_time = &c[&format!("{cavity}_oven_cook_time")];
            assert_eq!(cook_time["device_class"], "duration");
            assert_eq!(cook_time["unit_of_measurement"], "s");
        }
    }

    #[test]
    fn start_sends_the_exact_status_query_observed_from_lg_cloud() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();

        dev.start();

        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex(&thinq.outbox()[0]), STATUS_QUERY_PACKET);
    }

    // Publishing a value outside the declared options is an error in HA; only "None" is exempt.
    #[test]
    fn no_captured_frame_publishes_an_oven_state_or_mode_outside_the_declared_options() {
        let (ha, thinq, _) = make();
        let c = components(&ha);

        for capture in ALL_CAPTURES {
            emit(&thinq, capture);
            for entity in [
                "upper_oven_status",
                "lower_oven_status",
                "upper_oven_mode",
                "lower_oven_mode",
            ] {
                let value = p(&ha, entity);
                let declared = c[entity]["options"].as_array().unwrap();
                assert!(
                    value == "None" || declared.iter().any(|option| option == value.as_str()),
                    "{entity} published {value} for {capture}"
                );
            }
        }
    }

    // Every captured status arrived as EC, so only the EB framing is synthetic.
    #[test]
    fn eb_decodes_its_single_status_block() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&status_frame(0xeb, &[record(350, 0, 0)]));

        assert_eq!(p(&ha, "upper_oven_temperature"), "350");
    }

    #[test]
    fn ec_decodes_the_second_current_block_not_the_first() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&status_frame(0xec, &[record(100, 0, 0), record(350, 0, 0)]));

        assert_eq!(p(&ha, "upper_oven_temperature"), "350");
    }

    #[test]
    fn undecoded_state_and_mode_bytes_publish_unknown() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&status_frame(0xeb, &[record(0, 0x7f, 0x7e)]));

        assert_eq!(p(&ha, "upper_oven_status"), "None");
        assert_eq!(p(&ha, "upper_oven_mode"), "None");
    }

    // The remote-start flag over the "None" code masks down to "None", not to an unknown byte.
    #[test]
    fn remote_flag_without_an_operation_decodes_as_none() {
        let (ha, thinq, _) = make();

        thinq.emit_data(&status_frame(0xeb, &[record(0, 0x00, 0x80)]));

        assert_eq!(p(&ha, "upper_oven_mode"), "None");
    }

    #[test]
    fn real_cooktop_transitions_publish_off_on_then_off_from_the_five_slot_flags() {
        let (ha, thinq, _) = make();

        emit(&thinq, COOKTOP_OFF);
        assert_eq!(p(&ha, "cooktop_status"), "OFF");

        emit(&thinq, COOKTOP_OFF_TO_ON);
        assert_eq!(p(&ha, "cooktop_status"), "ON");

        emit(&thinq, COOKTOP_ON_TO_OFF);
        assert_eq!(p(&ha, "cooktop_status"), "OFF");
    }

    #[test]
    fn real_timer_frames_decode_set_rollover_and_stop_states() {
        let (ha, thinq, _) = make();

        emit(&thinq, TIMER_09_01_13);
        assert_eq!(p(&ha, "timer"), "09:01:13");

        emit(&thinq, TIMER_ROLLOVER_09_00_59);
        assert_eq!(p(&ha, "timer"), "09:00:59");

        emit(&thinq, TIMER_STOPPED);
        assert_eq!(p(&ha, "timer"), "00:00:00");
    }

    #[test]
    fn real_timer_expiry_alarm_event_publishes_nothing() {
        let (ha, thinq, _) = make();
        let before = snapshot(&ha);

        emit(&thinq, TIMER_EXPIRED_ALARM);

        assert_eq!(snapshot(&ha), before);
    }

    #[test]
    fn real_set_start_cancel_and_rejection_acknowledgements_publish_nothing() {
        let (ha, thinq, _) = make();
        let before = snapshot(&ha);

        emit(&thinq, START_ACCEPTED_ACK);
        emit(&thinq, "AA084000440063BB");
        emit(&thinq, START_REJECTED_ACK);

        assert_eq!(snapshot(&ha), before);
    }

    #[test]
    fn timer_set_and_stop_send_the_exact_captured_commands() {
        let (_, thinq, dev) = make();

        dev.set_property("timer", "09:01:13");
        assert_eq!(hex(&thinq.outbox()[0]), "AA0EF04323068080800D0109FEBB");

        thinq.reset_recorder();
        dev.set_property("timer_stop", "PRESS");
        assert_eq!(hex(&thinq.outbox()[0]), "AA0EF0432306808080000000C1BB");
    }

    // Captured from the app's "Sync with smartphone" at 23:12 and 23:15 local time.
    #[test]
    fn clock_sync_reproduces_the_captured_24_hour_preference_command() {
        let (_, thinq, dev) = make();

        dev.send_clock(23, 15);

        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex(&thinq.outbox()[0]),
            "AA15F043210E170F008080808080808080FF8093BB"
        );
    }

    // The 12-hour capture is not a shape the handler emits, but it pins the payload layout.
    #[test]
    fn the_captured_12_hour_clock_frame_differs_only_in_the_hour_and_byte_2() {
        let twelve_hour = hex_decode("AA15F043210E0B0C018080808080808080FF80EDBB");
        let twenty_four_hour = hex_decode("AA15F043210E170F008080808080808080FF8093BB");

        assert_eq!(
            payload(&twelve_hour),
            [[0x0b, 0x0c, 0x01].as_slice(), &untouched_tail()].concat()
        );
        assert_eq!(
            payload(&twenty_four_hour),
            [[0x17, 0x0f, 0x00].as_slice(), &untouched_tail()].concat()
        );
    }

    #[test]
    fn clock_sync_sends_the_host_time_and_leaves_every_other_preference_unchanged() {
        let (_, thinq, dev) = make();

        dev.set_property("clock_sync", "PRESS");

        assert_eq!(thinq.outbox().len(), 1);
        let packet = &thinq.outbox()[0];
        let payload = payload(packet);
        assert_eq!(hex(&packet[..6]), "AA15F043210E");
        assert!(payload[0] < 24);
        assert!(payload[1] < 60);
        assert_eq!(payload[2], 0x00);
        // Every field the user did not touch must go out as the no-change sentinel.
        assert_eq!(payload[3..], untouched_tail());
    }

    // One Save per value; each changed only payload index 8.
    #[test]
    fn beeper_volume_reproduces_all_three_captured_commands() {
        let (_, thinq, dev) = make();

        dev.set_property("beeper_volume", "Mute");
        dev.set_property("beeper_volume", "High");
        dev.set_property("beeper_volume", "Low");

        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 3);
        assert_eq!(
            hex(&outbox[0]),
            "AA15F043210E8080808080808080008080FF80F5BB"
        );
        assert_eq!(
            hex(&outbox[1]),
            "AA15F043210E8080808080808080028080FF80F7BB"
        );
        assert_eq!(
            hex(&outbox[2]),
            "AA15F043210E8080808080808080018080FF80F4BB"
        );
    }

    #[test]
    fn a_preference_write_leaves_every_field_it_does_not_set_at_its_no_change_sentinel() {
        let (_, thinq, dev) = make();

        dev.set_property("beeper_volume", "Mute");

        // Everything but index 8 (beeper) and index 11 (its own FF sentinel) stays 0x80.
        for (index, value) in payload(&thinq.outbox()[0]).into_iter().enumerate() {
            let want = match index {
                8 => 0x00,
                11 => 0xff,
                _ => 0x80,
            };
            assert_eq!(value, want, "payload index {index}");
        }
    }

    #[test]
    fn preheating_alarm_light_reproduces_both_captured_commands() {
        let (_, thinq, dev) = make();

        dev.set_property("preheat_alarm_light", "OFF");
        dev.set_property("preheat_alarm_light", "ON");

        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 2);
        assert_eq!(
            hex(&outbox[0]),
            "AA15F043210E8080808080808000808080FF80F5BB"
        );
        assert_eq!(
            hex(&outbox[1]),
            "AA15F043210E8080808080808001808080FF80F4BB"
        );
    }

    #[test]
    fn temperature_unit_reproduces_both_captured_commands() {
        let (_, thinq, dev) = make();

        dev.set_property("temperature_unit", "°C");
        dev.set_property("temperature_unit", "°F");

        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 2);
        assert_eq!(
            hex(&outbox[0]),
            "AA15F043210E8080808080808080800180FF80F4BB"
        );
        assert_eq!(
            hex(&outbox[1]),
            "AA15F043210E8080808080808080800080FF80F5BB"
        );
    }

    // The EC frames either side of the °C switch kept reporting Fahrenheit.
    #[test]
    fn the_display_temperature_unit_does_not_change_how_status_temperatures_decode() {
        let (ha, thinq, dev) = make();

        dev.set_property("temperature_unit", "°C");
        emit(&thinq, LOWER_CANCELLED);

        assert_eq!(p(&ha, "lower_oven_temperature"), "356");
    }

    #[test]
    fn auto_conversion_reproduces_both_captured_commands() {
        let (_, thinq, dev) = make();

        dev.set_property("auto_conversion", "OFF");
        dev.set_property("auto_conversion", "ON");

        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 2);
        assert_eq!(
            hex(&outbox[0]),
            "AA15F043210E8080800080808080808080FF80F5BB"
        );
        assert_eq!(
            hex(&outbox[1]),
            "AA15F043210E8080800180808080808080FF80F4BB"
        );
    }

    // One Save carrying Upper -2°F and Lower Off (index 4 names °F).
    #[test]
    fn temperature_adjustment_reproduces_the_captured_upper_minus_2_lower_off_command() {
        let (_, thinq, dev) = make();

        dev.send_temperature_adjustment(&[(CavityId::Upper, -2), (CavityId::Lower, 0)]);

        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex(&thinq.outbox()[0]),
            "AA15F043210E8080808000FE0080808080FF80CBBB"
        );
    }

    // With the capture above: a positive, a negative and a zero pin the two's-complement encoding.
    #[test]
    fn temperature_adjustment_reproduces_the_captured_upper_plus_2_lower_minus_1_command() {
        let (_, thinq, dev) = make();

        dev.send_temperature_adjustment(&[(CavityId::Upper, 2), (CavityId::Lower, -1)]);

        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex(&thinq.outbox()[0]),
            "AA15F043210E808080800002FF80808080FF80F4BB"
        );
    }

    // Not a shape the handler emits (it always names °F), but the evidence that index 4 is the
    // trims' unit and shares index 9's encoding.
    #[test]
    fn the_captured_celsius_trim_frame_names_celsius_at_the_temperature_adjustment_unit_index() {
        let packet = hex_decode("AA15F043210E808080800100FF80808080FF80F5BB");

        let payload = payload(&packet);
        assert_eq!(payload[4], 0x01);
        assert_eq!(payload[5], 0x00);
        assert_eq!(payload[6], 0xff);
    }

    #[test]
    fn temperature_adjustment_writes_one_cavity_and_leaves_the_other_unchanged() {
        let (_, thinq, dev) = make();

        dev.set_property("upper_temperature_adjustment", "-2");
        dev.set_property("lower_temperature_adjustment", "15");

        let outbox = thinq.outbox();
        // Upper write: index 5 carries the trim, index 6 stays at the no-change sentinel.
        assert_eq!(payload(&outbox[0])[4..7], [0x00, 0xfe, 0x80]);
        // Lower write: the reverse.
        assert_eq!(payload(&outbox[1])[4..7], [0x00, 0x80, 0x0f]);
    }

    #[test]
    fn out_of_range_and_non_numeric_temperature_adjustments_send_nothing() {
        let (_, thinq, dev) = make();

        dev.set_property("upper_temperature_adjustment", "-36");
        dev.set_property("upper_temperature_adjustment", "36");
        dev.set_property("upper_temperature_adjustment", "hot");
        dev.set_property("lower_temperature_adjustment", "");

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn an_unknown_auto_conversion_payload_sends_nothing() {
        let (_, thinq, dev) = make();

        dev.set_property("auto_conversion", "AUTO");

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn an_unknown_temperature_unit_option_sends_nothing() {
        let (_, thinq, dev) = make();

        dev.set_property("temperature_unit", "kelvin");

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn an_unknown_preheating_alarm_light_payload_sends_nothing() {
        let (_, thinq, dev) = make();

        dev.set_property("preheat_alarm_light", "MAYBE");

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn an_unknown_beeper_volume_option_sends_nothing() {
        let (_, thinq, dev) = make();

        dev.set_property("beeper_volume", "Deafening");

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn a_non_press_clock_sync_payload_sends_nothing() {
        let (_, thinq, dev) = make();

        dev.set_property("clock_sync", "not-a-press");

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn invalid_timer_values_and_non_press_button_payloads_send_nothing() {
        let (_, thinq, dev) = make();

        dev.set_property("timer", "9:01:13");
        dev.set_property("timer", "09:61:13");
        dev.set_property("timer_stop", "not-a-press");

        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn upper_and_lower_cancel_send_the_exact_captured_selector_commands() {
        let (_, thinq, dev) = make();

        dev.set_property("upper_cancel", "PRESS");
        dev.set_property("lower_cancel", "PRESS");

        assert_eq!(hex(&thinq.outbox()[0]), "AA07F04400B0BB");
        assert_eq!(hex(&thinq.outbox()[1]), "AA07F04401B3BB");
    }

    #[test]
    fn remote_start_reproduces_both_captured_start_commands() {
        let (_, thinq, dev) = make();

        // Stage the parameters the captured commands carried, not the constructor defaults.
        dev.set_property("upper_remote_temperature", "170");
        dev.set_property("upper_remote_cook_time", "3");
        dev.set_property("lower_remote_temperature", "350");
        dev.set_property("lower_remote_cook_time", "0");
        dev.set_property("lower_remote_mode", "Convection Roast");

        dev.set_property("upper_start", "PRESS");
        dev.set_property("lower_start", "PRESS");

        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 2);
        assert_eq!(hex(&outbox[0]), "AA13F043200B010000000000AA000300009CBB");
        assert_eq!(hex(&outbox[1]), "AA13F043200B1800000000015E00000000C7BB");
    }

    // The oven answers an unarmed start with START_REJECTED_ACK, so no lockout is added here.
    #[test]
    fn remote_start_is_sent_without_a_readiness_check_of_our_own() {
        let (_, thinq, dev) = make();

        dev.set_property("upper_start", "PRESS");

        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex(&thinq.outbox()[0]),
            "AA13F043200B0100000000015E000A0000D0BB"
        );
    }

    #[test]
    fn remote_start_inputs_publish_and_encode_temperature_and_cook_time() {
        let (ha, thinq, dev) = make();

        dev.set_property("upper_remote_temperature", "400");
        dev.set_property("upper_remote_cook_time", "90");

        assert_eq!(p(&ha, "upper_remote_temperature"), "400");
        assert_eq!(p(&ha, "upper_remote_cook_time"), "90");
        // Staging a value must not put anything on the wire; only the Start button does that.
        assert!(thinq.outbox().is_empty());

        dev.set_property("upper_start", "PRESS");

        let command = thinq.outbox()[0].clone();
        assert_eq!(
            command[2..command.len() - 2],
            [
                0xf0, 0x43, 0x20, 0x0b, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x90, 0x01, 0x1e, 0x00,
                0x00
            ]
        );
    }

    #[test]
    fn lower_mode_selection_validates_publishes_and_encodes_every_captured_operation() {
        let (ha, thinq, dev) = make();

        dev.set_property("lower_remote_mode", "Convection Roast");
        dev.set_property("lower_remote_mode", "Steam Bake");
        assert_eq!(p(&ha, "lower_remote_mode"), "Convection Roast");
        assert!(thinq.outbox().is_empty());

        dev.set_property("lower_remote_temperature", "300");
        dev.set_property("lower_remote_cook_time", "10");
        dev.set_property("lower_remote_mode", "Bake");
        assert_eq!(p(&ha, "lower_remote_mode"), "Bake");
        emit(&thinq, LOWER_ONLY_REMOTE_START_READY);
        dev.set_property("lower_start", "PRESS");
        assert_eq!(
            hex(&thinq.outbox()[0]),
            "AA13F043200B1500000000012C000A000032BB"
        );

        dev.set_property("lower_remote_mode", "Convection Bake");
        assert_eq!(p(&ha, "lower_remote_mode"), "Convection Bake");
        emit(&thinq, LOWER_ONLY_REMOTE_START_READY);
        dev.set_property("lower_start", "PRESS");
        assert_eq!(
            hex(&thinq.outbox()[1]),
            "AA13F043200B1700000000012C000A00003CBB"
        );

        dev.set_property("lower_remote_mode", "Convection Roast");
        assert_eq!(p(&ha, "lower_remote_mode"), "Convection Roast");
        emit(&thinq, LOWER_ONLY_REMOTE_START_READY);
        dev.set_property("lower_start", "PRESS");
        assert_eq!(
            hex(&thinq.outbox()[2]),
            "AA13F043200B1800000000012C000A00003FBB"
        );
    }

    #[test]
    fn real_upper_oven_remote_start_pins_cavity_order_temperature_cook_time_and_readiness() {
        let (ha, thinq, _) = make();

        emit(&thinq, BOTH_OVENS_REMOTE_START_READY);
        assert_eq!(p(&ha, "upper_oven_remote_start"), "ON");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "ON");

        emit(&thinq, UPPER_OVEN_170F_THREE_MINUTES);
        assert_eq!(p(&ha, "upper_oven_status"), "Preheating");
        assert_eq!(p(&ha, "upper_oven_mode"), "Bake");
        assert_eq!(p(&ha, "lower_oven_status"), "Idle");
        assert_eq!(p(&ha, "upper_oven_temperature"), "None");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "170");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "180");
        assert_eq!(p(&ha, "lower_oven_temperature"), "77");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "lower_oven_cook_time"), "0");
        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "OFF");

        emit(&thinq, UPPER_OVEN_170F_TWO_MINUTES_59);
        assert_eq!(p(&ha, "upper_oven_cook_time"), "179");

        emit(&thinq, UPPER_OVEN_FINISHED_AT_124F);
        assert_eq!(p(&ha, "upper_oven_status"), "Complete");
        assert_eq!(p(&ha, "upper_oven_temperature"), "124");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "0");
        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");

        let before_event = snapshot(&ha);
        emit(&thinq, UPPER_OVEN_FINISHED_EVENT);
        assert_eq!(snapshot(&ha), before_event);

        emit(&thinq, UPPER_OVEN_CLEARED_AT_149F);
        assert_eq!(p(&ha, "upper_oven_status"), "Idle");
        assert_eq!(p(&ha, "upper_oven_temperature"), "149");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "0");
        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "OFF");
    }

    #[test]
    fn real_125c_upper_bake_frame_confirms_mode_normalized_setpoint_and_countdown() {
        let (ha, thinq, _) = make();

        emit(&thinq, UPPER_BAKE_125C_ONE_HOUR_THREE_MINUTES);
        assert_eq!(p(&ha, "upper_oven_status"), "Preheating");
        assert_eq!(p(&ha, "upper_oven_mode"), "Bake");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "257");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "3780");

        emit(&thinq, UPPER_BAKE_125C_ONE_HOUR_TWO_MINUTES_59);
        assert_eq!(p(&ha, "upper_oven_cook_time"), "3779");

        let before_event = snapshot(&ha);
        emit(&thinq, UPPER_BAKE_PREHEAT_COMPLETE_EVENT);
        assert_eq!(snapshot(&ha), before_event);

        emit(&thinq, UPPER_BAKE_PREHEAT_COMPLETE);
        assert_eq!(p(&ha, "upper_oven_status"), "Active");
        assert_eq!(p(&ha, "upper_oven_mode"), "Bake");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "257");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "3511");

        emit(&thinq, UPPER_BAKE_CANCELLED_AT_266F);
        assert_eq!(p(&ha, "upper_oven_status"), "Idle");
        assert_eq!(p(&ha, "upper_oven_mode"), "None");
        assert_eq!(p(&ha, "upper_oven_temperature"), "266");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "0");
    }

    #[test]
    fn real_lower_only_remote_start_frame_publishes_only_lower_as_ready() {
        let (ha, thinq, _) = make();

        emit(&thinq, LOWER_ONLY_REMOTE_START_READY);

        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "ON");
    }

    #[test]
    fn real_285c_lower_bake_frame_confirms_mode_normalized_setpoint_and_countdown() {
        let (ha, thinq, _) = make();

        emit(&thinq, LOWER_BAKE_285C_FIVE_MINUTES);
        assert_eq!(p(&ha, "upper_oven_status"), "Idle");
        assert_eq!(p(&ha, "lower_oven_status"), "Preheating");
        assert_eq!(p(&ha, "lower_oven_mode"), "Bake");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "545");
        assert_eq!(p(&ha, "lower_oven_cook_time"), "300");

        emit(&thinq, LOWER_BAKE_285C_FOUR_MINUTES_59);
        assert_eq!(p(&ha, "lower_oven_cook_time"), "299");

        emit(&thinq, LOWER_BAKE_CANCELLED_AT_252F);
        assert_eq!(p(&ha, "upper_oven_status"), "Idle");
        assert_eq!(p(&ha, "lower_oven_status"), "Idle");
        assert_eq!(p(&ha, "lower_oven_mode"), "None");
        assert_eq!(p(&ha, "lower_oven_temperature"), "252");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "lower_oven_cook_time"), "0");
    }

    #[test]
    fn real_300f_lower_convection_bake_confirms_mode_auto_converted_setpoint_and_countdown() {
        let (ha, thinq, _) = make();

        emit(&thinq, LOWER_CONVECTION_BAKE_300F_TEN_MINUTES);
        assert_eq!(p(&ha, "upper_oven_status"), "Idle");
        assert_eq!(p(&ha, "lower_oven_status"), "Preheating");
        assert_eq!(p(&ha, "lower_oven_mode"), "Convection Bake");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "275");
        assert_eq!(p(&ha, "lower_oven_cook_time"), "600");

        emit(&thinq, LOWER_CONVECTION_BAKE_300F_NINE_MINUTES_59);
        assert_eq!(p(&ha, "lower_oven_cook_time"), "599");

        let before_event = snapshot(&ha);
        emit(&thinq, LOWER_PREHEAT_COMPLETE_EVENT);
        assert_eq!(snapshot(&ha), before_event);

        emit(&thinq, LOWER_CONVECTION_BAKE_PREHEAT_COMPLETE);
        assert_eq!(p(&ha, "lower_oven_status"), "Active");
        assert_eq!(p(&ha, "lower_oven_mode"), "Convection Bake");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "275");
        assert_eq!(p(&ha, "lower_oven_cook_time"), "545");
    }

    #[test]
    fn real_lower_convection_roast_frame_decodes_16_bit_auto_converted_setpoint_and_time_off() {
        let (ha, thinq, _) = make();

        emit(&thinq, LOWER_CONVECTION_ROAST_350F_TIME_OFF);

        assert_eq!(p(&ha, "upper_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "0");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "325");
        assert_eq!(p(&ha, "lower_oven_status"), "Preheating");
        assert_eq!(p(&ha, "lower_oven_mode"), "Convection Roast");
        assert_eq!(p(&ha, "lower_oven_cook_time"), "0");
        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "OFF");
    }

    #[test]
    fn real_manual_upper_broil_high_frame_decodes_both_simultaneously_active_cavities() {
        let (ha, thinq, _) = make();

        emit(&thinq, UPPER_BROIL_HIGH_WITH_LOWER_PREHEATING);

        assert_eq!(p(&ha, "upper_oven_status"), "Active");
        assert_eq!(p(&ha, "upper_oven_mode"), "Broil");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "400");
        assert_eq!(p(&ha, "upper_oven_cook_time"), "0");
        assert_eq!(p(&ha, "lower_oven_status"), "Preheating");
        assert_eq!(p(&ha, "lower_oven_mode"), "Convection Roast");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "325");
        assert_eq!(p(&ha, "lower_oven_cook_time"), "0");
    }

    #[test]
    fn real_lower_preheat_complete_event_and_transition_publish_lower_active() {
        let (ha, thinq, _) = make();

        let before_event = snapshot(&ha);
        emit(&thinq, LOWER_PREHEAT_COMPLETE_EVENT);
        assert_eq!(snapshot(&ha), before_event);

        emit(&thinq, LOWER_PREHEAT_COMPLETE_WITH_UPPER_BROIL);

        assert_eq!(p(&ha, "upper_oven_status"), "Active");
        assert_eq!(p(&ha, "upper_oven_mode"), "Broil");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "400");
        assert_eq!(p(&ha, "lower_oven_status"), "Active");
        assert_eq!(p(&ha, "lower_oven_mode"), "Convection Roast");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "325");
        assert_eq!(p(&ha, "lower_oven_temperature"), "None");
    }

    #[test]
    fn real_upper_cancel_transition_clears_only_upper_and_preserves_active_lower() {
        let (ha, thinq, _) = make();

        emit(&thinq, UPPER_CANCELLED_WITH_LOWER_ACTIVE);

        assert_eq!(p(&ha, "upper_oven_status"), "Idle");
        assert_eq!(p(&ha, "upper_oven_mode"), "None");
        assert_eq!(p(&ha, "upper_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "upper_oven_temperature"), "206");
        assert_eq!(p(&ha, "lower_oven_status"), "Active");
        assert_eq!(p(&ha, "lower_oven_mode"), "Convection Roast");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "325");
        assert_eq!(p(&ha, "lower_oven_temperature"), "None");
    }

    #[test]
    fn real_lower_cancel_transition_clears_lower_and_decodes_16_bit_current_temperatures() {
        let (ha, thinq, _) = make();

        emit(&thinq, LOWER_CANCELLED);

        assert_eq!(p(&ha, "upper_oven_status"), "Idle");
        assert_eq!(p(&ha, "upper_oven_temperature"), "204");
        assert_eq!(p(&ha, "lower_oven_status"), "Idle");
        assert_eq!(p(&ha, "lower_oven_mode"), "None");
        assert_eq!(p(&ha, "lower_oven_set_temperature"), "None");
        assert_eq!(p(&ha, "lower_oven_temperature"), "356");
        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "OFF");
    }

    #[test]
    fn other_oven_opcodes_are_ignored() {
        let (ha, thinq, _) = make();
        let before = snapshot(&ha);

        thinq.emit_data(&frame(&[0x40, 0x31, 0x01, 0x02]));

        assert_eq!(snapshot(&ha), before);
    }

    #[test]
    fn foreign_class_bytes_and_malformed_status_records_are_ignored_not_decoded_as_status() {
        let (ha, thinq, _) = make();
        let before = snapshot(&ha);

        thinq.emit_data(&frame(&[0x32, 0xeb, 0x01]));
        thinq.emit_data(&frame(&[0x40, 0xeb, 0x01]));
        thinq.emit_data(&frame(&[0x40, 0xec, 0x01]));
        // A 4-byte frame with an unknown command family is not a known acknowledgement.
        thinq.emit_data(&frame(&[0x40, 0x00, 0x45, 0x00]));

        assert_eq!(snapshot(&ha), before);
    }

    #[test]
    fn non_numeric_and_unknown_option_payloads_leave_the_staged_value_alone() {
        let (ha, thinq, dev) = make();

        dev.set_property("upper_remote_temperature", "425");
        dev.set_property("lower_remote_mode", "Convection Bake");
        dev.set_property("upper_remote_temperature", "");
        dev.set_property("upper_remote_temperature", "hot");
        dev.set_property("lower_remote_mode", "Steam Bake");

        assert_eq!(p(&ha, "upper_remote_temperature"), "425");
        assert_eq!(p(&ha, "lower_remote_mode"), "Convection Bake");

        // The staged values, not the discarded payloads, are what a start actually sends.
        dev.set_property("upper_start", "PRESS");
        dev.set_property("lower_start", "PRESS");
        assert_eq!(
            hex(&thinq.outbox()[0]),
            "AA13F043200B010000000001A9000A000085BB"
        );
        assert_eq!(
            hex(&thinq.outbox()[1]),
            "AA13F043200B1700000000015E000A0000CEBB"
        );
    }

    #[test]
    fn blank_remote_cook_time_keeps_the_staged_value() {
        let (ha, thinq, dev) = make();

        dev.set_property("upper_remote_temperature", "170");
        dev.set_property("upper_remote_cook_time", "3");
        dev.set_property("upper_remote_cook_time", "");
        dev.set_property("upper_remote_cook_time", "  ");
        assert_eq!(p(&ha, "upper_remote_cook_time"), "3");

        dev.set_property("upper_start", "PRESS");
        assert_eq!(
            hex(&thinq.outbox()[0]),
            "AA13F043200B010000000000AA000300009CBB"
        );
    }

    #[test]
    fn reconnect_resets_staged_remote_parameters_and_readiness_to_defaults() {
        let (ha, thinq, dev) = make();

        dev.set_property("upper_remote_temperature", "425");
        dev.set_property("lower_remote_mode", "Convection Bake");
        emit(&thinq, BOTH_OVENS_REMOTE_START_READY);
        assert_eq!(p(&ha, "upper_oven_remote_start"), "ON");

        // The bridge constructs a fresh Device each time the appliance transport reopens.
        let reconnected = MockThinq2Device::new(DEVICE_ID, meta("WFV474PGV"));
        let _dev = Device::new(ha.clone(), reconnected, meta("WFV474PGV"));

        assert_eq!(p(&ha, "upper_remote_temperature"), "350");
        assert_eq!(p(&ha, "lower_remote_mode"), "Bake");
        assert_eq!(p(&ha, "upper_oven_remote_start"), "OFF");
        assert_eq!(p(&ha, "lower_oven_remote_start"), "OFF");
    }

    #[test]
    fn unflagged_panel_started_mode_codes_decode_via_the_remote_start_flag_mask() {
        let (ha, thinq, _) = make();
        let mut current = vec![0u8; STATUS_RECORD_LENGTH];
        current[1] = 0x01; // panel Bake: remote-start flag 0x80 clear
        current[19] = 0x17; // panel Convection Bake in the Lower block

        thinq.emit_data(&status_frame(0xeb, &[current]));

        assert_eq!(p(&ha, "upper_oven_mode"), "Bake");
        assert_eq!(p(&ha, "lower_oven_mode"), "Convection Bake");
    }
}
