//! 2REF12EICD__2 — LG French Door refrigerator with Craft Ice (AABB, 26-byte status).
//!
//! Port of upstream PR #273. Uses nonlinear Celsius setpoint tables, 41-byte
//! `F0 17` command frames (zeros at 21/22/23/26/31), Ice Plus, Sabbath Mode,
//! Smart Learner, and Craft Ice (`3 ice` / `6 ice`).

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    freezer_range, fridge_range, status_get, unpack_status, TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LENGTH: usize = 26;
const COMMAND_LENGTH: usize = 41;
const COMMAND_FIXED: &[usize] = &[21, 22, 23, 26, 31];

const FRIDGE_TEMP_C: &[(u8, i64)] = &[
    (1, 7), (2, 6), (3, 5), (5, 4), (7, 3), (9, 2), (11, 1),
];
const FRIDGE_TEMP_F: &[(u8, i64)] = &[
    (1, 43), (2, 42), (3, 41), (4, 40), (5, 39), (6, 38),
    (7, 37), (8, 36), (9, 35), (10, 34), (11, 33),
];
const FREEZER_TEMP_C: &[(u8, i64)] = &[
    (1, -15), (3, -16), (5, -17), (7, -18), (9, -19),
    (10, -20), (11, -21), (12, -22), (13, -23),
];
const FREEZER_TEMP_F: &[(u8, i64)] = &[
    (1, 5), (2, 4), (3, 3), (4, 2), (5, 1), (6, 0), (7, -1),
    (8, -2), (9, -3), (10, -4), (11, -5), (12, -6), (13, -7),
];

fn raw_to_temp(table: &[(u8, i64)], raw: u8) -> i64 {
    table
        .iter()
        .find(|(c, _)| *c == raw)
        .map_or_else(|| i64::from(raw), |(_, v)| *v)
}

fn temp_to_raw(table: &[(u8, i64)], temp: f64) -> u8 {
    table
        .iter()
        .find(|(_, v)| *v as f64 == temp)
        .map_or(0xff, |(c, _)| *c)
}

fn components(unit: TemperatureUnit) -> HashMap<String, Value> {
    let (uom, f_min, f_max) = fridge_range(unit);
    let (_, z_min, z_max) = freezer_range(unit);
    [
        ("fridge_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-fridge_setpoint","state_topic":"$this/fridge_setpoint","command_topic":"$this/fridge_setpoint/set","name":"Fridge temperature","unit_of_measurement":uom,"min":f_min,"max":f_max})),
        ("freezer_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer temperature","unit_of_measurement":uom,"min":z_min,"max":z_max})),
        ("door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door"})),
        ("ice_plus", json!({"platform":"switch","icon":"mdi:snowflake","unique_id":"$deviceid-ice_plus","state_topic":"$this/ice_plus","command_topic":"$this/ice_plus/set","name":"Ice Plus","payload_on":"ON","payload_off":"OFF"})),
        ("sabbath_mode", json!({"platform":"switch","icon":"mdi:candle","unique_id":"$deviceid-sabbath_mode","state_topic":"$this/sabbath_mode","command_topic":"$this/sabbath_mode/set","name":"Sabbath Mode","payload_on":"ON","payload_off":"OFF"})),
        ("smart_learner", json!({"platform":"switch","icon":"mdi:brain","unique_id":"$deviceid-smart_learner","state_topic":"$this/smart_learner","command_topic":"$this/smart_learner/set","name":"Smart Learner","payload_on":"ON","payload_off":"OFF"})),
        ("craft_ice", json!({"platform":"select","icon":"mdi:ice-pop","unique_id":"$deviceid-craft_ice","state_topic":"$this/craft_ice","command_topic":"$this/craft_ice/set","name":"Craft Ice Mode","options":["3 ice","6 ice"]})),
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
                if let Some(cur) = process_common_status(inner, 0x10, STATUS_LENGTH) {
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
        let (fridge_tbl, freezer_tbl) = if unit == TemperatureUnit::C {
            (FRIDGE_TEMP_C, FREEZER_TEMP_C)
        } else {
            (FRIDGE_TEMP_F, FREEZER_TEMP_F)
        };
        self.core.publish_property(
            "fridge_setpoint",
            PropertyValue::Int(raw_to_temp(fridge_tbl, status_get(&status, "fridgeSetpoint"))),
        );
        self.core.publish_property(
            "freezer_setpoint",
            PropertyValue::Int(raw_to_temp(freezer_tbl, status_get(&status, "freezerSetpoint"))),
        );
        self.core
            .publish_on_off("door", status_get(&status, "anyDoorOpen") == 1);
        self.core
            .publish_on_off("ice_plus", status_get(&status, "expressFreeze") == 2);
        self.core
            .publish_on_off("sabbath_mode", status_get(&status, "sabbathMode") == 1);
        self.core
            .publish_on_off("smart_learner", status_get(&status, "smartCare") == 1);
        let craft = match status_get(&status, "craftIce") {
            2 => "6 ice",
            1 => "3 ice",
            _ => "off",
        };
        self.core.publish_property("craft_ice", craft.into());
    }

    fn command(&self, fields: &[(usize, u8)]) {
        let mut message = vec![0xffu8; 2 + COMMAND_LENGTH];
        message[0] = 0xf0;
        message[1] = 0x17;
        for &idx in COMMAND_FIXED {
            message[2 + idx] = 0x00;
        }
        for &(idx, val) in fields {
            message[2 + idx] = val;
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
    fn set_property(&self, prop: &str, value: &str) {
        let unit = self.unit.lock().unwrap_or(TemperatureUnit::F);
        let unit_byte = u8::from(unit == TemperatureUnit::C);
        match prop {
            "fridge_setpoint" => {
                let raw = crate::js::number(value).map_or(0xff, |n| {
                    temp_to_raw(
                        if unit == TemperatureUnit::C {
                            FRIDGE_TEMP_C
                        } else {
                            FRIDGE_TEMP_F
                        },
                        n,
                    )
                });
                self.command(&[(1, raw), (8, unit_byte)]);
            }
            "freezer_setpoint" => {
                let raw = crate::js::number(value).map_or(0xff, |n| {
                    temp_to_raw(
                        if unit == TemperatureUnit::C {
                            FREEZER_TEMP_C
                        } else {
                            FREEZER_TEMP_F
                        },
                        n,
                    )
                });
                self.command(&[(2, raw), (8, unit_byte)]);
            }
            "ice_plus" => self.command(&[(3, if value == "ON" { 2 } else { 1 })]),
            "sabbath_mode" => self.command(&[(14, u8::from(value == "ON"))]),
            "smart_learner" => self.command(&[(17, u8::from(value == "ON"))]),
            "craft_ice" => self.command(&[(25, if value == "6 ice" { 2 } else { 1 })]),
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
    use rethink_core::{hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_26_byte_celsius_status_and_writes_41_byte_f017() {
        let meta = Metadata::new("2REF12EICD__2", "2REF12EICD__2", "1.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        let mut payload = vec![0x10, 0xeb];
        let mut status = vec![0xffu8; STATUS_LENGTH];
        status[0] = 2;
        status[1] = 5; // 4°C
        status[2] = 3; // -16°C (non-linear!)
        status[3] = 2; // ice_plus ON
        status[7] = 1; // door open
        status[8] = 1; // Celsius
        status[14] = 0; // sabbath OFF
        status[17] = 1; // smart learner ON
        status[25] = 2; // 6 ice
        payload.extend_from_slice(&status);
        thinq.emit_data(&wrap_aabb(&payload));

        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-16"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "ice_plus").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "smart_learner").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "craft_ice").as_deref(), Some("6 ice"));

        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-18");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA2FF017FFFF07FFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF93BB"
        );
    }
}
