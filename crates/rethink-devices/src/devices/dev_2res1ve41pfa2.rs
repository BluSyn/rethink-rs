//! 2RES1VE41PFA2 — LG side-by-side refrigerator (T20_VS4M, AABB, 27-byte status).
//!
//! Port of upstream PR #298. Uses `fridge_common` linear temperature formulas,
//! 41-byte `F0 17` setting writes with zeros at indices 21/22/23/26/31, per-door
//! `10 A8` events, Sabbath mode, and water filter status.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    pack_status, status_get, unpack_status, Status, TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LENGTH: usize = 27;
const SETTING_LENGTH: usize = 41;
const EXPRESS_ON: u8 = 2;
const EXPRESS_OFF: u8 = 1;
const WATER_FILTER_REPLACE_FROM: u8 = 6;
const WATER_FILTER_MAX: u8 = 12;

fn door_compartment(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("fridge_door"),
        2 => Some("freezer_door"),
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
    let mut body = pack_status(&status, SETTING_LENGTH);
    body[31] = 0;
    let mut cmd = Vec::with_capacity(2 + body.len());
    cmd.extend_from_slice(&[0xf0, 0x17]);
    cmd.extend(body);
    cmd
}

fn components(unit: TemperatureUnit) -> HashMap<String, Value> {
    let (uom, f_min, f_max) = fridge_range(unit);
    let (_, z_min, z_max) = freezer_range(unit);
    [
        ("fridge_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-fridge_setpoint","state_topic":"$this/fridge_setpoint","command_topic":"$this/fridge_setpoint/set","name":"Fridge temperature","unit_of_measurement":uom,"min":f_min,"max":f_max})),
        ("freezer_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer temperature","unit_of_measurement":uom,"min":z_min,"max":z_max})),
        ("express_freeze", json!({"platform":"switch","unique_id":"$deviceid-express_freeze","state_topic":"$this/express_freeze","command_topic":"$this/express_freeze/set","icon":"mdi:snowflake","name":"Express Freeze"})),
        ("fridge_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-fridge_door","state_topic":"$this/fridge_door","name":"Fridge door"})),
        ("freezer_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-freezer_door","state_topic":"$this/freezer_door","name":"Freezer door"})),
        ("sabbath", json!({"platform":"binary_sensor","icon":"mdi:candle","unique_id":"$deviceid-sabbath","state_topic":"$this/sabbath","name":"Sabbath mode"})),
        ("water_filter", json!({"platform":"binary_sensor","device_class":"problem","entity_category":"diagnostic","icon":"mdi:water-opacity","unique_id":"$deviceid-water_filter","state_topic":"$this/water_filter","name":"Water filter health"})),
        ("water_filter_months", json!({"platform":"sensor","entity_category":"diagnostic","icon":"mdi:calendar-clock","unique_id":"$deviceid-water_filter_months","state_topic":"$this/water_filter_months","name":"Water filter months","unit_of_measurement":"months","suggested_display_precision":0})),
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
                dev.process_aabb(inner);
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

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&0x10) {
            return;
        }
        if buf.len() == 4 && buf[1] == 0xa8 {
            if let Some(comp) = door_compartment(buf[2]) {
                self.core.publish_on_off(comp, buf[3] == 1);
            }
            return;
        }
        if let Some(cur) = process_common_status(buf, 0x10, STATUS_LENGTH) {
            self.process_status(cur);
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
        self.core.publish_property(
            "fridge_setpoint",
            PropertyValue::Int(convert_fridge_temperature(
                unit,
                i32::from(status_get(&status, "fridgeSetpoint")),
            ) as i64),
        );
        self.core.publish_property(
            "freezer_setpoint",
            PropertyValue::Int(convert_freezer_temperature(
                unit,
                i32::from(status_get(&status, "freezerSetpoint")),
            ) as i64),
        );
        self.core.publish_on_off(
            "express_freeze",
            status_get(&status, "expressFreeze") == EXPRESS_ON,
        );
        self.core
            .publish_on_off("sabbath", status_get(&status, "sabbathMode") == 1);

        let months = status_get(&status, "waterFilter");
        if months <= WATER_FILTER_MAX {
            self.core
                .publish_property("water_filter_months", PropertyValue::Int(i64::from(months)));
            self.core
                .publish_on_off("water_filter", months >= WATER_FILTER_REPLACE_FROM);
        }
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
        let mut fields = Vec::new();
        match prop {
            "fridge_setpoint" | "freezer_setpoint" => {
                let Some(n) = crate::js::number(value) else {
                    return;
                };
                let temp = n as i32;
                if prop == "fridge_setpoint" {
                    fields.push(("fridgeSetpoint", convert_fridge_temperature(unit, temp) as u8));
                } else {
                    fields.push(("freezerSetpoint", convert_freezer_temperature(unit, temp) as u8));
                }
                fields.push(("tempUnit", if unit == TemperatureUnit::C { 1 } else { 0 }));
            }
            "express_freeze" => {
                fields.push((
                    "expressFreeze",
                    if value == "ON" { EXPRESS_ON } else { EXPRESS_OFF },
                ));
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
    const DELTA_FRIDGE_36F: &str = "AA3C10EC0207070107020500000001FFFFFF00FF0001FFFFFFFFFFFFFF01010208070107020500000001FFFFFF00FF0001FFFFFFFFFFFFFF010146BB";
    const DELTA_EXPRESS_ON: &str = "AA3C10EC0207070107020500000001FFFFFF00FF0001FFFFFFFFFFFFFF01010207070207020500000001FFFFFF00FF0001FFFFFFFFFFFFFF010146BB";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_status_and_emits_captured_writes() {
        let meta = Metadata::new("2RES1VE41PFA2", "2RES1VE41PFA2", "1.03");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        dev.start();
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1211010000010400EBBB");

        thinq.emit_data(&hex_decode(DELTA_FRIDGE_36F));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("36"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-1"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "water_filter_months").as_deref(), Some("5"));
        assert_eq!(prop(&ha, "water_filter").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "sabbath").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(DELTA_EXPRESS_ON));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode("AA0810A8010139BB"));
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("ON"));

        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "37");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA2FF017FF07FFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF90BB"
        );
    }
}
