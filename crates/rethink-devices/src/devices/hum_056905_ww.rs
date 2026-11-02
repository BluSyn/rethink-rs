//! HUM_056905_WW — LG PuriCare air-purifying humidifier (deviceType 404).
//!
//! Tag ids and enum values come from this model's modelJSON, and every registered tag is present
//! in the captured values response. The writable core (power 0x1f7, mode 0x1f9, fan 0x1fa, target
//! humidity 0x253, night 0x109, sleep 0x1e4, auto operation 0x1e6) was confirmed by driving the
//! appliance through LG's cloud while bridged. 0x1fd is half-degrees, like the AC family: wire 55
//! is 27.5 °C, which the cloud read from the same appliance at the same moment. 0x336 humidity is
//! one-for-one. Only the capabilities answer carries 0x3e8.
//!
//! Port of upstream PR #114.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, ReadXform, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::{json, Value};
use std::sync::Arc;

const HALF_DEGREE: f64 = 2.0;

const OP_MODES: &[(&str, u32)] = &[("Air Clean", 5), ("Humidify+Clean", 12), ("Humidify", 24)];

const FAN_LEVELS: &[(&str, u32)] = &[
    ("Auto", 8),
    ("Low", 2),
    ("Mid", 4),
    ("High", 6),
    ("Turbo", 7),
];

const HYGIENE_DRY: &[(&str, u32)] = &[
    ("Off", 0),
    ("Gentle", 1),
    ("Quiet", 2),
    ("Quick", 3),
    ("Focus", 4),
    ("Default", 5),
];

const STANDBY_STERILIZE: &[(&str, u32)] =
    &[("Rapid", 0), ("Normal", 1), ("Silent", 2), ("Power", 3)];

const DISPLAY_BRIGHTNESS: &[(&str, u32)] =
    &[("Off", 0), ("Level 1", 8), ("Level 2", 9), ("Level 3", 10)];

/// 0x1e3, LG's humidifier.productStatus.
const PRODUCT_STATUS: &[(&str, u32)] = &[
    ("Normal", 0),
    ("No tank", 1),
    ("Low water", 2),
    ("Sterilizing", 3),
    ("Drying", 4),
    ("Boiling", 5),
    ("Boiling (low)", 6),
];

/// 0x3e0, LG's mood-light palette. Only the colours this model names are offered.
const MOOD_COLORS: &[(&str, u32)] = &[
    ("Blue", 1),
    ("Green", 2),
    ("Red", 3),
    ("Purple", 4),
    ("Yellow", 6),
    ("Pink", 7),
    ("Pure White", 8),
    ("Aquarium", 9),
    ("Candlelight", 10),
    ("Sunlight", 11),
    ("Rose", 12),
    ("Mint", 13),
    ("Lime", 14),
    ("Very Peri", 15),
    ("Warm White", 16),
    ("Cool White", 17),
    ("Sapphire", 18),
    ("Rainbow", 19),
];

/// Components earlier versions of this handler published: entities this model's capture does not
/// back, and the old keys of the off timer and the filter/auto-dry sensors. HA removes a component
/// whose discovery carries only `platform`.
const WITHDRAWN: &[(&str, &str)] = &[
    ("water_lack", "binary_sensor"),
    ("sensor_mon", "select"),
    ("over_prevention", "switch"),
    ("auto_dry", "switch"),
    ("watertank_light", "number"),
    ("off_timer", "number"),
    ("start_time", "number"),
    ("stop_time", "number"),
    ("air_quality", "sensor"),
    ("filter_used", "sensor"),
    ("filter_max", "sensor"),
    ("auto_dry_remain", "sensor"),
    ("error", "sensor"),
];

fn options(levels: &[(&'static str, u32)]) -> Vec<&'static str> {
    levels.iter().map(|(label, _)| *label).collect()
}

fn label_for(levels: &[(&'static str, u32)], raw: u32) -> Option<&'static str> {
    levels.iter().find(|(_, w)| *w == raw).map(|(l, _)| *l)
}

fn wire_for(levels: &[(&str, u32)], label: &str) -> Option<u32> {
    levels.iter().find(|(l, _)| *l == label).map(|(_, w)| *w)
}

/// `levels` label, or "None" for a code it does not list.
fn enum_label(levels: &'static [(&'static str, u32)]) -> ReadXform {
    Box::new(move |raw| Some(label_for(levels, raw).unwrap_or("None").into()))
}

fn is_caps_response(tlv: &[Tlv]) -> bool {
    tlv.iter().any(|e| e.t == 0x3e8)
}

fn is_values_response(tlv: &[Tlv]) -> bool {
    tlv.iter().any(|e| matches!(e.t, 0x1f7 | 0x1f9 | 0x1fa))
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
        let core = TlvDeviceCore::new(ha, thinq);
        *core.is_caps_response.lock() = Some(Box::new(is_caps_response));
        *core.is_values_response.lock() = Some(Box::new(is_values_response));

        let mut config =
            default_config(&meta, Some(json!({"name": "LG Humidifying Air Purifier"})));
        config.components.insert(
            "humidifier".into(),
            json!({
                "platform": "humidifier",
                "unique_id": "$deviceid-humidifier",
                "name": null,
                "device_class": "humidifier",
                "modes": options(OP_MODES),
                // LG's own value_validation for airState.humidity.desired.
                "min_humidity": 0,
                "max_humidity": 100,
            }),
        );
        for (key, platform) in WITHDRAWN {
            config
                .components
                .insert(key.to_string(), json!({ "platform": platform }));
        }

        add_fields(&core, &mut config);
        if let Some(Value::Object(hum)) = config.components.get_mut("humidifier") {
            hum.insert(
                "current_humidity_topic".into(),
                json!("$this/current_humidity-"),
            );
            hum.insert("state_topic".into(), json!("$this/humidifier-power"));
            hum.insert("command_topic".into(), json!("$this/humidifier-power/set"));
        }
        core.set_config(config);
        Arc::new(Self { core })
    }
}

fn add_fields(core: &Arc<TlvDeviceCore>, config: &mut DeviceDiscovery) {
    // Wired to the humidifier's bare state/command topics.
    let mut power = FieldDefinition::new("humidifier", "power").with_id(0x1f7);
    power.read_xform = Some(Box::new(|raw| {
        Some(if raw != 0 { "ON" } else { "OFF" }.into())
    }));
    power.write_xform = Some(Box::new(|val| Some(i64::from(val == "ON").into())));
    core.add_field(config, power, false);

    let core_w = core.clone();
    let mut mode = FieldDefinition::new("humidifier", "mode").with_id(0x1f9);
    mode.read_xform = Some(enum_label(OP_MODES));
    mode.write_xform = Some(Box::new(move |val| {
        let wire = wire_for(OP_MODES, val)?;
        // Selecting a mode while off is how the LG app turns it on.
        core_w.set_raw(0x1f7, 1);
        Some(wire.into())
    }));
    mode.write_attach_static = Some(vec![0x1f7]);
    core.add_field(config, mode, true);

    let mut target = FieldDefinition::new("humidifier", "target_humidity").with_id(0x253);
    target.write_xform = Some(Box::new(|val| {
        let humidity = crate::js::number(val).filter(|h| (0.0..=100.0).contains(h))?;
        Some(((humidity / 5.0).round() * 5.0).into())
    }));
    core.add_field(config, target, true);

    add_sensor(
        core,
        config,
        0x336,
        "current_humidity",
        "Current humidity",
        json!({
            "device_class": "humidity",
            "unit_of_measurement": "%",
            "state_class": "measurement",
        }),
        None,
    );

    for (id, comp, name, icon, levels) in [
        (0x1fa, "fan_speed", "Fan speed", "mdi:fan", FAN_LEVELS),
        (
            0x1e9,
            "hygiene_dry",
            "Sanitary dry",
            "mdi:hair-dryer",
            HYGIENE_DRY,
        ),
        (
            0x164,
            "standby_sterilize",
            "Standby sterilize",
            "mdi:shimmer",
            STANDBY_STERILIZE,
        ),
        (
            0x21f,
            "display",
            "Screen brightness",
            "mdi:brightness-6",
            DISPLAY_BRIGHTNESS,
        ),
        (
            0x3e0,
            "mood_color",
            "Mood light color",
            "mdi:palette",
            MOOD_COLORS,
        ),
    ] {
        add_select(core, config, id, comp, name, icon, levels);
    }

    for (id, comp, name, icon) in [
        (0x109, "night_mode", "Night mode", "mdi:weather-night"),
        (0x1e4, "sleep_mode", "Sleep mode", "mdi:power-sleep"),
        (0x1e6, "auto_strength", "Auto operation", "mdi:autorenew"),
        (0x1e7, "humidify", "Humidify", "mdi:water"),
        (0x161, "anti_glare", "Anti-glare", "mdi:eye-off"),
        (0x1b8, "mood_light", "Mood light", "mdi:lightbulb-on"),
        (0x3a0, "bell_sound", "Notification sound", "mdi:bell"),
    ] {
        add_switch(core, config, id, comp, name, icon);
    }

    // Same tag and minutes-on-the-wire unit as RAC_056905_WW; this model caps it at 720 minutes.
    // Once set, the device counts down and reports the remaining time every minute.
    config.components.insert(
        "stoptimer".into(),
        json!({
            "platform": "number",
            "unique_id": "$deviceid-stoptimer",
            "name": "Turn-off timer",
            "icon": "mdi:timer-stop",
            "device_class": "duration",
            "unit_of_measurement": "h",
            "min": 0,
            "max": 12,
            "step": 0.25,
            "mode": "slider",
        }),
    );
    let mut timer = FieldDefinition::new("stoptimer", "").with_id(0x21b);
    timer.read_xform = Some(Box::new(|raw| {
        Some(((f64::from(raw) / 60.0 / 0.25).ceil() * 0.25).into())
    }));
    timer.write_xform = Some(Box::new(|val| {
        crate::js::number(val).map(|hours| (hours * 60.0).round().into())
    }));
    core.add_field(config, timer, true);

    add_sensor(
        core,
        config,
        0x1fd,
        "temperature",
        "Current temperature",
        json!({
            "device_class": "temperature",
            "unit_of_measurement": "°C",
            "state_class": "measurement",
        }),
        Some(Box::new(|raw| Some((f64::from(raw) / HALF_DEGREE).into()))),
    );
    for (id, comp, name) in [
        (0x333, "pm1", "PM1.0"),
        (0x334, "pm25", "PM2.5"),
        (0x335, "pm10", "PM10"),
    ] {
        add_sensor(
            core,
            config,
            id,
            comp,
            name,
            json!({
                "device_class": comp,
                "unit_of_measurement": "µg/m³",
                "state_class": "measurement",
            }),
            None,
        );
    }
    add_sensor(
        core,
        config,
        0x1ee,
        "watertank_remain",
        "Tank level",
        json!({
            "icon": "mdi:cup-water",
            "unit_of_measurement": "%",
            "state_class": "measurement",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        0x2ad,
        "watertank_time",
        "Tank time remaining",
        json!({
            "icon": "mdi:timer-sand",
            "unit_of_measurement": "min",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        0x1ed,
        "water_filter",
        "Water filter level",
        json!({
            "icon": "mdi:filter",
            "unit_of_measurement": "%",
            "state_class": "measurement",
        }),
        None,
    );
    // Named and attributed as RAC_056905_WW publishes the same three.
    add_sensor(
        core,
        config,
        0x355,
        "filterused",
        "Filter used time",
        json!({
            "icon": "mdi:air-filter",
            "device_class": "duration",
            "unit_of_measurement": "h",
            "state_class": "total_increasing",
            "entity_category": "diagnostic",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        0x356,
        "filterlife",
        "Filter life time",
        json!({
            "icon": "mdi:air-filter",
            "device_class": "duration",
            "unit_of_measurement": "h",
            "entity_category": "diagnostic",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        0x225,
        "autodryremain",
        "Auto dry remaining",
        json!({
            "icon": "mdi:hair-dryer-outline",
            "unit_of_measurement": "%",
            "suggested_display_precision": 0,
            "entity_category": "diagnostic",
        }),
        None,
    );
    add_sensor(
        core,
        config,
        0x1e3,
        "status",
        "Operating state",
        json!({
            "icon": "mdi:information-outline",
            "device_class": "enum",
            "options": options(PRODUCT_STATUS),
        }),
        Some(enum_label(PRODUCT_STATUS)),
    );
}

/// A select whose options are English labels and whose wire values are LG's enum codes. A code
/// the table does not list is not published.
fn add_select(
    core: &TlvDeviceCore,
    config: &mut DeviceDiscovery,
    id: u16,
    comp: &str,
    name: &str,
    icon: &str,
    levels: &'static [(&'static str, u32)],
) {
    config.components.insert(
        comp.into(),
        json!({
            "platform": "select",
            "unique_id": format!("$deviceid-{comp}"),
            "name": name,
            "icon": icon,
            "options": options(levels),
        }),
    );
    let mut f = FieldDefinition::new(comp, "").with_id(id);
    f.read_xform = Some(Box::new(move |raw| {
        label_for(levels, raw).map(PropertyValue::from)
    }));
    f.write_xform = Some(Box::new(move |val| {
        wire_for(levels, val).map(PropertyValue::from)
    }));
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
    f.read_xform = Some(Box::new(|raw| {
        Some(if raw == 1 { "ON" } else { "OFF" }.into())
    }));
    f.write_xform = Some(Box::new(|val| Some(i64::from(val == "ON").into())));
    core.add_field(config, f, true);
}

fn add_sensor(
    core: &TlvDeviceCore,
    config: &mut DeviceDiscovery,
    id: u16,
    comp: &str,
    name: &str,
    attrs: Value,
    read_xform: Option<ReadXform>,
) {
    let mut component = json!({
        "platform": "sensor",
        "unique_id": format!("$deviceid-{comp}"),
        "name": name,
    });
    if let (Value::Object(component), Value::Object(attrs)) = (&mut component, attrs) {
        component.extend(attrs);
    }
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};
    use rethink_util::tlv;
    use std::collections::{BTreeSet, HashMap};

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "HUM_056905_WW";

    // Both frames are verbatim from the appliance: its answers to a capabilities query (0x1f5=1)
    // and a values query (0x1f5=2), injected through the management API on 2026-07-28.
    const CAPS_RESPONSE_HEX: &str = "000004000000a702012778640f644f6c4cb00eb0601020b480a5c0965020b0a001d4b4e05000b5b020100057c3580f79507bb6a04956b6e04378b700b750feb9501eb99046bc200800bc70020201b3c0bd1031bd60099fbd85d3c0d400dd04f7905af7d014fa01fb0bf870fffc40ef83b5c5b600b642b5ccb600b642b5d018b600b642e059";
    const QUERY_RESPONSE_HEX: &str = "000004000000a702042d9d6880698069c06a006a406a806ac06b006b406b806bc06c006cc06e405902fa807dc17e457e827f50374241584179c07a437a807b50647b8fb4501cb7901c86c0870087c08940898094d041d8808790c8cd46cd05ccc49001cd90308840d5503ed59064ab007f00e801e900e94ae989ebc1ed08ed40ee405cf0180c055d00f80b6e0178c179007980ab40b5c5b600b642b5ccb600b642b5d018b600b64290c1";

    fn build_ready_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new(MODEL_ID, "TEST HUM", "4378");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn components(ha: &MockHaConnection) -> HashMap<String, Value> {
        ha.device(DEVICE_ID).unwrap().config.unwrap().components
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<PropertyValue> {
        ha.device(DEVICE_ID)?.properties.get(name).cloned()
    }

    fn written_fields(thinq: &MockThinq2Device) -> Vec<(u16, u32)> {
        let packet = thinq.outbox().last().cloned().expect("a packet was sent");
        tlv::parse(&packet[11..packet.len() - 2])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect()
    }

    #[test]
    fn the_captured_values_frame_populates_the_entities_it_is_meant_to() {
        let (ha, _, _) = build_ready_device();
        assert_eq!(prop(&ha, "humidifier-power"), Some("ON".into()));
        assert_eq!(prop(&ha, "humidifier-mode"), Some("Air Clean".into()));
        assert_eq!(
            prop(&ha, "humidifier-target_humidity"),
            Some(PropertyValue::Int(65))
        );
        assert_eq!(prop(&ha, "fan_speed-"), Some("Low".into()));
        assert_eq!(prop(&ha, "current_humidity-"), Some(PropertyValue::Int(48)));
        // 0x1fd is half-degrees: wire 55 is 27.5 degC, which is what the cloud read at the time.
        assert_eq!(prop(&ha, "temperature-"), Some(PropertyValue::Num(27.5)));
        assert_eq!(prop(&ha, "night_mode-"), Some("ON".into()));
        assert_eq!(prop(&ha, "sleep_mode-"), Some("OFF".into()));
        assert_eq!(prop(&ha, "auto_strength-"), Some("OFF".into()));
        // The tank was out of its seat during the capture, which is what LG's code 1 means.
        assert_eq!(prop(&ha, "status-"), Some("No tank".into()));
    }

    #[test]
    fn config_offers_the_lg_mode_and_fan_vocabularies() {
        let (ha, _, _) = build_ready_device();
        let components = components(&ha);
        assert_eq!(
            components["humidifier"]["modes"],
            json!(["Air Clean", "Humidify+Clean", "Humidify"])
        );
        assert_eq!(
            components["fan_speed"]["options"],
            json!(["Auto", "Low", "Mid", "High", "Turbo"])
        );
    }

    #[test]
    fn every_mode_code_decodes_to_its_label() {
        let (ha, _, dev) = build_ready_device();
        for (label, wire) in OP_MODES {
            dev.core.process_key_value(0x1f9, *wire);
            assert_eq!(prop(&ha, "humidifier-mode"), Some((*label).into()));
        }
    }

    #[test]
    fn every_mode_label_writes_its_wire_code_and_turns_the_appliance_on() {
        let (_, thinq, dev) = build_ready_device();
        for (label, wire) in OP_MODES {
            thinq.reset_recorder();
            dev.set_property("humidifier-mode", label);
            assert_eq!(
                written_fields(&thinq),
                vec![(0x1f9, *wire), (0x1f7, 1)],
                "{label}"
            );
        }
    }

    #[test]
    fn every_fan_code_decodes_and_every_fan_label_writes_0x1fa_alone() {
        let (ha, thinq, dev) = build_ready_device();
        for (label, wire) in FAN_LEVELS {
            dev.core.process_key_value(0x1fa, *wire);
            assert_eq!(prop(&ha, "fan_speed-"), Some((*label).into()));
            thinq.reset_recorder();
            dev.set_property("fan_speed-", label);
            assert_eq!(written_fields(&thinq), vec![(0x1fa, *wire)], "{label}");
        }
    }

    #[test]
    fn target_humidity_snaps_to_lgs_5_percent_step() {
        let (_, thinq, dev) = build_ready_device();
        dev.set_property("humidifier-target_humidity", "52");
        assert_eq!(written_fields(&thinq), vec![(0x253, 50)]);
    }

    #[test]
    fn target_humidity_rejects_non_finite_and_out_of_range_writes() {
        let (_, thinq, dev) = build_ready_device();
        for value in ["NaN", "Infinity", "-1", "101"] {
            dev.set_property("humidifier-target_humidity", value);
        }
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn target_humidity_payloads_parse_like_javascript_number() {
        let (_, thinq, dev) = build_ready_device();
        for (payload, wire) in [(" 47 ", 45), ("52.5", 55), ("0x32", 50), ("100", 100)] {
            thinq.reset_recorder();
            dev.set_property("humidifier-target_humidity", payload);
            assert_eq!(written_fields(&thinq), vec![(0x253, wire)], "{payload:?}");
        }
        thinq.reset_recorder();
        dev.set_property("humidifier-target_humidity", "abc");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn enum_entities_use_the_none_fallback_and_declare_their_options() {
        let (ha, _, dev) = build_ready_device();
        dev.core.process_key_value(0x1f9, 99);
        dev.core.process_key_value(0x1e3, 99);
        assert_eq!(prop(&ha, "humidifier-mode"), Some("None".into()));
        assert_eq!(prop(&ha, "status-"), Some("None".into()));

        let components = components(&ha);
        assert_eq!(components["status"]["device_class"], "enum");
        assert_eq!(
            components["status"]["options"],
            json!([
                "Normal",
                "No tank",
                "Low water",
                "Sterilizing",
                "Drying",
                "Boiling",
                "Boiling (low)",
            ])
        );
    }

    #[test]
    fn a_select_code_the_table_does_not_list_is_not_published() {
        let (ha, _, dev) = build_ready_device();
        dev.core.process_key_value(0x1fa, 99);
        assert_eq!(prop(&ha, "fan_speed-"), Some("Low".into()));
    }

    #[test]
    fn display_brightness_round_trips_its_lg_codes() {
        let (ha, thinq, dev) = build_ready_device();
        dev.core.process_key_value(0x21f, 9);
        assert_eq!(prop(&ha, "display-"), Some("Level 2".into()));
        thinq.reset_recorder();
        dev.set_property("display-", "Level 3");
        assert_eq!(written_fields(&thinq), vec![(0x21f, 10)]);
    }

    #[test]
    fn every_registered_tag_is_present_in_the_captured_values_response() {
        let (_, _, dev) = build_ready_device();
        let frame = hex_decode(QUERY_RESPONSE_HEX);
        let captured: BTreeSet<u16> = tlv::parse(&frame[11..frame.len() - 2])
            .into_iter()
            .map(|e| e.t)
            .collect();
        let registered: Vec<u16> = dev
            .core
            .fields_by_id
            .lock()
            .iter()
            .enumerate()
            .filter(|(_, f)| f.is_some())
            .map(|(tag, _)| tag as u16)
            .collect();
        assert_eq!(registered.len(), 28);
        let missing: Vec<u16> = registered
            .into_iter()
            .filter(|tag| !captured.contains(tag))
            .collect();
        assert_eq!(missing, Vec::<u16>::new());
    }

    #[test]
    fn unverified_and_raw_only_entities_are_not_exposed() {
        let (ha, _, _) = build_ready_device();
        let components = components(&ha);
        for name in [
            "over_prevention",
            "sensor_mon",
            "auto_dry",
            "watertank_light",
            "air_quality",
            "error",
            "water_lack",
        ] {
            // Only a removal tombstone remains of each.
            let keys: Vec<&String> = components[name].as_object().unwrap().keys().collect();
            assert_eq!(keys, ["platform"], "{name}");
        }
    }

    #[test]
    fn the_off_timer_is_hours_in_ha_and_minutes_on_the_wire_as_on_rac_056905_ww() {
        let (ha, thinq, dev) = build_ready_device();
        dev.core.process_key_value(0x21b, 90);
        assert_eq!(prop(&ha, "stoptimer-"), Some(PropertyValue::Num(1.5)));
        thinq.reset_recorder();
        dev.set_property("stoptimer-", "2.25");
        assert_eq!(written_fields(&thinq), vec![(0x21b, 135)]);
        // LG caps targetTimeToStop at 720 minutes.
        assert_eq!(components(&ha)["stoptimer"]["max"], 12);
    }

    #[test]
    fn the_absolute_hhmm_schedule_tags_are_not_exposed() {
        let (ha, _, _) = build_ready_device();
        let components = components(&ha);
        for name in ["start_time", "stop_time"] {
            assert_eq!(components[name], json!({ "platform": "number" }), "{name}");
        }
    }

    #[test]
    fn withdrawn_components_are_removal_tombstones() {
        let (ha, _, _) = build_ready_device();
        let components = components(&ha);
        for (key, platform) in [
            ("water_lack", "binary_sensor"),
            ("sensor_mon", "select"),
            ("over_prevention", "switch"),
            ("auto_dry", "switch"),
            ("watertank_light", "number"),
            ("off_timer", "number"),
            ("start_time", "number"),
            ("stop_time", "number"),
            ("air_quality", "sensor"),
            ("filter_used", "sensor"),
            ("filter_max", "sensor"),
            ("auto_dry_remain", "sensor"),
            ("error", "sensor"),
        ] {
            assert_eq!(components[key], json!({ "platform": platform }), "{key}");
        }
        let entities = components
            .values()
            .filter(|c| c.get("unique_id").is_some())
            .count();
        assert_eq!(entities, 26);
    }
}
