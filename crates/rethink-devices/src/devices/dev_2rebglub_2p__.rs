//! 2REBGLUB_2P__ — LG GBBSJ2CCEP bottom freezer.
//!
//! 96-byte class `0x10` status. The F017 write is the 118-byte frame shared
//! with 2REF11EIDG__4, including the fixed bytes. `auto_ack` is on.
//! Express Cool is Super Cool, ecoFriendly is Vacation, and byte 95 is the
//! writable Fresh Converter. `10AF` watt-hours are summed, `10C5` carries
//! door totals, and `1031` is the two firmware modules.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const STATUS_LENGTH: usize = 96;
const COMMAND_LENGTH: usize = 118;
const FRESH_CONVERTER: usize = 95;
const DOOR_OPENINGS_RECORD: u8 = 0x01;
const DOOR_OPEN_TIME_RECORD: u8 = 0x11;
const FIRMWARE_RECORD_LENGTH: usize = 24;

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

const FRESH_OPTIONS: &[&str] = &["Meat", "Fish", "Cheese"];

fn fresh_name(code: u8) -> &'static str {
    match code {
        0 => "Cheese",
        1 => "Fish",
        2 => "Meat",
        _ => "None",
    }
}

fn fresh_code(name: &str) -> Option<u8> {
    match name {
        "Cheese" => Some(0),
        "Fish" => Some(1),
        "Meat" => Some(2),
        _ => None,
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    temperature_unit: Mutex<Option<TemperatureUnit>>,
    energy_wh: Mutex<i64>,
    meta: Metadata,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            temperature_unit: Mutex::new(None),
            energy_wh: Mutex::new(0),
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
        {
            let mut current = self.temperature_unit.lock();
            if *current == Some(unit) {
                return;
            }
            *current = Some(unit);
        }
        let (fu, fmin, fmax) = fridge_range(unit);
        let (zu, zmin, zmax) = freezer_range(unit);
        let mut config = default_config(&self.meta, Some(json!({"name": "LG Fridge"})));
        config.components = [
            ("fridge_setpoint".into(), json!({
                "platform": "number", "device_class": "temperature",
                "unique_id": "$deviceid-fridge_setpoint",
                "state_topic": "$this/fridge_setpoint",
                "command_topic": "$this/fridge_setpoint/set",
                "name": "Fridge temperature",
                "unit_of_measurement": fu, "min": fmin, "max": fmax,
            })),
            ("freezer_setpoint".into(), json!({
                "platform": "number", "device_class": "temperature",
                "unique_id": "$deviceid-freezer_setpoint",
                "state_topic": "$this/freezer_setpoint",
                "command_topic": "$this/freezer_setpoint/set",
                "name": "Freezer temperature",
                "unit_of_measurement": zu, "min": zmin, "max": zmax,
            })),
            ("door".into(), json!({
                "platform": "binary_sensor", "device_class": "door",
                "unique_id": "$deviceid-door",
                "state_topic": "$this/door",
                "name": "Door",
            })),
            ("express_freeze".into(), json!({
                "platform": "switch", "icon": "mdi:snowflake",
                "unique_id": "$deviceid-express_freeze",
                "state_topic": "$this/express_freeze",
                "command_topic": "$this/express_freeze/set",
                "name": "Express Freeze",
            })),
            ("super_cool".into(), json!({
                "platform": "switch", "icon": "mdi:snowflake-variant",
                "unique_id": "$deviceid-super_cool",
                "state_topic": "$this/super_cool",
                "command_topic": "$this/super_cool/set",
                "name": "Super Cool",
            })),
            ("smart_learner".into(), json!({
                "platform": "switch", "icon": "mdi:brain",
                "unique_id": "$deviceid-smart_learner",
                "state_topic": "$this/smart_learner",
                "command_topic": "$this/smart_learner/set",
                "name": "Smart Learner",
            })),
            ("vacation".into(), json!({
                "platform": "switch", "icon": "mdi:airplane",
                "unique_id": "$deviceid-vacation",
                "state_topic": "$this/vacation",
                "command_topic": "$this/vacation/set",
                "name": "Vacation",
            })),
            ("fresh_converter".into(), json!({
                "platform": "select", "icon": "mdi:fridge-bottom",
                "unique_id": "$deviceid-fresh_converter",
                "state_topic": "$this/fresh_converter",
                "command_topic": "$this/fresh_converter/set",
                "name": "Fresh Converter+",
                "options": FRESH_OPTIONS,
            })),
            ("energy".into(), json!({
                "platform": "sensor", "device_class": "energy",
                "unique_id": "$deviceid-energy",
                "state_topic": "$this/energy",
                "unit_of_measurement": "Wh", "state_class": "total_increasing",
                "name": "Energy",
            })),
            ("door_openings".into(), json!({
                "platform": "sensor", "icon": "mdi:door-open",
                "unique_id": "$deviceid-door_openings",
                "state_topic": "$this/door_openings",
                "state_class": "total_increasing",
                "name": "Door openings",
            })),
            ("door_open_time".into(), json!({
                "platform": "sensor", "device_class": "duration",
                "unique_id": "$deviceid-door_open_time",
                "state_topic": "$this/door_open_time",
                "unit_of_measurement": "s", "state_class": "total_increasing",
                "name": "Door open time",
            })),
            ("firmware_1".into(), json!({
                "platform": "sensor", "entity_category": "diagnostic", "icon": "mdi:chip",
                "unique_id": "$deviceid-firmware_1",
                "state_topic": "$this/firmware_1",
                "name": "Firmware 1",
            })),
            ("firmware_2".into(), json!({
                "platform": "sensor", "entity_category": "diagnostic", "icon": "mdi:chip",
                "unique_id": "$deviceid-firmware_2",
                "state_topic": "$this/firmware_2",
                "name": "Firmware 2",
            })),
        ]
        .into_iter()
        .collect();
        self.core.set_config(config);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, 0x10, STATUS_LENGTH) {
            self.process_status(rec);
            return;
        }
        if buf.first() != Some(&0x10) || buf.len() < 2 {
            return;
        }
        if buf[1] == 0xaf && buf.len() >= 5 {
            let wh = u16::from_be_bytes([buf[3], buf[4]]);
            self.process_energy(wh);
        } else if buf[1] == 0xc5 && buf.len() >= 4 {
            self.process_usage(&buf[2..]);
        } else if buf[1] == 0x31 && buf.len() >= 3 {
            self.process_firmware(&buf[2..]);
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
        self.core.publish_on_off("express_freeze", rec[3] == 2);
        self.core.publish_on_off("super_cool", rec[16] == 1);
        self.core.publish_on_off("smart_learner", rec[17] == 1);
        self.core.publish_on_off("vacation", rec[12] == 1);
        self.core
            .publish_property("fresh_converter", fresh_name(rec[FRESH_CONVERTER]).into());
    }

    fn process_energy(&self, wh: u16) {
        let total = {
            let mut energy = self.energy_wh.lock();
            *energy += i64::from(wh);
            *energy
        };
        if self.temperature_unit.lock().is_some() {
            self.core
                .publish_property("energy", PropertyValue::Int(total));
        }
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
            let total = u16::from_be_bytes([record[4], record[5]]) as i64;
            if record[0] == DOOR_OPENINGS_RECORD {
                self.core
                    .publish_property("door_openings", PropertyValue::Int(total));
            } else if record[0] == DOOR_OPEN_TIME_RECORD {
                self.core
                    .publish_property("door_open_time", PropertyValue::Int(total));
            }
        }
    }

    fn process_firmware(&self, report: &[u8]) {
        if report.is_empty() {
            return;
        }
        let count = report[0] as usize;
        for i in 0..count {
            let start = 1 + i * FIRMWARE_RECORD_LENGTH;
            let Some(record) = report.get(start..start + FIRMWARE_RECORD_LENGTH) else {
                return;
            };
            if record[0] != 1 && record[0] != 2 {
                continue;
            }
            let part = String::from_utf8_lossy(&record[1..12]);
            let version = format!("{:02x}{:02x}.{}", record[14], record[15], record[16]);
            let name = format!("firmware_{}", record[0]);
            self.core
                .publish_property(&name, format!("{version} ({part})").into());
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
    fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
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
            "express_freeze" => self.command(&[(3, if mqtt_value == "ON" { 2 } else { 1 })]),
            "vacation" => self.command(&[(12, if mqtt_value == "ON" { 1 } else { 0 })]),
            "super_cool" => self.command(&[(16, if mqtt_value == "ON" { 1 } else { 0 })]),
            "smart_learner" => self.command(&[(17, if mqtt_value == "ON" { 1 } else { 0 })]),
            "fresh_converter" => {
                if let Some(code) = fresh_code(mqtt_value) {
                    self.command(&[(FRESH_CONVERTER, code)]);
                }
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
    const STATUS_INITIAL: &str = "AA6610EB02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF00000DBB";
    const STATUS_DOOR_OPEN: &str = "AAC610EC02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF020101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000002040401FFFFFF0101FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF020101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF0000AABB";
    const STATUS_SUPER_COOL: &str = "AAC610EC02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000002040401FFFFFF0201FFFFFF00FFFFFF0101FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000052BB";
    const STATUS_FRESH_CONVERTER_2: &str = "AAC610EC02040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF000002040401FFFFFF0201FFFFFF00FFFFFF0001FFFFFFFFFFFFFFFF050101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF0000FF00025DBB";
    const ENERGY_8WH: &str = "AA0B10AF0000080404D1BB";
    const USAGE: &str = "AA2010C5150401000100001203000000000011000A0000A6130000000000F6BB";
    const FIRMWARE: &str = "AA37103102015341413330383830383031000068E0000018000003E8000253414133303631393030310000968F000000000000A8004CBB";
    const APP_FRIDGE_5C: &str = "AA7CF017FF03FFFFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBBBB";
    const APP_FREEZER_M19C: &str = "AA7CF017FFFF05FFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA5BB";
    const APP_EXPRESS_FREEZE_ON: &str = "AA7CF017FFFFFF02FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB";
    const APP_VACATION_ON: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB";
    const APP_SUPER_COOL_ON: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFBB";
    const APP_SMART_LEARNER_OFF: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBCBB";
    const APP_FRESH_CONVERTER_2: &str = "AA7CF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000FFFF00FFFFFFFF00FFFFFFFFFFFFFFFFFF00FFFFFF1EFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0AFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF02FFFFFFFFFFFF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBEBB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2REBGLUB_2P__", "2REBGLUB_2P__", "");
        let thinq = MockThinq2Device::new(ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn ready() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let made = make();
        made.1.emit_data(&hex_decode(STATUS_INITIAL));
        made.1.reset_recorder();
        made
    }

    #[test]
    fn config_not_published_until_a_status_frame() {
        let (ha, _, _) = make();
        assert!(ha.device(ID).is_none());
    }

    #[test]
    fn initial_status() {
        let (ha, _, _) = ready();
        let comps = ha.device(ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°C"));
        assert!(comps.get("drawer_mode").is_none());
        assert!(comps.get("express_cool").is_none());
        assert_eq!(comps["fresh_converter"]["options"], json!(["Meat", "Fish", "Cheese"]));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-18"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "super_cool").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "smart_learner").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "vacation").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fresh_converter").as_deref(), Some("Cheese"));
    }

    #[test]
    fn status_changes() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(STATUS_DOOR_OPEN));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(STATUS_SUPER_COOL));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "super_cool").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(STATUS_FRESH_CONVERTER_2));
        assert_eq!(prop(&ha, "fresh_converter").as_deref(), Some("Meat"));
    }

    #[test]
    fn energy_accumulates() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("16"));
    }

    #[test]
    fn energy_before_the_unit_is_counted() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        assert!(prop(&ha, "energy").is_none());
        thinq.emit_data(&hex_decode(STATUS_INITIAL));
        thinq.emit_data(&hex_decode(ENERGY_8WH));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("16"));
    }

    #[test]
    fn door_usage_totals() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(USAGE));
        assert_eq!(prop(&ha, "door_openings").as_deref(), Some("18"));
        assert_eq!(prop(&ha, "door_open_time").as_deref(), Some("166"));
    }

    #[test]
    fn firmware_versions() {
        let (ha, thinq, _) = ready();
        thinq.emit_data(&hex_decode(FIRMWARE));
        assert_eq!(
            prop(&ha, "firmware_1").as_deref(),
            Some("68e0.0 (SAA30880801)")
        );
        assert_eq!(
            prop(&ha, "firmware_2").as_deref(),
            Some("968f.0 (SAA30619001)")
        );
    }

    #[test]
    fn start_sends_the_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1211010000010400EBBB");
    }

    #[test]
    fn writes_match_the_app_commands() {
        let cases = [
            ("fridge_setpoint", "5", APP_FRIDGE_5C),
            ("freezer_setpoint", "-19", APP_FREEZER_M19C),
            ("express_freeze", "ON", APP_EXPRESS_FREEZE_ON),
            ("vacation", "ON", APP_VACATION_ON),
            ("super_cool", "ON", APP_SUPER_COOL_ON),
            ("smart_learner", "OFF", APP_SMART_LEARNER_OFF),
            ("fresh_converter", "Meat", APP_FRESH_CONVERTER_2),
        ];
        for (prop_name, value, expected) in cases {
            let (_, thinq, dev) = ready();
            dev.set_property(prop_name, value);
            assert_eq!(
                hex_encode(&thinq.outbox()[0]),
                expected,
                "{prop_name}={value}"
            );
        }
    }

    #[test]
    fn unknown_property_sends_nothing() {
        let (_, thinq, dev) = ready();
        dev.set_property("nonsense", "whatever");
        dev.set_property("drawer_mode", "Fish");
        assert!(thinq.outbox().is_empty());
    }
}
