//! 2REF11EIDG__4 fridge (LG LRFXC2606S). 68-byte AABB status, no flex drawer.
//!
//! Commands are the LG app's 118-byte `F017` frame. `10 C5` usage reports carry
//! water dispensed; record `0x21` total is millilitres.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const STATUS_LENGTH: usize = 68;
const COMMAND_LENGTH: usize = 118;
const FRESH_AIR_FILTER: usize = 35;
const WATER_FILTER: usize = 36;
const WATER_DISPENSED_RECORD: u8 = 0x21;

const COMMAND_FIXED: &[(usize, u8)] = &[
    (21, 0x00),
    (22, 0x00),
    (23, 0x00),
    (26, 0x00),
    (31, 0x00),
    (41, 0x00),
    (45, 0x1e),
    (70, 0x0a),
    (102, 0x00),
];

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    meta: Metadata,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        // This fridge needs the cloud's acks.
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
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
                "door".into(),
                json!({
                    "platform": "binary_sensor", "device_class": "door",
                    "unique_id": "$deviceid-door",
                    "state_topic": "$this/door",
                    "name": "Door",
                }),
            ),
            (
                "ice_plus".into(),
                json!({
                    "platform": "switch", "icon": "mdi:snowflake",
                    "unique_id": "$deviceid-ice_plus",
                    "state_topic": "$this/ice_plus",
                    "command_topic": "$this/ice_plus/set",
                    "name": "Ice Plus",
                    "payload_on": "ON", "payload_off": "OFF",
                }),
            ),
            (
                "sabbath_mode".into(),
                json!({
                    "platform": "switch", "icon": "mdi:candle",
                    "unique_id": "$deviceid-sabbath_mode",
                    "state_topic": "$this/sabbath_mode",
                    "command_topic": "$this/sabbath_mode/set",
                    "name": "Sabbath Mode",
                    "payload_on": "ON", "payload_off": "OFF",
                }),
            ),
            (
                "smart_learner".into(),
                json!({
                    "platform": "switch", "icon": "mdi:brain",
                    "unique_id": "$deviceid-smart_learner",
                    "state_topic": "$this/smart_learner",
                    "command_topic": "$this/smart_learner/set",
                    "name": "Smart Learner",
                    "payload_on": "ON", "payload_off": "OFF",
                }),
            ),
            (
                "fresh_air_filter".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:air-filter",
                    "unique_id": "$deviceid-fresh_air_filter",
                    "state_topic": "$this/fresh_air_filter",
                    "unit_of_measurement": "%", "state_class": "measurement",
                    "name": "Fresh air filter",
                }),
            ),
            (
                "water_filter".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:water-check",
                    "unique_id": "$deviceid-water_filter",
                    "state_topic": "$this/water_filter",
                    "unit_of_measurement": "%", "state_class": "measurement",
                    "name": "Water filter",
                }),
            ),
            (
                "fresh_air_filter_reset".into(),
                json!({
                    "platform": "button", "icon": "mdi:restore",
                    "unique_id": "$deviceid-fresh_air_filter_reset",
                    "command_topic": "$this/fresh_air_filter_reset/set",
                    "payload_press": "",
                    "name": "Reset fresh air filter",
                }),
            ),
            (
                "water_filter_reset".into(),
                json!({
                    "platform": "button", "icon": "mdi:restore",
                    "unique_id": "$deviceid-water_filter_reset",
                    "command_topic": "$this/water_filter_reset/set",
                    "payload_press": "",
                    "name": "Reset water filter",
                }),
            ),
            (
                "water_dispensed".into(),
                json!({
                    "platform": "sensor", "device_class": "water",
                    "unique_id": "$deviceid-water_dispensed",
                    "state_topic": "$this/water_dispensed",
                    "unit_of_measurement": "L", "state_class": "total_increasing",
                    "name": "Water dispensed",
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
        if buf.len() >= 4 && buf[0] == 0x10 && buf[1] == 0xc5 {
            self.process_usage(&buf[2..]);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        if rec.len() < STATUS_LENGTH {
            return;
        }
        let unit = if rec[8] != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        self.core.publish_property(
            "fridge_setpoint",
            convert_fridge_temperature(unit, i32::from(rec[1])).into(),
        );
        self.core.publish_property(
            "freezer_setpoint",
            convert_freezer_temperature(unit, i32::from(rec[2])).into(),
        );
        self.core.publish_on_off("door", rec[7] == 1);
        self.core.publish_on_off("ice_plus", rec[3] == 2);
        self.core.publish_on_off("sabbath_mode", rec[14] == 1);
        self.core.publish_on_off("smart_learner", rec[17] == 1);
        self.core
            .publish_property("fresh_air_filter", i64::from(rec[FRESH_AIR_FILTER]).into());
        self.core
            .publish_property("water_filter", i64::from(rec[WATER_FILTER]).into());
    }

    fn process_usage(&self, report: &[u8]) {
        if report.len() < 2 {
            return;
        }
        let count = report[1] as usize;
        for i in 0..count {
            let start = 2 + i * 6;
            let Some(record) = report.get(start..start + 6) else {
                return;
            };
            if record[0] == WATER_DISPENSED_RECORD {
                let ml = u16::from_be_bytes([record[4], record[5]]);
                self.core
                    .publish_property("water_dispensed", (f64::from(ml) / 1000.0).into());
            }
        }
    }

    fn command(&self, fields: &[(usize, u8)]) {
        let mut message = vec![0xffu8; 2 + COMMAND_LENGTH];
        message[0] = 0xf0;
        message[1] = 0x17;
        for &(index, value) in COMMAND_FIXED {
            message[2 + index] = value;
        }
        for &(index, value) in fields {
            message[2 + index] = value;
        }
        self.core.send(&message);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::F);
        let unit_byte = if unit == TemperatureUnit::C { 1 } else { 0 };
        match prop {
            "fridge_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                self.command(&[
                    (1, convert_fridge_temperature(unit, display) as u8),
                    (8, unit_byte),
                ]);
            }
            "freezer_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                self.command(&[
                    (2, convert_freezer_temperature(unit, display) as u8),
                    (8, unit_byte),
                ]);
            }
            "ice_plus" => self.command(&[(3, if mqtt_value == "ON" { 2 } else { 1 })]),
            "sabbath_mode" => self.command(&[(14, if mqtt_value == "ON" { 1 } else { 0 })]),
            "smart_learner" => self.command(&[(17, if mqtt_value == "ON" { 1 } else { 0 })]),
            "fresh_air_filter_reset" => self.command(&[(FRESH_AIR_FILTER, 100)]),
            "water_filter_reset" => self.command(&[(WATER_FILTER, 100)]),
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const DEVICE_ID: &str = "test-id";
    const FRAME_FRIDGE_38: &str = "AA8E10EC0207060102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000206060102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000086BB";
    const FRAME_FREEZER_1: &str = "AA8E10EC0207060102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207050102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000086BB";
    const FRAME_ICE_PLUS_ON: &str = "AA8E10EC0207060102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207060202000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000080BB";
    const FRAME_DOOR_OPEN: &str = "AA8E10EC0207060102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207060102000001000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000080BB";
    const FRAME_SMART_LEARNER_ON: &str = "AA8E10EC0207060102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207060102000000000001FFFFFF00FFFF01FFFFFFFFFFFFFF02150505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000080BB";
    const FRAME_SABBATH_ON: &str = "AA8E10EC0207060102000000000001FFFFFF00FFFF00FFFFFFFFFFFFFF021A0505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000207060102000000000001FFFFFF01FFFF00FFFFFFFFFFFFFF021A0505FF00000101FF6464FFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00008ABB";
    const FRAME_USAGE_GALLON: &str = "AA3810C5030801000000000302000000000003000000000111000000018A120000000000130000000010210ECC001E8A2201A70001A7E7BB";

    fn meta() -> Metadata {
        Metadata::new("2REF11EIDG__4", "2REF11EIDG__4", "")
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
    fn config_waits_for_the_temperature_unit_and_has_no_flex_drawer() {
        let (ha, thinq, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
        thinq.emit_data(&hex_decode(FRAME_FRIDGE_38));
        let info = ha.device(DEVICE_ID).unwrap();
        let comps = &info.config.as_ref().unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], "°F");
        assert!(comps.get("flex_setpoint").is_none());
        for key in [
            "ice_plus",
            "sabbath_mode",
            "smart_learner",
            "water_filter",
            "fresh_air_filter",
            "water_dispensed",
        ] {
            assert!(comps.contains_key(key), "{key}");
        }
    }

    #[test]
    fn setpoints_decode_from_the_captured_app_changes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_FRIDGE_38));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("38"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("0"));
        thinq.emit_data(&hex_decode(FRAME_FREEZER_1));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("37"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("1"));
    }

    #[test]
    fn ice_plus_door_smart_learner_and_sabbath_decode() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_ICE_PLUS_ON));
        assert_eq!(prop(&ha, "ice_plus").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(FRAME_DOOR_OPEN));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "ice_plus").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(FRAME_SMART_LEARNER_ON));
        assert_eq!(prop(&ha, "smart_learner").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "sabbath_mode").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(FRAME_SABBATH_ON));
        assert_eq!(prop(&ha, "sabbath_mode").as_deref(), Some("ON"));
    }

    #[test]
    fn filter_life_reads_as_a_percent_and_usage_is_litres() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRAME_FRIDGE_38));
        assert_eq!(prop(&ha, "fresh_air_filter").as_deref(), Some("100"));
        assert_eq!(prop(&ha, "water_filter").as_deref(), Some("100"));
        assert!(thinq.sent().iter().all(|m| m.cmd != "ack"));
        thinq.emit_data(&hex_decode(FRAME_USAGE_GALLON));
        assert_eq!(prop(&ha, "water_dispensed").as_deref(), Some("7.818"));
        assert!(thinq.sent().iter().any(|m| m.cmd == "ack"));
    }

    #[test]
    fn every_command_matches_what_the_lg_app_sent() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(FRAME_FRIDGE_38));
        let cases = [
            ("fridge_setpoint", "38", "AA7CF017FF06FFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA5BB"),
            ("freezer_setpoint", "1", "AA7CF017FFFF05FFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBABB"),
            ("ice_plus", "ON", "AA7CF017FFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB"),
            ("ice_plus", "OFF", "AA7CF017FFFFFF01FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB"),
            ("smart_learner", "ON", "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF01FFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB"),
            ("sabbath_mode", "ON", "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB"),
            ("sabbath_mode", "OFF", "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBCBB"),
            ("fresh_air_filter_reset", "", "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFF64FFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFF18BB"),
            ("water_filter_reset", "", "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFF64FFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFF18BB"),
        ];
        for (prop_name, value, expected) in cases {
            dev.set_property(prop_name, value);
            assert_eq!(
                hex_encode(thinq.outbox().last().unwrap()),
                expected,
                "{prop_name}={value}"
            );
        }
    }

    #[test]
    fn start_sends_the_fridge_status_query() {
        let (_ha, thinq, dev) = make();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1211010000010400EBBB"
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("2REF11EIDG__4").is_some());
    }
}
