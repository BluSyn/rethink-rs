//! 2REFTBDII4P_U refrigerator. 68-byte AABB status.
//!
//! F017 writes start from a captured base frame. `0xFF` means leave unchanged.
//! Temperature writes also set the unit byte.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const STATUS_LENGTH: usize = 68;

const BASE_SETTING: &str = "F017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";

const ICE_MAKER: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Making ice"),
    (2, "Full"),
    (3, "Quiet mode"),
];
const TEMPERATURE_STATUS: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Well maintained"),
    (2, "Very high"),
    (3, "High"),
    (4, "Low"),
    (5, "Stabilizing"),
    (14, "Sensor error"),
    (15, "Uncertain"),
];

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or("None")
}

fn fresh_air_filter(code: u8) -> &'static str {
    match code {
        0 | 1 | 2 | 4 | 5 | 6 => "Good",
        3 => "Replace",
        _ => "None",
    }
}

fn water_filter(code: u8) -> &'static str {
    match code {
        0..=5 => "Good",
        6..=12 => "Replace",
        _ => "None",
    }
}

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
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
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
        let temp_options = labels(TEMPERATURE_STATUS);
        let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        config.components = [
            (
                "fridge_setpoint".into(),
                json!({
                    "platform": "number", "device_class": "temperature",
                    "unique_id": "$deviceid-fridge_setpoint",
                    "state_topic": "$this/fridge_setpoint",
                    "command_topic": "$this/fridge_setpoint/set",
                    "name": "Fridge temperature",
                    "unit_of_measurement": fu, "min": fmin, "max": fmax,
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
                    "unit_of_measurement": zu, "min": zmin, "max": zmax,
                }),
            ),
            (
                "fridge_temperature_status".into(),
                json!({
                    "platform": "sensor", "device_class": "enum", "entity_category": "diagnostic",
                    "unique_id": "$deviceid-fridge_temperature_status",
                    "state_topic": "$this/fridge_temperature_status",
                    "name": "Fridge temperature status",
                    "icon": "mdi:thermometer-check",
                    "options": temp_options,
                }),
            ),
            (
                "freezer_temperature_status".into(),
                json!({
                    "platform": "sensor", "device_class": "enum", "entity_category": "diagnostic",
                    "unique_id": "$deviceid-freezer_temperature_status",
                    "state_topic": "$this/freezer_temperature_status",
                    "name": "Freezer temperature status",
                    "icon": "mdi:thermometer-check",
                    "options": temp_options,
                }),
            ),
            (
                "door".into(),
                json!({
                    "platform": "binary_sensor", "device_class": "door",
                    "unique_id": "$deviceid-door",
                    "state_topic": "$this/door",
                    "name": "Door",
                }),
            ),
            (
                "express_freeze".into(),
                json!({
                    "platform": "switch", "icon": "mdi:snowflake",
                    "unique_id": "$deviceid-express_freeze",
                    "state_topic": "$this/express_freeze",
                    "command_topic": "$this/express_freeze/set",
                    "name": "Express Freeze",
                    "payload_on": "ON", "payload_off": "OFF",
                }),
            ),
            (
                "ice_maker_status".into(),
                json!({
                    "platform": "sensor", "device_class": "enum",
                    "unique_id": "$deviceid-ice_maker_status",
                    "state_topic": "$this/ice_maker_status",
                    "name": "Ice maker status",
                    "icon": "mdi:ice-pop",
                    "options": labels(ICE_MAKER),
                }),
            ),
            (
                "fresh_air_filter_status".into(),
                json!({
                    "platform": "sensor", "device_class": "enum", "entity_category": "diagnostic",
                    "unique_id": "$deviceid-fresh_air_filter_status",
                    "state_topic": "$this/fresh_air_filter_status",
                    "name": "Fresh air filter",
                    "icon": "mdi:air-filter",
                    "options": ["Good", "Replace"],
                }),
            ),
            (
                "water_filter_status".into(),
                json!({
                    "platform": "sensor", "device_class": "enum", "entity_category": "diagnostic",
                    "unique_id": "$deviceid-water_filter_status",
                    "state_topic": "$this/water_filter_status",
                    "name": "Water filter",
                    "icon": "mdi:water-check",
                    "options": ["Good", "Replace"],
                }),
            ),
            (
                "water_filter_months".into(),
                json!({
                    "platform": "sensor", "entity_category": "diagnostic",
                    "unique_id": "$deviceid-water_filter_months",
                    "state_topic": "$this/water_filter_months",
                    "name": "Water filter months",
                    "icon": "mdi:calendar-clock",
                    "unit_of_measurement": "months",
                    "suggested_display_precision": 0,
                }),
            ),
        ]
        .into_iter()
        .collect();
        self.core.set_config(config);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, 0x10, STATUS_LENGTH) {
            self.process_status(rec);
        }
    }

    fn process_status(&self, status: &[u8]) {
        if status.len() < STATUS_LENGTH {
            return;
        }
        let unit = if status[8] != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        self.core.publish_property(
            "fridge_setpoint",
            convert_fridge_temperature(unit, i32::from(status[1])).into(),
        );
        self.core.publish_property(
            "freezer_setpoint",
            convert_freezer_temperature(unit, i32::from(status[2])).into(),
        );
        self.core.publish_on_off("express_freeze", status[3] == 2);
        self.core
            .publish_property("fresh_air_filter_status", fresh_air_filter(status[4]).into());
        self.core
            .publish_property("water_filter_status", water_filter(status[6]).into());
        if status[6] <= 12 {
            self.core
                .publish_property("water_filter_months", i64::from(status[6]).into());
        } else {
            self.core.publish_property("water_filter_months", "None".into());
        }
        self.core.publish_on_off("door", status[7] == 1);
        self.core.publish_property(
            "fridge_temperature_status",
            named(TEMPERATURE_STATUS, status[27]).into(),
        );
        self.core.publish_property(
            "freezer_temperature_status",
            named(TEMPERATURE_STATUS, status[28]).into(),
        );
        self.core
            .publish_property("ice_maker_status", named(ICE_MAKER, status[32]).into());
    }

    fn send_setting(&self, edit: impl FnOnce(&mut [u8])) {
        let mut setting = hex_decode(BASE_SETTING);
        edit(&mut setting);
        self.core.send(&setting);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        let unit_byte = if unit == TemperatureUnit::C { 1 } else { 0 };
        match prop {
            "fridge_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                let raw = convert_fridge_temperature(unit, display) as u8;
                self.send_setting(|setting| {
                    setting[2 + 1] = raw;
                    setting[2 + 8] = unit_byte;
                });
            }
            "freezer_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                let raw = convert_freezer_temperature(unit, display) as u8;
                self.send_setting(|setting| {
                    setting[2 + 2] = raw;
                    setting[2 + 8] = unit_byte;
                });
            }
            "express_freeze" => {
                let raw = if mqtt_value == "ON" { 2 } else { 1 };
                self.send_setting(|setting| setting[2 + 3] = raw);
            }
            _ => {}
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
    use rethink_core::device_base::wrap_aabb;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const DEVICE_ID: &str = "test-id";
    const DOOR_OPEN: &str = "AA8E10EC0207040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207040102FF0201010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000EABB";
    const DOOR_CLOSED: &str = "AA8E10EC0207040102FF0201010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000EABB";
    const EXPRESS_ON: &str = "AA8E10EC0207040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207040202FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000EABB";
    const FRIDGE_2C: &str = "AA8E10EC0207040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000206040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000E8BB";
    const FREEZER_M17: &str = "AA8E10EC0207040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207030102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000E8BB";
    const ICE_FULL: &str = "AA8E10EC0207040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000001FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207040102FF0200010001FFFFFFFFFFFF00FFFFFFFFFFFFFFFF620101FF000002FFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF0000EABB";

    fn meta() -> Metadata {
        Metadata::new("2REFTBDII4P_U", "2REFTBDII4P_U", "1.0")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn config_waits_for_a_status_packet() {
        let (ha, _, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn door_open_publishes_the_celsius_state() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(DOOR_OPEN));
        let info = ha.device(DEVICE_ID).unwrap();
        let comps = &info.config.as_ref().unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], "°C");
        assert_eq!(comps["fridge_setpoint"]["min"], 1);
        assert_eq!(comps["fridge_setpoint"]["max"], 7);
        assert_eq!(comps["freezer_setpoint"]["min"], -23);
        assert_eq!(comps["freezer_setpoint"]["max"], -15);
        assert_eq!(
            comps["ice_maker_status"]["options"],
            json!(["Off", "Making ice", "Full", "Quiet mode"])
        );
        assert_eq!(
            comps["fresh_air_filter_status"]["options"],
            json!(["Good", "Replace"])
        );
        assert_eq!(
            comps["water_filter_status"]["options"],
            json!(["Good", "Replace"])
        );
        assert_eq!(
            comps["fridge_temperature_status"]["options"],
            json!([
                "Off",
                "Well maintained",
                "Very high",
                "High",
                "Low",
                "Stabilizing",
                "Sensor error",
                "Uncertain"
            ])
        );
        assert_eq!(
            comps["freezer_temperature_status"]["options"],
            comps["fridge_temperature_status"]["options"]
        );
        assert_eq!(comps["water_filter_months"]["unit_of_measurement"], "months");
        assert!(comps.get("night_view").is_none());
        assert!(comps.get("quiet_time").is_none());
        assert!(comps.get("smart_learner").is_none());
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "ice_maker_status").as_deref(), Some("Making ice"));
        assert_eq!(prop(&ha, "fresh_air_filter_status").as_deref(), Some("Good"));
        assert_eq!(prop(&ha, "water_filter_status").as_deref(), Some("Good"));
        assert_eq!(prop(&ha, "water_filter_months").as_deref(), Some("2"));
        assert_eq!(
            prop(&ha, "fridge_temperature_status").as_deref(),
            Some("Well maintained")
        );
        assert_eq!(
            prop(&ha, "freezer_temperature_status").as_deref(),
            Some("Well maintained")
        );
    }

    #[test]
    fn door_close_express_temperatures_and_full_ice() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(DOOR_CLOSED));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(EXPRESS_ON));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(FRIDGE_2C));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("2"));
        thinq.emit_data(&hex_decode(FREEZER_M17));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-17"));
        thinq.emit_data(&hex_decode(ICE_FULL));
        assert_eq!(prop(&ha, "ice_maker_status").as_deref(), Some("Full"));
    }

    #[test]
    fn replacement_thresholds_come_through_the_status_frame() {
        let (ha, thinq, _) = make();
        let mut status = vec![0xffu8; STATUS_LENGTH];
        status[4] = 3;
        status[6] = 6;
        status[8] = 1;
        let mut inner = vec![0x10, 0xeb];
        inner.extend(status);
        thinq.emit_data(&wrap_aabb(&inner));
        assert_eq!(prop(&ha, "fresh_air_filter_status").as_deref(), Some("Replace"));
        assert_eq!(prop(&ha, "water_filter_status").as_deref(), Some("Replace"));
        assert_eq!(prop(&ha, "water_filter_months").as_deref(), Some("6"));
    }

    #[test]
    fn unknown_shapes_publish_nothing() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA08109901020304BB"));
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn start_and_writes_match_the_captured_commands() {
        let (_ha, thinq, dev) = make();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1211010000010400EBBB"
        );
        thinq.emit_data(&hex_decode(DOOR_CLOSED));
        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "2");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA7CF017FF06FFFFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA4BB"
        );
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-17");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA7CF017FFFF03FFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBBBB"
        );
        thinq.reset_recorder();
        dev.set_property("express_freeze", "ON");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA7CF017FFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB"
        );
        thinq.reset_recorder();
        dev.set_property("does-not-exist", "1");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("2REFTBDII4P_U").is_some());
    }
}
