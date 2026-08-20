//! CST_570004_WW — LG ceiling-cassette IDU (deviceType 401).
//!
//! Port of upstream PR #122. Same DualCool TLV as RAC on header 0xA7, but the
//! cassette’s mode/fan/swing tables and extra entities differ — not a RAC alias.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::json;
use std::sync::Arc;

const TAG_POWER: u16 = 0x1f7;
const TAG_MODE: u16 = 0x1f9;
const TAG_FAN: u16 = 0x1fa;
const TAG_CURRENT_TEMP: u16 = 0x1fd;
const TAG_TARGET_TEMP: u16 = 0x1fe;
const TAG_SWING_V: u16 = 0x205;
const TAG_SWING_H: u16 = 0x206;
const TAG_ENERGYSAVE: u16 = 0x20d;
const TAG_AUTO_DRY: u16 = 0x20e;
const TAG_DISPLAY: u16 = 0x21f;
const TAG_AUTO_DRY_REMAIN: u16 = 0x225;
const TAG_COMFORT: u16 = 0x23f;
const TAG_POWER_W: u16 = 0x2b3;
const TAG_HUMIDITY: u16 = 0x336;
const TAG_FILTER_REMAIN: u16 = 0x355;
const TAG_FILTER_LIFE: u16 = 0x356;

const WIND_FLAGS: [u16; 5] = [0x290, 0x291, 0x3d5, 0x3d6, 0x3d7];

fn mode_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        0 => Some("cool"),
        1 => Some("dry"),
        2 => Some("fan_only"),
        3 => Some("auto"),
        _ => None,
    }
}

fn ha_to_mode(val: &str) -> Option<u32> {
    match val {
        "cool" => Some(0),
        "dry" => Some(1),
        "fan_only" => Some(2),
        "auto" => Some(3),
        _ => None,
    }
}

fn fan_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        8 => Some("auto"),
        1 => Some("very low"),
        2 => Some("low"),
        4 => Some("medium"),
        6 => Some("high"),
        7 => Some("power"),
        _ => None,
    }
}

fn ha_to_fan(val: &str) -> Option<u32> {
    match val {
        "auto" => Some(8),
        "very low" => Some(1),
        "low" => Some(2),
        "medium" => Some(4),
        "high" => Some(6),
        "power" => Some(7),
        _ => None,
    }
}

fn autodry_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        0 => Some("off"),
        1 => Some("10 min"),
        2 => Some("30 min"),
        3 => Some("60 min"),
        255 => Some("smart"),
        _ => None,
    }
}

fn ha_to_autodry(val: &str) -> Option<u32> {
    match val {
        "off" => Some(0),
        "10 min" => Some(1),
        "30 min" => Some(2),
        "60 min" => Some(3),
        "smart" => Some(255),
        _ => None,
    }
}

fn display_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        100 => Some("off"),
        150 => Some("50%"),
        200 => Some("100%"),
        _ => None,
    }
}

fn ha_to_display(val: &str) -> Option<u32> {
    match val {
        "off" => Some(100),
        "50%" => Some(150),
        "100%" => Some(200),
        _ => None,
    }
}

fn swing_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        1 => Some("on"),
        0 => Some("off"),
        _ => None,
    }
}

pub struct Device {
    pub core: Arc<TlvDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = TlvDeviceCore::new(ha, thinq);
        let this = Arc::new(Self { core: core.clone() });

        *core.is_caps_response.lock() = Some(Box::new(|tlv| tlv.iter().any(|e| e.t == 0x2da)));
        *core.is_values_response.lock() = Some(Box::new(|tlv| {
            tlv.len() >= 10 && tlv.iter().any(|e| e.t == TAG_POWER)
        }));

        let t_kv = this.clone();
        *core.on_key_value_extra.lock() = Some(Box::new(move |k, v| t_kv.on_key_value(k, v)));

        let mut config = default_config(&meta, Some(json!({"name": "LG Cassette AC"})));
        config.components.insert(
            "climate".into(),
            json!({
                "platform": "climate",
                "unique_id": "$deviceid-climate",
                "name": null,
                "temperature_unit": "C",
                "temp_step": 0.5,
                "precision": 0.5,
                "min_temp": 16,
                "max_temp": 30,
                "modes": ["off", "cool", "dry", "fan_only", "auto"],
                "fan_modes": ["auto", "very low", "low", "medium", "high", "power"],
                "swing_modes": ["on", "off"],
                "swing_horizontal_modes": ["on", "off"],
            }),
        );

        this.add_fields(&mut config);
        core.set_config(config);
        this
    }

    fn add_fields(self: &Arc<Self>, config: &mut DeviceDiscovery) {
        let core = &self.core;

        {
            let mut f = FieldDefinition::new("climate", "current_temperature")
                .with_id(TAG_CURRENT_TEMP)
                .read_only();
            f.state_topic = Some("topic".into());
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            core.add_field(config, f, true);
        }
        {
            let mut f = FieldDefinition::new("climate", "temperature").with_id(TAG_TARGET_TEMP);
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            f.write_xform = Some(Box::new(|val_str| {
                let val: f64 = val_str.parse().ok()?;
                Some(PropertyValue::Int((val.clamp(16.0, 30.0) * 2.0).round() as i64))
            }));
            f.write_attach_static = Some(vec![TAG_MODE, TAG_FAN]);
            core.add_field(config, f, true);
        }
        {
            let core_c = core.clone();
            let mut power = FieldDefinition::new("climate", "power").with_id(TAG_POWER);
            power.readable = false;
            power.write_xform = Some(Box::new(|val| {
                Some(if val == "ON" { 1i64.into() } else { 0i64.into() })
            }));
            power.write_attach = Some(Box::new(|raw| if raw != 0 { vec![TAG_MODE] } else { vec![] }));
            power.read_xform = Some(Box::new(|raw| {
                Some(if raw != 0 { "ON".into() } else { "OFF".into() })
            }));
            power.read_callback = Some(Box::new(move |_val| {
                if let Some(mode) = core_c.get_raw(TAG_MODE) {
                    core_c.process_key_value(TAG_MODE, mode);
                }
                false
            }));
            core.add_field(config, power, true);
        }
        {
            let core_r = core.clone();
            let core_w = core.clone();
            let core_cb = core.clone();
            let mut mode = FieldDefinition::new("climate", "mode").with_id(TAG_MODE);
            mode.read_xform = Some(Box::new(move |raw| {
                if core_r.get_raw(TAG_POWER) == Some(0) {
                    return Some("off".into());
                }
                mode_to_ha(raw).map(|s| s.into())
            }));
            mode.write_xform = Some(Box::new(move |val| {
                if val == "off" {
                    core_w.set_raw(TAG_POWER, 0);
                    return Some(PropertyValue::Int(core_w.get_raw(TAG_MODE).unwrap_or(0) as i64));
                }
                core_w.set_raw(TAG_POWER, 1);
                ha_to_mode(val).map(|n| PropertyValue::Int(n as i64))
            }));
            mode.write_callback = Some(Box::new(move |_num| {
                if core_cb.get_raw(TAG_POWER) == Some(0) {
                    core_cb.send(&[1, 1, 2, 1, 1], &[Tlv::new(TAG_POWER, 0)]);
                    return false;
                }
                true
            }));
            mode.write_attach_static = Some(vec![TAG_POWER, TAG_FAN, TAG_TARGET_TEMP]);
            core.add_field(config, mode, true);
        }
        {
            let mut f = FieldDefinition::new("climate", "fan_mode").with_id(TAG_FAN);
            f.read_xform = Some(Box::new(|raw| fan_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| ha_to_fan(val).map(|n| PropertyValue::Int(n as i64))));
            f.write_attach_static = Some(vec![TAG_MODE, TAG_TARGET_TEMP]);
            core.add_field(config, f, true);
        }
        {
            let mut f = FieldDefinition::new("climate", "swing_mode").with_id(TAG_SWING_V);
            f.read_xform = Some(Box::new(|raw| swing_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| {
                Some(PropertyValue::Int(if val == "on" { 1 } else { 0 }))
            }));
            core.add_field(config, f, true);
        }
        {
            let mut f = FieldDefinition::new("climate", "swing_horizontal_mode").with_id(TAG_SWING_H);
            f.read_xform = Some(Box::new(|raw| swing_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| {
                Some(PropertyValue::Int(if val == "on" { 1 } else { 0 }))
            }));
            core.add_field(config, f, true);
        }

        self.add_switch(config, TAG_ENERGYSAVE, "energysave", "Energy saving", "mdi:leaf");
        self.add_switch(config, TAG_COMFORT, "comfort_saving", "Comfort energy saving", "mdi:leaf");

        config.components.insert(
            "autodry_setting".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-autodry_setting",
                "name": "Auto dry",
                "options": ["off", "10 min", "30 min", "60 min", "smart"],
            }),
        );
        {
            let mut f = FieldDefinition::new("autodry_setting", "").with_id(TAG_AUTO_DRY);
            f.read_xform = Some(Box::new(|raw| autodry_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| {
                ha_to_autodry(val).map(|n| PropertyValue::Int(n as i64))
            }));
            core.add_field(config, f, true);
        }

        config.components.insert(
            "autodryremain".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-autodryremain",
                "name": "Auto dry remaining",
                "unit_of_measurement": "min",
                "device_class": "duration",
            }),
        );
        {
            let f = FieldDefinition::new("autodryremain", "")
                .with_id(TAG_AUTO_DRY_REMAIN)
                .read_only();
            core.add_field(config, f, true);
        }

        config.components.insert(
            "display".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-display",
                "name": "Display",
                "icon": "mdi:brightness-6",
                "options": ["off", "50%", "100%"],
            }),
        );
        {
            let mut f = FieldDefinition::new("display", "").with_id(TAG_DISPLAY);
            f.read_xform = Some(Box::new(|raw| display_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| {
                ha_to_display(val).map(|n| PropertyValue::Int(n as i64))
            }));
            core.add_field(config, f, true);
        }

        config.components.insert(
            "humidity".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-humidity",
                "name": "Humidity",
                "device_class": "humidity",
                "unit_of_measurement": "%",
                "state_class": "measurement",
                "suggested_display_precision": 0,
            }),
        );
        {
            let mut f = FieldDefinition::new("humidity", "").with_id(TAG_HUMIDITY).read_only();
            f.read_xform = Some(Box::new(|raw| {
                Some(PropertyValue::Int((raw as f64 / 10.0).round() as i64))
            }));
            core.add_field(config, f, true);
        }

        config.components.insert(
            "energy_current".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-energy_current",
                "name": "Power",
                "device_class": "power",
                "unit_of_measurement": "W",
                "state_class": "measurement",
            }),
        );
        {
            let f = FieldDefinition::new("energy_current", "").with_id(TAG_POWER_W).read_only();
            core.add_field(config, f, true);
        }

        config.components.insert(
            "filter_remaining".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-filter_remaining",
                "state_topic": "$this/filter_remaining",
                "name": "Filter remaining",
                "unit_of_measurement": "%",
                "entity_category": "diagnostic",
            }),
        );
        config.components.insert(
            "filter_used".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-filter_used",
                "state_topic": "$this/filter_used",
                "name": "Filter used",
                "unit_of_measurement": "h",
                "entity_category": "diagnostic",
            }),
        );

        config.components.insert(
            "wind_mode".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-wind_mode",
                "name": "Wind mode",
                "icon": "mdi:weather-windy",
                "entity_category": "config",
                "options": ["off", "manner", "long power", "study", "auto temp"],
                "state_topic": "$this/wind_mode",
                "command_topic": "$this/wind_mode/set",
            }),
        );
    }

    fn add_switch(self: &Arc<Self>, config: &mut DeviceDiscovery, id: u16, key: &str, name: &str, icon: &str) {
        config.components.insert(
            key.into(),
            json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{key}"),
                "name": name,
                "icon": icon,
            }),
        );
        let mut f = FieldDefinition::new(key, "").with_id(id);
        f.read_xform = Some(Box::new(|raw| {
            Some(if raw != 0 { "ON".into() } else { "OFF".into() })
        }));
        f.write_xform = Some(Box::new(|val| {
            Some(PropertyValue::Int(if val == "ON" { 1 } else { 0 }))
        }));
        self.core.add_field(config, f, true);
    }

    fn wind_mode_from_state(&self) -> &'static str {
        if self.core.get_raw(0x3d6) == Some(1) {
            "manner"
        } else if self.core.get_raw(0x3d7) == Some(1) {
            "long power"
        } else if self.core.get_raw(0x291) == Some(1) {
            "study"
        } else if self.core.get_raw(0x290) == Some(1) {
            "auto temp"
        } else {
            "off"
        }
    }

    fn on_key_value(&self, k: u16, _v: u32) {
        if WIND_FLAGS.contains(&k) {
            self.core.ha.publish_property(
                &self.core.id,
                "wind_mode",
                self.wind_mode_from_state().into(),
            );
        }
        if k == TAG_FILTER_LIFE || k == TAG_FILTER_REMAIN {
            self.publish_filter();
        }
    }

    fn publish_filter(&self) {
        let Some(life) = self.core.get_raw(TAG_FILTER_LIFE) else {
            return;
        };
        let remain = self.core.get_raw(TAG_FILTER_REMAIN).unwrap_or(0);
        let used = life.saturating_sub(remain);
        let pct = if life == 0 {
            0
        } else {
            (remain as f64 * 100.0 / life as f64).round() as i64
        };
        self.core
            .ha
            .publish_property(&self.core.id, "filter_remaining", pct.into());
        self.core
            .ha
            .publish_property(&self.core.id, "filter_used", (used as i64).into());
    }

    fn write_wind_mode(&self, mqtt_value: &str) {
        let on = match mqtt_value {
            "manner" => Some(0x3d6),
            "long power" => Some(0x3d7),
            "study" => Some(0x291),
            "auto temp" => Some(0x290),
            _ => None,
        };
        let mut tlvs = Vec::new();
        for id in WIND_FLAGS {
            let v = u32::from(Some(id) == on);
            self.core.set_raw(id, v);
            tlvs.push(Tlv::new(id, v));
        }
        self.core.send(&[1, 1, 2, 1, 1], &tlvs);
        self.core
            .ha
            .publish_property(&self.core.id, "wind_mode", mqtt_value.into());
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        if prop == "wind_mode" {
            self.write_wind_mode(value);
            return;
        }
        self.core.set_property(prop, value);
    }

    pub fn set_raw(&self, id: u16, v: u32) {
        self.core.set_raw(id, v);
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device, PropertyValue};
    use rethink_util::tlv;
    use std::collections::HashMap;

    const DEVICE_ID: &str = "test-id";
    const CAPS_RESPONSE_HEX: &str = concat!(
        "000004000000A702010077B00AB04FB0A001D5B0C1B103B4F0401011B710FEBB81BBC0BC41BC88BCC1B37",
        "01FE037B381B54EB2E01006B7600120B85020B8903CB8D020B9103CBD600203B5C0B646B61030B5C1B646",
        "B61030B5C2B646B61030B5C3B646B603B69037B6F0570004D41010BD30800400DD1020B5B010000064903",
        "EFA05EFE0"
    );
    const QUERY_RESPONSE_HEX: &str = concat!(
        "000004000000A7020400837DC07E407E867F50347F90307F0086808840D4C0D500C84181408180C940A38",
        "0A3C0A400A440F540F580F5C08340838389501E83C08FC0CD40CD00CCC0CDA0032BADA01CC6ACC0AD41B54",
        "ED56002FBD5A00960BC88D5D03CD61020C9009C40A88087D0C8AC40E9C16640668066C067006740678067C",
        "068008F80F7407C8189501E9380D558"
    );

    fn meta() -> Metadata {
        Metadata::new("CST_570004_WW", "CST_570004_WW", "1.0")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        let d = dev.clone();
        ha.on_set_property(move |id, prop, value| {
            if id == DEVICE_ID {
                d.set_property(prop, value);
            }
        });
        (ha, thinq, dev)
    }

    fn build_ready() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let (ha, thinq, dev) = make();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn components(ha: &MockHaConnection) -> serde_json::Map<String, serde_json::Value> {
        ha.device(DEVICE_ID)
            .unwrap()
            .config
            .unwrap()
            .components
            .into_iter()
            .collect()
    }

    fn sent_tlvs(pkt: &[u8]) -> HashMap<u16, u32> {
        tlv::parse(&pkt[11..pkt.len() - 2])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect()
    }

    #[test]
    fn a7_caps_triggers_values_query() {
        let (_ha, thinq, _) = make();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        assert_eq!(thinq.outbox().len(), 1);
    }

    #[test]
    fn config_is_cassette_not_wall_rac() {
        let (ha, _, _) = build_ready();
        let c = components(&ha);
        assert_eq!(c["climate"]["platform"], "climate");
        assert!(c.get("jet").is_none());
        assert!(c.get("autodry").is_none());
        assert_eq!(
            c["climate"]["modes"],
            json!(["off", "cool", "dry", "fan_only", "auto"])
        );
        assert_eq!(c["climate"]["min_temp"], 16);
        assert_eq!(c["climate"]["max_temp"], 30);
        assert!(c.contains_key("energysave"));
        assert_eq!(
            c["climate"]["fan_modes"],
            json!(["auto", "very low", "low", "medium", "high", "power"])
        );
        assert_eq!(c["climate"]["swing_modes"], json!(["on", "off"]));
        assert_eq!(c["climate"]["swing_horizontal_modes"], json!(["on", "off"]));
        assert_eq!(c["autodry_setting"]["platform"], "select");
        assert_eq!(
            c["autodry_setting"]["options"],
            json!(["off", "10 min", "30 min", "60 min", "smart"])
        );
        assert_eq!(c["autodryremain"]["unit_of_measurement"], "min");
        assert_eq!(c["display"]["platform"], "select");
        assert_eq!(c["comfort_saving"]["platform"], "switch");
        assert_eq!(c["wind_mode"]["platform"], "select");
        assert_eq!(c["humidity"]["device_class"], "humidity");
        assert_eq!(c["energy_current"]["device_class"], "power");
        assert!(c.get("filterused").is_none() && c.get("filterlife").is_none() && c.get("filterreset").is_none());
        assert_eq!(c["filter_remaining"]["unit_of_measurement"], "%");
        assert_eq!(c["filter_used"]["unit_of_measurement"], "h");
    }

    #[test]
    fn query_dump_publishes_documented_values() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("off".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some("high".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "current_temperature"),
            Some(PropertyValue::from(26.0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "temperature_state"),
            Some(PropertyValue::from(24.0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "humidity", "state"),
            Some(81i64.into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "energy_current", "state"),
            Some(0i64.into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "autodry_setting", "state"),
            Some("60 min".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "autodryremain", "state"),
            Some(30i64.into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "display", "state"),
            Some("100%".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "comfort_saving", "state"),
            Some("OFF".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "wind_mode", "state"),
            Some("off".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "filter_remaining", "state"),
            Some(32i64.into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "filter_used", "state"),
            Some(1637i64.into())
        );
    }

    #[test]
    fn auto_mode_uses_wire_3_not_rac_6() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(TAG_POWER, 1);
        dev.core.process_key_value(TAG_MODE, 3);
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("auto".into())
        );
        ha.set_property(DEVICE_ID, "climate", "mode_command", "auto");
        assert_eq!(thinq.outbox().len(), 1);
        let map = sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(map.get(&TAG_MODE).copied(), Some(3));
    }

    #[test]
    fn mode_write_while_off_attaches_power() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(TAG_POWER, 0);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "cool");
        let map = sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(map.get(&TAG_POWER).copied(), Some(1));
        assert_eq!(map.get(&TAG_MODE).copied(), Some(0));
    }

    #[test]
    fn wind_mode_is_exclusive_one_hot() {
        let (ha, thinq, _) = build_ready();
        ha.set_property(DEVICE_ID, "wind_mode", "command", "manner");
        let map = sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(map.get(&0x3d6).copied(), Some(1));
        assert_eq!(map.get(&0x290).copied(), Some(0));
        assert_eq!(map.get(&0x291).copied(), Some(0));
        assert_eq!(map.get(&0x3d5).copied(), Some(0));
        assert_eq!(map.get(&0x3d7).copied(), Some(0));
    }

    #[test]
    fn constructor_sends_query_caps() {
        let (_ha, thinq, _) = make();
        assert_eq!(thinq.outbox().len(), 1);
    }

    #[test]
    fn registry_resolves_separately_from_rac() {
        let cst = crate::registry::t2_factory("CST_570004_WW").unwrap();
        let rac = crate::registry::t2_factory("RAC_056905_WW").unwrap();
        assert!(!std::ptr::fn_addr_eq(cst, rac));
    }
}
