//! 2REF11EBIVPC4 fridge — always Celsius, 43-byte status, Shabbat mode.

use crate::device_trait::DeviceHandler;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::json;
use std::sync::Arc;

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        let (fridge_uom, fridge_min, fridge_max) = fridge_range(crate::fridge_common::TemperatureUnit::C);
        let (freezer_uom, freezer_min, freezer_max) = freezer_range(crate::fridge_common::TemperatureUnit::C);
        let mut config = default_config(&meta, Some(json!({"name": "LG Fridge"})));
        config.components = [
            (
                "fridge_setpoint".into(),
                json!({
                    "platform": "number", "device_class": "temperature",
                    "unique_id": "$deviceid-fridge_setpoint",
                    "state_topic": "$this/fridge_setpoint",
                    "command_topic": "$this/fridge_setpoint/set",
                    "name": "Fridge temperature",
                    "unit_of_measurement": fridge_uom, "min": fridge_min, "max": fridge_max,
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
                    "unit_of_measurement": freezer_uom, "min": freezer_min, "max": freezer_max,
                }),
            ),
            (
                "door".into(),
                json!({
                    "platform": "binary_sensor", "device_class": "door",
                    "unique_id": "$deviceid-door", "state_topic": "$this/door", "name": "Door",
                }),
            ),
            (
                "express_freeze".into(),
                json!({
                    "platform": "switch", "icon": "mdi:snowflake",
                    "unique_id": "$deviceid-express_freeze",
                    "state_topic": "$this/express_freeze",
                    "command_topic": "$this/express_freeze/set",
                    "name": "Express Freeze", "payload_on": "ON", "payload_off": "OFF",
                }),
            ),
            (
                "shabbat_mode".into(),
                json!({
                    "platform": "switch", "icon": "mdi:candle",
                    "unique_id": "$deviceid-shabbat_mode",
                    "state_topic": "$this/shabbat_mode",
                    "command_topic": "$this/shabbat_mode/set",
                    "name": "Shabbat Mode", "payload_on": "ON", "payload_off": "OFF",
                }),
            ),
        ]
        .into_iter()
        .collect();
        // Door binary_sensor above — automate on state.
        core.set_config(config);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_status(&self, cur: &[u8]) {
        if cur.len() < 15 {
            return;
        }
        // Direct formulas for raw->C (not convert functions)
        let setpoint_fridge = (13.0 - cur[1] as f64) / 2.0;
        let setpoint_freezer = -((cur[2] as f64) + 29.0) / 2.0;
        let any_door_open = cur[7] == 1;
        let express_freeze_on = cur[3] == 2;
        let shabbat_on = cur[14] == 1;

        self.core.publish_door_with_trigger(any_door_open);
        // Prefer integer when whole number
        self.core.publish_property(
            "fridge_setpoint",
            if setpoint_fridge == setpoint_fridge.trunc() {
                PropertyValue::Int(setpoint_fridge as i64)
            } else {
                PropertyValue::Num(setpoint_fridge)
            },
        );
        self.core.publish_property(
            "freezer_setpoint",
            if setpoint_freezer == setpoint_freezer.trunc() {
                PropertyValue::Int(setpoint_freezer as i64)
            } else {
                PropertyValue::Num(setpoint_freezer)
            },
        );
        self.core.publish_on_off("express_freeze", express_freeze_on);
        self.core.publish_on_off("shabbat_mode", shabbat_on);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 2 + 43 * 2 && buf[0] == 0x10 && buf[1] == 0xec {
            self.process_status(&buf[2 + 43..2 + 43 + 43]);
        }
        if buf.len() == 2 + 43 && buf[0] == 0x10 && buf[1] == 0xeb {
            self.process_status(&buf[2..2 + 43]);
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {}
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, mqtt_value: &str) {
        let mut base = rethink_util::hex::decode(
            "F017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        )
        .unwrap();
        match prop {
            "fridge_setpoint" => {
                let n: i32 = mqtt_value.parse().unwrap_or(0);
                base[2 + 1] = convert_fridge_temperature(crate::fridge_common::TemperatureUnit::C, n) as u8;
                base[2 + 8] = 1;
                self.core.send(&base);
            }
            "freezer_setpoint" => {
                let n: i32 = mqtt_value.parse().unwrap_or(0);
                base[2 + 2] = convert_freezer_temperature(crate::fridge_common::TemperatureUnit::C, n) as u8;
                base[2 + 8] = 1;
                self.core.send(&base);
            }
            "express_freeze" => {
                base[2 + 3] = if mqtt_value == "ON" { 2 } else { 1 };
                self.core.send(&base);
            }
            "shabbat_mode" => {
                base[2 + 14] = if mqtt_value == "ON" { 1 } else { 0 };
                self.core.send(&base);
            }
            _ => eprintln!("Unknown property {prop}"),
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";

    fn status_baseline() -> String {
        format!("{}{}", "02070701FFFFFF00FFFFFFFFFFFF00", "FF".repeat(28))
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2REF11EBIVPC4", "2REF11EBIVPC4", "1.0");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    #[test]
    fn config_at_construction_celsius() {
        let (ha, _, _) = make();
        let dev = ha.device(DEVICE_ID).unwrap();
        let comps = &dev.config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°C"));
        assert_eq!(comps["fridge_setpoint"]["min"], json!(1));
        assert_eq!(comps["fridge_setpoint"]["max"], json!(7));
        assert_eq!(comps["freezer_setpoint"]["min"], json!(-23));
        assert!(comps.contains_key("express_freeze"));
        assert!(comps.contains_key("shabbat_mode"));
        assert!(!comps.contains_key("flex_setpoint"));
    }

    #[test]
    fn initial_status_decodes() {
        let (ha, thinq, _) = make();
        let pkt = format!("AA3110EB{}00BB", status_baseline());
        thinq.emit_data(&hex_decode(&pkt));
        let props = ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(props.get("fridge_setpoint").map(|p| p.as_string()).as_deref(), Some("3"));
        assert_eq!(props.get("freezer_setpoint").map(|p| p.as_string()).as_deref(), Some("-18"));
        assert_eq!(props.get("door").map(|p| p.as_string()).as_deref(), Some("OFF"));
        assert_eq!(props.get("express_freeze").map(|p| p.as_string()).as_deref(), Some("OFF"));
        assert_eq!(props.get("shabbat_mode").map(|p| p.as_string()).as_deref(), Some("OFF"));
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }
    #[test]
    fn config_immediate_celsius() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], "°C");
        assert_eq!(comps["fridge_setpoint"]["min"], 1);
        assert_eq!(comps["freezer_setpoint"]["min"], -23);
        assert!(comps.contains_key("express_freeze"));
        assert!(comps.contains_key("shabbat_mode"));
        assert!(!comps.contains_key("flex_setpoint"));
    }

    #[test]
    fn decode_status() {
        let (ha, thinq, _) = make();
        let pkt = format!("AA3110EB{}00BB", status_baseline());
        thinq.emit_data(&hex_decode(&pkt));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "shabbat_mode").as_deref(), Some("OFF"));

        // door open delta
        let cur = format!(
            "{}{}",
            "02070701FFFFFF01FFFFFFFFFFFF00",
            "FF".repeat(28)
        );
        let pkt = format!("AA5C10EC{}{}00BB", status_baseline(), cur);
        thinq.emit_data(&hex_decode(&pkt));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
    }

    #[test]
    fn writes_and_start() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 0);

        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "5");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[2], 0xf0);
        assert_eq!(pkt[3], 0x17);
        assert_eq!(pkt[5], 3);
        assert_eq!(pkt[12], 1);

        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-20");
        assert_eq!(thinq.outbox()[0][6], 6);
        assert_eq!(thinq.outbox()[0][12], 1);

        thinq.reset_recorder();
        dev.set_property("express_freeze", "ON");
        assert_eq!(thinq.outbox()[0][7], 2);
        thinq.reset_recorder();
        dev.set_property("express_freeze", "OFF");
        assert_eq!(thinq.outbox()[0][7], 1);

        thinq.reset_recorder();
        dev.set_property("shabbat_mode", "ON");
        assert_eq!(thinq.outbox()[0][18], 1);
        thinq.reset_recorder();
        dev.set_property("shabbat_mode", "OFF");
        assert_eq!(thinq.outbox()[0][18], 0);

        thinq.reset_recorder();
        dev.set_property("does-not-exist", "1");
        assert_eq!(thinq.outbox().len(), 0);
    }

}
