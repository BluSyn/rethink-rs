//! 2RES1VE61NFA2 fridge — STATUS_LENGTH = 27.

use crate::device_trait::DeviceHandler;
use crate::fridge_common::*;
use rethink_util::sync::Mutex;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::json;
use std::sync::Arc;

const STATUS_LENGTH: usize = 27;

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    meta: Metadata,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core,
            temperature_unit: Mutex::new(None),
            meta,
        });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn set_temperature_unit(&self, unit: TemperatureUnit) {
        if *self.temperature_unit.lock() == Some(unit) {
            return;
        }
        *self.temperature_unit.lock() = Some(unit);
        let (fu, fmin, fmax) = fridge_range(unit);
        let (zu, zmin, zmax) = freezer_range(unit);
        let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        config.components = [
            ("fridge_setpoint".into(), json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-fridge_setpoint","state_topic":"$this/fridge_setpoint","command_topic":"$this/fridge_setpoint/set","name":"Fridge temperature","unit_of_measurement":fu,"min":fmin,"max":fmax})),
            ("express_cool".into(), json!({"platform":"switch","unique_id":"$deviceid-express_cool","state_topic":"$this/express_cool","command_topic":"$this/express_cool/set","icon":"mdi:snowflake-variant","name":"Express Cool"})),
            ("freezer_setpoint".into(), json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer temperature","unit_of_measurement":zu,"min":zmin,"max":zmax})),
            ("express_freeze".into(), json!({"platform":"switch","unique_id":"$deviceid-express_freeze","state_topic":"$this/express_freeze","command_topic":"$this/express_freeze/set","icon":"mdi:snowflake","name":"Express Freeze"})),
            ("door".into(), json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door"})),
        ].into_iter().collect();
        self.core.set_config(config);
    }

    fn process_status(&self, cur: &[u8]) {
        let unit = if status_at(cur, ST_TEMP_UNIT) != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        self.core
            .publish_on_off("door", status_at(cur, ST_ANY_DOOR_OPEN) == 1);
        self.core.publish_property(
            "fridge_setpoint",
            PropertyValue::Int(
                convert_fridge_temperature(unit, status_at(cur, ST_FRIDGE_SETPOINT) as i32) as i64,
            ),
        );
        self.core.publish_property(
            "freezer_setpoint",
            PropertyValue::Int(
                convert_freezer_temperature(unit, status_at(cur, ST_FREEZER_SETPOINT) as i32) as i64,
            ),
        );
        self.core
            .publish_on_off("express_cool", status_at(cur, ST_EXPRESS_COOL) == 1);
        self.core
            .publish_on_off("express_freeze", status_at(cur, ST_EXPRESS_FREEZE) == 2);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 2 + STATUS_LENGTH * 2 && buf[0] == 0x10 && buf[1] == 0xec {
            self.process_status(&buf[2 + STATUS_LENGTH..2 + STATUS_LENGTH * 2]);
        }
        if buf.len() == 2 + STATUS_LENGTH && buf[0] == 0x10 && buf[1] == 0xeb {
            self.process_status(&buf[2..2 + STATUS_LENGTH]);
        }
    }

    fn send_setting(&self, setting: &Status) {
        let mut payload = Vec::with_capacity(2 + STATUS_LENGTH);
        payload.extend_from_slice(&CMD_F017);
        payload.extend_from_slice(&pack_status(setting, STATUS_LENGTH));
        self.core.send(&payload);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        crate::devices::washer_ctrl::request_fridge_status(&self.core);
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        let mut setting = Status::new();
        setting.insert("tempUnit", if unit == TemperatureUnit::C { 1 } else { 0 });
        match prop {
            "fridge_setpoint" => {
                let n: i32 = mqtt_value.parse().unwrap_or(0);
                setting.insert("fridgeSetpoint", convert_fridge_temperature(unit, n) as u8);
                self.send_setting(&setting);
            }
            "freezer_setpoint" => {
                let n: i32 = mqtt_value.parse().unwrap_or(0);
                setting.insert("freezerSetpoint", convert_freezer_temperature(unit, n) as u8);
                self.send_setting(&setting);
            }
            "express_cool" => {
                setting.insert("expressCool", if mqtt_value == "ON" { 1 } else { 0 });
                self.send_setting(&setting);
            }
            "express_freeze" => {
                setting.insert("expressFreeze", if mqtt_value == "ON" { 2 } else { 1 });
                self.send_setting(&setting);
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

    const DEVICE_ID: &str = "test-id";
    const SAMPLE_INITIAL: &str = "AA2110EB0202040107000000010001FFFFFF00FF0001FFFFFFFFFFFFFF020085BB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2RES1VE61NFA2", "2RES1VE61NFA2", "1.0");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    #[test]
    fn initial_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        let dev = ha.device(DEVICE_ID).unwrap();
        assert_eq!(
            dev.config.as_ref().unwrap().components["fridge_setpoint"]["unit_of_measurement"],
            json!("°C")
        );
        assert_eq!(
            dev.properties.get("fridge_setpoint").map(|p| p.as_string()),
            Some("6".into())
        );
        assert_eq!(
            dev.properties
                .get("freezer_setpoint")
                .map(|p| p.as_string()),
            Some("-18".into())
        );
        assert_eq!(
            dev.properties.get("door").map(|p| p.as_string()),
            Some("OFF".into())
        );
    }

    // extra fixtures from former integration test
    const SAMPLE_DELTA: &str =
    "AA3C10EC0201040102000001010001FFFFFF00FF0100FFFFFFFFFFFFFF02060202040102000001010001FFFFFF00FF0100FFFFFFFFFFFFFF0206ACBB";
    const SAMPLE_QUIESCENT: &str =
    "AA3C10EC0202040102000000010001FFFFFF00FF0000FFFFFFFFFFFFFF02010202040102000000010001FFFFFF00FF0000FFFFFFFFFFFFFF0202B8BB";
    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }
    #[test]
    fn config_not_until_status() {
        let (ha, _, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn delta_door_express() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "express_cool").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(SAMPLE_QUIESCENT));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_cool").as_deref(), Some("OFF"));
    }

    #[test]
    fn start_and_writes() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1211010000010400EBBB");

        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "4");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[4 + 1], 4);
        assert_eq!(pkt[4 + 8], 1);
        assert_eq!(pkt[4 + 0], 0xff);

        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-20");
        assert_eq!(thinq.outbox()[0][4 + 2], 6);

        thinq.reset_recorder();
        dev.set_property("express_cool", "ON");
        assert_eq!(thinq.outbox()[0][4 + 16], 1);

        thinq.reset_recorder();
        dev.set_property("express_freeze", "ON");
        assert_eq!(thinq.outbox()[0][4 + 3], 2);

        thinq.reset_recorder();
        dev.set_property("does-not-exist", "1");
        assert_eq!(thinq.outbox().len(), 0);
    }

}
