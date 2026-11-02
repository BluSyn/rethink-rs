//! VENT_725601_WW — LG residential heat-recovery ventilator (ERV), e.g. Z-H0151B2SR (deviceType 407).
//!
//! TLV on 0xA7/0x87. One fan entity carries power (0x1f7), speed (0x1fa) and the preset mode (0x1f9).
//! Every TLV frame counts as both the capabilities and the values reply. CO2 and the indoor/outdoor
//! particulate readings come in a separate frame that carries the model name; they start 40 bytes
//! before it.
//!
//! Port of upstream PR #232.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::{DeviceDiscovery, HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::tlv::Tlv;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const TAG_POWER: u16 = 0x1f7;
const TAG_MODE: u16 = 0x1f9;
const TAG_SPEED: u16 = 0x1fa;
const TAG_ROOM_TEMP: u16 = 0x1fd;
const TAG_UV_NANO: u16 = 0x2a2;
const TAG_AIR_QUALITY: u16 = 0x334;
const TAG_TEMP_OUTDOOR: u16 = 0x34a;
const TAG_TEMP_EXHAUST: u16 = 0x34b;
const TAG_TEMP_INDOOR: u16 = 0x34c;
const TAG_FILTER_PRE: u16 = 0x355;
const TAG_FILTER_LIFE: u16 = 0x38d;

// The ThinQ app's Low/High/Turbo; HA sees them as speed steps 1..3.
const SPEED_LOW: u32 = 2;
const SPEED_HIGH: u32 = 6;
const SPEED_TURBO: u32 = 7;

const MODE_AUTO: u32 = 0x27;
// Bypass and ventilation are the same code, which reads back as bypass.
const PRESET_MODES: &[(&str, u32)] = &[
    ("auto", MODE_AUTO),
    ("saving", 0x25),
    ("bypass", 0x26),
    ("ventilation", 0x26),
];

const AIR_QUALITY_MARKER: &[u8] = b"Z-H0151B2SR";
const AIR_QUALITY_LEAD: usize = 40;
const AQ_CO2: usize = 0;
const AQ_IN_LEVEL: usize = 2;
const AQ_IN_PM10: usize = 3;
const AQ_IN_PM25: usize = 5;
const AQ_IN_PM1: usize = 7;
const AQ_OUT_PM10: usize = 9;
const AQ_OUT_PM25: usize = 11;
const AQ_OUT_PM1: usize = 13;
const AQ_OUT_LEVEL: usize = 16;
const CO2_LIMIT: u16 = 5000;

fn speed_step(raw: u32) -> i64 {
    match raw {
        0..=2 => 1,
        3..=6 => 2,
        _ => 3,
    }
}

fn mode_label(raw: u32) -> &'static str {
    PRESET_MODES
        .iter()
        .find(|(_, code)| *code == raw)
        .map_or("auto", |(label, _)| *label)
}

fn mode_code(label: &str) -> u32 {
    let label = label.to_lowercase();
    PRESET_MODES
        .iter()
        .find(|(l, _)| *l == label)
        .map_or(MODE_AUTO, |(_, code)| *code)
}

fn air_quality_level(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("Good"),
        2 => Some("Normal"),
        3 => Some("Bad"),
        4 => Some("Very Bad"),
        _ => None,
    }
}

fn on_off(on: bool) -> PropertyValue {
    if on { "ON" } else { "OFF" }.into()
}

/// `Number(value)`: blank reads as 0 and anything unparsable as NaN.
fn parse_number(value: &str) -> f64 {
    let value = value.trim();
    if value.is_empty() {
        0.0
    } else {
        value.parse().unwrap_or(f64::NAN)
    }
}

fn components() -> HashMap<String, Value> {
    [
        ("fan", json!({"platform":"fan","unique_id":"$deviceid-fan","name":null,"state_topic":"$this/fan-power","command_topic":"$this/fan-power/set","percentage_state_topic":"$this/fan-speed","percentage_command_topic":"$this/fan-speed/set","speed_range_min":1,"speed_range_max":3,"preset_mode_state_topic":"$this/fan-mode","preset_mode_command_topic":"$this/fan-mode/set","preset_modes":PRESET_MODES.iter().map(|(label, _)| *label).collect::<Vec<_>>(),"icon":"mdi:hvac"})),
        ("temp_controller", json!({"platform":"sensor","unique_id":"$deviceid-temp-controller","name":"Room temperature","device_class":"temperature","state_class":"measurement","unit_of_measurement":"°C","state_topic":"$this/sensor-temp_controller"})),
        ("temp_indoor", json!({"platform":"sensor","unique_id":"$deviceid-temp-indoor","name":"Intake temperature","device_class":"temperature","state_class":"measurement","unit_of_measurement":"°C","state_topic":"$this/sensor-temp_indoor","entity_category":"diagnostic"})),
        ("temp_exhaust", json!({"platform":"sensor","unique_id":"$deviceid-temp-exhaust","name":"Exhaust temperature","device_class":"temperature","state_class":"measurement","unit_of_measurement":"°C","state_topic":"$this/sensor-temp_exhaust","entity_category":"diagnostic"})),
        ("temp_outdoor", json!({"platform":"sensor","unique_id":"$deviceid-temp-outdoor","name":"Outdoor temperature","device_class":"temperature","state_class":"measurement","unit_of_measurement":"°C","state_topic":"$this/sensor-temp_outdoor","entity_category":"diagnostic"})),
        ("filter_pre", json!({"platform":"sensor","unique_id":"$deviceid-filter-pre","name":"Pre-filter life","state_class":"measurement","unit_of_measurement":"%","state_topic":"$this/sensor-filter_pre","icon":"mdi:air-filter"})),
        ("filter_life", json!({"platform":"sensor","unique_id":"$deviceid-filter-life","name":"Filter life","state_class":"measurement","unit_of_measurement":"%","state_topic":"$this/sensor-filter_life","icon":"mdi:air-filter"})),
        ("air_quality", json!({"platform":"sensor","unique_id":"$deviceid-air-quality","name":"Air quality index","state_topic":"$this/sensor-air_quality","icon":"mdi:air-filter","entity_category":"diagnostic"})),
        ("co2", json!({"platform":"sensor","unique_id":"$deviceid-co2","name":"CO2","device_class":"carbon_dioxide","state_class":"measurement","unit_of_measurement":"ppm","state_topic":"$this/sensor-co2","icon":"mdi:molecule-co2"})),
        ("in_pm1", json!({"platform":"sensor","unique_id":"$deviceid-in-pm1","name":"Indoor PM1.0","device_class":"pm1","state_class":"measurement","unit_of_measurement":"µg/m³","state_topic":"$this/sensor-in_pm1","icon":"mdi:blur"})),
        ("in_pm25", json!({"platform":"sensor","unique_id":"$deviceid-in-pm25","name":"Indoor PM2.5","device_class":"pm25","state_class":"measurement","unit_of_measurement":"µg/m³","state_topic":"$this/sensor-in_pm25","icon":"mdi:blur"})),
        ("in_pm10", json!({"platform":"sensor","unique_id":"$deviceid-in-pm10","name":"Indoor PM10","device_class":"pm10","state_class":"measurement","unit_of_measurement":"µg/m³","state_topic":"$this/sensor-in_pm10","icon":"mdi:blur"})),
        ("in_air_quality_level", json!({"platform":"sensor","unique_id":"$deviceid-in-air-quality-level","name":"Indoor air quality","state_topic":"$this/sensor-in_air_quality_level","icon":"mdi:air-filter"})),
        ("out_pm1", json!({"platform":"sensor","unique_id":"$deviceid-out-pm1","name":"Outdoor PM1.0","device_class":"pm1","state_class":"measurement","unit_of_measurement":"µg/m³","state_topic":"$this/sensor-out_pm1","icon":"mdi:blur"})),
        ("out_pm25", json!({"platform":"sensor","unique_id":"$deviceid-out-pm25","name":"Outdoor PM2.5","device_class":"pm25","state_class":"measurement","unit_of_measurement":"µg/m³","state_topic":"$this/sensor-out_pm25","icon":"mdi:blur"})),
        ("out_pm10", json!({"platform":"sensor","unique_id":"$deviceid-out-pm10","name":"Outdoor PM10","device_class":"pm10","state_class":"measurement","unit_of_measurement":"µg/m³","state_topic":"$this/sensor-out_pm10","icon":"mdi:blur"})),
        ("out_air_quality_level", json!({"platform":"sensor","unique_id":"$deviceid-out-air-quality-level","name":"Outdoor air quality","state_topic":"$this/sensor-out_air_quality_level","icon":"mdi:air-filter"})),
        ("uv_nano", json!({"platform":"switch","unique_id":"$deviceid-uv_nano","name":"UVnano","icon":"mdi:shield-sun"})),
    ]
    .into_iter()
    .map(|(key, component)| (key.to_string(), component))
    .collect()
}

pub struct Device {
    core: Arc<TlvDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = TlvDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        *core.is_caps_response.lock() = Some(Box::new(|_| true));
        *core.is_values_response.lock() = Some(Box::new(|_| true));

        let mut config = default_config(&meta, Some(json!({"name": "Ventilation"})));
        config.components = components();
        this.add_fields(&mut config);
        core.set_config(config);

        let dev = this.clone();
        thinq.on_data(Box::new(move |data| dev.process_air_quality(data)));
        this
    }

    fn add_fields(&self, config: &mut DeviceDiscovery) {
        let core = &self.core;

        let mut power = FieldDefinition::new("fan", "power").with_id(TAG_POWER);
        power.read_xform = Some(Box::new(|raw| Some(on_off(raw != 0))));
        core.add_field(config, power, false);

        let mut speed = FieldDefinition::new("fan", "speed").with_id(TAG_SPEED);
        speed.read_xform = Some(Box::new(|raw| Some(speed_step(raw).into())));
        core.add_field(config, speed, false);

        let mut mode = FieldDefinition::new("fan", "mode").with_id(TAG_MODE);
        mode.read_xform = Some(Box::new(|raw| Some(mode_label(raw).into())));
        core.add_field(config, mode, false);

        let mut uv_nano = FieldDefinition::new("uv_nano", "").with_id(TAG_UV_NANO);
        uv_nano.read_xform = Some(Box::new(|raw| Some(on_off(raw != 0))));
        core.add_field(config, uv_nano, true);

        for (id, name) in [
            (TAG_TEMP_INDOOR, "temp_indoor"),
            (TAG_TEMP_EXHAUST, "temp_exhaust"),
            (TAG_TEMP_OUTDOOR, "temp_outdoor"),
            (TAG_AIR_QUALITY, "air_quality"),
        ] {
            let field = FieldDefinition::new("sensor", name).with_id(id).read_only();
            core.add_field(config, field, false);
        }

        // Measured by the wall controller, in half degrees.
        let mut room = FieldDefinition::new("sensor", "temp_controller")
            .with_id(TAG_ROOM_TEMP)
            .read_only();
        room.read_xform = Some(Box::new(|raw| Some((f64::from(raw) / 2.0).into())));
        core.add_field(config, room, false);

        for (id, name) in [
            (TAG_FILTER_PRE, "filter_pre"),
            (TAG_FILTER_LIFE, "filter_life"),
        ] {
            let field = FieldDefinition::new("sensor", name).with_id(id).read_only();
            core.add_field(config, field, false);
        }
    }

    /// Dedupes against the same cache as the TLV fields, so a device report after an optimistic
    /// publish is not mistaken for a repeat.
    fn publish(&self, prop: &str, value: PropertyValue) {
        {
            let mut cache = self.core.publish_cache.lock();
            if cache.get(prop).is_some_and(|prev| value.eq_str(prev)) {
                return;
            }
            cache.insert(prop.to_string(), value.as_string());
        }
        self.core.ha.publish_property(&self.core.id, prop, value);
    }

    fn process_air_quality(&self, buf: &[u8]) {
        let Some(marker) = buf
            .windows(AIR_QUALITY_MARKER.len())
            .position(|window| window == AIR_QUALITY_MARKER)
        else {
            return;
        };
        let Some(start) = marker.checked_sub(AIR_QUALITY_LEAD) else {
            return;
        };
        let rec = &buf[start..marker];
        let word = |offset: usize| u16::from_be_bytes([rec[offset], rec[offset + 1]]);
        let reading = |offset: usize| PropertyValue::from(i64::from(word(offset)));

        if let Some(level) = air_quality_level(rec[AQ_IN_LEVEL]) {
            self.publish("sensor-in_air_quality_level", level.into());
            self.publish("sensor-in_pm1", reading(AQ_IN_PM1));
            self.publish("sensor-in_pm25", reading(AQ_IN_PM25));
            self.publish("sensor-in_pm10", reading(AQ_IN_PM10));
        }
        if let Some(level) = air_quality_level(rec[AQ_OUT_LEVEL]) {
            self.publish("sensor-out_air_quality_level", level.into());
            self.publish("sensor-out_pm1", reading(AQ_OUT_PM1));
            self.publish("sensor-out_pm25", reading(AQ_OUT_PM25));
            self.publish("sensor-out_pm10", reading(AQ_OUT_PM10));
        }
        if word(AQ_CO2) < CO2_LIMIT {
            self.publish("sensor-co2", reading(AQ_CO2));
        }
    }

    fn write(&self, tlvs: &[Tlv]) {
        self.core.send(&[1, 1, 2, 1, 1], tlvs);
    }

    /// The stored speed, falling back to (and storing) Low.
    fn speed_or_low(&self) -> u32 {
        match self.core.get_raw(TAG_SPEED) {
            Some(raw) if raw > 0 => raw,
            _ => {
                self.core.set_raw(TAG_SPEED, SPEED_LOW);
                SPEED_LOW
            }
        }
    }

    /// The stored mode, falling back to (and storing) auto.
    fn mode_or_auto(&self) -> u32 {
        self.core.get_raw(TAG_MODE).unwrap_or_else(|| {
            self.core.set_raw(TAG_MODE, MODE_AUTO);
            MODE_AUTO
        })
    }

    fn power_off(&self) {
        self.core.set_raw(TAG_POWER, 0);
        self.write(&[Tlv::new(TAG_POWER, 0)]);
        self.publish("fan-power", on_off(false));
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        match prop {
            "fan-power" if value == "ON" => {
                self.core.set_raw(TAG_POWER, 1);
                let speed = self.speed_or_low();
                let mode = self.mode_or_auto();
                self.write(&[
                    Tlv::new(TAG_POWER, 1),
                    Tlv::new(TAG_MODE, mode),
                    Tlv::new(TAG_SPEED, speed),
                ]);
                self.publish("fan-power", on_off(true));
                self.publish("fan-speed", speed_step(speed).into());
            }
            "fan-power" => self.power_off(),
            "fan-speed" => {
                let mut step = parse_number(value).round();
                if step > 3.0 {
                    // a 0..100 percentage rather than a speed step
                    step = if step <= 33.0 {
                        1.0
                    } else if step <= 66.0 {
                        2.0
                    } else {
                        3.0
                    };
                }
                if step <= 0.0 {
                    self.power_off();
                    return;
                }
                let speed = match step as i64 {
                    1 => SPEED_LOW,
                    2 => SPEED_HIGH,
                    _ => SPEED_TURBO,
                };
                self.core.set_raw(TAG_SPEED, speed);
                self.core.set_raw(TAG_POWER, 1);
                let mode = self.mode_or_auto();
                self.write(&[
                    Tlv::new(TAG_POWER, 1),
                    Tlv::new(TAG_SPEED, speed),
                    Tlv::new(TAG_MODE, mode),
                ]);
                self.publish("fan-power", on_off(true));
                self.publish("fan-speed", step.into());
            }
            "fan-mode" => {
                let mode = mode_code(value);
                self.core.set_raw(TAG_MODE, mode);
                self.core.set_raw(TAG_POWER, 1);
                let speed = self.speed_or_low();
                self.write(&[
                    Tlv::new(TAG_POWER, 1),
                    Tlv::new(TAG_MODE, mode),
                    Tlv::new(TAG_SPEED, speed),
                ]);
                self.publish("fan-power", on_off(true));
                self.publish("fan-mode", value.into());
            }
            "uv_nano-" => {
                let on = u32::from(value == "ON");
                self.core.set_raw(TAG_UV_NANO, on);
                self.write(&[Tlv::new(TAG_UV_NANO, on)]);
                self.publish("uv_nano-", value.into());
            }
            _ => self.core.set_property(prop, value),
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};
    use rethink_util::tlv;

    const DEVICE_ID: &str = "test-id";

    // Real captured packets from an LG residential ERV unit (Z-H0151B2SR).
    const CAPS_RESPONSE_HEX: &str = "000004000000A702010049B004B4A001C0B0A001C4B4D011B710B4BC81B2C0B330400002B77001F01EBD43D401BC600601BD200410B5D025B642B600B5D026B642B600B5D027B642B600B69042B6F0725601FA0156CC";
    const QUERY_RESPONSE_HEX: &str = "000004000000A7020400317DC17F50337E827E502786808840CD4ACD08CCC7D31023D2D01BD29019CD8096C0CA80A8818580D5501CE35064BC81C900BEF7";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("VENT_725601_WW", "Z-H0151B2SR", "725601");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);
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

    fn prop(ha: &MockHaConnection, name: &str) -> Option<PropertyValue> {
        ha.device(DEVICE_ID)?.properties.get(name).cloned()
    }

    /// The TLVs of the first packet sent: after the 11-byte header, before the CRC.
    fn written(thinq: &MockThinq2Device) -> Vec<(u16, u32)> {
        let out = &thinq.outbox()[0];
        tlv::parse(&out[11..out.len() - 2])
            .into_iter()
            .map(|el| (el.t, el.v))
            .collect()
    }

    fn value_of(tlvs: &[(u16, u32)], t: u16) -> Option<u32> {
        tlvs.iter().find(|(tag, _)| *tag == t).map(|(_, v)| *v)
    }

    #[test]
    fn config_exposes_expected_fan_switch_and_sensor_components() {
        let (ha, _, _) = build_ready();

        let device = ha.device(DEVICE_ID).expect("HA configuration published");
        let components = device.config.unwrap().components;

        let fan = components.get("fan").expect("fan component present");
        assert_eq!(fan["platform"], "fan");
        assert_eq!(fan["speed_range_min"], 1);
        assert_eq!(fan["speed_range_max"], 3);
        assert_eq!(
            fan["preset_modes"],
            json!(["auto", "saving", "bypass", "ventilation"])
        );

        let uv_nano = components
            .get("uv_nano")
            .expect("uv_nano switch component present");
        assert_eq!(uv_nano["platform"], "switch");

        for key in [
            "temp_controller",
            "temp_indoor",
            "temp_exhaust",
            "temp_outdoor",
            "filter_pre",
            "filter_life",
        ] {
            let sensor = components
                .get(key)
                .unwrap_or_else(|| panic!("{key} present"));
            assert_eq!(sensor["platform"], "sensor", "{key}");
        }
    }

    #[test]
    fn query_response_publishes_all_expected_ha_properties() {
        let (ha, _, _) = build_ready();
        let state = |component: &str, topic: &str| ha.get_property(DEVICE_ID, component, topic);

        assert_eq!(state("fan", "state"), Some("ON".into()));
        assert_eq!(
            state("fan", "percentage_state"),
            Some(PropertyValue::Int(1))
        );
        assert_eq!(state("fan", "preset_mode_state"), Some("auto".into()));
        assert_eq!(
            state("temp_controller", "state"),
            Some(PropertyValue::from(25.5))
        );
        assert_eq!(state("temp_indoor", "state"), Some(PropertyValue::Int(35)));
        assert_eq!(state("temp_exhaust", "state"), Some(PropertyValue::Int(27)));
        assert_eq!(state("temp_outdoor", "state"), Some(PropertyValue::Int(25)));
        assert_eq!(state("filter_pre", "state"), Some(PropertyValue::Int(28)));
        assert_eq!(state("filter_life", "state"), Some(PropertyValue::Int(100)));
        assert_eq!(state("uv_nano", "state"), Some("ON".into()));
    }

    #[test]
    fn ha_write_fan_power_off_emits_expected_tlv_packet() {
        let (ha, thinq, _) = build_ready();

        ha.set_property(DEVICE_ID, "fan", "command", "OFF");

        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(written(&thinq), vec![(0x1f7, 0)]);
        assert_eq!(prop(&ha, "fan-power"), Some("OFF".into()));
    }

    #[test]
    fn ha_write_fan_power_on_emits_combined_packet_with_default_speed_and_mode() {
        let (ha, thinq, _) = build_ready();

        // Set explicit mode and turn OFF first
        ha.set_property(DEVICE_ID, "fan", "preset_mode_command", "auto");
        ha.set_property(DEVICE_ID, "fan", "command", "OFF");
        thinq.reset_recorder();

        ha.set_property(DEVICE_ID, "fan", "command", "ON");

        assert_eq!(thinq.outbox().len(), 1);
        let tlvs = written(&thinq);
        assert_eq!(value_of(&tlvs, 0x1f7), Some(1));
        assert_eq!(value_of(&tlvs, 0x1f9), Some(0x27));
        assert_eq!(value_of(&tlvs, 0x1fa), Some(2));
        assert_eq!(prop(&ha, "fan-power"), Some("ON".into()));
    }

    #[test]
    fn ha_write_fan_percentage_speed_mapping() {
        let (ha, thinq, _) = build_ready();

        // Speed 2 (High): raw 6
        ha.set_property(DEVICE_ID, "fan", "percentage_command", "2");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(value_of(&written(&thinq), 0x1fa), Some(6));
        assert_eq!(prop(&ha, "fan-speed"), Some(PropertyValue::Int(2)));

        thinq.reset_recorder();

        // Speed 3 (Turbo): raw 7
        ha.set_property(DEVICE_ID, "fan", "percentage_command", "3");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(value_of(&written(&thinq), 0x1fa), Some(7));
        assert_eq!(prop(&ha, "fan-speed"), Some(PropertyValue::Int(3)));
    }

    #[test]
    fn ha_write_fan_preset_modes_mapping() {
        let (ha, thinq, _) = build_ready();

        ha.set_property(DEVICE_ID, "fan", "preset_mode_command", "saving");
        assert_eq!(value_of(&written(&thinq), 0x1f9), Some(0x25));
        assert_eq!(prop(&ha, "fan-mode"), Some("saving".into()));

        thinq.reset_recorder();

        ha.set_property(DEVICE_ID, "fan", "preset_mode_command", "bypass");
        assert_eq!(value_of(&written(&thinq), 0x1f9), Some(0x26));
        assert_eq!(prop(&ha, "fan-mode"), Some("bypass".into()));
    }

    #[test]
    fn ha_write_uv_nano_switch() {
        let (ha, thinq, _) = build_ready();

        ha.set_property(DEVICE_ID, "uv_nano", "command", "OFF");
        assert_eq!(value_of(&written(&thinq), 0x2a2), Some(0));

        thinq.reset_recorder();

        ha.set_property(DEVICE_ID, "uv_nano", "command", "ON");
        assert_eq!(value_of(&written(&thinq), 0x2a2), Some(1));
    }

    #[test]
    fn air_quality_frame_publishes_co2_and_particulates() {
        // Synthetic (no capture in the PR): the readings, then the model name 40 bytes later.
        const AIR_QUALITY_HEX: &str = "0000000000026402001E0014000A012C00C80064000400000000000000000000000000000000000000000000005A2D48303135314232535200000000";
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(AIR_QUALITY_HEX));
        assert_eq!(prop(&ha, "sensor-co2"), Some(PropertyValue::Int(612)));
        assert_eq!(
            prop(&ha, "sensor-in_air_quality_level"),
            Some("Normal".into())
        );
        assert_eq!(prop(&ha, "sensor-in_pm1"), Some(PropertyValue::Int(10)));
        assert_eq!(prop(&ha, "sensor-in_pm25"), Some(PropertyValue::Int(20)));
        assert_eq!(prop(&ha, "sensor-in_pm10"), Some(PropertyValue::Int(30)));
        assert_eq!(
            prop(&ha, "sensor-out_air_quality_level"),
            Some("Very Bad".into())
        );
        assert_eq!(prop(&ha, "sensor-out_pm1"), Some(PropertyValue::Int(100)));
        assert_eq!(prop(&ha, "sensor-out_pm25"), Some(PropertyValue::Int(200)));
        assert_eq!(prop(&ha, "sensor-out_pm10"), Some(PropertyValue::Int(300)));

        // Out-of-range levels and CO2 publish nothing.
        let mut rejected = hex_decode(AIR_QUALITY_HEX);
        rejected[5..7].copy_from_slice(&CO2_LIMIT.to_be_bytes());
        rejected[7] = 0;
        rejected[21] = 5;
        let (ha, thinq, _) = make();
        thinq.emit_data(&rejected);
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());
    }

    #[test]
    fn registry_resolves_model_and_aliases() {
        let vent = crate::registry::t2_factory("VENT_725601_WW").unwrap();
        for alias in ["VENT_725601", "Z-H0151B2SR"] {
            let factory = crate::registry::t2_factory(alias).unwrap();
            assert!(std::ptr::fn_addr_eq(vent, factory), "{alias}");
        }
    }
}
