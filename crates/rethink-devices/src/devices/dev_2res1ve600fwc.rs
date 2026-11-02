//! 2RES1VE600FWC fridge — STATUS_LENGTH = 12, custom temp encoding.

use crate::device_trait::DeviceHandler;
use crate::fridge_common::{
    pack_status, status_at, Status, TemperatureUnit, CMD_F017, ST_ANY_DOOR_OPEN, ST_EXPRESS_FREEZE,
    ST_FREEZER_SETPOINT, ST_FRESH_AIR_FILTER, ST_FRIDGE_SETPOINT, ST_TEMP_UNIT,
};
use rethink_util::sync::Mutex;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::json;
use std::sync::Arc;

const STATUS_LENGTH: usize = 12;
/// 2RES1VE61NFWA (GSX961MCCZ) is the only one of the two models with a Pure N Fresh filter.
const AIR_FILTER_MODEL: &str = "2RES1VE61NFWA";
/// REPLACE, as confirmed on 2RES1VE61NFWA and as used by 2REF11EII__M2.
const FRESH_AIR_REPLACE: u8 = 4;

fn fridge_range(unit: TemperatureUnit) -> (String, i32, i32) {
    match unit {
        TemperatureUnit::F => ("°F".into(), 33, 46),
        TemperatureUnit::C => ("°C".into(), 1, 7),
    }
}

fn freezer_range(unit: TemperatureUnit) -> (String, i32, i32) {
    match unit {
        TemperatureUnit::F => ("°F".into(), -17, 7),
        TemperatureUnit::C => ("°C".into(), -24, -14),
    }
}

fn convert_fridge_temperature(unit: TemperatureUnit, input: i32) -> i32 {
    match unit {
        TemperatureUnit::F => 47 - input,
        TemperatureUnit::C => 8 - input,
    }
}

fn convert_to_freezer_temperature(unit: TemperatureUnit, input: i32) -> i32 {
    match unit {
        TemperatureUnit::F => {
            if input < -15 {
                15
            } else if input < -12 {
                14
            } else if input < -7 {
                13
            } else if input < -3 {
                12
            } else {
                8 - input
            }
        }
        TemperatureUnit::C => -13 - input,
    }
}

fn convert_from_freezer_temperature(unit: TemperatureUnit, input: u8) -> Option<i32> {
    match unit {
        TemperatureUnit::F => {
            let table: [i32; 16] = [7, 7, 6, 5, 4, 3, 2, 1, 0, -1, -2, -3, -7, -12, -15, -17];
            table.get(input as usize).copied()
        }
        TemperatureUnit::C => Some(-13 - input as i32),
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    has_air_filter: bool,
    meta: Metadata,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core,
            temperature_unit: Mutex::new(None),
            has_air_filter: thinq.meta().model_id == AIR_FILTER_MODEL,
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
            ("freezer_setpoint".into(), json!({"platform":"number","device_class":"temperature","unique_id":"$deviceid-freezer_setpoint","state_topic":"$this/freezer_setpoint","command_topic":"$this/freezer_setpoint/set","name":"Freezer temperature","unit_of_measurement":zu,"min":zmin,"max":zmax})),
            ("express_freeze".into(), json!({"platform":"switch","unique_id":"$deviceid-express_freeze","state_topic":"$this/express_freeze","command_topic":"$this/express_freeze/set","icon":"mdi:snowflake","name":"Express Freeze"})),
            ("door".into(), json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door"})),
        ].into_iter().collect();
        if self.has_air_filter {
            config.components.insert(
                "fresh_air_filter_replace".into(),
                json!({
                    "platform": "binary_sensor",
                    "device_class": "problem",
                    "entity_category": "diagnostic",
                    "icon": "mdi:air-filter",
                    "unique_id": "$deviceid-fresh_air_filter_replace",
                    "state_topic": "$this/fresh_air_filter_replace",
                    "name": "Fresh air filter health",
                }),
            );
        }
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
            convert_from_freezer_temperature(unit, status_at(cur, ST_FREEZER_SETPOINT))
                .map_or_else(|| "None".into(), PropertyValue::from),
        );
        self.core
            .publish_on_off("express_freeze", status_at(cur, ST_EXPRESS_FREEZE) == 2);
        if self.has_air_filter {
            self.core.publish_on_off(
                "fresh_air_filter_replace",
                status_at(cur, ST_FRESH_AIR_FILTER) == FRESH_AIR_REPLACE,
            );
        }
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
                setting.insert(
                    "freezerSetpoint",
                    convert_to_freezer_temperature(unit, n) as u8,
                );
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
    use rethink_core::{hex_decode, hex_encode, wrap_aabb, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    // Captures from a GSB470BASZ (issue #44), Celsius: 10EC <12 prev> <12 cur>
    const SAMPLE_DELTA_DOOR_OPEN: &str =
        "AA1E10EC02030501FFFFFF0001FF01FF02030501FFFFFF0101FF01FF80BB";
    const SAMPLE_DELTA_DOOR_CLOSE: &str =
        "AA1E10EC02030501FFFFFF0101FF01FF02030501FFFFFF0001FF01FF80BB";
    const SAMPLE_DELTA_FRIDGE_MINUS_1: &str =
        "AA1E10EC02030501FFFFFF0001FF01FF02040501FFFFFF0001FF01FF80BB";
    const SAMPLE_DELTA_FRIDGE_PLUS_1: &str =
        "AA1E10EC02040501FFFFFF0001FF01FF02030501FFFFFF0001FF01FF80BB";
    const SAMPLE_DELTA_FREEZER_MINUS_1: &str =
        "AA1E10EC02030501FFFFFF0001FF01FF02030601FFFFFF0001FF01FF80BB";
    const SAMPLE_DELTA_FREEZER_PLUS_1: &str =
        "AA1E10EC02030601FFFFFF0001FF01FF02030501FFFFFF0001FF01FF80BB";
    const SAMPLE_DELTA_EXPRESS_FREEZE_ON: &str =
        "AA1E10EC02030501FFFFFF0001FF01FF02030502FFFFFF0001FF01FF80BB";
    const SAMPLE_DELTA_EXPRESS_FREEZE_OFF: &str =
        "AA1E10EC02030502FFFFFF0001FF01FF02030501FFFFFF0001FF01FF80BB";

    // Captures from a 2RES1VE61NFWA (GSX961MCCZ) around a Pure N Fresh filter change.
    const AIR_FILTER_MODEL_ID: &str = "2RES1VE61NFWA";
    // The panel asks for a filter change: cur[4] = 4.
    const SAMPLE_DELTA_AIR_FILTER_REPLACE: &str =
        "AA1E10EC020405010402000001000100020405010402000001000100B9BB";
    // Pure N Fresh held 3 s, the panel clears the icon: cur[4] 4 -> 2.
    const SAMPLE_DELTA_AIR_FILTER_RESET: &str =
        "AA1E10EC020405010402000001000100020405010202000001000100BFBB";

    fn make_model(model_id: &str) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new(model_id, model_id, "1.0");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_model("2RES1VE600FWC")
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?
            .properties
            .get(name)
            .map(|p| p.as_string())
    }

    fn emit(thinq: &MockThinq2Device, hex: &str) {
        thinq.emit_data(&hex_decode(hex));
    }

    /// SAMPLE_DELTA_DOOR_OPEN with the current record in °F and the freezer byte replaced.
    fn fahrenheit_freezer(raw: u8) -> Vec<u8> {
        let frame = hex_decode(SAMPLE_DELTA_DOOR_OPEN);
        let mut inner = frame[2..frame.len() - 2].to_vec();
        let cur = 2 + STATUS_LENGTH;
        inner[cur + ST_TEMP_UNIT] = 0;
        inner[cur + ST_FREEZER_SETPOINT] = raw;
        wrap_aabb(&inner)
    }

    #[test]
    fn gsx961_alias_uses_this_handler() {
        let canonical = crate::t2_factory("2RES1VE600FWC").unwrap();
        let alias = crate::t2_factory("2RES1VE61NFWA").unwrap();
        assert!(std::ptr::fn_addr_eq(canonical, alias));
    }

    #[test]
    fn config_waits_for_a_status_frame() {
        let (ha, _, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn first_delta_publishes_celsius_config_and_setpoints() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°C"));
        assert_eq!(
            comps["freezer_setpoint"]["unit_of_measurement"],
            json!("°C")
        );
        assert!(comps.contains_key("express_freeze"));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("5"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
    }

    #[test]
    fn door_close_publishes_door_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_CLOSE);
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn fridge_5c_to_4c() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_FRIDGE_MINUS_1);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("4"));
    }

    #[test]
    fn fridge_4c_to_5c() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_FRIDGE_PLUS_1);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("5"));
    }

    #[test]
    fn freezer_minus_18c_to_minus_19c() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_FREEZER_MINUS_1);
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-19"));
    }

    #[test]
    fn freezer_minus_19c_to_minus_18c() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_FREEZER_PLUS_1);
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
    }

    #[test]
    fn fahrenheit_freezer_byte_without_a_table_entry_publishes_none() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&fahrenheit_freezer(15));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-17"));
        thinq.emit_data(&fahrenheit_freezer(16));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("None"));
    }

    #[test]
    fn express_freeze_toggle() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_EXPRESS_FREEZE_ON);
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_DELTA_EXPRESS_FREEZE_OFF);
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
    }

    #[test]
    fn frames_outside_the_aa_bb_envelope_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, "001122");
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn unrecognised_inner_shape_is_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, "AA08109901020304BB");
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn start_sends_the_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1211010000010400EBBB"
        );
    }

    #[test]
    fn write_fridge_5c() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "5");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[2], 0xf0);
        assert_eq!(pkt[3], 0x17);
        assert_eq!(pkt[4 + 1], 3);
        assert_eq!(pkt[4 + 8], 1);
    }

    #[test]
    fn write_freezer_minus_19c() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-19");
        assert_eq!(thinq.outbox()[0][4 + 2], 6);
    }

    #[test]
    fn write_express_freeze_on() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        thinq.reset_recorder();
        dev.set_property("express_freeze", "ON");
        assert_eq!(thinq.outbox()[0][4 + 3], 0x02);
    }

    #[test]
    fn write_express_freeze_off() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        thinq.reset_recorder();
        dev.set_property("express_freeze", "OFF");
        assert_eq!(thinq.outbox()[0][4 + 3], 0x01);
    }

    #[test]
    fn write_unknown_property_sends_nothing() {
        let (_, thinq, dev) = make();
        emit(&thinq, SAMPLE_DELTA_DOOR_OPEN);
        thinq.reset_recorder();
        dev.set_property("nonsense", "whatever");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn air_filter_change_request_publishes_replace_on() {
        let (ha, thinq, _) = make_model(AIR_FILTER_MODEL_ID);
        emit(&thinq, SAMPLE_DELTA_AIR_FILTER_REPLACE);
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(
            comps["fresh_air_filter_replace"]["device_class"],
            json!("problem")
        );
        assert_eq!(prop(&ha, "fresh_air_filter_replace").as_deref(), Some("ON"));
    }

    #[test]
    fn air_filter_panel_reset_clears_replace() {
        let (ha, thinq, _) = make_model(AIR_FILTER_MODEL_ID);
        emit(&thinq, SAMPLE_DELTA_AIR_FILTER_REPLACE);
        assert_eq!(prop(&ha, "fresh_air_filter_replace").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_DELTA_AIR_FILTER_RESET);
        assert_eq!(
            prop(&ha, "fresh_air_filter_replace").as_deref(),
            Some("OFF")
        );
    }

    #[test]
    fn air_filter_not_exposed_on_the_filterless_2res1ve600fwc() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELTA_AIR_FILTER_REPLACE);
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert!(!comps.contains_key("fresh_air_filter_replace"));
        assert!(prop(&ha, "fresh_air_filter_replace").is_none());
    }
}
