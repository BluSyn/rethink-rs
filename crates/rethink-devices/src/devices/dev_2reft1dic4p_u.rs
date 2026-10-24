//! 2REFT1DIC4P_U French-door fridge (LG GF-V700BSLC). 68-byte AABB status.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_fridge_status;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    TemperatureUnit, CMD_F017,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const STATUS_LENGTH: usize = 68;

const CRAFT_ICE: &[(u8, &str)] = &[(0, "Off"), (1, "3 balls"), (2, "6 balls")];
const CRAFT_ICE_MAKER: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Making ice"),
    (2, "Full"),
    (3, "Quiet mode"),
];
const NIGHT_MODE: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Every day"),
    (2, "Sunset to sunrise"),
    (3, "Custom"),
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

fn code_of(table: &'static [(u8, &'static str)], name: &str) -> Option<u8> {
    table.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
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
                "express_freeze".into(),
                json!({
                    "platform": "switch",
                    "unique_id": "$deviceid-express_freeze",
                    "state_topic": "$this/express_freeze",
                    "command_topic": "$this/express_freeze/set",
                    "icon": "mdi:snowflake",
                    "name": "Express Freeze",
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
                "craft_ice_mode".into(),
                json!({
                    "platform": "select", "icon": "mdi:snowflake-variant",
                    "unique_id": "$deviceid-craft_ice_mode",
                    "state_topic": "$this/craft_ice_mode",
                    "command_topic": "$this/craft_ice_mode/set",
                    "name": "Craft ice",
                    "options": labels(CRAFT_ICE),
                }),
            ),
            (
                "craft_ice_maker_status".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-craft_ice_maker_status",
                    "state_topic": "$this/craft_ice_maker_status",
                    "name": "Craft ice maker status",
                    "icon": "mdi:snowflake-variant",
                    "device_class": "enum",
                    "options": labels(CRAFT_ICE_MAKER),
                }),
            ),
            (
                "night_view".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:weather-night",
                    "unique_id": "$deviceid-night_view",
                    "state_topic": "$this/night_view",
                    "name": "Night view",
                    "device_class": "enum",
                    "options": labels(NIGHT_MODE),
                }),
            ),
            (
                "quiet_mode".into(),
                json!({
                    "platform": "sensor", "icon": "mdi:volume-off",
                    "unique_id": "$deviceid-quiet_mode",
                    "state_topic": "$this/quiet_mode",
                    "name": "Quiet mode",
                    "device_class": "enum",
                    "options": labels(NIGHT_MODE),
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
        self.core.publish_on_off("door", rec[7] == 1);
        self.core.publish_property(
            "fridge_setpoint",
            convert_fridge_temperature(unit, i32::from(rec[1])).into(),
        );
        self.core.publish_property(
            "freezer_setpoint",
            convert_freezer_temperature(unit, i32::from(rec[2])).into(),
        );
        self.core.publish_on_off("express_freeze", rec[3] == 2);
        self.core
            .publish_property("night_view", named(NIGHT_MODE, rec[30]).into());
        self.core
            .publish_property("quiet_mode", named(NIGHT_MODE, rec[31]).into());
        self.core
            .publish_property("craft_ice_mode", named(CRAFT_ICE, rec[25]).into());
        self.core.publish_property(
            "craft_ice_maker_status",
            named(CRAFT_ICE_MAKER, rec[34]).into(),
        );
    }

    fn send_byte(&self, index: usize, value: u8) {
        let mut payload = vec![0xffu8; STATUS_LENGTH];
        payload[index] = value;
        let mut pkt = Vec::with_capacity(2 + STATUS_LENGTH);
        pkt.extend_from_slice(&CMD_F017);
        pkt.extend(payload);
        self.core.send(&pkt);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        match prop {
            "fridge_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                self.send_byte(1, convert_fridge_temperature(unit, display) as u8);
            }
            "freezer_setpoint" => {
                let display = mqtt_value.parse().unwrap_or(0);
                self.send_byte(2, convert_freezer_temperature(unit, display) as u8);
            }
            "express_freeze" => self.send_byte(3, if mqtt_value == "ON" { 2 } else { 1 }),
            "craft_ice_mode" => {
                if let Some(code) = code_of(CRAFT_ICE, mqtt_value) {
                    self.send_byte(25, code);
                }
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const DEVICE_ID: &str = "test-id";
    const FRIDGE_TEMP: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000206050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000052BB";
    const FREEZER_TEMP: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205090102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00005FBB";
    const EXPRESS_ON: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050202FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000052BB";
    const DOOR_OPEN: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0401010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000052BB";
    const DOOR_CLOSE: &str = "AA8E10EC0205050102FF0401010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02000101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000052BB";
    const CRAFT_3: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02060101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF01060101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000044BB";
    const CRAFT_6: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF01060101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02060101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000044BB";
    const MAKER_OFF: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF00060101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF00060101FF000001FF00FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000058BB";
    const MAKER_ON: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02060101FF000001FF00FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02060101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000044BB";
    const NIGHT_CUSTOM: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02060101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02070101FF030001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000043BB";
    const NIGHT_OFF: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF02090101FF030001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF020A0101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF000049BB";
    const QUIET_CUSTOM: &str = "AA8E10EC0205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF020A0101FF000001FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00000205050102FF0400010001FFFFFFFFFFFF00FFFFFFFFFFFFFF020B0101FF000301FF01FFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FF00004BBB";

    fn meta() -> Metadata {
        Metadata::new("2REFT1DIC4P_U", "2REFT1DIC4P_U", "1.0")
    }

    fn make() -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<Device>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(name).map(|p| p.as_string())
    }

    #[test]
    fn config_not_published_until_a_status_frame() {
        let (ha, _, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
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
    fn fridge_setpoint_change_publishes_celsius_config() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FRIDGE_TEMP));
        let dev = ha.device(DEVICE_ID).unwrap();
        let fridge = &dev.config.as_ref().unwrap().components["fridge_setpoint"];
        let freezer = &dev.config.as_ref().unwrap().components["freezer_setpoint"];
        assert_eq!(fridge["unit_of_measurement"], "°C");
        assert_eq!(fridge["min"], 1);
        assert_eq!(fridge["max"], 7);
        assert_eq!(freezer["unit_of_measurement"], "°C");
        assert_eq!(freezer["min"], -23);
        assert_eq!(freezer["max"], -15);
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
    }

    #[test]
    fn freezer_setpoint_change() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(FREEZER_TEMP));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-23"));
    }

    #[test]
    fn express_freeze_turns_on() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EXPRESS_ON));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("ON"));
    }

    #[test]
    fn door_opens_then_closes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(DOOR_OPEN));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(DOOR_CLOSE));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn craft_ice_mode_and_maker_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(CRAFT_3));
        assert_eq!(prop(&ha, "craft_ice_mode").as_deref(), Some("3 balls"));
        thinq.emit_data(&hex_decode(CRAFT_6));
        assert_eq!(prop(&ha, "craft_ice_mode").as_deref(), Some("6 balls"));
        thinq.emit_data(&hex_decode(MAKER_OFF));
        assert_eq!(prop(&ha, "craft_ice_maker_status").as_deref(), Some("Off"));
        thinq.emit_data(&hex_decode(MAKER_ON));
        assert_eq!(
            prop(&ha, "craft_ice_maker_status").as_deref(),
            Some("Making ice")
        );
    }

    #[test]
    fn night_view_and_quiet_mode() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(NIGHT_CUSTOM));
        assert_eq!(prop(&ha, "night_view").as_deref(), Some("Custom"));
        thinq.emit_data(&hex_decode(NIGHT_OFF));
        assert_eq!(prop(&ha, "night_view").as_deref(), Some("Off"));
        thinq.emit_data(&hex_decode(QUIET_CUSTOM));
        assert_eq!(prop(&ha, "quiet_mode").as_deref(), Some("Custom"));
    }

    #[test]
    fn unframed_and_unknown_shapes_publish_nothing() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("001122"));
        thinq.emit_data(&hex_decode("AA08109901020304BB"));
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn writes_match_the_confirmed_packets_and_ignore_read_only_fields() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(FRIDGE_TEMP));
        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "2");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA4AF017FF06FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBBB"
        );
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-20");
        assert_eq!(thinq.outbox()[0][4 + 2], 6);
        assert_eq!(thinq.outbox()[0][4 + 1], 0xff);
        thinq.reset_recorder();
        dev.set_property("express_freeze", "ON");
        assert_eq!(thinq.outbox()[0][4 + 3], 2);
        thinq.reset_recorder();
        dev.set_property("craft_ice_mode", "3 balls");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA4AF017FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF01FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFECBB"
        );
        thinq.reset_recorder();
        dev.set_property("night_view", "Custom");
        dev.set_property("quiet_mode", "Custom");
        dev.set_property("craft_ice_mode", "nope");
        dev.set_property("nonsense", "whatever");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("2REFT1DIC4P_U").is_some());
    }
}
