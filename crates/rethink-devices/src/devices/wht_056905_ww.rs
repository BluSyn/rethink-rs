//! WHT_056905_WW — LG heat pump water heater (WH27S, APHWC801M).
//!
//! TLV on kind byte 0xA7. LG's modes 25 Heat Pump, 26 Auto (heat pump first, elements on high
//! demand), 27 Turbo (elements always on) and 28 Vacation (tank kept at a minimum temperature)
//! are published as the HA built-in modes heat_pump, eco, performance and off; HA has no
//! vacation mode. Power draw (0x2b3), error code (0x221, 0 = none) and compressor frequency
//! (0x22a, 0 while idle) use the RAC family's tags. 0x229 is the hot water remaining in percent
//! and 0x232 the accumulated energy in Wh, confirmed against an external meter.
//! Port of upstream PR #184.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, ReadXform, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const TAG_POWER: u16 = 0x1f7;
const TAG_MODE: u16 = 0x1f9;
const TAG_ERROR: u16 = 0x221;
const TAG_HOT_WATER: u16 = 0x229;
const TAG_COMPRESSOR_HZ: u16 = 0x22a;
const TAG_ENERGY_WH: u16 = 0x232;
const TAG_CURRENT_TEMP: u16 = 0x255;
const TAG_TARGET_TEMP: u16 = 0x256;
const TAG_POWER_DRAW: u16 = 0x2b3;
const TAG_EEPROM_CHECKSUM: u16 = 0x2da;

const MIN_TEMP_C: f64 = 35.0;
const MAX_TEMP_C: f64 = 60.0;

const MODES: &[(&str, u32)] = &[
    ("heat_pump", 25),
    ("eco", 26),
    ("performance", 27),
    ("off", 28),
];

fn mode_label(code: u32) -> Option<&'static str> {
    MODES.iter().find(|(_, c)| *c == code).map(|(l, _)| *l)
}

fn mode_code(label: &str) -> Option<u32> {
    MODES.iter().find(|(l, _)| *l == label).map(|(_, c)| *c)
}

fn half_degrees() -> ReadXform {
    Box::new(|raw| Some(PropertyValue::from(f64::from(raw) / 2.0)))
}

pub struct Device {
    pub core: Arc<TlvDeviceCore>,
    initial_values_received: AtomicBool,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = TlvDeviceCore::new(ha, thinq);
        let this = Arc::new(Self {
            core: core.clone(),
            initial_values_received: AtomicBool::new(false),
        });

        *core.is_caps_response.lock() = Some(Box::new(|tlv| {
            tlv.iter().any(|e| e.t == TAG_EEPROM_CHECKSUM)
        }));
        *core.is_values_response.lock() =
            Some(Box::new(|tlv| tlv.iter().any(|e| e.t == TAG_POWER)));
        let t_vals = this.clone();
        *core.on_values.lock() = Some(Box::new(move || t_vals.values_received()));

        let mut config = default_config(&meta, Some(json!({"name": "LG Water Heater"})));
        config.components.insert(
            "water_heater".into(),
            json!({
                "platform": "water_heater",
                "unique_id": "$deviceid-water_heater",
                "name": null,
                "icon": "mdi:water-boiler",
                "temperature_unit": "C",
                "temp_step": 0.5,
                "min_temp": 35,
                "max_temp": 60,
                "modes": MODES.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
            }),
        );
        add_fields(&core, &mut config);
        core.set_config(config);
        this
    }

    fn values_received(&self) {
        if self.initial_values_received.swap(true, Ordering::Relaxed) {
            return;
        }
        self.core
            .thinq
            .send("setMaskingInfo", 0, json!({ "blacklist_tlv": "1200" }));
    }
}

fn add_fields(core: &TlvDeviceCore, config: &mut DeviceDiscovery) {
    let mut f = FieldDefinition::new("water_heater", "current_temperature")
        .with_id(TAG_CURRENT_TEMP)
        .read_only();
    f.state_topic = Some("topic".into());
    f.read_xform = Some(half_degrees());
    core.add_field(config, f, true);

    let mut f = FieldDefinition::new("water_heater", "mode").with_id(TAG_MODE);
    f.read_xform = Some(Box::new(|raw| mode_label(raw).map(PropertyValue::from)));
    f.write_xform = Some(Box::new(|val| mode_code(val).map(PropertyValue::from)));
    f.write_attach_static = Some(vec![TAG_TARGET_TEMP]);
    core.add_field(config, f, true);

    let mut f = FieldDefinition::new("water_heater", "temperature").with_id(TAG_TARGET_TEMP);
    f.read_xform = Some(half_degrees());
    f.write_xform = Some(Box::new(|val| {
        let num: f64 = val.parse().ok()?;
        let clamped = num.clamp(MIN_TEMP_C, MAX_TEMP_C);
        Some(PropertyValue::Int((clamped * 2.0).round() as i64))
    }));
    f.write_attach_static = Some(vec![TAG_MODE]);
    core.add_field(config, f, true);

    // Same +60 bias as the RAC family's power tag: an idle unit reads about 5 W.
    add_sensor(
        core,
        config,
        "energy",
        TAG_POWER_DRAW,
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-energy_current",
            "name": "Power",
            "device_class": "power",
            "unit_of_measurement": "W",
            "state_class": "measurement",
            "suggested_display_precision": 0,
        }),
        Some(Box::new(|raw| {
            Some(PropertyValue::Int((i64::from(raw) - 60).max(5)))
        })),
    );
    add_sensor(
        core,
        config,
        "energy_total",
        TAG_ENERGY_WH,
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-energy_total",
            "name": "Energy total",
            "device_class": "energy",
            "unit_of_measurement": "Wh",
            "state_class": "total_increasing",
            "entity_category": "diagnostic",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        "error",
        TAG_ERROR,
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-error_code",
            "name": "Error code",
            "icon": "mdi:alert",
            "entity_category": "diagnostic",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        "water",
        TAG_HOT_WATER,
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-water_level",
            "name": "Hot water remaining",
            "icon": "mdi:water-percent",
            "unit_of_measurement": "%",
            "state_class": "measurement",
            "entity_category": "diagnostic",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        "compressor",
        TAG_COMPRESSOR_HZ,
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-compressor_frequency",
            "name": "Compressor frequency",
            "icon": "mdi:fan",
            "device_class": "frequency",
            "unit_of_measurement": "Hz",
            "state_class": "measurement",
            "entity_category": "diagnostic",
        }),
        None,
    );
}

fn add_sensor(
    core: &TlvDeviceCore,
    config: &mut DeviceDiscovery,
    comp: &str,
    id: u16,
    component: Value,
    read_xform: Option<ReadXform>,
) {
    config.components.insert(comp.into(), component);
    let mut f = FieldDefinition::new(comp, "").with_id(id).read_only();
    f.read_xform = read_xform;
    core.add_field(config, f, true);
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
        self.core.set_property(prop, value);
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
    use rethink_util::tlv;
    use std::collections::HashMap;

    const DEVICE_ID: &str = "test-id";

    const CAPS_REQUEST_HEX: &str = "01010400000065020201027D416A0D";
    // Carries 0x2da (eeprom checksum) and the 0x2d7/0x2d8 mode table.
    const CAPS_RESPONSE_HEX: &str = concat!(
        "000004000000A702017E3FB01012B4901EB0C1B101B4F0020000B541B543B6A00FAEB6F0330704BD3",
        "001FD00B701BC41BD43B5D019B61078B5D01AB61078B5D01BB61078B5D01CB6107872D1"
    );
    // 0x1f9=25 heat_pump, 0x1f7=1, 0x255=119 (59.5 °C), 0x256=120 (60 °C), 0x221=0,
    // 0x229=92 %, 0x22a=0, 0x2b3=50 (5 W).
    const QUERY_RESPONSE_HEX: &str = concat!(
        "000004000000A702047F2E7E50197DC1955077959078A2407F0088408A028A505C8A808CA09CCE8CD",
        "037ACD032D56004AAD5A010E0C900AC40D555"
    );

    fn meta() -> Metadata {
        Metadata::new("WHT_056905_WW", "TEST", "1.0")
    }

    fn make_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        let d = dev.clone();
        ha.on_set_property(move |id, prop, value| {
            if id == DEVICE_ID {
                d.core.set_property(prop, value);
            }
        });
        (ha, thinq, dev)
    }

    /// Caps → values flow, with the recorder cleared afterwards.
    fn build_ready() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let (ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID)
            .expect("HA configuration published")
            .config
            .expect("config")
            .components
    }

    fn sent_tlvs(packet: &[u8]) -> Vec<(u16, u32)> {
        tlv::parse(&packet[11..packet.len() - 2])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect()
    }

    #[test]
    fn caps_and_values_responses_publish_the_config() {
        let (ha, thinq, _dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));

        let c = components(&ha);
        let wh = &c["water_heater"];
        assert_eq!(wh["platform"], "water_heater");
        assert_eq!(wh["temperature_unit"], "C");
        assert_eq!(wh["min_temp"], 35);
        assert_eq!(wh["max_temp"], 60);
        assert_eq!(wh["temp_step"], 0.5);

        assert_eq!(c["energy"]["platform"], "sensor");
        assert_eq!(c["energy"]["device_class"], "power");
        assert_eq!(c["energy"]["unit_of_measurement"], "W");
        assert_eq!(c["error"]["platform"], "sensor");
        assert_eq!(c["error"]["entity_category"], "diagnostic");
        assert_eq!(c["water"]["platform"], "sensor");
        assert_eq!(c["water"]["unit_of_measurement"], "%");

        // A named field would produce "<name>_state_topic", which HA ignores on a sensor.
        assert_eq!(c["energy"]["state_topic"], "$this/energy-");
        assert_eq!(c["error"]["state_topic"], "$this/error-");
        assert_eq!(c["water"]["state_topic"], "$this/water-");

        assert_eq!(c["compressor"]["platform"], "sensor");
        assert_eq!(c["compressor"]["device_class"], "frequency");
        assert_eq!(c["compressor"]["unit_of_measurement"], "Hz");
        assert_eq!(c["compressor"]["entity_category"], "diagnostic");
        assert_eq!(c["compressor"]["state_topic"], "$this/compressor-");
    }

    #[test]
    fn initial_state_response_publishes_expected_properties() {
        let (ha, thinq, _dev) = build_ready();
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));

        let get = |comp: &str, topic: &str| ha.get_property(DEVICE_ID, comp, topic);
        assert_eq!(
            get("water_heater", "current_temperature"),
            Some(PropertyValue::from(59.5))
        );
        assert_eq!(
            get("water_heater", "temperature_state"),
            Some(PropertyValue::from(60.0))
        );
        assert_eq!(get("water_heater", "mode_state"), Some("heat_pump".into()));
        assert_eq!(get("energy", "state"), Some(PropertyValue::Int(5)));
        assert_eq!(get("error", "state"), Some(PropertyValue::Int(0)));
        assert_eq!(get("water", "state"), Some(PropertyValue::Int(92)));
        assert_eq!(get("compressor", "state"), Some(PropertyValue::Int(0)));
    }

    #[test]
    fn constructor_sends_a_query_caps_packet() {
        let (_ha, thinq, _dev) = make_device();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CAPS_REQUEST_HEX);
    }

    #[test]
    fn mode_labels_are_ha_built_in_water_heater_modes() {
        let (ha, _thinq, dev) = build_ready();
        let allowed = [
            "off",
            "eco",
            "electric",
            "gas",
            "heat_pump",
            "high_demand",
            "performance",
        ];
        for mode in components(&ha)["water_heater"]["modes"].as_array().unwrap() {
            let mode = mode.as_str().unwrap();
            assert!(
                allowed.contains(&mode),
                "{mode:?} is a built-in HA water_heater mode"
            );
        }

        for (code, expected) in [
            (25, "heat_pump"),
            (26, "eco"),
            (27, "performance"),
            (28, "off"),
        ] {
            dev.core.set_raw(TAG_POWER, 1);
            dev.core.set_raw(TAG_MODE, code);
            dev.core.process_key_value(TAG_MODE, code);
            assert_eq!(
                ha.get_property(DEVICE_ID, "water_heater", "mode_state"),
                Some(expected.into())
            );
        }
    }

    #[test]
    fn writing_a_standard_mode_label_sends_the_device_code() {
        let (ha, thinq, dev) = build_ready();
        dev.core.set_raw(TAG_POWER, 1);
        dev.core.set_raw(TAG_MODE, 25);
        dev.core.set_raw(TAG_TARGET_TEMP, 110); // 55 °C

        ha.set_property(DEVICE_ID, "water_heater", "mode_command", "performance");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            dev.core.get_raw(TAG_MODE),
            Some(27),
            "raw state holds the performance code"
        );
        assert_eq!(
            sent_tlvs(&thinq.outbox()[0]),
            vec![(TAG_MODE, 27), (TAG_TARGET_TEMP, 110)]
        );
    }

    #[test]
    fn writing_off_selects_vacation_mode() {
        let (ha, thinq, dev) = build_ready();
        dev.core.set_raw(TAG_POWER, 1);
        dev.core.set_raw(TAG_MODE, 25);

        ha.set_property(DEVICE_ID, "water_heater", "mode_command", "off");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(dev.core.get_raw(TAG_MODE), Some(28));
    }

    #[test]
    fn target_temperature_write_is_clamped_and_attaches_the_mode() {
        let (ha, thinq, _dev) = build_ready();
        for (value, raw) in [("52.5", 105), ("70", 120), ("20", 70)] {
            thinq.reset_recorder();
            ha.set_property(DEVICE_ID, "water_heater", "temperature_command", value);
            assert_eq!(
                sent_tlvs(&thinq.outbox()[0]),
                vec![(TAG_TARGET_TEMP, raw), (TAG_MODE, 25)],
                "{value} °C"
            );
        }
    }

    #[test]
    fn energy_total_is_a_total_increasing_energy_sensor() {
        let (ha, _thinq, dev) = build_ready();
        let c = components(&ha);
        assert_eq!(c["energy_total"]["state_class"], "total_increasing");
        assert_eq!(c["energy_total"]["device_class"], "energy");

        dev.core.process_key_value(TAG_ENERGY_WH, 688943);
        assert_eq!(
            ha.get_property(DEVICE_ID, "energy_total", "state"),
            Some(PropertyValue::Int(688943))
        );
    }

    #[test]
    fn first_values_response_sends_masking_info_once() {
        let (_ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        let sent = thinq.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].cmd, "setMaskingInfo");
        assert_eq!(sent[0].msg_type, 0);
        assert_eq!(sent[0].data, json!({ "blacklist_tlv": "1200" }));
    }

    /// Upstream checks its retry and refresh timers here. The Rust core keeps no timers, so this
    /// checks the retry state it does keep, and that start() and drop send nothing.
    #[test]
    fn drop_after_ready_leaves_nothing_pending() {
        let (ha, thinq, dev) = build_ready();
        assert!(
            !dev.core.waiting_caps.load(Ordering::Relaxed),
            "caps retry cleared"
        );
        assert!(
            !dev.core.waiting_values.load(Ordering::Relaxed),
            "values retry cleared"
        );

        dev.start();
        dev.drop_device();
        assert!(thinq.outbox().is_empty());
        assert_eq!(
            ha.device(DEVICE_ID).unwrap().availability.as_deref(),
            Some("offline")
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("WHT_056905_WW").is_some());
    }
}
