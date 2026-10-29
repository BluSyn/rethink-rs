//! 2REF11EII__M2 — LG LMXS28626 ThinQ fridge.
//!
//! Nine-byte class `0x10` status. Temperatures are sparse model-JSON maps,
//! not the linear `fridge_common` formulas. Config waits for the first status
//! because that packet is what says Fahrenheit or Celsius. `auto_ack` stays
//! off: the cloud does not ack this appliance.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{pack_status, status_get, unpack_status, Status, TemperatureUnit};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LEN: usize = 9;
const SETTING_LEN: usize = 41;
const EXPRESS_ON: u8 = 2;
const EXPRESS_OFF: u8 = 1;
const FRESH_AIR_REPLACE: u8 = 4;
const WATER_FILTER_REPLACE_FROM: u8 = 6;
const WATER_FILTER_MAX: u8 = 12;

const FRIDGE_F: &[(u8, i64)] = &[
    (1, 46),
    (2, 45),
    (3, 44),
    (4, 43),
    (5, 42),
    (6, 41),
    (7, 40),
    (8, 39),
    (9, 38),
    (10, 37),
    (11, 36),
    (12, 35),
    (13, 34),
    (14, 33),
];
const FREEZER_F: &[(u8, i64)] = &[
    (1, 8),
    (2, 6),
    (3, 5),
    (4, 4),
    (5, 3),
    (6, 2),
    (7, 1),
    (8, 0),
    (9, -1),
    (10, -2),
    (11, -3),
    (12, -4),
    (13, -6),
];
const FRIDGE_C: &[(u8, i64)] = &[
    (1, 8),
    (3, 7),
    (5, 6),
    (7, 5),
    (9, 4),
    (10, 3),
    (12, 2),
    (14, 1),
];
const FREEZER_C: &[(u8, i64)] = &[
    (1, -13),
    (2, -14),
    (3, -15),
    (5, -16),
    (7, -17),
    (9, -18),
    (11, -19),
    (12, -20),
    (13, -21),
];

fn tables(unit: TemperatureUnit) -> (&'static [(u8, i64)], &'static [(u8, i64)]) {
    match unit {
        TemperatureUnit::F => (FRIDGE_F, FREEZER_F),
        TemperatureUnit::C => (FRIDGE_C, FREEZER_C),
    }
}

fn lookup(table: &[(u8, i64)], raw: u8) -> Option<i64> {
    table.iter().find(|(code, _)| *code == raw).map(|(_, v)| *v)
}

fn inverse(table: &[(u8, i64)], value: i64) -> Option<u8> {
    table.iter().find(|(_, v)| *v == value).map(|(code, _)| *code)
}

fn range_ends(table: &[(u8, i64)]) -> (i64, i64) {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for (_, value) in table {
        min = min.min(*value);
        max = max.max(*value);
    }
    (min, max)
}

fn fresh_air_name(code: u8) -> Option<&'static str> {
    match code {
        2 => Some("Auto"),
        3 => Some("Power"),
        _ => None,
    }
}

fn smart_grid_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Not In Use"),
        2 => Some("Seasonal Energy Saver"),
        3 => Some("Peak Energy Saver"),
        _ => None,
    }
}

/// Months above 12 are not a reading. `None` means skip both filter sensors.
fn water_filter_problem(months: u8) -> Option<bool> {
    if months > WATER_FILTER_MAX {
        None
    } else {
        Some(months >= WATER_FILTER_REPLACE_FROM)
    }
}

fn door_compartment(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("fridge_door"),
        2 => Some("freezer_door"),
        _ => None,
    }
}

fn components(
    uom: &str,
    fridge_min: i64,
    fridge_max: i64,
    freezer_min: i64,
    freezer_max: i64,
) -> HashMap<String, Value> {
    [
        ("fridge_setpoint".into(), json!({
            "platform": "number",
            "device_class": "temperature",
            "unique_id": "$deviceid-fridge_setpoint",
            "state_topic": "$this/fridge_setpoint",
            "command_topic": "$this/fridge_setpoint/set",
            "name": "Fridge temperature",
            "unit_of_measurement": uom,
            "min": fridge_min,
            "max": fridge_max,
        })),
        ("freezer_setpoint".into(), json!({
            "platform": "number",
            "device_class": "temperature",
            "unique_id": "$deviceid-freezer_setpoint",
            "state_topic": "$this/freezer_setpoint",
            "command_topic": "$this/freezer_setpoint/set",
            "name": "Freezer temperature",
            "unit_of_measurement": uom,
            "min": freezer_min,
            "max": freezer_max,
        })),
        ("express_freeze".into(), json!({
            "platform": "switch",
            "icon": "mdi:snowflake",
            "unique_id": "$deviceid-express_freeze",
            "state_topic": "$this/express_freeze",
            "command_topic": "$this/express_freeze/set",
            "name": "Ice Plus",
        })),
        ("fresh_air_filter".into(), json!({
            "platform": "select",
            "icon": "mdi:air-filter",
            "unique_id": "$deviceid-fresh_air_filter",
            "state_topic": "$this/fresh_air_filter",
            "command_topic": "$this/fresh_air_filter/set",
            "name": "Fresh air filter mode",
            "options": ["Auto", "Power"],
        })),
        ("water_filter".into(), json!({
            "platform": "binary_sensor",
            "device_class": "problem",
            "entity_category": "diagnostic",
            "icon": "mdi:water-opacity",
            "unique_id": "$deviceid-water_filter",
            "state_topic": "$this/water_filter",
            "name": "Water filter health",
        })),
        ("water_filter_months".into(), json!({
            "platform": "sensor",
            "entity_category": "diagnostic",
            "unique_id": "$deviceid-water_filter_months",
            "state_topic": "$this/water_filter_months",
            "name": "Water filter months",
            "icon": "mdi:calendar-clock",
            "unit_of_measurement": "months",
            "suggested_display_precision": 0,
        })),
        ("fresh_air_filter_replace".into(), json!({
            "platform": "binary_sensor",
            "device_class": "problem",
            "entity_category": "diagnostic",
            "icon": "mdi:air-filter",
            "unique_id": "$deviceid-fresh_air_filter_replace",
            "state_topic": "$this/fresh_air_filter_replace",
            "name": "Fresh air filter health",
        })),
        ("smart_grid".into(), json!({
            "platform": "select",
            "icon": "mdi:transmission-tower",
            "unique_id": "$deviceid-smart_grid",
            "state_topic": "$this/smart_grid",
            "command_topic": "$this/smart_grid/set",
            "name": "Smart Grid",
            "options": ["Not In Use", "Seasonal Energy Saver", "Peak Energy Saver"],
        })),
        ("fridge_door".into(), json!({
            "platform": "binary_sensor",
            "device_class": "door",
            "unique_id": "$deviceid-fridge_door",
            "state_topic": "$this/fridge_door",
            "name": "Fridge door",
        })),
        ("freezer_door".into(), json!({
            "platform": "binary_sensor",
            "device_class": "door",
            "unique_id": "$deviceid-freezer_door",
            "state_topic": "$this/freezer_door",
            "name": "Freezer door",
        })),
    ]
    .into_iter()
    .collect()
}

/// F017 body is 41 bytes of packed status. Captures put 0x00 at byte 31.
fn build_setting(fields: &[(&'static str, u8)]) -> Vec<u8> {
    let mut status = Status::new();
    status.insert("dispenserMode", 0);
    status.insert("dispenserCapacity", 0);
    status.insert("dispenserUnit", 0);
    status.insert("monDataNumber", 0);
    for (key, value) in fields {
        status.insert(key, *value);
    }
    let mut body = pack_status(&status, SETTING_LEN);
    body[31] = 0;
    let mut cmd = Vec::with_capacity(2 + body.len());
    cmd.extend_from_slice(&[0xf0, 0x17]);
    cmd.extend(body);
    cmd
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    meta: Metadata,
    unit: Mutex<Option<TemperatureUnit>>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            meta,
            unit: Mutex::new(None),
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
            let mut current = self.unit.lock();
            if *current == Some(unit) {
                return;
            }
            *current = Some(unit);
        }
        let (fridge, freezer) = tables(unit);
        let (fridge_min, fridge_max) = range_ends(fridge);
        let (freezer_min, freezer_max) = range_ends(freezer);
        let uom = match unit {
            TemperatureUnit::F => "°F",
            TemperatureUnit::C => "°C",
        };
        let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        config.components = components(uom, fridge_min, fridge_max, freezer_min, freezer_max);
        self.core.set_config(config);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&0x10) {
            return;
        }
        if buf.len() == 4 && buf[1] == 0xa8 {
            if let Some(name) = door_compartment(buf[2]) {
                self.core.publish_on_off(name, buf[3] == 1);
            }
            return;
        }
        if buf.len() < 2 || (buf[1] != 0xec && buf[1] != 0xeb) {
            return;
        }
        if buf[1] == 0xec && buf.len() == 2 + STATUS_LEN * 2 {
            self.process_status(&buf[2 + STATUS_LEN..]);
        } else if buf[1] == 0xeb && buf.len() == 2 + STATUS_LEN {
            self.process_status(&buf[2..2 + STATUS_LEN]);
        } else {
            tracing::debug!(
                "unexpected {:02x}{:02x} status length {}",
                buf[0],
                buf[1],
                buf.len()
            );
        }
    }

    fn process_status(&self, cur: &[u8]) {
        let status = unpack_status(cur);
        let unit = if status_get(&status, "tempUnit") != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        let (fridge, freezer) = tables(unit);
        if let Some(value) = lookup(fridge, status_get(&status, "fridgeSetpoint")) {
            self.core
                .publish_property("fridge_setpoint", PropertyValue::Int(value));
        }
        if let Some(value) = lookup(freezer, status_get(&status, "freezerSetpoint")) {
            self.core
                .publish_property("freezer_setpoint", PropertyValue::Int(value));
        }
        self.core.publish_on_off(
            "express_freeze",
            status_get(&status, "expressFreeze") == EXPRESS_ON,
        );
        let fresh = status_get(&status, "freshAirFilter");
        if let Some(name) = fresh_air_name(fresh) {
            self.core.publish_property("fresh_air_filter", name.into());
        }
        self.core
            .publish_on_off("fresh_air_filter_replace", fresh == FRESH_AIR_REPLACE);
        if let Some(name) = smart_grid_name(status_get(&status, "smartSaving")) {
            self.core.publish_property("smart_grid", name.into());
        }
        let months = status_get(&status, "waterFilter");
        if let Some(problem) = water_filter_problem(months) {
            self.core.publish_on_off("water_filter", problem);
            self.core
                .publish_property("water_filter_months", PropertyValue::Int(months as i64));
        }
        // The flag does not say which door. Only a closed reading resyncs both.
        if status_get(&status, "anyDoorOpen") != 1 {
            self.core.publish_on_off("fridge_door", false);
            self.core.publish_on_off("freezer_door", false);
        }
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
    fn set_property(&self, prop: &str, value: &str) {
        let unit = self.unit.lock().unwrap_or(TemperatureUnit::F);
        let (fridge, freezer) = tables(unit);
        let mut fields: Vec<(&'static str, u8)> = Vec::new();
        match prop {
            "fridge_setpoint" | "freezer_setpoint" => {
                let Ok(parsed) = value.parse::<i64>() else {
                    tracing::debug!("{prop}: no code for {value}");
                    return;
                };
                let table = if prop == "fridge_setpoint" { fridge } else { freezer };
                let Some(raw) = inverse(table, parsed) else {
                    tracing::debug!("{prop}: no {unit:?} code for {value}");
                    return;
                };
                let key = if prop == "fridge_setpoint" {
                    "fridgeSetpoint"
                } else {
                    "freezerSetpoint"
                };
                fields.push((key, raw));
                fields.push((
                    "tempUnit",
                    if unit == TemperatureUnit::C { 1 } else { 0 },
                ));
            }
            "fresh_air_filter" => {
                let code = match value {
                    "Auto" => 2,
                    "Power" => 3,
                    _ => {
                        tracing::debug!("{prop}: cannot be set to {value}");
                        return;
                    }
                };
                fields.push(("freshAirFilter", code));
            }
            "smart_grid" => {
                let code = match value {
                    "Not In Use" => 0,
                    "Seasonal Energy Saver" => 2,
                    "Peak Energy Saver" => 3,
                    _ => {
                        tracing::debug!("Unexpected value {value}");
                        return;
                    }
                };
                fields.push(("smartSaving", code));
            }
            "express_freeze" => {
                fields.push((
                    "expressFreeze",
                    if value == "ON" { EXPRESS_ON } else { EXPRESS_OFF },
                ));
            }
            _ => {
                tracing::debug!("Unknown property {prop}");
                return;
            }
        }
        self.core.send(&build_setting(&fields));
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
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const SAMPLE_DELTA_EXPRESS_OFF: &str = "AA1810EC020C08020202040000020C08010202040000A8BB";
    const SAMPLE_DELTA_FRIDGE_36F: &str = "AA1810EC020C08010202040000020B08010202040000AEBB";
    const CLOUD_CMD_EXPRESS_OFF: &str =
        "AA2FF017FFFFFF01FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFEBBB";
    const CLOUD_CMD_FRIDGE_36F: &str =
        "AA2FF017FF0BFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF9CBB";
    const SAMPLE_DELTA_FREEZER_M3F: &str = "AA1810EC020B08010202040000020B0B010202040000A8BB";
    const CLOUD_CMD_FREEZER_M3F: &str =
        "AA2FF017FFFF0BFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF9CBB";
    const CLOUD_CMD_FREEZER_0F: &str =
        "AA2FF017FFFF08FFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF93BB";
    const SAMPLE_DELTA_FRESH_AIR_POWER: &str = "AA1810EC020B08010202040000020B08010302040000AEBB";
    const CLOUD_CMD_FRESH_AIR_POWER: &str =
        "AA2FF017FFFFFFFF03FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF95BB";
    const SAMPLE_DELTA_FRESH_AIR_AUTO: &str = "AA1810EC020B08010302040000020B08010202040000AEBB";
    const CLOUD_CMD_FRESH_AIR_AUTO: &str =
        "AA2FF017FFFFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFEABB";
    const SAMPLE_DELTA_SMART_GRID_OFF: &str = "AA1810EC020B08010202040000020B08010200040000ADBB";
    const CLOUD_CMD_SMART_GRID_OFF: &str =
        "AA2FF017FFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFE8BB";
    const SAMPLE_DELTA_SMART_GRID_PEAK: &str = "AA1810EC020C08010200040000020C08010203040000AEBB";
    const SAMPLE_DELTA_SMART_GRID_SEASONAL: &str = "AA1810EC020B08010200040000020B08010202040000ADBB";
    const CLOUD_CMD_SMART_GRID_SEASONAL: &str =
        "AA2FF017FFFFFFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFEABB";
    const SAMPLE_DELTA_DOOR_OPEN: &str = "AA1810EC020B08010202040000020B08010202040100AEBB";
    const SAMPLE_DELTA_DOOR_CLOSED: &str = "AA1810EC020B08010202040100020B08010202040000AEBB";
    const FRIDGE_DOOR_OPEN: &str = "AA0810A8010139BB";
    const FRIDGE_DOOR_CLOSED: &str = "AA0810A801003EBB";
    const FREEZER_DOOR_OPEN: &str = "AA0810A8020138BB";
    const FREEZER_DOOR_CLOSED: &str = "AA0810A8020039BB";
    const SAMPLE_DELTA_CELSIUS: &str = "AA1810EC020B08010202040000020C09010202040001A8BB";
    const CLOUD_CMD_START_QUERY: &str = "AA0EF0ED1211010000010400EBBB";
    const SAMPLE_INITIAL: &str = "AA0F10EB020C0801020204000086BB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2REF11EII__M2", "2REF11EII__M2", "1.0");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn emit(thinq: &MockThinq2Device, hex: &str) {
        thinq.emit_data(&hex_decode(hex));
    }

    fn with_fresh_air(code: u8) -> Vec<u8> {
        let mut inner = hex_decode("10EC020B08010302040000020B08010302040000");
        inner[2 + 9 + 4] = code;
        wrap_aabb(&inner)
    }

    #[test]
    fn no_config_until_a_status_packet_reveals_the_temperature_unit() {
        let (ha, _, _) = make();
        assert!(ha.device(ID).and_then(|d| d.config).is_none());
    }

    #[test]
    fn start_reproduces_the_query_the_cloud_sends_on_reconnect() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_START_QUERY);
    }

    #[test]
    fn initial_10eb_is_a_single_9_byte_block() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_INITIAL);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("35"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "water_filter").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "water_filter_months").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "fresh_air_filter_replace").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "smart_grid").as_deref(), Some("Seasonal Energy Saver"));
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("OFF"));
    }

    #[test]
    fn fahrenheit_config_comes_from_the_model_json_tables() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        let comps = ha.device(ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°F"));
        assert_eq!(comps["fridge_setpoint"]["min"], json!(33));
        assert_eq!(comps["fridge_setpoint"]["max"], json!(46));
        assert_eq!(comps["freezer_setpoint"]["unit_of_measurement"], json!("°F"));
        assert_eq!(comps["freezer_setpoint"]["min"], json!(-6));
        assert_eq!(comps["freezer_setpoint"]["max"], json!(8));
        assert_eq!(comps["fresh_air_filter"]["options"], json!(["Auto", "Power"]));
        assert_eq!(comps["water_filter"]["device_class"], json!("problem"));
        assert_eq!(comps["fresh_air_filter_replace"]["device_class"], json!("problem"));
        assert!(comps.get("flex_setpoint").is_none());
    }

    #[test]
    fn baseline_decodes_to_35f_and_0f() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("35"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("OFF"));
    }

    #[test]
    fn fridge_setpoint_transition_35f_to_36f() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        emit(&thinq, SAMPLE_DELTA_FRIDGE_36F);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("36"));
    }

    #[test]
    fn door_open_flag_does_not_say_which_door() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        assert!(prop(&ha, "fridge_door").is_none());
        assert!(prop(&ha, "freezer_door").is_none());
        emit(&thinq, FRIDGE_DOOR_OPEN);
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("ON"));
    }

    #[test]
    fn a8_events_give_per_compartment_door_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, FREEZER_DOOR_OPEN);
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("ON"));
        assert!(prop(&ha, "fridge_door").is_none());
        emit(&thinq, FREEZER_DOOR_CLOSED);
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("OFF"));
        emit(&thinq, FRIDGE_DOOR_OPEN);
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("OFF"));
        emit(&thinq, FRIDGE_DOOR_CLOSED);
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("OFF"));
    }

    #[test]
    fn all_closed_flag_resynchronises_per_compartment_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, FRIDGE_DOOR_OPEN);
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_DELTA_DOOR_CLOSED);
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("OFF"));
    }

    #[test]
    fn celsius_reports_the_same_temperatures_in_the_other_unit() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        emit(&thinq, SAMPLE_DELTA_FRIDGE_36F);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("36"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("0"));
        emit(&thinq, SAMPLE_DELTA_CELSIUS);
        let comps = ha.device(ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°C"));
        assert_eq!(comps["fridge_setpoint"]["min"], json!(1));
        assert_eq!(comps["fridge_setpoint"]["max"], json!(8));
        assert_eq!(comps["freezer_setpoint"]["unit_of_measurement"], json!("°C"));
        assert_eq!(comps["freezer_setpoint"]["min"], json!(-21));
        assert_eq!(comps["freezer_setpoint"]["max"], json!(-13));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
    }

    #[test]
    fn write_fridge_36f_matches_the_cloud_command() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "36");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_FRIDGE_36F);
    }

    #[test]
    fn write_express_off_matches_the_cloud_command() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("express_freeze", "OFF");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_EXPRESS_OFF);
    }

    #[test]
    fn freezer_transition_0f_to_minus_3f() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_FREEZER_M3F);
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-3"));
    }

    #[test]
    fn write_freezer_minus_3f_matches_the_cloud_command() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-3");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_FREEZER_M3F);
    }

    #[test]
    fn telemetry_and_diagnostics_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, "AA0B10AF2700130B0894BB");
        emit(&thinq, "AA06F014E1BB");
        assert!(ha.device(ID).and_then(|d| d.config).is_none());
    }

    #[test]
    fn write_freezer_0f_matches_the_cloud_command() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "0");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_FREEZER_0F);
    }

    #[test]
    fn fresh_air_filter_auto_to_power() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_FRESH_AIR_POWER);
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("Power"));
        assert_eq!(prop(&ha, "fresh_air_filter_replace").as_deref(), Some("OFF"));
    }

    #[test]
    fn write_fresh_air_power_matches_the_cloud_command() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("fresh_air_filter", "Power");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_FRESH_AIR_POWER);
    }

    #[test]
    fn fresh_air_off_is_really_auto() {
        let (ha, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_FRESH_AIR_AUTO);
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("Auto"));
        thinq.reset_recorder();
        dev.set_property("fresh_air_filter", "Auto");
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_FRESH_AIR_AUTO);
    }

    #[test]
    fn fresh_air_mode_holds_through_off_and_replace() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_FRESH_AIR_POWER);
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("Power"));
        thinq.emit_data(&with_fresh_air(4));
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("Power"));
        assert_eq!(prop(&ha, "fresh_air_filter_replace").as_deref(), Some("ON"));
        thinq.emit_data(&with_fresh_air(1));
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("Power"));
        assert_eq!(prop(&ha, "fresh_air_filter_replace").as_deref(), Some("OFF"));
    }

    #[test]
    fn smart_grid_decodes_the_three_usable_modes() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        assert_eq!(prop(&ha, "smart_grid").as_deref(), Some("Seasonal Energy Saver"));
        emit(&thinq, SAMPLE_DELTA_SMART_GRID_OFF);
        assert_eq!(prop(&ha, "smart_grid").as_deref(), Some("Not In Use"));
        emit(&thinq, SAMPLE_DELTA_SMART_GRID_SEASONAL);
        assert_eq!(prop(&ha, "smart_grid").as_deref(), Some("Seasonal Energy Saver"));
        emit(&thinq, SAMPLE_DELTA_SMART_GRID_PEAK);
        assert_eq!(prop(&ha, "smart_grid").as_deref(), Some("Peak Energy Saver"));
    }

    #[test]
    fn write_smart_grid_matches_both_captured_commands() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("smart_grid", "Not In Use");
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_SMART_GRID_OFF);
        thinq.reset_recorder();
        dev.set_property("smart_grid", "Seasonal Energy Saver");
        assert_eq!(hex_encode(&thinq.outbox()[0]), CLOUD_CMD_SMART_GRID_SEASONAL);
    }

    #[test]
    fn fresh_air_report_only_states_are_refused() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("fresh_air_filter", "Replace filter");
        dev.set_property("fresh_air_filter", "Off");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn fahrenheit_freezer_value_with_no_code_is_refused() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "7");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn unknown_property_is_ignored() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_OFF);
        thinq.reset_recorder();
        dev.set_property("does-not-exist", "1");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn factory_registers_the_model() {
        assert!(crate::registry::t2_factory("2REF11EII__M2").is_some());
    }
}
