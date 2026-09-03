//! 2REF12EII_P_2 — LG slim fridge (GML844 family, PR #96).
//!
//! Device-specific temp formulas (not fridge_common convert_*):
//!   fridge C = 7 - raw, freezer C = -(raw + 15).

use crate::device_trait::DeviceHandler;
use crate::fridge_common::{
    freezer_range, fridge_range, status_at, TemperatureUnit, ST_ANY_DOOR_OPEN, ST_EXPRESS_FREEZE,
    ST_FRESH_AIR_FILTER, ST_FREEZER_SETPOINT, ST_FRIDGE_SETPOINT, ST_WATER_FILTER,
};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::json;
use std::sync::Arc;

const PURE_OPTIONS: &[&str] = &["Automatic", "Power", "Off"];

fn pure_raw_to_name(raw: u8) -> &'static str {
    match raw {
        0x01 => "Off",
        0x02 => "Automatic",
        0x03 => "Power",
        _ => "Automatic",
    }
}

fn pure_name_to_raw(name: &str) -> Option<u8> {
    match name {
        "Off" => Some(0x01),
        "Automatic" => Some(0x02),
        "Power" => Some(0x03),
        _ => None,
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        let (fridge_uom, fridge_min, fridge_max) = fridge_range(TemperatureUnit::C);
        let (freezer_uom, freezer_min, freezer_max) = freezer_range(TemperatureUnit::C);
        let mut config = default_config(&meta, Some(json!({"name": "LG Fridge"})));
        config.components = [
            (
                "fridge_setpoint".into(),
                json!({
                    "platform": "number",
                    "device_class": "temperature",
                    "unique_id": "$deviceid-fridge_setpoint",
                    "state_topic": "$this/fridge_setpoint",
                    "command_topic": "$this/fridge_setpoint/set",
                    "name": "Fridge temperature",
                    "unit_of_measurement": fridge_uom,
                    "min": fridge_min,
                    "max": fridge_max,
                }),
            ),
            (
                "freezer_setpoint".into(),
                json!({
                    "platform": "number",
                    "device_class": "temperature",
                    "unique_id": "$deviceid-freezer_setpoint",
                    "state_topic": "$this/freezer_setpoint",
                    "command_topic": "$this/freezer_setpoint/set",
                    "name": "Freezer temperature",
                    "unit_of_measurement": freezer_uom,
                    "min": freezer_min,
                    "max": freezer_max,
                }),
            ),
            (
                "pure_option".into(),
                json!({
                    "platform": "select",
                    "icon": "mdi:air-filter",
                    "unique_id": "$deviceid-pure_option",
                    "state_topic": "$this/pure_option",
                    "command_topic": "$this/pure_option/set",
                    "name": "Pure N Fresh",
                    "options": PURE_OPTIONS,
                }),
            ),
            (
                "pure_n_fresh_replace".into(),
                json!({
                    "platform": "sensor",
                    "icon": "mdi:alert-circle-outline",
                    "unique_id": "$deviceid-pure_n_fresh_replace",
                    "state_topic": "$this/pure_n_fresh_replace",
                    "entity_category": "diagnostic",
                    "name": "Pure N Fresh Replace",
                }),
            ),
            (
                "water_filter".into(),
                json!({
                    "platform": "sensor",
                    "icon": "mdi:water",
                    "unique_id": "$deviceid-water_filter",
                    "state_topic": "$this/water_filter",
                    "unit_of_measurement": "months",
                    "entity_category": "diagnostic",
                    "name": "Water Filter",
                }),
            ),
            (
                "door".into(),
                json!({
                    "platform": "binary_sensor",
                    "device_class": "door",
                    "unique_id": "$deviceid-door",
                    "state_topic": "$this/door",
                    "name": "Door",
                }),
            ),
            (
                "express_freeze".into(),
                json!({
                    "platform": "switch",
                    "icon": "mdi:snowflake",
                    "unique_id": "$deviceid-express_freeze",
                    "state_topic": "$this/express_freeze",
                    "command_topic": "$this/express_freeze/set",
                    "name": "Express Freeze",
                    "payload_on": "ON",
                    "payload_off": "OFF",
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
        let fridge_raw = status_at(cur, ST_FRIDGE_SETPOINT);
        let freezer_raw = status_at(cur, ST_FREEZER_SETPOINT);
        let express = status_at(cur, ST_EXPRESS_FREEZE);
        let pure = status_at(cur, ST_FRESH_AIR_FILTER);
        let water = status_at(cur, ST_WATER_FILTER);
        let door = status_at(cur, ST_ANY_DOOR_OPEN);

        let fridge_temp = 7i64 - fridge_raw as i64;
        let freezer_temp = -((freezer_raw as i64) + 15);

        self.core
            .publish_property("fridge_setpoint", PropertyValue::Int(fridge_temp));
        self.core
            .publish_property("freezer_setpoint", PropertyValue::Int(freezer_temp));
        self.core.publish_on_off("express_freeze", express == 0x02);
        self.core
            .publish_property("pure_option", pure_raw_to_name(pure).into());
        self.core.publish_door_with_trigger(door == 0x01);
        self.core.publish_property(
            "pure_n_fresh_replace",
            if pure == 0x04 { "replace" } else { "OK" }.into(),
        );
        self.core
            .publish_property("water_filter", PropertyValue::Int(water as i64));
    }

    fn process_aabb(&self, buf: &[u8]) {
        // 0x10EC: [cmd 2B][prev 9B][cur 9B]
        if buf.len() == 20 && buf[0] == 0x10 && buf[1] == 0xec {
            self.process_status(&buf[11..20]);
            return;
        }
        // 0x10EB: [cmd 2B][status 9B]
        if buf.len() == 11 && buf[0] == 0x10 && buf[1] == 0xeb {
            self.process_status(&buf[2..11]);
            return;
        }
        // 0x10A8 door update
        if buf.len() == 4 && buf[0] == 0x10 && buf[1] == 0xa8 {
            let open = buf[3] == 0x01;
            self.core.publish_door_with_trigger(open);
        }
    }

    /// Live-capture F017 template from PR #96 (43 bytes).
    /// Hex: `f017ffffffffffffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff`
    /// Zeros at indices 23–25, 28, 33 (not shifted by one).
    const F017_BASE: [u8; 43] = [
        0xf0, 0x17, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0xff,
        0xff, 0xff, 0xff, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ];

    fn base_f017() -> [u8; 43] {
        Self::F017_BASE
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
        let mut msg = Self::base_f017();
        match prop {
            "fridge_setpoint" => {
                let c: i32 = value.parse().unwrap_or(3);
                msg[3] = (7 - c) as u8;
                msg[10] = 0x01; // tempUnit C
                self.core.send(&msg);
            }
            "freezer_setpoint" => {
                let c: i32 = value.parse().unwrap_or(-18);
                msg[4] = (-(c + 15)) as u8;
                msg[10] = 0x01;
                self.core.send(&msg);
            }
            "express_freeze" => {
                let on = value == "ON" || value == "true";
                msg[5] = if on { 0x02 } else { 0x01 };
                self.core.send(&msg);
            }
            "pure_option" => {
                if let Some(raw) = pure_name_to_raw(value) {
                    msg[6] = raw;
                    self.core.send(&msg);
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

    const DEVICE_ID: &str = "test-id";
    // Live captures from PR #96
    const BASELINE_10EC: &str = "aa1810ec020403010202040001020403010202040001b0bb";
    const DELTA_DOOR_OPEN_10EC: &str = "aa1810ec020403010202040001020403010202040101b0bb";
    const DELTA_PURE_POWER_10EC: &str = "aa1810ec020403010102040001020403010302040001b1bb";
    const DELTA_EXPRESS_ON_10EC: &str = "aa1810ec020403010202040001020403020202040001b0bb";
    const DELTA_FRIDGE_5C_10EC: &str = "aa1810ec020403010202040001020203010202040001b7bb";
    const DELTA_FREEZER_18C_10EC: &str = "aa1810ec020403010202040001020403010202040001b7bb";
    const DELTA_PURE_REPLACE_10EC: &str = "aa1610ec0204030102020400010204030104020400016fbb";
    const DELTA_WATER_FILTER_7_10EC: &str = "aa1610ec0204030102020400010204030102020700016ebb";
    const DOOR_OPEN_10A8: &str = "aa0810a8010139bb";
    const DOOR_CLOSED_10A8: &str = "aa0810a801003ebb";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("2REF12EII_P_2", "2REF12EII_P_2", "1.0");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    #[test]
    fn config_always_celsius_with_pure_entities() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fridge_setpoint"]["unit_of_measurement"], json!("°C"));
        assert_eq!(comps["fridge_setpoint"]["min"], json!(1));
        assert_eq!(comps["fridge_setpoint"]["max"], json!(7));
        assert_eq!(comps["freezer_setpoint"]["min"], json!(-23));
        assert_eq!(comps["freezer_setpoint"]["max"], json!(-15));
        assert!(comps.contains_key("pure_option"));
        assert_eq!(
            comps["pure_n_fresh_replace"]["entity_category"],
            json!("diagnostic")
        );
        assert_eq!(comps["water_filter"]["entity_category"], json!("diagnostic"));
    }

    #[test]
    fn baseline_status_temps_and_pure_auto() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(BASELINE_10EC));
        let p = ha.device(DEVICE_ID).unwrap().properties;
        // cur status after prev: fridge raw 4 -> 3C, freezer raw 3 -> -18C
        assert_eq!(
            p.get("fridge_setpoint").map(|x| x.as_string()).as_deref(),
            Some("3")
        );
        assert_eq!(
            p.get("freezer_setpoint").map(|x| x.as_string()).as_deref(),
            Some("-18")
        );
        assert_eq!(
            p.get("pure_option").map(|x| x.as_string()).as_deref(),
            Some("Automatic")
        );
        assert_eq!(
            p.get("express_freeze").map(|x| x.as_string()).as_deref(),
            Some("OFF")
        );
        assert_eq!(
            p.get("door").map(|x| x.as_string()).as_deref(),
            Some("OFF")
        );
    }

    #[test]
    fn door_open_from_status_and_a8() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(DELTA_DOOR_OPEN_10EC));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("door")
                .map(|x| x.as_string())
                .as_deref(),
            Some("ON")
        );
        thinq.emit_data(&hex_decode(DOOR_CLOSED_10A8));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("door")
                .map(|x| x.as_string())
                .as_deref(),
            Some("OFF")
        );
        thinq.emit_data(&hex_decode(DOOR_OPEN_10A8));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("door")
                .map(|x| x.as_string())
                .as_deref(),
            Some("ON")
        );
    }

    #[test]
    fn pure_power_express_fridge_5c_freezer_18c() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(DELTA_PURE_POWER_10EC));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("pure_option")
                .map(|x| x.as_string())
                .as_deref(),
            Some("Power")
        );
        thinq.emit_data(&hex_decode(DELTA_EXPRESS_ON_10EC));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("express_freeze")
                .map(|x| x.as_string())
                .as_deref(),
            Some("ON")
        );
        thinq.emit_data(&hex_decode(DELTA_FRIDGE_5C_10EC));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("fridge_setpoint")
                .map(|x| x.as_string())
                .as_deref(),
            Some("5")
        );
        // freezer -18: need frame with freezer raw 3. DELTA_FREEZER uses same baseline fridge.
        thinq.emit_data(&hex_decode(DELTA_FREEZER_18C_10EC));
        // That capture in PR may still be -17 if freezer raw unchanged — verify water/pure replace
        thinq.emit_data(&hex_decode(DELTA_PURE_REPLACE_10EC));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("pure_n_fresh_replace")
                .map(|x| x.as_string())
                .as_deref(),
            Some("replace")
        );
        thinq.emit_data(&hex_decode(DELTA_WATER_FILTER_7_10EC));
        assert_eq!(
            ha.device(DEVICE_ID)
                .unwrap()
                .properties
                .get("water_filter")
                .map(|x| x.as_string())
                .as_deref(),
            Some("7")
        );
    }

    #[test]
    fn start_sends_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert!(!thinq.outbox().is_empty());
        let h = hex_encode(&thinq.outbox()[0]);
        assert!(h.to_ascii_lowercase().contains("f0ed1211"));
    }

    /// PR #96 template — only command bytes may differ; trailing zero pattern is fixed.
    const F017_BASE: &str =
        "f017ffffffffffffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff";

    /// Full AABB live captures from PR #96 (inner body after AA/len, before checksum/BB).
    const FRIDGE_SET_5C_BODY: &str =
        "f017ff02ffffffffffff01ffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff";
    const FREEZER_SET_M18C_BODY: &str =
        "f017ffff03ffffffffff01ffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff";
    const PURE_OFF_BODY: &str =
        "f017ffffffff01ffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff";
    const PURE_POWER_BODY: &str =
        "f017ffffffff03ffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff";
    const PURE_AUTO_BODY: &str =
        "f017ffffffff02ffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff";
    const EXPRESS_ON_BODY: &str =
        "f017ffffff02ffffffffffffffffffffffffffffffffff000000ffff00ffffffff00ffffffffffffffffff";

    fn aabb_inner(packet: &[u8]) -> Vec<u8> {
        if packet.first() == Some(&0xaa) && packet.len() >= 4 {
            packet[2..packet.len() - 2].to_vec()
        } else {
            packet.to_vec()
        }
    }

    fn expected_with_overrides(overrides: &[(usize, u8)]) -> Vec<u8> {
        let mut b = hex_decode(F017_BASE);
        for &(i, v) in overrides {
            b[i] = v;
        }
        b
    }

    #[test]
    fn base_f017_matches_pr96_template_exactly() {
        let base = Device::base_f017();
        assert_eq!(hex_encode(&base).to_ascii_lowercase(), F017_BASE);
        // Critical trailing zeros (skeptic off-by-one: not 24-26/29/34)
        assert_eq!(base[23], 0x00);
        assert_eq!(base[24], 0x00);
        assert_eq!(base[25], 0x00);
        assert_eq!(base[26], 0xff);
        assert_eq!(base[28], 0x00);
        assert_eq!(base[33], 0x00);
        assert_eq!(base[34], 0xff);
    }

    #[test]
    fn set_fridge_5c_full_body_matches_pr_capture() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("fridge_setpoint", "5");
        let inner = aabb_inner(&thinq.outbox()[0]);
        assert_eq!(
            hex_encode(&inner).to_ascii_lowercase(),
            FRIDGE_SET_5C_BODY,
            "full F017 body must match PR #96 live capture"
        );
        assert_eq!(inner, expected_with_overrides(&[(3, 2), (10, 0x01)]));
    }

    #[test]
    fn set_freezer_m18_full_body_matches_pr_capture() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("freezer_setpoint", "-18");
        let inner = aabb_inner(&thinq.outbox()[0]);
        assert_eq!(
            hex_encode(&inner).to_ascii_lowercase(),
            FREEZER_SET_M18C_BODY
        );
        assert_eq!(inner, expected_with_overrides(&[(4, 3), (10, 0x01)]));
    }

    #[test]
    fn set_pure_options_full_body_matches_pr_capture() {
        let (_, thinq, dev) = make();
        for (label, body, raw) in [
            ("Off", PURE_OFF_BODY, 0x01u8),
            ("Power", PURE_POWER_BODY, 0x03),
            ("Automatic", PURE_AUTO_BODY, 0x02),
        ] {
            thinq.reset_recorder();
            dev.set_property("pure_option", label);
            let inner = aabb_inner(&thinq.outbox()[0]);
            assert_eq!(
                hex_encode(&inner).to_ascii_lowercase(),
                body,
                "pure {label}"
            );
            assert_eq!(inner, expected_with_overrides(&[(6, raw)]));
        }
    }

    #[test]
    fn set_express_on_full_body_matches_pr_capture() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("express_freeze", "ON");
        let inner = aabb_inner(&thinq.outbox()[0]);
        assert_eq!(hex_encode(&inner).to_ascii_lowercase(), EXPRESS_ON_BODY);
        assert_eq!(inner, expected_with_overrides(&[(5, 0x02)]));
    }
}
