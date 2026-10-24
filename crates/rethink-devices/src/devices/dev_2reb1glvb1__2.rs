//! 2REB1GLVB1__2 fridge (AABB, STATUS_LENGTH=17).

use crate::device_trait::DeviceHandler;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    pack_status, status_at, Status, TemperatureUnit, CMD_F017, ST_ANY_DOOR_OPEN, ST_EXPRESS_COOL,
    ST_EXPRESS_FREEZE, ST_FREEZER_SETPOINT, ST_FRIDGE_SETPOINT, ST_TEMP_UNIT,
};
use rethink_util::sync::Mutex;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::sync::Arc;

const STATUS_LENGTH: usize = 17;
const ECO_FRIENDLY: usize = 12;
const APP_COMMAND_LENGTH: usize = 41;

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    meta: Metadata,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
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
        {
            let mut cur = self.temperature_unit.lock();
            if *cur == Some(unit) {
                return;
            }
            *cur = Some(unit);
        }
        let (fu, fmin, fmax) = fridge_range(unit);
        let (zu, zmin, zmax) = freezer_range(unit);
        let mut base = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        let mut components = Map::new();
        components.insert(
            "fridge_setpoint".into(),
            json!({
                "platform": "number",
                "device_class": "temperature",
                "unique_id": "$deviceid-fridge_setpoint",
                "state_topic": "$this/fridge_setpoint",
                "command_topic": "$this/fridge_setpoint/set",
                "name": "Fridge temperature",
                "unit_of_measurement": fu,
                "min": fmin,
                "max": fmax,
            }),
        );
        components.insert(
            "express_cool".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-express_cool",
                "state_topic": "$this/express_cool",
                "command_topic": "$this/express_cool/set",
                "icon": "mdi:snowflake-variant",
                "name": "Express Cool",
            }),
        );
        components.insert(
            "freezer_setpoint".into(),
            json!({
                "platform": "number",
                "device_class": "temperature",
                "unique_id": "$deviceid-freezer_setpoint",
                "state_topic": "$this/freezer_setpoint",
                "command_topic": "$this/freezer_setpoint/set",
                "name": "Freezer temperature",
                "unit_of_measurement": zu,
                "min": zmin,
                "max": zmax,
            }),
        );
        components.insert(
            "express_freeze".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-express_freeze",
                "state_topic": "$this/express_freeze",
                "command_topic": "$this/express_freeze/set",
                "icon": "mdi:snowflake",
                "name": "Express Freeze",
            }),
        );
        components.insert(
            "eco_friendly".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-eco_friendly",
                "state_topic": "$this/eco_friendly",
                "command_topic": "$this/eco_friendly/set",
                "icon": "mdi:leaf",
                "name": "Eco Friendly",
            }),
        );
        components.insert(
            "door".into(),
            json!({
                "platform": "binary_sensor",
                "device_class": "door",
                "unique_id": "$deviceid-door",
                "state_topic": "$this/door",
                "name": "Door",
            }),
        );
        base.components = components.into_iter().collect();
        self.core.set_config(base);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, 0x10, STATUS_LENGTH) {
            self.process_status(rec);
        }
    }

    fn process_status(&self, cur_status: &[u8]) {
        let unit = if status_at(cur_status, ST_TEMP_UNIT) != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        self.core
            .publish_on_off("door", status_at(cur_status, ST_ANY_DOOR_OPEN) == 1);
        self.core.publish_property(
            "fridge_setpoint",
            convert_fridge_temperature(unit, status_at(cur_status, ST_FRIDGE_SETPOINT) as i32).into(),
        );
        self.core.publish_property(
            "freezer_setpoint",
            convert_freezer_temperature(unit, status_at(cur_status, ST_FREEZER_SETPOINT) as i32)
                .into(),
        );
        self.core
            .publish_on_off("express_cool", status_at(cur_status, ST_EXPRESS_COOL) == 1);
        self.core.publish_on_off(
            "express_freeze",
            status_at(cur_status, ST_EXPRESS_FREEZE) == 2,
        );
        self.core
            .publish_on_off("eco_friendly", status_at(cur_status, ECO_FRIENDLY) == 1);
    }

    /// LG app F017 for Eco Friendly: 41 bytes of 0xFF except the eco byte and a few fixed zeros.
    fn app_command(&self, eco: u8) {
        let mut payload = vec![0xffu8; APP_COMMAND_LENGTH];
        payload[ECO_FRIENDLY] = eco;
        for index in [21usize, 22, 23, 26, 31] {
            payload[index] = 0;
        }
        let mut pkt = Vec::with_capacity(2 + APP_COMMAND_LENGTH);
        pkt.extend_from_slice(&CMD_F017);
        pkt.extend(payload);
        self.core.send(&pkt);
    }

    fn send_setting(&self, setting: &Status) {
        let mut pkt = Vec::with_capacity(2 + STATUS_LENGTH);
        pkt.extend_from_slice(&CMD_F017);
        pkt.extend(pack_status(setting, STATUS_LENGTH));
        self.core.send(&pkt);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        let mut setting = Status::new();
        setting.insert("tempUnit", if unit == TemperatureUnit::C { 1 } else { 0 });

        if prop == "fridge_setpoint" {
            let v: i32 = mqtt_value.parse().unwrap_or(0);
            setting.insert(
                "fridgeSetpoint",
                convert_fridge_temperature(unit, v) as u8,
            );
            self.send_setting(&setting);
        } else if prop == "freezer_setpoint" {
            let v: i32 = mqtt_value.parse().unwrap_or(0);
            setting.insert(
                "freezerSetpoint",
                convert_freezer_temperature(unit, v) as u8,
            );
            self.send_setting(&setting);
        } else if prop == "express_cool" {
            setting.insert("expressCool", if mqtt_value == "ON" { 1 } else { 0 });
            self.send_setting(&setting);
        } else if prop == "express_freeze" {
            setting.insert("expressFreeze", if mqtt_value == "ON" { 2 } else { 1 });
            self.send_setting(&setting);
        } else if prop == "eco_friendly" {
            self.app_command(if mqtt_value == "ON" { 1 } else { 0 });
        }
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
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
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
    use rethink_core::{Thinq2Device, hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Metadata, PropertyValue};
    use crate::device_trait::DeviceHandler;

    const DEVICE_ID: &str = "test-id";
    const SAMPLE_INITIAL: &str = "AA1710EB020504010000000201000100000000000099BB";

    fn meta() -> Metadata {
        Metadata::new("2REB1GLVB1__2", "2REB1GLVB1__2", "1.0")
    }

    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn config_not_published_until_status() {
        let (ha, _, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn initial_status_celsius() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        let dev = ha.device(DEVICE_ID).unwrap();
        let comps = &dev.config.as_ref().unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], "°C");
        assert_eq!(comps["fridge_setpoint"]["min"], 1);
        assert_eq!(comps["fridge_setpoint"]["max"], 7);
        assert_eq!(comps["freezer_setpoint"]["min"], -23);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_cool").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
    }

    #[test]
    fn ignores_bad_frames() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("001122"));
        assert!(ha.device(DEVICE_ID).is_none());
        thinq.emit_data(&hex_decode("AA08109901020304BB"));
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn start_sends_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1211010000010400EBBB");
    }

    #[test]
    fn ha_writes() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        thinq.reset_recorder();

        dev.set_property("fridge_setpoint", "4");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[2], 0xf0);
        assert_eq!(pkt[3], 0x17);
        assert_eq!(pkt[4 + 1], 4);
        assert_eq!(pkt[4 + 8], 1);
        assert_eq!(pkt[4 + 2], 0xff);

        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-20");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[4 + 2], 6);
        assert_eq!(pkt[4 + 1], 0xff);

        thinq.reset_recorder();
        dev.set_property("express_cool", "ON");
        assert_eq!(thinq.outbox()[0][4 + 16], 1);

        thinq.reset_recorder();
        dev.set_property("express_freeze", "ON");
        assert_eq!(thinq.outbox()[0][4 + 3], 2);

        thinq.reset_recorder();
        dev.set_property("nonsense", "whatever");
        assert_eq!(thinq.outbox().len(), 0);
    }

    #[test]
    fn eco_friendly_reads_the_current_record_and_writes_the_app_command() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(
            "AA2810EC02070401000000020100010000000000000207040100000002010001000100000000A6BB",
        ));
        assert_eq!(prop(&ha, "eco_friendly").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(
            "AA2810EC02070401000000020100010001000000000207040100000002010001000000000000A6BB",
        ));
        assert_eq!(prop(&ha, "eco_friendly").as_deref(), Some("OFF"));

        thinq.reset_recorder();
        dev.set_property("eco_friendly", "ON");
        dev.set_property("eco_friendly", "OFF");
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(
            sent,
            vec![
                "AA2FF017FFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFEBBB"
                    .to_string(),
                "AA2FF017FFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFFE8BB"
                    .to_string(),
            ]
        );
    }

    // silence unused
    #[allow(dead_code)]
    fn _pv() -> PropertyValue {
        PropertyValue::Int(0)
    }
}
