//! 2RES1VE61PFA2 — LG GC-L257SLP side-by-side refrigerator (AABB, 27-byte status).
//!
//! Port of upstream PR #324. Uses `fridge_common` linear temperature formulas,
//! 41-byte `F0 17` writes with fixed zeros at 21/22/23/26/31, per-door `10 A8`
//! events, Express Cool, Express Freeze, Smart Learner, and water filter status.

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
const COMMAND_LENGTH: usize = 41;
const COMMAND_FIXED_OFFSETS: &[usize] = &[21, 22, 23, 26, 31];

fn door_compartment(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("fridge_door"),
        2 => Some("freezer_door"),
        _ => None,
    }
}

fn water_filter_status(months: u8) -> &'static str {
    match months {
        0..=5 => "Good",
        6..=12 => "Replace",
        _ => "None",
    }
}

fn components(unit: TemperatureUnit) -> HashMap<String, Value> {
    let (uom, f_min, f_max) = fridge_range(unit);
    let (_, z_min, z_max) = freezer_range(unit);
    [
        ("fridge_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-fridge_setpoint","state_topic":"$this/fridge_setpoint","command_topic":"$this/fridge_setpoint/set","name":"Fridge temperature","unit_of_measurement":uom,"min":f_min,"max":f_max})),
        ("express_cool", json!({"platform":"switch","unique_id":"$deviceid-express_cool","state_topic":"$this/express_cool","command_topic":"$this/express_cool/set","icon":"mdi:snowflake-variant","name":"Express Cool"})),
        ("freezer_setpoint", json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer temperature","unit_of_measurement":uom,"min":z_min,"max":z_max})),
        ("express_freeze", json!({"platform":"switch","unique_id":"$deviceid-express_freeze","state_topic":"$this/express_freeze","command_topic":"$this/express_freeze/set","icon":"mdi:snowflake","name":"Express Freeze"})),
        ("door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door"})),
        ("fridge_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-fridge_door","state_topic":"$this/fridge_door","name":"Fridge door"})),
        ("freezer_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-freezer_door","state_topic":"$this/freezer_door","name":"Freezer door"})),
        ("smart_learner", json!({"platform":"binary_sensor","unique_id":"$deviceid-smart_learner","state_topic":"$this/smart_learner","name":"Smart Learner","icon":"mdi:brain"})),
        ("water_filter_status", json!({"platform":"sensor","device_class":"enum","entity_category":"diagnostic","unique_id":"$deviceid-water_filter_status","state_topic":"$this/water_filter_status","name":"Water filter","icon":"mdi:water-check","options":["Good","Replace"]})),
        ("water_filter_months", json!({"platform":"sensor","entity_category":"diagnostic","unique_id":"$deviceid-water_filter_months","state_topic":"$this/water_filter_months","name":"Water filter months","icon":"mdi:calendar-clock","unit_of_measurement":"months","suggested_display_precision":0})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    meta: Metadata,
    unit: Mutex<Option<TemperatureUnit>>,
    configured: Mutex<bool>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        core.publish_property("fridge_door", "None".into());
        core.publish_property("freezer_door", "None".into());
        let this = Arc::new(Self {
            core: core.clone(),
            meta,
            unit: Mutex::new(None),
            configured: Mutex::new(false),
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
        *self.configured.lock() = true;
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 4 && buf[0] == 0x10 && buf[1] == 0xa8 {
            if let Some(comp) = door_compartment(buf[2]) {
                if buf[3] == 0 || buf[3] == 1 {
                    self.core.publish_on_off(comp, buf[3] == 1);
                }
            }
            return;
        }
        if let Some(cur) = process_common_status(buf, 0x10, STATUS_LENGTH) {
            self.process_status(cur);
        }
    }

    fn process_status(&self, cur: &[u8]) {
        let s = unpack_status(cur);
        let temp_unit = status_get(&s, "tempUnit");
        if temp_unit == 0 || temp_unit == 1 {
            self.set_temperature_unit(if temp_unit == 1 {
                TemperatureUnit::C
            } else {
                TemperatureUnit::F
            });
        } else {
            *self.unit.lock() = None;
        }

        if *self.configured.lock() {
            let door = status_get(&s, "anyDoorOpen");
            self.core.publish_property(
                "door",
                match door {
                    0 => "OFF",
                    1 => "ON",
                    _ => "None",
                }
                .into(),
            );
            let unit = *self.unit.lock();
            let fridge_val = unit.and_then(|u| {
                let raw = status_get(&s, "fridgeSetpoint");
                if raw == 0xff {
                    return None;
                }
                let v = convert_fridge_temperature(u, i32::from(raw));
                let (_, min, max) = fridge_range(u);
                (v >= min && v <= max).then_some(PropertyValue::Int(v as i64))
            });
            self.core
                .publish_property("fridge_setpoint", fridge_val.unwrap_or_else(|| "None".into()));
            let freezer_val = unit.and_then(|u| {
                let raw = status_get(&s, "freezerSetpoint");
                if raw == 0xff {
                    return None;
                }
                let v = convert_freezer_temperature(u, i32::from(raw));
                let (_, min, max) = freezer_range(u);
                (v >= min && v <= max).then_some(PropertyValue::Int(v as i64))
            });
            self.core.publish_property(
                "freezer_setpoint",
                freezer_val.unwrap_or_else(|| "None".into()),
            );
        }

        if status_get(&s, "anyDoorOpen") == 0 {
            self.core.publish_on_off("fridge_door", false);
            self.core.publish_on_off("freezer_door", false);
        }

        self.core.publish_property(
            "express_cool",
            match status_get(&s, "expressCool") {
                0 => "OFF",
                1 => "ON",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "express_freeze",
            match status_get(&s, "expressFreeze") {
                1 => "OFF",
                2 => "ON",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "smart_learner",
            match status_get(&s, "smartCare") {
                0 => "OFF",
                1 => "ON",
                _ => "None",
            }
            .into(),
        );
        let months = status_get(&s, "waterFilter");
        self.core.publish_property(
            "water_filter_months",
            if months <= 12 {
                PropertyValue::Int(i64::from(months))
            } else {
                "None".into()
            },
        );
        self.core
            .publish_property("water_filter_status", water_filter_status(months).into());
    }

    fn send_setting(&self, fields: &[(&'static str, u8)]) {
        let mut status = Status::new();
        for &(k, v) in fields {
            status.insert(k, v);
        }
        let mut payload = pack_status(&status, COMMAND_LENGTH);
        for &offset in COMMAND_FIXED_OFFSETS {
            payload[offset] = 0;
        }
        let mut cmd = Vec::with_capacity(2 + payload.len());
        cmd.extend_from_slice(&[0xf0, 0x17]);
        cmd.extend(payload);
        self.core.send(&cmd);
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
        match prop {
            "express_cool" if value == "ON" || value == "OFF" => {
                self.send_setting(&[("expressCool", u8::from(value == "ON"))]);
            }
            "express_freeze" if value == "ON" || value == "OFF" => {
                self.send_setting(&[("expressFreeze", if value == "ON" { 2 } else { 1 })]);
            }
            "fridge_setpoint" | "freezer_setpoint" => {
                let Some(unit) = *self.unit.lock() else {
                    return;
                };
                let Ok(temp) = value.parse::<i32>() else {
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
                let raw = if is_fridge {
                    convert_fridge_temperature(unit, temp) as u8
                } else {
                    convert_freezer_temperature(unit, temp) as u8
                };
                let key = if is_fridge {
                    "fridgeSetpoint"
                } else {
                    "freezerSetpoint"
                };
                self.send_setting(&[
                    ("tempUnit", u8::from(unit == TemperatureUnit::C)),
                    (key, raw),
                ]);
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const INITIAL_CLOSED: &str = "aa2110eb0206090107000200010001ffffff00ff0001ffffffffffffff020288bb";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_status_doors_and_commands() {
        let meta = Metadata::new("2RES1VE61PFA2", "2RES1VE61PFA2", "1.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(INITIAL_CLOSED));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-23"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fridge_door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "water_filter_months").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "water_filter_status").as_deref(), Some("Good"));

        thinq.emit_data(&hex_decode("aa0810a8020138bb"));
        assert_eq!(prop(&ha, "freezer_door").as_deref(), Some("ON"));

        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "3");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA2FF017FF05FFFFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF91BB"
        );
    }
}
