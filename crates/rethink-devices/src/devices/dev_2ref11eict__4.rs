//! 2REF11EICT__4 — LG LRYXC2606S 3-door French door fridge.
//!
//! Same AABB framing, shared status fields and 118-byte F017 write as 2REF11EIDG__4, with a
//! 43-byte status record. Confirmed against the app: setpoints [1]/[2], Ice Plus [3] (1 off,
//! 2 on), door [7], unit [8] (0 °F, 1 °C), Sabbath Mode [14], Smart Learner [17] and Craft Ice
//! [25]. Celsius is not linear, so it comes from this model's modelJson tables. [35]/[36] are the
//! fresh air / water filter life in percent (a reset writes 100), and [32]/[34]/[41] the
//! Crushed/Cubed, Craft Ice and Mini Cubed ice maker states. `10 C5` usage record `0x21` is water
//! dispensed in millilitres. Sabbath's Set Schedule (F010) is cloud-scheduled, so it is left out.
//!
//! Port of upstream PR #225.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x10;
const RECORD_LENGTH: usize = 43;
const COMMAND_LENGTH: usize = 118;
const FRESH_AIR_FILTER: usize = 35;
const WATER_FILTER: usize = 36;
const WATER_DISPENSED_RECORD: u8 = 0x21;
const MINI_CUBED_ICE_OFFSET: usize = 41;
const CRAFT_ICE_STATUS_OFFSET: usize = 34;
const CRUSHED_CUBED_ICE_OFFSET: usize = 32;

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

const CRAFT_ICE_MODE: &[(u8, &str)] = &[(0, "Off"), (1, "3 ICE"), (2, "6 ICE")];
const ICE_MAKER_STATUS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Making ice"),
    (2, "Full ice bin"),
    (3, "Quiet Mode"),
];

/// Raw byte to °C, from this model's modelJson (`fridgeTemp_C` / `freezerTemp_C`).
const FRIDGE_C_TABLE: &[(u8, i32)] = &[(1, 7), (2, 6), (3, 5), (5, 4), (7, 3), (9, 2), (11, 1)];
const FREEZER_C_TABLE: &[(u8, i32)] = &[
    (1, -15),
    (3, -16),
    (5, -17),
    (7, -18),
    (9, -19),
    (10, -20),
    (11, -21),
    (12, -22),
    (13, -23),
];

type Convert = fn(TemperatureUnit, i32) -> i32;

/// Celsius is looked up in `table`; Fahrenheit uses the shared formula.
fn decode(unit: TemperatureUnit, raw: u8, table: &[(u8, i32)], convert: Convert) -> Option<i32> {
    match unit {
        TemperatureUnit::C => table.iter().find(|&&(r, _)| r == raw).map(|&(_, c)| c),
        TemperatureUnit::F => Some(convert(unit, i32::from(raw))),
    }
}

fn encode(
    unit: TemperatureUnit,
    display: i32,
    table: &[(u8, i32)],
    convert: Convert,
) -> Option<u8> {
    match unit {
        TemperatureUnit::C => table.iter().find(|&&(_, c)| c == display).map(|&(r, _)| r),
        TemperatureUnit::F => Some(convert(unit, display) as u8),
    }
}

/// A whole-number setpoint, also in the "4.0" form.
fn parse_display(mqtt_value: &str) -> Option<i32> {
    let value: f64 = mqtt_value.trim().parse().ok()?;
    (value.fract() == 0.0 && value.abs() <= f64::from(i16::MAX)).then_some(value as i32)
}

fn number_or_none(value: Option<i32>) -> PropertyValue {
    value.map_or_else(|| "None".into(), PropertyValue::from)
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or("None")
}

fn code_of(table: &'static [(u8, &'static str)], name: &str) -> Option<u8> {
    table.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    meta: Metadata,
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
            temperature_unit: Mutex::new(None),
            meta,
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
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
            (
                "fridge_setpoint".into(),
                json!({
                    "platform": "number", "device_class": "temperature",
                    "unique_id": "$deviceid-fridge_setpoint",
                    "state_topic": "$this/fridge_setpoint",
                    "command_topic": "$this/fridge_setpoint/set",
                    "name": "Fridge temperature",
                    "unit_of_measurement": fu, "min": fmin, "max": fmax,
                }),
            ),
            (
                "freezer_setpoint".into(),
                json!({
                    "platform": "number", "device_class": "temperature",
                    "unique_id": "$deviceid-freezer_setpoint",
                    "state_topic": "$this/freezer_setpoint",
                    "command_topic": "$this/freezer_setpoint/set",
                    "name": "Freezer temperature",
                    "unit_of_measurement": zu, "min": zmin, "max": zmax,
                }),
            ),
            (
                "door".into(),
                json!({
                    "platform": "binary_sensor", "device_class": "door",
                    "unique_id": "$deviceid-door",
                    "state_topic": "$this/door",
                    "name": "Door",
                }),
            ),
            (
                "ice_plus".into(),
                json!({
                    "platform": "switch", "icon": "mdi:snowflake",
                    "unique_id": "$deviceid-ice_plus",
                    "state_topic": "$this/ice_plus",
                    "command_topic": "$this/ice_plus/set",
                    "name": "Ice Plus",
                    "payload_on": "ON", "payload_off": "OFF",
                    "entity_category": "config",
                }),
            ),
            (
                "sabbath_mode".into(),
                json!({
                    "platform": "switch", "icon": "mdi:candle",
                    "unique_id": "$deviceid-sabbath_mode",
                    "state_topic": "$this/sabbath_mode",
                    "command_topic": "$this/sabbath_mode/set",
                    "name": "Sabbath Mode",
                    "payload_on": "ON", "payload_off": "OFF",
                    "entity_category": "config",
                }),
            ),
            (
                "smart_learner".into(),
                json!({
                    "platform": "switch", "icon": "mdi:brain",
                    "unique_id": "$deviceid-smart_learner",
                    "state_topic": "$this/smart_learner",
                    "command_topic": "$this/smart_learner/set",
                    "name": "Smart Learner",
                    "payload_on": "ON", "payload_off": "OFF",
                    "entity_category": "config",
                }),
            ),
            (
                "craft_ice".into(),
                json!({
                    "platform": "select", "icon": "mdi:ice-cream",
                    "unique_id": "$deviceid-craft_ice",
                    "state_topic": "$this/craft_ice",
                    "command_topic": "$this/craft_ice/set",
                    "name": "Craft Ice",
                    "options": labels(CRAFT_ICE_MODE),
                    "entity_category": "config",
                }),
            ),
            (
                "fresh_air_filter".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:air-filter",
                    "unique_id": "$deviceid-fresh_air_filter",
                    "state_topic": "$this/fresh_air_filter",
                    "unit_of_measurement": "%", "state_class": "measurement",
                    "entity_category": "diagnostic",
                    "name": "Fresh air filter",
                }),
            ),
            (
                "water_filter".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:water-check",
                    "unique_id": "$deviceid-water_filter",
                    "state_topic": "$this/water_filter",
                    "unit_of_measurement": "%", "state_class": "measurement",
                    "entity_category": "diagnostic",
                    "name": "Water filter",
                }),
            ),
            (
                "fresh_air_filter_reset".into(),
                json!({
                    "platform": "button", "icon": "mdi:restore",
                    "unique_id": "$deviceid-fresh_air_filter_reset",
                    "command_topic": "$this/fresh_air_filter_reset/set",
                    "payload_press": "",
                    "entity_category": "diagnostic",
                    "name": "Reset fresh air filter",
                }),
            ),
            (
                "water_filter_reset".into(),
                json!({
                    "platform": "button", "icon": "mdi:restore",
                    "unique_id": "$deviceid-water_filter_reset",
                    "command_topic": "$this/water_filter_reset/set",
                    "payload_press": "",
                    "entity_category": "diagnostic",
                    "name": "Reset water filter",
                }),
            ),
            (
                "water_dispensed".into(),
                json!({
                    "platform": "sensor", "device_class": "water",
                    "unique_id": "$deviceid-water_dispensed",
                    "state_topic": "$this/water_dispensed",
                    "unit_of_measurement": "L", "state_class": "total_increasing",
                    "entity_category": "diagnostic",
                    "name": "Water dispensed",
                }),
            ),
            (
                "mini_cubed_ice".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:ice-pop",
                    "unique_id": "$deviceid-mini_cubed_ice",
                    "state_topic": "$this/mini_cubed_ice",
                    "name": "Mini Cubed Ice",
                    "device_class": "enum", "options": labels(ICE_MAKER_STATUS),
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "craft_ice_status".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:ice-cream",
                    "unique_id": "$deviceid-craft_ice_status",
                    "state_topic": "$this/craft_ice_status",
                    "name": "Craft Ice status",
                    "device_class": "enum", "options": labels(ICE_MAKER_STATUS),
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "crushed_cubed_ice".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:ice-pop",
                    "unique_id": "$deviceid-crushed_cubed_ice",
                    "state_topic": "$this/crushed_cubed_ice",
                    "name": "Crushed/Cubed Ice",
                    "device_class": "enum", "options": labels(ICE_MAKER_STATUS),
                    "entity_category": "diagnostic",
                }),
            ),
        ]
        .into_iter()
        .collect();
        self.core.set_config(config);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, CLASS_BYTE, RECORD_LENGTH) {
            self.process_status(rec);
        } else if buf.len() >= 4 && buf[0] == CLASS_BYTE && buf[1] == 0xc5 {
            self.process_usage(&buf[2..]);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        let unit = if rec[8] != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        let fridge = decode(unit, rec[1], FRIDGE_C_TABLE, convert_fridge_temperature);
        let freezer = decode(unit, rec[2], FREEZER_C_TABLE, convert_freezer_temperature);
        self.core
            .publish_property("fridge_setpoint", number_or_none(fridge));
        self.core
            .publish_property("freezer_setpoint", number_or_none(freezer));
        self.core.publish_on_off("door", rec[7] == 1);
        self.core.publish_on_off("ice_plus", rec[3] == 2);
        self.core.publish_on_off("sabbath_mode", rec[14] == 1);
        self.core.publish_on_off("smart_learner", rec[17] == 1);
        self.core
            .publish_property("craft_ice", named(CRAFT_ICE_MODE, rec[25]).into());
        self.core
            .publish_property("fresh_air_filter", i64::from(rec[FRESH_AIR_FILTER]).into());
        self.core
            .publish_property("water_filter", i64::from(rec[WATER_FILTER]).into());
        for (prop, offset) in [
            ("mini_cubed_ice", MINI_CUBED_ICE_OFFSET),
            ("craft_ice_status", CRAFT_ICE_STATUS_OFFSET),
            ("crushed_cubed_ice", CRUSHED_CUBED_ICE_OFFSET),
        ] {
            self.core
                .publish_property(prop, named(ICE_MAKER_STATUS, rec[offset]).into());
        }
    }

    /// `<seq> <count>`, then `<count>` records of `<id> <recent: u16> <?> <total: u16>`.
    fn process_usage(&self, report: &[u8]) {
        let count = report[1] as usize;
        for i in 0..count {
            let start = 2 + i * 6;
            let Some(record) = report.get(start..start + 6) else {
                return;
            };
            if record[0] == WATER_DISPENSED_RECORD {
                let ml = u16::from_be_bytes([record[4], record[5]]);
                self.core
                    .publish_property("water_dispensed", (f64::from(ml) / 1000.0).into());
            }
        }
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
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::F);
        let unit_byte = if unit == TemperatureUnit::C { 1 } else { 0 };
        // A setpoint with no raw code sends nothing, rather than the 0 byte upstream would write.
        match prop {
            "fridge_setpoint" => {
                if let Some(raw) = parse_display(mqtt_value)
                    .and_then(|v| encode(unit, v, FRIDGE_C_TABLE, convert_fridge_temperature))
                {
                    self.command(&[(1, raw), (8, unit_byte)]);
                }
            }
            "freezer_setpoint" => {
                if let Some(raw) = parse_display(mqtt_value)
                    .and_then(|v| encode(unit, v, FREEZER_C_TABLE, convert_freezer_temperature))
                {
                    self.command(&[(2, raw), (8, unit_byte)]);
                }
            }
            "ice_plus" => self.command(&[(3, if mqtt_value == "ON" { 2 } else { 1 })]),
            "sabbath_mode" => self.command(&[(14, if mqtt_value == "ON" { 1 } else { 0 })]),
            "smart_learner" => self.command(&[(17, if mqtt_value == "ON" { 1 } else { 0 })]),
            "craft_ice" => {
                if let Some(mode) = code_of(CRAFT_ICE_MODE, mqtt_value) {
                    self.command(&[(25, mode)]);
                }
            }
            "fresh_air_filter_reset" => self.command(&[(FRESH_AIR_FILTER, 100)]),
            "water_filter_reset" => self.command(&[(WATER_FILTER, 100)]),
            _ => tracing::warn!(prop, "unknown property"),
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "2REF11EICT__4";

    // Real LRYXC2606S frames captured through the rethink bridge, confirmed against the app.
    const FRAME_BASELINE: &str = "AA5C10EC020B060102020201000001FFFFFF00FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF0100020B060102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF010058BB";
    const FRAME_DOOR_OPEN: &str = "AA5C10EC020B060102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF0100020B060102020201000001FFFFFF00FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF010058BB";
    const FRAME_ICE_PLUS_ON: &str = "AA5C10EC020B060102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF0100020B060202020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF010058BB";
    const FRAME_SABBATH_ON: &str = "AA5C10EC020B060202020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF0100020B060102020200000001FFFFFF01FFFF01FFFFFFFFFFFFFF00010101FF020001FF003A3AFFFFFFFF01005BBB";
    const FRAME_CRAFT_ICE_6ICE: &str = "AA5C10EC020A050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF000D0101FF020001FF003A3AFFFFFFFF0100020A050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF020D0101FF020001FF013A3AFFFFFFFF010076BB";
    const FRAME_CRAFT_ICE_3ICE: &str = "AA5C10EC0204050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF02120101FF030002FF013A3AFFFFFFFF02000204050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF02007CBB";
    const FRAME_SMART_LEARNER_OFF: &str = "AA5C10EC0204050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF02000204050102020200000001FFFFFF00FFFF00FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF020072BB";
    const FRAME_USAGE_BEFORE_DISPENSE: &str = "AA3810C5D80801000000000F020000000000030000000003110000000146120000000000130000000040210000000000220000000000FABB";
    const FRAME_USAGE_AFTER_DISPENSE: &str = "AA3810C5DA0801000000000F0200000000000300000000031100000001461200000000001300000000402103E60003E6220000000000D6BB";
    // Real panel toggle: fridge 40°F -> 4°C, freezer 1°F -> -17°C (both read off the display).
    const FRAME_UNIT_F: &str = "AA5C10EC0204050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF02000204050102020201000001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF02007CBB";
    const FRAME_UNIT_C: &str = "AA5C10EC0204050102020201000001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF02000205050102020201010001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF020079BB";
    // Real transition caught live: Mini Cubed Ice went "Making ice" -> "Full ice bin" in the app.
    const FRAME_MINI_CUBED_MAKING: &str = "AA5C10EC0204050102020201000001FFFFFF00FFFF01FFFFFFFFFFFFFF00120103FF030001FF003A3AFFFFFFFF01000204050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00120103FF030001FF003A3AFFFFFFFF010070BB";
    const FRAME_MINI_CUBED_FULL: &str = "AA5C10EC0204050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00120103FF030001FF003A3AFFFFFFFF01000204050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF00120103FF030001FF003A3AFFFFFFFF020070BB";
    // Same frame as FRAME_CRAFT_ICE_6ICE — Craft Ice status also read "Making ice" there.
    const FRAME_CRAFT_ICE_MAKING: &str = FRAME_CRAFT_ICE_6ICE;
    // Real snapshot: Crushed/Cubed and Mini Cubed Ice both read "Full ice bin" simultaneously.
    const FRAME_BOTH_FULL: &str = "AA5C10EC0205050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF0200020A050102020200000001FFFFFF00FFFF01FFFFFFFFFFFFFF01120101FF030002FF013A3AFFFFFFFF02007ABB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new(MODEL_ID, MODEL_ID, "");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?
            .properties
            .get(name)
            .map(|p| p.as_string())
    }

    fn fridge_unit(ha: &MockHaConnection) -> serde_json::Value {
        let config = ha.device(DEVICE_ID).unwrap().config.unwrap();
        config.components["fridge_setpoint"]["unit_of_measurement"].clone()
    }

    fn last_sent(thinq: &MockThinq2Device) -> String {
        hex_encode(thinq.outbox().last().unwrap())
    }

    #[test]
    fn setpoints_decode_from_the_app_confirmed_baseline() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_BASELINE));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("33"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("0"));
    }

    #[test]
    fn door_ice_plus_and_sabbath_mode_decode() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_BASELINE));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "ice_plus").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "sabbath_mode").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(FRAME_DOOR_OPEN));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(FRAME_ICE_PLUS_ON));
        assert_eq!(prop(&ha, "ice_plus").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(FRAME_SABBATH_ON));
        assert_eq!(prop(&ha, "sabbath_mode").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "ice_plus").as_deref(), Some("OFF"));
    }

    #[test]
    fn smart_learner_decodes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_BASELINE));
        assert_eq!(prop(&ha, "smart_learner").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(FRAME_SMART_LEARNER_OFF));
        assert_eq!(prop(&ha, "smart_learner").as_deref(), Some("OFF"));
    }

    #[test]
    fn craft_ice_decodes_all_three_states() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_BASELINE));
        assert_eq!(prop(&ha, "craft_ice").as_deref(), Some("Off"));

        thinq.emit_data(&hex_decode(FRAME_CRAFT_ICE_6ICE));
        assert_eq!(prop(&ha, "craft_ice").as_deref(), Some("6 ICE"));

        thinq.emit_data(&hex_decode(FRAME_CRAFT_ICE_3ICE));
        assert_eq!(prop(&ha, "craft_ice").as_deref(), Some("3 ICE"));
    }

    #[test]
    fn mini_cubed_ice_status_decodes_a_making_ice_to_full_ice_bin_transition() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_MINI_CUBED_MAKING));
        assert_eq!(prop(&ha, "mini_cubed_ice").as_deref(), Some("Making ice"));

        thinq.emit_data(&hex_decode(FRAME_MINI_CUBED_FULL));
        assert_eq!(prop(&ha, "mini_cubed_ice").as_deref(), Some("Full ice bin"));
    }

    #[test]
    fn craft_ice_status_decodes_off_and_making_ice() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_BASELINE));
        assert_eq!(prop(&ha, "craft_ice_status").as_deref(), Some("Off"));

        thinq.emit_data(&hex_decode(FRAME_CRAFT_ICE_MAKING));
        assert_eq!(prop(&ha, "craft_ice_status").as_deref(), Some("Making ice"));
    }

    #[test]
    fn crushed_cubed_and_mini_cubed_ice_both_decode_full_ice_bin() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_BOTH_FULL));
        assert_eq!(
            prop(&ha, "crushed_cubed_ice").as_deref(),
            Some("Full ice bin")
        );
        assert_eq!(prop(&ha, "mini_cubed_ice").as_deref(), Some("Full ice bin"));
    }

    #[test]
    fn fridge_and_freezer_setpoints_decode_in_both_f_and_c() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_UNIT_F));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("40"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("1"));
        assert_eq!(fridge_unit(&ha), "°F");

        thinq.emit_data(&hex_decode(FRAME_UNIT_C));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-17"));
        assert_eq!(fridge_unit(&ha), "°C");
    }

    #[test]
    fn water_dispensed_tracks_a_24oz_dispense() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_USAGE_BEFORE_DISPENSE));
        assert_eq!(prop(&ha, "water_dispensed").as_deref(), Some("0"));

        thinq.emit_data(&hex_decode(FRAME_USAGE_AFTER_DISPENSE));
        assert_eq!(prop(&ha, "water_dispensed").as_deref(), Some("0.998"));
    }

    #[test]
    fn filter_life_decodes_from_the_app_confirmed_baseline() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_BASELINE));
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("58"));
        assert_eq!(prop(&ha, "water_filter").as_deref(), Some("58"));
    }

    #[test]
    fn every_command_matches_what_the_lg_app_sent_byte_for_byte() {
        let (_ha, thinq, dev) = make();
        let cases = [
            (
                "ice_plus",
                "ON",
                "AA7CF017FFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB",
            ),
            (
                "sabbath_mode",
                "ON",
                "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB",
            ),
            (
                "sabbath_mode",
                "OFF",
                "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBCBB",
            ),
            (
                "freezer_setpoint",
                "1",
                "AA7CF017FFFF05FFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBABB",
            ),
            (
                "fridge_setpoint",
                "34",
                "AA7CF017FF0AFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA1BB",
            ),
            (
                "craft_ice",
                "6 ICE",
                "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FF0200FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB",
            ),
            (
                "craft_ice",
                "Off",
                "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FF0000FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBCBB",
            ),
            (
                "craft_ice",
                "3 ICE",
                "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FF0100FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB",
            ),
        ];
        for (prop_name, value, expected) in cases {
            dev.set_property(prop_name, value);
            assert_eq!(last_sent(&thinq), expected, "{prop_name}={value}");
        }
    }

    #[test]
    fn celsius_writes_use_the_model_json_tables() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(FRAME_UNIT_C));
        dev.set_property("fridge_setpoint", "4");
        dev.set_property("freezer_setpoint", "-17.0");
        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 2);
        // AA <len> F0 17, then the payload: [1]/[2] setpoint, [8] unit.
        assert_eq!((outbox[0][4 + 1], outbox[0][4 + 8]), (5, 1));
        assert_eq!((outbox[1][4 + 2], outbox[1][4 + 8]), (5, 1));
        // No raw code for 8 °C, and nothing to send for a non-number.
        dev.set_property("fridge_setpoint", "8");
        dev.set_property("freezer_setpoint", "cold");
        assert_eq!(thinq.outbox().len(), 2);
    }

    #[test]
    fn start_sends_the_fridge_status_query() {
        let (_ha, thinq, dev) = make();
        dev.start();
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1211010000010400EBBB"
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory(MODEL_ID).is_some());
    }
}
