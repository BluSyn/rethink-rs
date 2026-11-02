//! AIR_910604_WW — LG PuriCare AS309DWA air purifier (deviceType 402).
//!
//! TLV on kind byte 0xA7. The appliance sends full state reports and short changed-tag
//! notifications unprompted and does not answer the caps query, so nothing is queried and
//! discovery is published at construction from a fixed field map. Every write reproduces a
//! command frame the LG app was captured sending. A write that arrives while the unit is off is
//! acknowledged and ignored, so mode and fan writes force power on and attach it. In auto mode
//! the appliance drives the fan itself and ignores a bare fan write; that is left alone. Setting
//! the sleep timer also switches the clean indicator off, which the appliance reports itself.
//! The internal temperature (0x1fd) and humidity (0x336) do not track the room and are not
//! published. Port of upstream PR #172.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::{json, Value};
use std::sync::Arc;

const TAG_POWER: u16 = 0x1f7;
const TAG_MODE: u16 = 0x1f9;
const TAG_FAN: u16 = 0x1fa;
const TAG_SLEEP_TIMER: u16 = 0x21a;
const TAG_ERROR: u16 = 0x221;
const TAG_AIR_QUALITY: u16 = 0x240;
const TAG_ODOR: u16 = 0x241;
const TAG_INDICATOR: u16 = 0x24e;
const TAG_BOOSTER_FAN: u16 = 0x326;
const TAG_ROTATION: u16 = 0x327;
const TAG_PM1: u16 = 0x333;
const TAG_PM25: u16 = 0x334;
const TAG_PM10: u16 = 0x335;
const TAG_FILTER_REMAINING: u16 = 0x355;
const TAG_FILTER_LIFE: u16 = 0x356;
const TAG_STERILIZE: u16 = 0x360;
const TAG_TOP_FILTER_REMAINING: u16 = 0x363;
const TAG_TOP_FILTER_LIFE: u16 = 0x364;

const MODES: &[(&str, u32)] = &[
    ("clean_booster", 13),
    ("single_clean", 14),
    ("dual_clean", 15),
    ("auto", 16),
];

const FAN_SPEEDS: &[(&str, u32)] = &[
    ("low", 2),
    ("medium", 4),
    ("high", 6),
    ("turbo", 7),
    ("auto", 8),
];

fn options(table: &[(&'static str, u32)]) -> Vec<&'static str> {
    table.iter().map(|(l, _)| *l).collect()
}

fn label(table: &[(&'static str, u32)], code: u32) -> &'static str {
    table
        .iter()
        .find(|(_, c)| *c == code)
        .map_or("None", |(l, _)| *l)
}

fn code(table: &[(&str, u32)], label: &str) -> Option<u32> {
    table.iter().find(|(l, _)| *l == label).map(|(_, c)| *c)
}

fn on_off(raw: u32) -> Option<PropertyValue> {
    Some(if raw != 0 { "ON" } else { "OFF" }.into())
}

fn on_off_write(val: &str) -> Option<PropertyValue> {
    Some(PropertyValue::Int(i64::from(val == "ON")))
}

/// Same minimum-count guard as RAC_056905_WW: a power-only notification is not a full report.
fn is_values_response(tlv: &[Tlv]) -> bool {
    tlv.len() >= 10 && tlv.iter().any(|e| e.t == TAG_POWER)
}

pub struct Device {
    pub core: Arc<TlvDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = TlvDeviceCore::with_caps_query(ha, thinq, false);
        *core.is_values_response.lock() = Some(Box::new(is_values_response));

        let mut config = default_config(&meta, Some(json!({"name": "LG PuriCare Air Purifier"})));
        config.components.insert(
            "fan".into(),
            json!({
                "platform": "fan",
                "unique_id": "$deviceid-fan",
                "name": null,
                "icon": "mdi:air-purifier",
                "preset_modes": options(FAN_SPEEDS),
            }),
        );
        config.components.insert(
            "mode".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-mode",
                "name": "Mode",
                "icon": "mdi:tune-variant",
                "options": options(MODES),
            }),
        );
        config.components.insert(
            "clean_booster_fan_speed".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-clean_booster_fan_speed",
                "name": "Clean booster fan speed",
                "icon": "mdi:fan",
                "options": options(FAN_SPEEDS),
            }),
        );
        config.components.insert(
            "sleep_timer".into(),
            json!({
                "platform": "number",
                "unique_id": "$deviceid-sleep_timer",
                "name": "Sleep timer",
                "icon": "mdi:sleep",
                "device_class": "duration",
                "unit_of_measurement": "min",
                "min": 0,
                "max": 720,
                "step": 10,
                "mode": "box",
            }),
        );
        add_fields(&core, &mut config);
        core.set_config(config);
        Arc::new(Self { core })
    }
}

fn add_fields(core: &Arc<TlvDeviceCore>, config: &mut DeviceDiscovery) {
    // Switching on restores the last mode and fan speed in the same frame, as the app does.
    let mut f = FieldDefinition::new("fan", "").with_id(TAG_POWER);
    f.read_xform = Some(Box::new(on_off));
    f.write_xform = Some(Box::new(on_off_write));
    f.write_attach = Some(Box::new(|raw| {
        if raw != 0 {
            vec![TAG_MODE, TAG_FAN]
        } else {
            vec![]
        }
    }));
    core.add_field(config, f, true);

    add_powered_enum(
        core,
        config,
        TAG_FAN,
        "fan",
        "preset_mode",
        FAN_SPEEDS,
        &[TAG_POWER, TAG_MODE],
    );
    add_powered_enum(
        core,
        config,
        TAG_MODE,
        "mode",
        "",
        MODES,
        &[TAG_POWER, TAG_FAN],
    );
    add_powered_enum(
        core,
        config,
        TAG_BOOSTER_FAN,
        "clean_booster_fan_speed",
        "",
        FAN_SPEEDS,
        &[TAG_POWER],
    );

    add_switch(
        core,
        config,
        TAG_ROTATION,
        "circulation_rotation",
        "Circulation fan rotation",
        "mdi:rotate-3d-variant",
    );
    add_switch(
        core,
        config,
        TAG_INDICATOR,
        "clean_indicator",
        "Clean indicator",
        "mdi:lightbulb",
    );
    add_switch(
        core,
        config,
        TAG_STERILIZE,
        "air_sanitization",
        "Air sanitization",
        "mdi:shield-sun",
    );

    // Minutes, counting down, so the entity shows the live remaining time.
    let mut f = FieldDefinition::new("sleep_timer", "").with_id(TAG_SLEEP_TIMER);
    f.write_xform = Some(Box::new(|val| {
        val.parse::<f64>().ok().map(PropertyValue::from)
    }));
    core.add_field(config, f, true);

    for (id, comp, name) in [
        (TAG_PM1, "pm1", "PM1.0"),
        (TAG_PM25, "pm25", "PM2.5"),
        (TAG_PM10, "pm10", "PM10"),
    ] {
        add_sensor(
            core,
            config,
            id,
            comp,
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{comp}"),
                "name": name,
                "device_class": comp,
                "unit_of_measurement": "µg/m³",
                "state_class": "measurement",
                "suggested_display_precision": 0,
            }),
        );
    }
    add_sensor(
        core,
        config,
        TAG_AIR_QUALITY,
        "air_quality",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-air_quality",
            "name": "Air quality index",
            "icon": "mdi:weather-hazy",
            "state_class": "measurement",
        }),
    );
    add_sensor(
        core,
        config,
        TAG_ODOR,
        "odor",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-odor",
            "name": "Odor index",
            "icon": "mdi:scent",
            "state_class": "measurement",
        }),
    );
    add_sensor(
        core,
        config,
        TAG_ERROR,
        "error",
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-error",
            "name": "Error code",
            "icon": "mdi:alert",
            "entity_category": "diagnostic",
        }),
    );
    // LG calls 0x355/0x363 "useTime", but they count the hours remaining.
    for (id, comp, name) in [
        (
            TAG_FILTER_REMAINING,
            "filter_remaining_time",
            "Filter remaining time",
        ),
        (TAG_FILTER_LIFE, "filter_life_time", "Filter life time"),
        (
            TAG_TOP_FILTER_REMAINING,
            "top_filter_remaining_time",
            "Top filter remaining time",
        ),
        (
            TAG_TOP_FILTER_LIFE,
            "top_filter_life_time",
            "Top filter life time",
        ),
    ] {
        add_sensor(
            core,
            config,
            id,
            comp,
            json!({
                "platform": "sensor",
                "unique_id": format!("$deviceid-{comp}"),
                "name": name,
                "icon": "mdi:air-filter",
                "device_class": "duration",
                "unit_of_measurement": "h",
                "entity_category": "diagnostic",
            }),
        );
    }
}

/// Writes force power on and attach it, since the unit ignores them while off.
fn add_powered_enum(
    core: &Arc<TlvDeviceCore>,
    config: &mut DeviceDiscovery,
    id: u16,
    comp: &str,
    name: &str,
    table: &'static [(&'static str, u32)],
    attach: &[u16],
) {
    let core_w = core.clone();
    let mut f = FieldDefinition::new(comp, name).with_id(id);
    f.read_xform = Some(Box::new(move |raw| Some(label(table, raw).into())));
    f.write_xform = Some(Box::new(move |val| {
        core_w.set_raw(TAG_POWER, 1);
        code(table, val).map(PropertyValue::from)
    }));
    f.write_attach_static = Some(attach.to_vec());
    core.add_field(config, f, true);
}

fn add_switch(
    core: &TlvDeviceCore,
    config: &mut DeviceDiscovery,
    id: u16,
    comp: &str,
    name: &str,
    icon: &str,
) {
    config.components.insert(
        comp.into(),
        json!({
            "platform": "switch",
            "unique_id": format!("$deviceid-{comp}"),
            "name": name,
            "icon": icon,
        }),
    );
    let mut f = FieldDefinition::new(comp, "").with_id(id);
    f.read_xform = Some(Box::new(on_off));
    f.write_xform = Some(Box::new(on_off_write));
    core.add_field(config, f, true);
}

fn add_sensor(
    core: &TlvDeviceCore,
    config: &mut DeviceDiscovery,
    id: u16,
    comp: &str,
    component: Value,
) {
    config.components.insert(comp.into(), component);
    core.add_field(
        config,
        FieldDefinition::new(comp, "").with_id(id).read_only(),
        true,
    );
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};
    use rethink_util::crc16::crc16;
    use rethink_util::tlv;
    use std::collections::HashMap;

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "AIR_910604_WW";

    // Captured from the owner's unit. Full state: power on, single clean, clean booster fan
    // auto, rotation, clean indicator and air sanitization on, PM1/PM2.5/PM10 = 8, humidity 30 %.
    const BASE_HEX: &str = concat!(
        "000004000000A7020464627DC17E4E7E887F007F807F5050868086C087008980D7C0D801D840D88087809381",
        "CD48CD08CCC890419001CD901E8840D55022D5A00FA0D8E00C75D9200FA0CE80AB00C988C9C1CA00B5D010",
        "B600B648B5CDB600B648B5CFB600B648B5CEB60EB64EAFAA"
    );

    // Owner-labelled transition frames.
    const POWER_OFF_HEX: &str = "000004000000A702048C0A7DC09380AC600100AC84ACD5";
    const POWER_ON_HEX: &str = "000004000000A702048F0A7DC19381AC600100AC80BA8C";
    const CLEAN_BOOSTER_HEX: &str = "000004000000A7020455027E4D9A6B";
    const SINGLE_CLEAN_HEX: &str = "000004000000A7020430027E4E4F9F";
    const DUAL_CLEAN_HEX: &str = concat!(
        "000004000000A7020425627DC17E4F7E887F007F807F5050868086C087008980D7C0D801D840D88087809381",
        "CD48CD08CCC890419001CD901E8840D55022D5A00FA0D8E00C75D9200FA0CE80AB00C988C9C1CA00B5D010",
        "B600B648B5CDB600B648B5CFB600B648B5CEB60EB64E6B03"
    );
    const AUTO_HEX: &str = "000004000000A702044E037E5010B0FC";
    const BOOSTER_HIGH_HEX: &str = "000004000000A702046702C986B457";
    const BOOSTER_TURBO_HEX: &str = "000004000000A702046B02C987EB44";
    // Prior exact-model capture: main fan low plus the remembered-per-mode rows.
    const MAIN_FAN_LOW_HEX: &str =
        "000004000000A702047B147E82B5CDB600B642B5CFB600B642B5CEB60EB64E5078";
    const ROTATION_OFF_HEX: &str = "000004000000A702046D02C9C0F4FE";
    const ROTATION_ON_HEX: &str = "000004000000A702046E02C9C17F03";
    const INDICATOR_OFF_HEX: &str = "000004000000A7020480029380E022";
    const INDICATOR_ON_HEX: &str = "000004000000A70204820293811D6B";
    const SANITIZATION_OFF_HEX: &str = "000004000000A702048402D8006A6D";
    const SANITIZATION_ON_HEX: &str = "000004000000A702048502D8010CF8";
    const SLEEP_120_HEX: &str = "000004000000A702048605869078938038E2";
    const SLEEP_119_HEX: &str = "000004000000A702048A038690774AA4";
    const SLEEP_CANCEL_HEX: &str = "000004000000A702048B04868093817146";
    const TRUNCATED_HEX: &str = "000004000000A7020464627DC17E4E7E88";

    // Command frames the LG app sent to the same unit; each write must reproduce the TLVs of one.
    const CMD_MODE_SINGLE: &str = "01010400000065020100027e4e56d7";
    const CMD_MODE_BOOSTER: &str = "01010400000065020100027e4d66b4";
    const CMD_BOOSTER_MEDIUM: &str = "0101040000006502010002c984a94b";
    const CMD_BOOSTER_TURBO: &str = "0101040000006502010002c9879928";
    const CMD_ROTATION_OFF: &str = "0101040000006502010002c9c0a10b";
    const CMD_ROTATION_ON: &str = "0101040000006502010002c9c1b12a";
    const CMD_INDICATOR_OFF: &str = "0101040000006502010002938008bb";
    const CMD_INDICATOR_ON: &str = "01010400000065020100029381189a";
    const CMD_STERILIZE_OFF: &str = "0101040000006502010002d8004805";
    const CMD_STERILIZE_ON: &str = "0101040000006502010002d8015824";
    const CMD_SLEEP_CANCEL: &str = "01010400000065020100028680f43d";

    fn meta() -> Metadata {
        Metadata::new(MODEL_ID, "TEST", "1.0")
    }

    fn make_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    /// A fresh device that has seen the full state frame.
    fn with_base() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let (ha, thinq, dev) = make_device();
        thinq.emit_data(&hex_decode(BASE_HEX));
        (ha, thinq, dev)
    }

    fn state(ha: &MockHaConnection, comp: &str) -> Option<PropertyValue> {
        ha.get_property(DEVICE_ID, comp, "state")
    }

    fn frame_tlvs(frame: &[u8]) -> HashMap<u16, u32> {
        tlv::parse(&frame[11..11 + frame[10] as usize])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect()
    }

    /// TLVs of a captured toDevice command frame.
    fn command_tlvs(hex: &str) -> HashMap<u16, u32> {
        frame_tlvs(&hex_decode(hex))
    }

    /// TLVs the handler actually put on the wire.
    fn sent_tlvs(thinq: &MockThinq2Device) -> HashMap<u16, u32> {
        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 1, "exactly one frame should be sent");
        frame_tlvs(&outbox[0])
    }

    #[test]
    fn every_capture_fixture_has_a_valid_envelope_length_and_crc() {
        for frame in [
            BASE_HEX,
            POWER_OFF_HEX,
            POWER_ON_HEX,
            CLEAN_BOOSTER_HEX,
            SINGLE_CLEAN_HEX,
            DUAL_CLEAN_HEX,
            AUTO_HEX,
            BOOSTER_HIGH_HEX,
            BOOSTER_TURBO_HEX,
            MAIN_FAN_LOW_HEX,
            ROTATION_OFF_HEX,
            ROTATION_ON_HEX,
            INDICATOR_OFF_HEX,
            INDICATOR_ON_HEX,
            SANITIZATION_OFF_HEX,
            SANITIZATION_ON_HEX,
            SLEEP_120_HEX,
            SLEEP_119_HEX,
            SLEEP_CANCEL_HEX,
        ] {
            let packet = hex_decode(frame);
            assert_eq!(packet[6], 0xa7, "{frame}");
            assert_eq!(packet[10] as usize, packet.len() - 13, "{frame}");
            assert_eq!(crc16(&packet[2..]), 0, "{frame}");
        }
    }

    #[test]
    fn a_power_only_notification_is_not_a_full_values_response() {
        let (_ha, _thinq, dev) = make_device();
        let hook = dev.core.is_values_response.lock();
        let is_values = hook.as_ref().expect("values hook installed");
        assert!(!is_values(&[Tlv::new(0x1f7, 1)]));
        let mut full = vec![Tlv::new(0x1f7, 1)];
        full.extend((0..9u16).map(|i| Tlv::new(0x200 + i, u32::from(i))));
        assert!(is_values(&full));
    }

    #[test]
    fn discovery_is_immediate_and_a_delta_first_connection_publishes_state() {
        let (ha, thinq, _dev) = make_device();
        assert!(ha.device(DEVICE_ID).and_then(|d| d.config).is_some());
        thinq.emit_data(&hex_decode(DUAL_CLEAN_HEX));
        assert_eq!(state(&ha, "mode"), Some("dual_clean".into()));
        assert!(thinq.outbox().is_empty());
    }

    /// Upstream drives this through the HA bridge, which lives in rethink-cloud; the registry
    /// factory plus start() and drop is the same path here.
    #[test]
    fn registry_selects_this_handler_for_the_exact_model_id() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let factory = crate::registry::t2_factory(MODEL_ID).expect("registered");
        let handler = factory(ha.clone(), thinq.clone(), meta());
        handler.start();
        thinq.emit_data(&hex_decode(BASE_HEX));
        assert!(ha.device(DEVICE_ID).and_then(|d| d.config).is_some());
        assert_eq!(state(&ha, "mode"), Some("single_clean".into()));
        assert!(thinq.outbox().is_empty());
        assert!(thinq.sent().is_empty());
        handler.drop_device();
    }

    #[test]
    fn real_full_frame_publishes_discovery_and_initial_state() {
        let (ha, thinq, _dev) = with_base();
        let c = ha
            .device(DEVICE_ID)
            .expect("HA configuration published")
            .config
            .expect("config")
            .components;

        assert_eq!(
            c["mode"]["options"],
            json!(["clean_booster", "single_clean", "dual_clean", "auto"])
        );
        assert_eq!(
            c["fan"]["preset_modes"],
            json!(["low", "medium", "high", "turbo", "auto"])
        );
        assert_eq!(
            c["clean_booster_fan_speed"]["options"],
            json!(["low", "medium", "high", "turbo", "auto"])
        );
        assert!(
            !c.contains_key("humidity"),
            "internal humidity is not room humidity"
        );
        // Sensors stay strictly read-only even though the appliance is writable.
        for name in [
            "pm1",
            "pm25",
            "pm10",
            "air_quality",
            "odor",
            "error",
            "filter_remaining_time",
            "filter_life_time",
            "top_filter_remaining_time",
            "top_filter_life_time",
        ] {
            for key in c[name].as_object().unwrap().keys() {
                assert!(
                    !key.ends_with("command_topic"),
                    "{name}.{key} must not exist"
                );
            }
        }

        assert_eq!(state(&ha, "fan"), Some("ON".into()));
        assert_eq!(state(&ha, "mode"), Some("single_clean".into()));
        assert_eq!(
            ha.get_property(DEVICE_ID, "fan", "preset_mode_state"),
            Some("auto".into())
        );
        assert_eq!(state(&ha, "clean_booster_fan_speed"), Some("auto".into()));
        assert_eq!(state(&ha, "circulation_rotation"), Some("ON".into()));
        assert_eq!(state(&ha, "clean_indicator"), Some("ON".into()));
        assert_eq!(state(&ha, "air_sanitization"), Some("ON".into()));
        assert_eq!(state(&ha, "sleep_timer"), Some(PropertyValue::Int(0)));
        assert_eq!(state(&ha, "pm1"), Some(PropertyValue::Int(8)));
        assert_eq!(state(&ha, "pm25"), Some(PropertyValue::Int(8)));
        assert_eq!(state(&ha, "pm10"), Some(PropertyValue::Int(8)));
        assert_eq!(state(&ha, "humidity"), None);
        assert_eq!(state(&ha, "air_quality"), Some(PropertyValue::Int(1)));
        assert_eq!(state(&ha, "odor"), Some(PropertyValue::Int(1)));
        assert_eq!(state(&ha, "error"), Some(PropertyValue::Int(0)));
        assert_eq!(
            state(&ha, "filter_remaining_time"),
            Some(PropertyValue::Int(34))
        );
        assert_eq!(
            state(&ha, "filter_life_time"),
            Some(PropertyValue::Int(4000))
        );
        assert_eq!(
            state(&ha, "top_filter_remaining_time"),
            Some(PropertyValue::Int(3189))
        );
        assert_eq!(
            state(&ha, "top_filter_life_time"),
            Some(PropertyValue::Int(4000))
        );
        assert!(
            !c.contains_key("filter_life"),
            "derived percentage is not published"
        );
        assert!(
            !c.contains_key("top_filter_life"),
            "derived percentage is not published"
        );

        assert!(thinq.outbox().is_empty());
        assert!(thinq.sent().is_empty());
    }

    #[test]
    fn filter_counters_publish_raw_remaining_and_lifetime_hours() {
        let (ha, thinq, dev) = with_base();
        dev.core.process_key_value(0x355, 500);
        assert_eq!(
            state(&ha, "filter_remaining_time"),
            Some(PropertyValue::Int(500))
        );
        dev.core.process_key_value(0x356, 1000);
        assert_eq!(
            state(&ha, "filter_life_time"),
            Some(PropertyValue::Int(1000))
        );
        assert!(thinq.outbox().is_empty());
        assert!(thinq.sent().is_empty());
    }

    #[test]
    fn owner_labelled_mode_transitions_decode() {
        let (ha, thinq, _dev) = with_base();
        for (frame, expected) in [
            (CLEAN_BOOSTER_HEX, "clean_booster"),
            (SINGLE_CLEAN_HEX, "single_clean"),
            (DUAL_CLEAN_HEX, "dual_clean"),
            (AUTO_HEX, "auto"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(state(&ha, "mode"), Some(expected.into()));
        }
    }

    #[test]
    fn unknown_codes_publish_none() {
        let (ha, _thinq, dev) = with_base();
        dev.core.process_key_value(0x1f9, 99);
        assert_eq!(state(&ha, "mode"), Some("None".into()));
        dev.core.process_key_value(0x326, 3);
        assert_eq!(state(&ha, "clean_booster_fan_speed"), Some("None".into()));
    }

    #[test]
    fn clean_booster_fan_high_and_turbo_deltas_decode() {
        let (ha, thinq, _dev) = with_base();
        thinq.emit_data(&hex_decode(BOOSTER_HIGH_HEX));
        assert_eq!(state(&ha, "clean_booster_fan_speed"), Some("high".into()));
        thinq.emit_data(&hex_decode(BOOSTER_TURBO_HEX));
        assert_eq!(state(&ha, "clean_booster_fan_speed"), Some("turbo".into()));
    }

    #[test]
    fn main_fan_uses_0x1fa_independently_of_the_clean_booster_fan() {
        let (ha, thinq, _dev) = with_base();
        thinq.emit_data(&hex_decode(MAIN_FAN_LOW_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "fan", "preset_mode_state"),
            Some("low".into())
        );
        assert_eq!(state(&ha, "clean_booster_fan_speed"), Some("auto".into()));
    }

    #[test]
    fn rotation_indicator_and_sanitization_round_trips_decode() {
        let (ha, thinq, _dev) = with_base();
        for (frame, comp, expected) in [
            (ROTATION_OFF_HEX, "circulation_rotation", "OFF"),
            (ROTATION_ON_HEX, "circulation_rotation", "ON"),
            (INDICATOR_OFF_HEX, "clean_indicator", "OFF"),
            (INDICATOR_ON_HEX, "clean_indicator", "ON"),
            (SANITIZATION_OFF_HEX, "air_sanitization", "OFF"),
            (SANITIZATION_ON_HEX, "air_sanitization", "ON"),
        ] {
            thinq.emit_data(&hex_decode(frame));
            assert_eq!(state(&ha, comp), Some(expected.into()), "{comp}");
        }
    }

    #[test]
    fn sleep_timer_counts_down_in_minutes_and_cancel_restores_the_indicator() {
        let (ha, thinq, _dev) = with_base();
        thinq.emit_data(&hex_decode(SLEEP_120_HEX));
        assert_eq!(state(&ha, "sleep_timer"), Some(PropertyValue::Int(120)));
        assert_eq!(state(&ha, "clean_indicator"), Some("OFF".into()));
        thinq.emit_data(&hex_decode(SLEEP_119_HEX));
        assert_eq!(state(&ha, "sleep_timer"), Some(PropertyValue::Int(119)));
        thinq.emit_data(&hex_decode(SLEEP_CANCEL_HEX));
        assert_eq!(state(&ha, "sleep_timer"), Some(PropertyValue::Int(0)));
        assert_eq!(state(&ha, "clean_indicator"), Some("ON".into()));
    }

    #[test]
    fn short_delta_frames_preserve_omitted_state_and_power_round_trips() {
        let (ha, thinq, _dev) = with_base();
        thinq.emit_data(&hex_decode(POWER_OFF_HEX));
        assert_eq!(state(&ha, "fan"), Some("OFF".into()));
        assert_eq!(state(&ha, "mode"), Some("single_clean".into()));
        assert_eq!(state(&ha, "pm1"), Some(PropertyValue::Int(8)));
        thinq.emit_data(&hex_decode(POWER_ON_HEX));
        assert_eq!(state(&ha, "fan"), Some("ON".into()));
    }

    #[test]
    fn each_single_tag_write_reproduces_the_app_command() {
        for (prop, value, captured) in [
            ("clean_booster_fan_speed-", "medium", CMD_BOOSTER_MEDIUM),
            ("clean_booster_fan_speed-", "turbo", CMD_BOOSTER_TURBO),
            ("circulation_rotation-", "OFF", CMD_ROTATION_OFF),
            ("circulation_rotation-", "ON", CMD_ROTATION_ON),
            ("clean_indicator-", "OFF", CMD_INDICATOR_OFF),
            ("clean_indicator-", "ON", CMD_INDICATOR_ON),
            ("air_sanitization-", "OFF", CMD_STERILIZE_OFF),
            ("air_sanitization-", "ON", CMD_STERILIZE_ON),
            ("sleep_timer-", "0", CMD_SLEEP_CANCEL),
        ] {
            let (_ha, thinq, dev) = with_base();
            thinq.reset_recorder();

            dev.set_property(prop, value);
            let got = sent_tlvs(&thinq);
            for (tag, v) in command_tlvs(captured) {
                assert_eq!(got.get(&tag), Some(&v), "{prop}={value} tag 0x{tag:x}");
            }
        }
    }

    #[test]
    fn mode_and_fan_writes_force_power_on() {
        let (_ha, thinq, dev) = with_base();

        thinq.reset_recorder();
        dev.set_property("mode-", "single_clean");
        let got = sent_tlvs(&thinq);
        assert_eq!(
            got.get(&0x1f9).copied(),
            Some(command_tlvs(CMD_MODE_SINGLE)[&0x1f9])
        );
        assert_eq!(
            got.get(&0x1f7),
            Some(&1),
            "power is attached so the write is not ignored"
        );

        thinq.reset_recorder();
        dev.set_property("mode-", "clean_booster");
        let got = sent_tlvs(&thinq);
        assert_eq!(
            got.get(&0x1f9).copied(),
            Some(command_tlvs(CMD_MODE_BOOSTER)[&0x1f9])
        );

        thinq.reset_recorder();
        dev.set_property("fan-preset_mode", "high");
        let got = sent_tlvs(&thinq);
        assert_eq!(got.get(&0x1fa), Some(&6));
        assert_eq!(got.get(&0x1f7), Some(&1));
    }

    #[test]
    fn writes_while_off_attach_power_on() {
        let (_ha, thinq, dev) = with_base();
        for (prop, value) in [
            ("mode-", "auto"),
            ("fan-preset_mode", "low"),
            ("clean_booster_fan_speed-", "high"),
        ] {
            thinq.emit_data(&hex_decode(POWER_OFF_HEX));
            thinq.reset_recorder();
            dev.set_property(prop, value);
            assert_eq!(sent_tlvs(&thinq).get(&0x1f7), Some(&1), "{prop}");
        }
    }

    #[test]
    fn power_on_restores_mode_and_fan_in_one_frame_and_power_off_does_not() {
        let (_ha, thinq, dev) = with_base();

        thinq.reset_recorder();
        dev.set_property("fan-", "ON");
        let got = sent_tlvs(&thinq);
        assert_eq!(got.get(&0x1f7), Some(&1));
        assert_eq!(got.get(&0x1f9), Some(&14), "last mode is restored");
        assert_eq!(got.get(&0x1fa), Some(&8), "last fan speed is restored");

        thinq.reset_recorder();
        dev.set_property("fan-", "OFF");
        let got = sent_tlvs(&thinq);
        assert_eq!(got.get(&0x1f7), Some(&0));
        assert_eq!(
            got.get(&0x1f9),
            None,
            "nothing is attached when switching off"
        );
        assert_eq!(got.get(&0x1fa), None);
    }

    #[test]
    fn sensors_are_not_writable_and_malformed_frames_change_nothing() {
        let (ha, thinq, dev) = with_base();
        thinq.reset_recorder();

        for prop in ["pm25-", "air_quality-", "error-", "filter_remaining_time-"] {
            dev.set_property(prop, "42");
        }
        thinq.emit_data(&hex_decode(TRUNCATED_HEX));
        thinq.emit_data(&[0, 1, 2, 3]);

        assert!(
            thinq.outbox().is_empty(),
            "no sensor write reaches the wire"
        );
        assert!(thinq.sent().is_empty());
        assert_eq!(state(&ha, "fan"), Some("ON".into()));
        assert_eq!(state(&ha, "pm25"), Some(PropertyValue::Int(8)));
    }
}
