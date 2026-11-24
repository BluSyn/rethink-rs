//! 2REF11EICDX_2 — LG LHFS28XBS refrigerator (AABB, 26-byte status).
//!
//! Port of upstream PR #304. Uses nonlinear Celsius setpoint tables and linear
//! Fahrenheit formulas, 41-byte `F0 17` writes, Craft Ice (`3 balls`/`6 balls`),
//! Ice Plus, Sabbath Mode, Smart Learner, Smart Grid, water filter, and per-door
//! `10 A8` events.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LEN: usize = 26;
const SETTING_TEMPLATE: &str =
    "F017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF";

const FRIDGE_C: &[(u8, i64)] = &[
    (1, 7), (2, 6), (3, 5), (5, 4), (7, 3), (9, 2), (11, 1),
];
const FREEZER_C: &[(u8, i64)] = &[
    (1, -15), (3, -16), (5, -17), (7, -18), (9, -19),
    (10, -20), (11, -21), (12, -22), (13, -23),
];

fn door_prop(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("fridge_door"),
        2 => Some("freezer_door"),
        _ => None,
    }
}

fn craft_ice_name(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("3 balls"),
        2 => Some("6 balls"),
        _ => None,
    }
}

fn smart_grid_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Not In Use"),
        2 => Some("Seasonal Energy Saver"),
        _ => None,
    }
}

fn components(unit: TemperatureUnit) -> HashMap<String, Value> {
    let (uom, f_min, f_max) = fridge_range(unit);
    let (_, z_min, z_max) = freezer_range(unit);
    [
        ("fridge_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-fridge_setpoint","state_topic":"$this/fridge_setpoint","command_topic":"$this/fridge_setpoint/set","name":"Fridge temperature","unit_of_measurement":uom,"min":f_min,"max":f_max,"step":1})),
        ("freezer_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer temperature","unit_of_measurement":uom,"min":z_min,"max":z_max,"step":1})),
        ("express_freeze", json!({"platform":"switch","icon":"mdi:snowflake","unique_id":"$deviceid-express_freeze","state_topic":"$this/express_freeze","command_topic":"$this/express_freeze/set","name":"Ice Plus"})),
        ("craft_ice_mode", json!({"platform":"select","icon":"mdi:snowflake-variant","unique_id":"$deviceid-craft_ice_mode","state_topic":"$this/craft_ice_mode","command_topic":"$this/craft_ice_mode/set","name":"Craft ice","options":["3 balls","6 balls"]})),
        ("sabbath_mode", json!({"platform":"switch","icon":"mdi:candle","unique_id":"$deviceid-sabbath_mode","state_topic":"$this/sabbath_mode","command_topic":"$this/sabbath_mode/set","name":"Sabbath Mode"})),
        ("smart_learner", json!({"platform":"switch","icon":"mdi:brain","unique_id":"$deviceid-smart_learner","state_topic":"$this/smart_learner","command_topic":"$this/smart_learner/set","name":"Smart Learner"})),
        ("smart_grid", json!({"platform":"select","icon":"mdi:transmission-tower","unique_id":"$deviceid-smart_grid","state_topic":"$this/smart_grid","command_topic":"$this/smart_grid/set","name":"Smart Grid","options":["Not In Use","Seasonal Energy Saver"]})),
        ("water_filter", json!({"platform":"binary_sensor","device_class":"problem","entity_category":"diagnostic","icon":"mdi:water-opacity","unique_id":"$deviceid-water_filter","state_topic":"$this/water_filter","name":"Water filter health"})),
        ("water_filter_months", json!({"platform":"sensor","entity_category":"diagnostic","icon":"mdi:calendar-clock","unique_id":"$deviceid-water_filter_months","state_topic":"$this/water_filter_months","name":"Water filter months","unit_of_measurement":"months","suggested_display_precision":0})),
        ("door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Any door"})),
        ("fridge_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-fridge_door","state_topic":"$this/fridge_door","name":"Fridge door"})),
        ("freezer_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-freezer_door","state_topic":"$this/freezer_door","name":"Freezer door"})),
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
        let mut config = default_config(&meta, Some(json!({"name": "LG Fridge"})));
        config.components = components(TemperatureUnit::F);
        core.set_config(config);
        for prop in [
            "fridge_setpoint",
            "freezer_setpoint",
            "express_freeze",
            "craft_ice_mode",
            "sabbath_mode",
            "smart_learner",
            "smart_grid",
            "water_filter",
            "water_filter_months",
            "door",
            "fridge_door",
            "freezer_door",
        ] {
            core.publish_property(prop, "None".into());
        }
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

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&0x10) {
            return;
        }
        if buf.len() == 4 && buf[1] == 0xa8 {
            if let Some(prop) = door_prop(buf[2]) {
                if buf[3] == 0 || buf[3] == 1 {
                    self.core.publish_on_off(prop, buf[3] == 1);
                }
            }
            return;
        }
        if let Some(cur) = process_common_status(buf, 0x10, STATUS_LEN) {
            self.process_status(cur);
        }
    }

    fn process_status(&self, s: &[u8]) {
        let unit = match s[8] {
            0 => Some(TemperatureUnit::F),
            1 => Some(TemperatureUnit::C),
            _ => None,
        };
        {
            let mut cur = self.unit.lock();
            if let Some(u) = unit {
                if *cur != Some(u) {
                    let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
                    config.components = components(u);
                    self.core.set_config(config);
                }
            }
            *cur = unit;
        }
        let fridge = match unit {
            Some(TemperatureUnit::C) => FRIDGE_C.iter().find(|(c, _)| *c == s[1]).map(|(_, v)| *v),
            Some(TemperatureUnit::F) if (1..=11).contains(&s[1]) => {
                Some(convert_fridge_temperature(TemperatureUnit::F, i32::from(s[1])) as i64)
            }
            _ => None,
        };
        let freezer = match unit {
            Some(TemperatureUnit::C) => {
                FREEZER_C.iter().find(|(c, _)| *c == s[2]).map(|(_, v)| *v)
            }
            Some(TemperatureUnit::F) if (1..=13).contains(&s[2]) => {
                Some(convert_freezer_temperature(TemperatureUnit::F, i32::from(s[2])) as i64)
            }
            _ => None,
        };
        self.core.publish_property(
            "fridge_setpoint",
            fridge.map_or_else(|| "None".into(), PropertyValue::Int),
        );
        self.core.publish_property(
            "freezer_setpoint",
            freezer.map_or_else(|| "None".into(), PropertyValue::Int),
        );
        self.core.publish_property(
            "express_freeze",
            match s[3] {
                2 => "ON",
                1 => "OFF",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "craft_ice_mode",
            craft_ice_name(s[25]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "sabbath_mode",
            match s[14] {
                1 => "ON",
                0 => "OFF",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "smart_learner",
            match s[17] {
                1 => "ON",
                0 => "OFF",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "smart_grid",
            smart_grid_name(s[5]).unwrap_or("None").into(),
        );
        if s[6] <= 12 {
            self.core
                .publish_property("water_filter_months", PropertyValue::Int(i64::from(s[6])));
            self.core.publish_on_off("water_filter", s[6] >= 6);
        } else {
            self.core
                .publish_property("water_filter_months", "None".into());
            self.core.publish_property("water_filter", "None".into());
        }
        let door = s[7];
        self.core.publish_property(
            "door",
            match door {
                1 => "ON",
                0 | 2 => "OFF",
                _ => "None",
            }
            .into(),
        );
        if door == 0 || door == 2 {
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
        let mut setting = hex_decode(SETTING_TEMPLATE);
        match prop {
            "fridge_setpoint" | "freezer_setpoint" => {
                let Some(unit) = *self.unit.lock() else {
                    return;
                };
                if value.trim().is_empty() {
                    return;
                }
                let Ok(temp) = value.trim().parse::<i32>() else {
                    return;
                };
                let is_fridge = prop == "fridge_setpoint";
                let (_, min, max) = if is_fridge {
                    fridge_range(unit)
                } else {
                    freezer_range(unit)
                };
                if temp < min || temp > max {
                    return;
                }
                let raw = match unit {
                    TemperatureUnit::C => {
                        let tbl = if is_fridge { FRIDGE_C } else { FREEZER_C };
                        let Some(&(code, _)) = tbl.iter().find(|(_, d)| *d == i64::from(temp))
                        else {
                            return;
                        };
                        code
                    }
                    TemperatureUnit::F => {
                        if is_fridge {
                            convert_fridge_temperature(TemperatureUnit::F, temp) as u8
                        } else {
                            convert_freezer_temperature(TemperatureUnit::F, temp) as u8
                        }
                    }
                };
                setting[2 + if is_fridge { 1 } else { 2 }] = raw;
                setting[2 + 8] = u8::from(unit == TemperatureUnit::C);
            }
            "express_freeze" if value == "ON" || value == "OFF" => {
                setting[2 + 3] = if value == "ON" { 2 } else { 1 };
            }
            "craft_ice_mode" => {
                let code = match value {
                    "3 balls" => 1,
                    "6 balls" => 2,
                    _ => return,
                };
                setting[2 + 25] = code;
            }
            "sabbath_mode" if value == "ON" || value == "OFF" => {
                setting[2 + 14] = u8::from(value == "ON");
            }
            "smart_learner" if value == "ON" || value == "OFF" => {
                setting[2 + 17] = u8::from(value == "ON");
            }
            "smart_grid" => {
                let code = match value {
                    "Not In Use" => 0,
                    "Seasonal Energy Saver" => 2,
                    _ => return,
                };
                setting[2 + 5] = code;
            }
            _ => return,
        }
        self.core.send(&setting);
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
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_celsius_craft_ice_and_commands() {
        let meta = Metadata::new("2REF11EICDX_2", "2REF11EICDX_2", "1.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(
            "aa3a10ec0208060102000100000000ffffff00ffff00ffffffffffffff010209070102000100010000ffffff00ffff00ffffffffffffff01a0bb",
        ));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
        assert_eq!(prop(&ha, "craft_ice_mode").as_deref(), Some("3 balls"));

        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "3");
        dev.set_property("freezer_setpoint", "-23");
        dev.set_property("craft_ice_mode", "6 balls");
        dev.set_property("sabbath_mode", "ON");
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(
            sent,
            vec![
                "AA2FF017FF07FFFFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF93BB",
                "AA2FF017FFFF0DFFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF99BB",
                "AA2FF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FF0200FFFFFFFF00FFFFFFFFFFFFFFFFFFEABB",
                "AA2FF017FFFFFFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFEBBB",
            ]
        );
    }
}
