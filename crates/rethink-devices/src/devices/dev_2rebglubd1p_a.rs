//! 2REBGLUBD1P_A — LG GBBSJ11ESW bottom freezer.
//!
//! The 95-byte class `0x10` status record starts with the shared fridge layout. Panel sweeps
//! confirmed every Celsius setpoint, Super Freeze [3], Vacation [12], Super Cool [16] and the
//! fridge door [7]; opening only the freezer produced no door event. The 118-byte F017 write was
//! verified in both directions for all five controls. `auto_ack` is on: without the cloud ack
//! this unit repeats its `10C5`/`10AF` telemetry ten times.
//!
//! Port of upstream PR #260.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const STATUS_LENGTH: usize = 95;
const COMMAND_LENGTH: usize = 118;
const CELSIUS: TemperatureUnit = TemperatureUnit::C;

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

pub struct Device {
    core: Arc<AabbDeviceCore>,
    celsius: AtomicBool,
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
            celsius: AtomicBool::new(false),
        });
        let mut config = default_config(&meta, Some(json!({"name": "LG Fridge"})));
        config.device.model = Some("GBBSJ11ESW".into());
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
        if let Some(rec) = process_common_status(buf, 0x10, STATUS_LENGTH) {
            self.process_status(rec);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        let celsius = rec[8] == 1;
        self.celsius.store(celsius, Ordering::Relaxed);
        let fridge = (celsius && (1..=7).contains(&rec[1]))
            .then(|| convert_fridge_temperature(CELSIUS, i32::from(rec[1])));
        let freezer = (celsius && (1..=9).contains(&rec[2]))
            .then(|| convert_freezer_temperature(CELSIUS, i32::from(rec[2])));
        self.core
            .publish_property("fridge_setpoint", number_or_none(fridge));
        self.core
            .publish_property("freezer_setpoint", number_or_none(freezer));
        self.core.publish_property("door", on_off(rec[7], 1, 2));
        self.core
            .publish_property("super_cool", on_off(rec[16], 1, 0));
        self.core
            .publish_property("express_freeze", on_off(rec[3], 2, 1));
        self.core
            .publish_property("vacation", on_off(rec[12], 1, 0));
    }

    fn command(&self, fields: &[(usize, u8)]) {
        let mut message = vec![0xffu8; 2 + COMMAND_LENGTH];
        message[0] = 0xf0;
        message[1] = 0x17;
        for &(index, value) in COMMAND_FIXED.iter().chain(fields) {
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
        // Only the Celsius command path has been physically validated.
        if !self.celsius.load(Ordering::Relaxed) {
            return;
        }
        if let Some(fields) = command_fields(prop, mqtt_value) {
            self.command(&fields);
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

fn command_fields(prop: &str, mqtt_value: &str) -> Option<Vec<(usize, u8)>> {
    match prop {
        "fridge_setpoint" => {
            let value = parse_setpoint(mqtt_value, fridge_range(CELSIUS))?;
            Some(vec![
                (1, convert_fridge_temperature(CELSIUS, value) as u8),
                (8, 1),
            ])
        }
        "freezer_setpoint" => {
            let value = parse_setpoint(mqtt_value, freezer_range(CELSIUS))?;
            Some(vec![
                (2, convert_freezer_temperature(CELSIUS, value) as u8),
                (8, 1),
            ])
        }
        "super_cool" | "express_freeze" | "vacation" => {
            let on = match mqtt_value {
                "ON" => true,
                "OFF" => false,
                _ => return None,
            };
            Some(vec![match prop {
                "express_freeze" => (3, if on { 2 } else { 1 }),
                "super_cool" => (16, u8::from(on)),
                _ => (12, u8::from(on)),
            }])
        }
        _ => None,
    }
}

/// A whole number inside `range`. HA's number platform can send an integer setpoint as "4.0".
fn parse_setpoint(mqtt_value: &str, range: (String, i32, i32)) -> Option<i32> {
    let (_, min, max) = range;
    let unsigned = mqtt_value.strip_prefix('-').unwrap_or(mqtt_value);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, "0"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !digits(fraction) {
        return None;
    }
    let value: f64 = mqtt_value.parse().ok()?;
    (value.fract() == 0.0 && value >= f64::from(min) && value <= f64::from(max))
        .then_some(value as i32)
}

fn number_or_none(value: Option<i32>) -> PropertyValue {
    value.map_or_else(|| "None".into(), PropertyValue::from)
}

/// "ON" for `on`, "OFF" for `off` and "None" for any other byte.
fn on_off(raw: u8, on: u8, off: u8) -> PropertyValue {
    if raw == on {
        "ON".into()
    } else if raw == off {
        "OFF".into()
    } else {
        "None".into()
    }
}

fn components() -> HashMap<String, Value> {
    [
        (
            "fridge_setpoint",
            temperature("Fridge target temperature", "fridge_setpoint"),
        ),
        (
            "freezer_setpoint",
            temperature("Freezer target temperature", "freezer_setpoint"),
        ),
        // The setpoint sensors keep their IDs and topics; the controls are separate entities.
        (
            "fridge_temperature",
            temperature_control(
                "Fridge temperature",
                "fridge_temperature",
                "fridge_setpoint",
                fridge_range(CELSIUS),
            ),
        ),
        (
            "freezer_temperature",
            temperature_control(
                "Freezer temperature",
                "freezer_temperature",
                "freezer_setpoint",
                freezer_range(CELSIUS),
            ),
        ),
        (
            "super_cool",
            mode("Super Cool", "super_cool", "mdi:snowflake-variant"),
        ),
        (
            "express_freeze",
            mode("Super Freeze", "express_freeze", "mdi:snowflake"),
        ),
        ("vacation", mode("Vacation", "vacation", "mdi:airplane")),
        (
            "door",
            json!({
                "platform": "binary_sensor", "device_class": "door",
                "unique_id": "$deviceid-door",
                "state_topic": "$this/door",
                "name": "Fridge door",
            }),
        ),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

fn temperature(name: &str, key: &str) -> Value {
    json!({
        "platform": "sensor", "device_class": "temperature",
        "unit_of_measurement": "°C",
        "unique_id": format!("$deviceid-{key}"),
        "state_topic": format!("$this/{key}"),
        "name": name,
    })
}

fn temperature_control(name: &str, key: &str, state_key: &str, range: (String, i32, i32)) -> Value {
    let (unit, min, max) = range;
    json!({
        "platform": "number", "device_class": "temperature",
        "unique_id": format!("$deviceid-{key}"),
        "state_topic": format!("$this/{state_key}"),
        "command_topic": format!("$this/{state_key}/set"),
        "name": name,
        "step": 1,
        "unit_of_measurement": unit, "min": min, "max": max,
    })
}

fn mode(name: &str, key: &str, icon: &str) -> Value {
    json!({
        "platform": "switch",
        "unique_id": format!("$deviceid-{key}"),
        "state_topic": format!("$this/{key}"),
        "command_topic": format!("$this/{key}/set"),
        "name": name,
        "icon": icon,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const ID: &str = "fridge";

    // Local GBBSJ11ESW captures (clip_ble_v1.9.225, no LG cloud); the owner confirmed 3 °C / -18 °C
    // on the panel. OPEN is a 10 EB snapshot; CLOSED is a 10 EC delta in which only byte 7 changes,
    // from 1 to 2, as the fridge door closes.
    const OPEN: &str = "aa6510eb02050401ffffff0101ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff001abb";
    const CLOSED: &str = "aac410ec02050401ffffff0101ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a0bb";
    const USAGE: &str = "aa2010c5e90401000000000f03000000000011000000004f13000000000047bb";
    const ENERGY: &str = "aa0b10af0000040405d4bb";

    // From upstream's GBBSJ11ESW.json capture fixture.
    const BASELINE: &str = "aa6510eb02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0005bb";
    /// (property, value, rx): a front-panel change and the frame reporting it, in capture order.
    const PANEL: &[(&str, &str, &str)] = &[
        (
            "super_cool",
            "ON",
            "aac410ec02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0100ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "super_cool",
            "OFF",
            "aac410ec02050401ffffff0201ffffff00ffffff0100ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "express_freeze",
            "ON",
            "aac410ec02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050402ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "express_freeze",
            "OFF",
            "aac410ec02050402ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "vacation",
            "ON",
            "aac410ec02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff01ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "vacation",
            "OFF",
            "aac410ec02050401ffffff0201ffffff01ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "fridge_setpoint",
            "2",
            "aac410ec02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002060401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "fridge_setpoint",
            "1",
            "aac410ec02060401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002070401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00acbb",
        ),
        (
            "fridge_setpoint",
            "7",
            "aac410ec02070401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002010401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a1bb",
        ),
        (
            "fridge_setpoint",
            "6",
            "aac410ec02010401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002020401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00babb",
        ),
        (
            "fridge_setpoint",
            "5",
            "aac410ec02020401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002030401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a4bb",
        ),
        (
            "fridge_setpoint",
            "4",
            "aac410ec02030401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002040401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a6bb",
        ),
        (
            "fridge_setpoint",
            "3",
            "aac410ec02040401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a0bb",
        ),
        (
            "freezer_setpoint",
            "-19",
            "aac410ec02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050501ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "freezer_setpoint",
            "-20",
            "aac410ec02050501ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050601ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00acbb",
        ),
        (
            "freezer_setpoint",
            "-21",
            "aac410ec02050601ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050701ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00aebb",
        ),
        (
            "freezer_setpoint",
            "-22",
            "aac410ec02050701ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050801ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a8bb",
        ),
        (
            "freezer_setpoint",
            "-23",
            "aac410ec02050801ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050901ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00aabb",
        ),
        (
            "freezer_setpoint",
            "-15",
            "aac410ec02050901ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050101ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00adbb",
        ),
        (
            "freezer_setpoint",
            "-16",
            "aac410ec02050101ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050201ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a4bb",
        ),
        (
            "freezer_setpoint",
            "-17",
            "aac410ec02050201ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050301ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a6bb",
        ),
        (
            "freezer_setpoint",
            "-18",
            "aac410ec02050301ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a0bb",
        ),
        (
            "super_cool",
            "ON",
            "aac410ec02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0100ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "super_cool",
            "OFF",
            "aac410ec02050401ffffff0201ffffff00ffffff0100ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "express_freeze",
            "OFF",
            "aac410ec02050402ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "vacation",
            "OFF",
            "aac410ec02050401ffffff0201ffffff01ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a8bb",
        ),
        (
            "fridge_setpoint",
            "3",
            "aac410ec02040501ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050501ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a8bb",
        ),
    ];
    /// (property, value, tx, rx): a verified command and the appliance's response.
    const CONTROLS: &[(&str, &str, &str, &str)] = &[
        (
            "super_cool",
            "ON",
            "aa7cf017ffffffffffffffffffffffffffffffff01ffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbfbb",
            "aac410ec02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0100ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "super_cool",
            "OFF",
            "aa7cf017ffffffffffffffffffffffffffffffff00ffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbcbb",
            "aac410ec02050401ffffff0201ffffff00ffffff0100ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "express_freeze",
            "ON",
            "aa7cf017ffffff02ffffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbebb",
            "aa6510eb02050402ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0004bb",
        ),
        (
            "express_freeze",
            "OFF",
            "aa7cf017ffffff01ffffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbfbb",
            "aac410ec02050402ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a2bb",
        ),
        (
            "vacation",
            "ON",
            "aa7cf017ffffffffffffffffffffffff01ffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbfbb",
            "aa6510eb02050401ffffff0201ffffff01ffffff0000ffffffffffffffff000101ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0004bb",
        ),
        (
            "vacation",
            "OFF",
            "aa7cf017ffffffffffffffffffffffff00ffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbcbb",
            "aac410ec02050401ffffff0201ffffff01ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050401ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a8bb",
        ),
        (
            "fridge_setpoint",
            "4",
            "aa7cf017ff04ffffffffffff01ffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbabb",
            "aa6510eb02040401ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0007bb",
        ),
        (
            "freezer_setpoint",
            "-19",
            "aa7cf017ffff05ffffffffff01ffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffa5bb",
            "aa6510eb02040501ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0006bb",
        ),
        (
            "fridge_setpoint",
            "3",
            "aa7cf017ff05ffffffffffff01ffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffa5bb",
            "aac410ec02040501ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0002050501ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff00a8bb",
        ),
        (
            "freezer_setpoint",
            "-18",
            "aa7cf017ffff04ffffffffff01ffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff00ffffff1effffffffffffffffffffffffffffffffffffffffffffffff0affffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00ffffffffffffffffffffffffffffffbabb",
            "aa6510eb02050401ffffff0201ffffff00ffffff0000ffffffffffffffff000401ff00ffffffffffffffffff01ff00ffffffffffffffffffffffffffffffffffffffff0078ffffffffffffffffffffffffffffffffffffff0005ffffffffff0000ff0006bb",
        ),
    ];

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2REBGLUBD1P_A", "2REBGLUBD1P_A", "");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    fn props(ha: &MockHaConnection) -> HashMap<String, PropertyValue> {
        ha.device(ID).unwrap().properties
    }

    fn expected(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(prop, value)| (prop.to_string(), value.clone()))
            .collect()
    }

    /// What a fixture entry decodes to: setpoints are numbers, modes are strings.
    fn decoded(prop: &str, value: &str) -> PropertyValue {
        if prop.ends_with("_setpoint") {
            PropertyValue::Int(value.parse().unwrap())
        } else {
            value.into()
        }
    }

    fn hex(packet: &[u8]) -> String {
        hex_encode(packet).to_ascii_lowercase()
    }

    #[test]
    fn real_capture_matches_owner_confirmed_3c_and_minus_18c_and_door_open_to_closed() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(OPEN));
        assert_eq!(
            props(&ha),
            expected(&[
                ("fridge_setpoint", PropertyValue::Int(3)),
                ("freezer_setpoint", PropertyValue::Int(-18)),
                ("door", "ON".into()),
                ("super_cool", "OFF".into()),
                ("express_freeze", "OFF".into()),
                ("vacation", "OFF".into()),
            ])
        );
        thinq.emit_data(&hex_decode(CLOSED));
        assert_eq!(props(&ha)["door"], "OFF".into());
    }

    #[test]
    fn keeps_existing_sensors_and_routes_added_ha_temperature_controls_to_verified_commands() {
        let (ha, thinq, dev) = make();
        let components = ha.device(ID).unwrap().config.unwrap().components;
        for key in ["fridge_setpoint", "freezer_setpoint", "door"] {
            assert!(components[key].get("command_topic").is_none(), "{key}");
        }
        assert_eq!(
            components["fridge_setpoint"]["unique_id"],
            "$deviceid-fridge_setpoint"
        );
        assert_eq!(
            components["freezer_setpoint"]["unique_id"],
            "$deviceid-freezer_setpoint"
        );
        thinq.emit_data(&hex_decode(BASELINE));
        let d = dev.clone();
        ha.on_set_property(move |_, prop, value| d.set_property(prop, value));
        thinq.reset_recorder();
        ha.set_property(ID, "fridge_temperature", "command", "4");
        let (_, _, tx, _) = CONTROLS
            .iter()
            .find(|(prop, value, _, _)| *prop == "fridge_setpoint" && *value == "4")
            .unwrap();
        assert_eq!(hex(&thinq.outbox()[0]), *tx);
        assert_eq!(
            props(&ha)["fridge_setpoint"],
            PropertyValue::Int(3),
            "wait for appliance readback instead of publishing an optimistic value"
        );
        thinq.reset_recorder();
        dev.start();
        assert_eq!(hex(&thinq.outbox()[0]), "aa0ef0ed1211010000010400ebbb");
    }

    #[test]
    fn captured_panel_sweeps_decode_all_celsius_setpoints_and_mode_changes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(BASELINE));
        for &(prop, value, rx) in PANEL {
            thinq.emit_data(&hex_decode(rx));
            assert_eq!(props(&ha)[prop], decoded(prop, value), "{prop}={value}");
        }
        let swept = |setpoint: &str| {
            let mut values: Vec<i64> = PANEL
                .iter()
                .filter(|(prop, _, _)| *prop == setpoint)
                .map(|(_, value, _)| value.parse().unwrap())
                .collect();
            values.sort_unstable();
            values.dedup();
            values
        };
        assert_eq!(swept("fridge_setpoint"), [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(
            swept("freezer_setpoint"),
            [-23, -22, -21, -20, -19, -18, -17, -16, -15]
        );
    }

    #[test]
    fn all_five_controls_reproduce_physically_verified_commands_and_decode_appliance_responses() {
        for &(prop, value, tx, rx) in CONTROLS {
            let (ha, thinq, dev) = make();
            thinq.emit_data(&hex_decode(BASELINE));
            thinq.reset_recorder();
            dev.set_property(prop, value);
            let outbox = thinq.outbox();
            assert_eq!(outbox.len(), 1);
            assert_eq!(hex(&outbox[0]), tx, "{prop}={value}");
            if prop.ends_with("_setpoint") {
                thinq.reset_recorder();
                dev.set_property(prop, &format!("{value}.0"));
                assert_eq!(
                    hex(&thinq.outbox()[0]),
                    tx,
                    "HA float serialization keeps the same wire setpoint"
                );
            }
            thinq.emit_data(&hex_decode(rx));
            assert_eq!(props(&ha)[prop], decoded(prop, value));
        }
    }

    #[test]
    fn ignores_invalid_mqtt_values_unknown_controls_and_commands_before_a_valid_celsius_status() {
        let (_, thinq, dev) = make();
        dev.set_property("super_cool", "ON");
        assert!(thinq.outbox().is_empty());
        thinq.emit_data(&hex_decode(BASELINE));
        thinq.reset_recorder();
        for (prop, value) in [
            ("fridge_setpoint", "0"),
            ("fridge_setpoint", "8"),
            ("fridge_setpoint", "3.5"),
            ("fridge_setpoint", ""),
            ("fridge_setpoint", " "),
            ("fridge_setpoint", "NaN"),
            ("freezer_setpoint", "-24"),
            ("freezer_setpoint", "-14"),
            ("freezer_setpoint", "-18.5"),
            ("super_cool", "true"),
            ("express_freeze", "off"),
            ("vacation", "1"),
            ("smart_learner", "ON"),
        ] {
            dev.set_property(prop, value);
        }
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn acknowledges_captured_usage_and_energy_reports_so_telemetry_delivery_finishes() {
        let (_, thinq, _) = make();
        thinq.emit_data(&hex_decode(USAGE));
        thinq.emit_data(&hex_decode(ENERGY));
        let sent: Vec<(String, Value)> =
            thinq.sent().into_iter().map(|m| (m.cmd, m.data)).collect();
        assert_eq!(
            sent,
            [
                ("ack".to_string(), json!("AA08F000C5043EBB")),
                ("ack".to_string(), json!("AA08F000AF0400BB")),
            ]
        );
    }

    #[test]
    fn ignores_status_frames_with_incorrect_record_lengths() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&wrap_aabb(&hex_decode("10eb02050401")));
        assert!(props(&ha).is_empty());
    }

    #[test]
    fn unsupported_units_and_sentinel_fields_become_unknown() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(OPEN));
        let open = hex_decode(OPEN);
        let mut inner = open[2..open.len() - 2].to_vec();
        inner[2 + 8] = 0xff;
        inner[2 + 7] = 0xff;
        thinq.emit_data(&wrap_aabb(&inner));
        assert_eq!(
            props(&ha),
            expected(&[
                ("fridge_setpoint", "None".into()),
                ("freezer_setpoint", "None".into()),
                ("door", "None".into()),
                ("super_cool", "OFF".into()),
                ("express_freeze", "OFF".into()),
                ("vacation", "OFF".into()),
            ])
        );
        thinq.reset_recorder();
        dev.set_property("super_cool", "ON");
        dev.set_property("fridge_setpoint", "4");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("2REBGLUBD1P_A").is_some());
    }
}
