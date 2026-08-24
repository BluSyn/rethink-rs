//! 2REBGLUB_2P__ fridge (LG GBBS322CEV). AABB class 0x10, STATUS_LENGTH=96.
//!
//! Port of unmerged upstream PR #142. Same 0x10EB / 0x10EC family as
//! 2REB1GLVB1__2, but the status block is 96 bytes and drawer mode lives at
//! offset 95 (Cheese / Fish / Meat), which is outside the shared 27-field
//! `fridge_common` name table.

use crate::device_trait::DeviceHandler;
use crate::fridge_common::{
    convert_freezer_temperature, convert_fridge_temperature, freezer_range, fridge_range,
    pack_status, status_get, unpack_status, Status, TemperatureUnit,
};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map};
use std::sync::Arc;

const STATUS_LENGTH: usize = 96;

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
            "door".into(),
            json!({
                "platform": "binary_sensor",
                "device_class": "door",
                "unique_id": "$deviceid-door",
                "state_topic": "$this/door",
                "name": "Door",
            }),
        );
        components.insert(
            "drawer_mode".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-drawer_mode",
                "state_topic": "$this/drawer_mode",
                "icon": "mdi:food",
                "name": "Drawer mode",
            }),
        );
        components.insert(
            "drawer_mode_raw".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-drawer_mode_raw",
                "state_topic": "$this/drawer_mode_raw",
                "icon": "mdi:code-brackets",
                "name": "Drawer mode raw",
            }),
        );
        base.components = components.into_iter().collect();
        self.core.set_config(base);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 2 + STATUS_LENGTH * 2 && buf[0] == 0x10 && buf[1] == 0xec {
            self.process_status(&buf[2 + STATUS_LENGTH..2 + STATUS_LENGTH * 2]);
        }
        if buf.len() == 2 + STATUS_LENGTH && buf[0] == 0x10 && buf[1] == 0xeb {
            self.process_status(&buf[2..2 + STATUS_LENGTH]);
        }
    }

    fn drawer_mode_name(raw: Option<u8>) -> String {
        match raw {
            Some(0) => "Cheese".into(),
            Some(1) => "Fish".into(),
            Some(2) => "Meat".into(),
            Some(0xff) | None => "Unknown".into(),
            Some(n) => format!("Unknown {n}"),
        }
    }

    fn process_status(&self, cur_status: &[u8]) {
        let s = unpack_status(cur_status);
        let unit = if status_get(&s, "tempUnit") != 0 {
            TemperatureUnit::C
        } else {
            TemperatureUnit::F
        };
        self.set_temperature_unit(unit);
        let unit = self.temperature_unit.lock().unwrap_or(TemperatureUnit::C);
        self.core.publish_property(
            "door",
            if status_get(&s, "anyDoorOpen") == 1 {
                "ON"
            } else {
                "OFF"
            }
            .into(),
        );
        self.core.publish_property(
            "fridge_setpoint",
            convert_fridge_temperature(unit, status_get(&s, "fridgeSetpoint") as i32).into(),
        );
        self.core.publish_property(
            "freezer_setpoint",
            convert_freezer_temperature(unit, status_get(&s, "freezerSetpoint") as i32).into(),
        );
        self.core.publish_property(
            "express_cool",
            if status_get(&s, "expressCool") == 1 {
                "ON"
            } else {
                "OFF"
            }
            .into(),
        );
        self.core.publish_property(
            "express_freeze",
            if status_get(&s, "expressFreeze") == 2 {
                "ON"
            } else {
                "OFF"
            }
            .into(),
        );

        let drawer_raw = cur_status.get(95).copied();
        self.core.publish_property(
            "drawer_mode_raw",
            match drawer_raw {
                Some(v) => v.to_string(),
                None => "Unknown".into(),
            }
            .into(),
        );
        self.core
            .publish_property("drawer_mode", Self::drawer_mode_name(drawer_raw).into());
    }

    fn send_setting(&self, setting: &Status) {
        let mut pkt = hex_decode("F017");
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
    use crate::device_trait::DeviceHandler;
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("2REBGLUB_2P__", Device::new)
    }

    // Real packet captures from an LG GBBS322CEV (PR #142).
    const SAMPLE_INITIAL: &str =
        "AA6610EB02050601FFFFFF0201FFFFFF00FFFFFF0000FFFFFFFFFFFFFFFF010101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF01010101000EBB";
    const SAMPLE_DOOR_OPEN: &str =
        "AA6610EB02050601FFFFFF0101FFFFFF00FFFFFF0000FFFFFFFFFFFFFFFF010101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF01010101000FBB";
    const SAMPLE_EXPRESS_FREEZE_ON: &str =
        "AA6610EB02050902FFFFFF0101FFFFFF00FFFFFF0000FFFFFFFFFFFFFFFF010103FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF01010000000BBB";
    const SAMPLE_EXPRESS_COOL_ON: &str =
        "AA6610EB02050601FFFFFF0101FFFFFF00FFFFFF0100FFFFFFFFFFFFFFFF010103FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF01010000000EBB";
    const SAMPLE_DRAWER_FISH: &str =
        "AA6610EB02050601FFFFFF0101FFFFFF00FFFFFF0000FFFFFFFFFFFFFFFF010103FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF010101010108BB";
    const SAMPLE_DRAWER_MEAT: &str =
        "AA6610EB02050601FFFFFF0101FFFFFF00FFFFFF0000FFFFFFFFFFFFFFFF010103FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF01010101020BBB";
    const SAMPLE_DELTA_DOOR_OPEN_TO_CLOSED: &str =
        "AAC610EC02050601FFFFFF0101FFFFFF00FFFFFF0000FFFFFFFFFFFFFFFF010101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF010101010002050601FFFFFF0201FFFFFF00FFFFFF0000FFFFFFFFFFFFFFFF010101FF02FFFFFFFFFFFFFFFFFF01FF00FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0078FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0005FFFFFFFFFF01010101005EBB";

    #[test]
    fn config_not_published_until_status() {
        let (ha, _, _) = make();
        assert!(ha.device(DEVICE_ID).is_none());
    }

    #[test]
    fn initial_status_celsius_and_drawer_cheese() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        let dev = ha.device(DEVICE_ID).unwrap();
        let comps = &dev.config.as_ref().unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], "°C");
        assert_eq!(comps["fridge_setpoint"]["min"], 1);
        assert_eq!(comps["fridge_setpoint"]["max"], 7);
        assert_eq!(comps["freezer_setpoint"]["min"], -23);
        assert_eq!(comps["freezer_setpoint"]["max"], -15);
        assert!(comps.contains_key("door"));
        assert!(comps.contains_key("express_cool"));
        assert!(comps.contains_key("express_freeze"));
        assert!(comps.contains_key("drawer_mode"));
        assert!(comps.contains_key("drawer_mode_raw"));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-20"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_cool").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "drawer_mode").as_deref(), Some("Cheese"));
        assert_eq!(prop(&ha, "drawer_mode_raw").as_deref(), Some("0"));
    }

    #[test]
    fn door_open() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DOOR_OPEN));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-20"));
    }

    #[test]
    fn express_freeze_on() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EXPRESS_FREEZE_ON));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-23"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "express_cool").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
    }

    #[test]
    fn express_cool_on() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EXPRESS_COOL_ON));
        assert_eq!(prop(&ha, "express_cool").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
    }

    #[test]
    fn drawer_fish_and_meat() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DRAWER_FISH));
        assert_eq!(prop(&ha, "drawer_mode_raw").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "drawer_mode").as_deref(), Some("Fish"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DRAWER_MEAT));
        assert_eq!(prop(&ha, "drawer_mode_raw").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "drawer_mode").as_deref(), Some("Meat"));
    }

    #[test]
    fn delta_uses_current_status_block() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_DOOR_OPEN_TO_CLOSED));
        assert_eq!(prop(&ha, "fridge_setpoint").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "freezer_setpoint").as_deref(), Some("-20"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_cool").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "express_freeze").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "drawer_mode").as_deref(), Some("Cheese"));
        assert_eq!(prop(&ha, "drawer_mode_raw").as_deref(), Some("0"));
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
        assert_eq!(pkt[4 + 0], 0xff);
        assert_eq!(pkt[4 + 2], 0xff);
        assert_eq!(pkt.len(), 4 + STATUS_LENGTH + 2);

        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-20");
        let pkt = thinq.outbox()[0].clone();
        assert_eq!(pkt[4 + 2], 6);
        assert_eq!(pkt[4 + 1], 0xff);
        assert_eq!(pkt[4 + 8], 1);

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
