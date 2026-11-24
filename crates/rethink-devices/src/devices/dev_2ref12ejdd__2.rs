//! 2REF12EJDD__2 — LG LRFDS3006D refrigerator (AABB, 18-byte status).
//!
//! Port of upstream PR #337. Uses sparse Celsius/Fahrenheit model-JSON maps,
//! 41-byte `F0 17` writes, `auto_ack` enabled, Ice Plus, Smart Grid, Sabbath
//! Mode, aggregate door sensor, and water/fresh-air filter health.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{pack_status, status_get, unpack_status, Status, TemperatureUnit};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LEN: usize = 18;
const SETTING_LEN: usize = 41;
const EXPRESS_ON: u8 = 2;
const EXPRESS_OFF: u8 = 1;
const FRESH_AIR_REPLACE: u8 = 3;
const WATER_FILTER_REPLACE_FROM: u8 = 6;
const WATER_FILTER_MAX: u8 = 12;

const FRIDGE_F: &[(u8, i64)] = &[
    (1, 43), (2, 42), (3, 41), (4, 40), (5, 39), (6, 38),
    (7, 37), (8, 36), (9, 35), (10, 34), (11, 33),
];
const FREEZER_F: &[(u8, i64)] = &[
    (1, 5), (2, 4), (3, 3), (4, 2), (5, 1), (6, 0), (7, -1),
    (8, -2), (9, -3), (10, -4), (11, -5), (12, -6), (13, -7),
];
const FRIDGE_C: &[(u8, i64)] = &[
    (1, 7), (2, 6), (3, 5), (5, 4), (7, 3), (9, 2), (11, 1),
];
const FREEZER_C: &[(u8, i64)] = &[
    (1, -15), (3, -16), (5, -17), (7, -18), (9, -19),
    (10, -20), (11, -21), (12, -22), (13, -23),
];

fn tables(unit: TemperatureUnit) -> (&'static [(u8, i64)], &'static [(u8, i64)]) {
    match unit {
        TemperatureUnit::F => (FRIDGE_F, FREEZER_F),
        TemperatureUnit::C => (FRIDGE_C, FREEZER_C),
    }
}

fn lookup(table: &[(u8, i64)], raw: u8) -> Option<i64> {
    table.iter().find(|(c, _)| *c == raw).map(|(_, v)| *v)
}

fn inverse(table: &[(u8, i64)], val: f64) -> Option<u8> {
    table.iter().find(|(_, v)| *v as f64 == val).map(|(c, _)| *c)
}

fn range_ends(table: &[(u8, i64)]) -> (i64, i64) {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for &(_, v) in table {
        min = min.min(v);
        max = max.max(v);
    }
    (min, max)
}

fn smart_grid_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Not In Use"),
        2 => Some("Seasonal Energy Saver"),
        3 => Some("Peak Energy Saver"),
        _ => None,
    }
}

fn build_setting(fields: &[(&'static str, u8)]) -> Vec<u8> {
    let mut status = Status::new();
    status.insert("dispenserMode", 0);
    status.insert("dispenserCapacity", 0);
    status.insert("dispenserUnit", 0);
    status.insert("monDataNumber", 0);
    for &(k, v) in fields {
        status.insert(k, v);
    }
    let mut body = pack_status(&status, SETTING_LEN);
    body[31] = 0;
    let mut cmd = Vec::with_capacity(2 + body.len());
    cmd.extend_from_slice(&[0xf0, 0x17]);
    cmd.extend(body);
    cmd
}

fn components(unit: TemperatureUnit) -> HashMap<String, Value> {
    let (fridge, freezer) = tables(unit);
    let (f_min, f_max) = range_ends(fridge);
    let (z_min, z_max) = range_ends(freezer);
    let uom = if unit == TemperatureUnit::C { "°C" } else { "°F" };
    [
        ("fridge_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-fridge_setpoint","state_topic":"$this/fridge_setpoint","command_topic":"$this/fridge_setpoint/set","name":"Fridge temperature","unit_of_measurement":uom,"min":f_min,"max":f_max})),
        ("freezer_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer temperature","unit_of_measurement":uom,"min":z_min,"max":z_max})),
        ("express_freeze", json!({"platform":"switch","icon":"mdi:snowflake","unique_id":"$deviceid-express_freeze","state_topic":"$this/express_freeze","command_topic":"$this/express_freeze/set","name":"Ice Plus"})),
        ("water_filter", json!({"platform":"binary_sensor","device_class":"problem","entity_category":"diagnostic","icon":"mdi:water-opacity","unique_id":"$deviceid-water_filter","state_topic":"$this/water_filter","name":"Water filter health"})),
        ("water_filter_months", json!({"platform":"sensor","entity_category":"diagnostic","unique_id":"$deviceid-water_filter_months","state_topic":"$this/water_filter_months","name":"Water filter months","icon":"mdi:calendar-clock","unit_of_measurement":"months","suggested_display_precision":0})),
        ("fresh_air_filter_replace", json!({"platform":"binary_sensor","device_class":"problem","entity_category":"diagnostic","icon":"mdi:air-filter","unique_id":"$deviceid-fresh_air_filter_replace","state_topic":"$this/fresh_air_filter_replace","name":"Fresh air filter health"})),
        ("smart_grid", json!({"platform":"select","icon":"mdi:transmission-tower","unique_id":"$deviceid-smart_grid","state_topic":"$this/smart_grid","command_topic":"$this/smart_grid/set","name":"Smart Grid","options":["Not In Use","Seasonal Energy Saver","Peak Energy Saver"]})),
        ("door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door"})),
        ("sabbath_mode", json!({"platform":"switch","icon":"mdi:candle","unique_id":"$deviceid-sabbath_mode","state_topic":"$this/sabbath_mode","command_topic":"$this/sabbath_mode/set","name":"Sabbath Mode"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    meta: Metadata,
    unit: Mutex<Option<TemperatureUnit>>,
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
            meta,
            unit: Mutex::new(None),
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(cur) = process_common_status(inner, 0x10, STATUS_LEN) {
                    dev.process_status(cur);
                }
            }
        }));
        this
    }

    fn set_temperature_unit(&self, unit: TemperatureUnit) {
        {
            let mut cur = self.unit.lock();
            if *cur == Some(unit) {
                return;
            }
            *cur = Some(unit);
        }
        let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        config.components = components(unit);
        self.core.set_config(config);
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
        self.core.publish_property(
            "fridge_setpoint",
            lookup(fridge, status_get(&status, "fridgeSetpoint"))
                .map_or_else(|| "None".into(), PropertyValue::Int),
        );
        self.core.publish_property(
            "freezer_setpoint",
            lookup(freezer, status_get(&status, "freezerSetpoint"))
                .map_or_else(|| "None".into(), PropertyValue::Int),
        );
        self.core.publish_on_off(
            "express_freeze",
            status_get(&status, "expressFreeze") == EXPRESS_ON,
        );
        self.core.publish_on_off(
            "fresh_air_filter_replace",
            status_get(&status, "freshAirFilter") == FRESH_AIR_REPLACE,
        );
        self.core.publish_property(
            "smart_grid",
            smart_grid_name(status_get(&status, "smartSaving"))
                .unwrap_or("None")
                .into(),
        );
        let months = status_get(&status, "waterFilter");
        if months <= WATER_FILTER_MAX {
            self.core
                .publish_on_off("water_filter", months >= WATER_FILTER_REPLACE_FROM);
            self.core
                .publish_property("water_filter_months", PropertyValue::Int(i64::from(months)));
        } else {
            self.core.publish_property("water_filter", "None".into());
            self.core
                .publish_property("water_filter_months", "None".into());
        }
        self.core
            .publish_on_off("door", status_get(&status, "anyDoorOpen") == 1);
        self.core
            .publish_on_off("sabbath_mode", status_get(&status, "sabbathMode") == 1);
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
        let mut fields = Vec::new();
        match prop {
            "fridge_setpoint" | "freezer_setpoint" => {
                let table = if prop == "fridge_setpoint" { fridge } else { freezer };
                let Some(raw) = crate::js::number(value).and_then(|n| inverse(table, n)) else {
                    return;
                };
                fields.push((
                    if prop == "fridge_setpoint" {
                        "fridgeSetpoint"
                    } else {
                        "freezerSetpoint"
                    },
                    raw,
                ));
                fields.push(("tempUnit", u8::from(unit == TemperatureUnit::C)));
            }
            "smart_grid" => {
                let code = match value {
                    "Not In Use" => 0,
                    "Seasonal Energy Saver" => 2,
                    "Peak Energy Saver" => 3,
                    _ => return,
                };
                fields.push(("smartSaving", code));
            }
            "express_freeze" => {
                fields.push((
                    "expressFreeze",
                    if value == "ON" { EXPRESS_ON } else { EXPRESS_OFF },
                ));
            }
            "sabbath_mode" => {
                fields.push(("sabbathMode", u8::from(value == "ON")));
            }
            _ => return,
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_status_and_emits_captured_commands() {
        let meta = Metadata::new("2REF12EJDD__2", "2REF12EJDD__2", "clip_hna_v1.9.230");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode("AA1810EB02080A01020002000000FFFFFFFF00FFFF0085BB"));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("36"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-4"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "smart_grid").as_deref(), Some("Not In Use"));
        assert_eq!(prop(&ha, "water_filter_months").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "water_filter").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode("AA1810EB02080A01020202000000FFFFFFFF01FFFF0086BB"));
        assert_eq!(prop(&ha, "sabbath_mode").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "smart_grid").as_deref(), Some("Seasonal Energy Saver"));

        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "37");
        dev.set_property("freezer_setpoint", "-3");
        dev.set_property("express_freeze", "ON");
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(
            sent,
            vec![
                "AA2FF017FF07FFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF90BB",
                "AA2FF017FFFF09FFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF92BB",
                "AA2FF017FFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFEABB",
            ]
        );
    }
}
