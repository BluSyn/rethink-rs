//! DHUM_231006_WW — LG dehumidifier (deviceType 403, BEKEN_BK7234, protocolVer 7).
//!
//! The DHUM_056905_WW TLV map with this model's own mode and fan codes: a captured values packet
//! reports modes 19/20/85/86 and fan rows using 2/7/8, captured cloud writes add fan 6, and LG's
//! model metadata defines 4 as the remaining medium level. Fan writes are a bare 0x1fa. The
//! initial response has no 0x2da marker; it combines the current values with the repeated
//! 0x2d7/0x2d8/0x2d9 mode/fan capability rows, so that shape also answers the capabilities query,
//! and its current humidity (0x336) is live.
//!
//! Port of upstream PR #113.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MODES: &[(u32, &str)] = &[
    (86, "Smart Plus"),
    (19, "Silent"),
    (20, "Intensive"),
    (85, "Quick"),
];

const FANS: &[(u32, &str)] = &[
    (8, "auto"),
    (2, "low"),
    (4, "medium"),
    (6, "high"),
    (7, "turbo"),
];

/// Bucket-full entities earlier versions of this handler published. Bucket-full reporting is not
/// confirmed on this model. HA removes a component whose discovery carries only `platform`.
const WITHDRAWN: &[(&str, &str)] = &[("bucket_full", "binary_sensor"), ("bucket_alert", "event")];

fn options(table: &[(u32, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, label)| *label).collect()
}

fn label(table: &[(u32, &'static str)], raw: u32) -> PropertyValue {
    table
        .iter()
        .find(|(code, _)| *code == raw)
        .map_or("None", |(_, label)| *label)
        .into()
}

/// Labels match case-insensitively.
fn code(table: &[(u32, &str)], val: &str) -> Option<u32> {
    let val = val.to_lowercase();
    table
        .iter()
        .find(|(_, label)| label.to_lowercase() == val)
        .map(|(code, _)| *code)
}

fn on_off(raw: u32) -> Option<PropertyValue> {
    Some(if raw != 0 { "ON" } else { "OFF" }.into())
}

fn on_off_write(val: &str) -> Option<PropertyValue> {
    Some(PropertyValue::Int(i64::from(val == "ON")))
}

fn is_caps_response(tlv: &[Tlv]) -> bool {
    let has = |t: u16| tlv.iter().any(|e| e.t == t);
    [0x1f7, 0x1f9, 0x1fa, 0x2d7, 0x2d8, 0x2d9]
        .into_iter()
        .all(has)
        || has(0x2da)
}

fn is_values_response(tlv: &[Tlv]) -> bool {
    tlv.iter().any(|e| {
        matches!(
            e.t,
            0x1f7 | 0x1f9 | 0x1fa | 0x21b | 0x21e | 0x2b2 | 0x253 | 0x2a2 | 0x336 | 0x360
        )
    })
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
        let core = TlvDeviceCore::new(ha, thinq.clone());
        *core.is_caps_response.lock() = Some(Box::new(is_caps_response));
        *core.is_values_response.lock() = Some(Box::new(is_values_response));
        let masked = AtomicBool::new(false);
        *core.on_values.lock() = Some(Box::new(move || {
            if !masked.swap(true, Ordering::Relaxed) {
                thinq.send("setMaskingInfo", 0, json!({ "blacklist_tlv": "1200" }));
            }
        }));
        // The mode/fan rows repeat once per mode; they are not device state.
        *core.key_value_intercept.lock() = Some(Box::new(|k, _| matches!(k, 0x2d7..=0x2d9)));

        let mut config = default_config(&meta, Some(json!({"name": "LG Dehumidifier"})));
        config.components.insert(
            "humidifier".into(),
            json!({
                "platform": "humidifier",
                "unique_id": "$deviceid-humidifier",
                "name": null,
                "device_class": "dehumidifier",
                "modes": options(MODES),
                "min_humidity": 30,
                "max_humidity": 70,
            }),
        );
        config.components.insert(
            "ionizer".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-ionizer",
                "name": "Ionizer",
                "icon": "mdi:air-filter",
            }),
        );
        config.components.insert(
            "uv_nano".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-uv_nano",
                "name": "UVnano",
                "icon": "mdi:lightbulb",
            }),
        );
        config.components.insert(
            "bucket_light".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-bucket_light",
                "name": "Bucket Light",
                "icon": "mdi:lightbulb-on",
            }),
        );
        // The MQTT humidifier has no fan mode, so the fan is a select.
        config.components.insert(
            "fan_speed".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-fan_speed",
                "name": "Fan speed",
                "icon": "mdi:fan",
                "options": options(FANS),
            }),
        );
        config.components.insert(
            "current_humidity".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-current_humidity",
                "name": "Current humidity",
                "device_class": "humidity",
                "unit_of_measurement": "%",
                "state_class": "measurement",
                "state_topic": "$this/humidifier-current_humidity",
            }),
        );
        config.components.insert(
            "off_timer".into(),
            json!({
                "platform": "number",
                "unique_id": "$deviceid-off_timer",
                "name": "Sleep timer",
                "icon": "mdi:bed-clock",
                "device_class": "duration",
                "unit_of_measurement": "h",
                "min": 0,
                "max": 9,
                "step": 1,
                "mode": "slider",
            }),
        );
        for (key, platform) in WITHDRAWN {
            config
                .components
                .insert(key.to_string(), json!({ "platform": platform }));
        }

        add_fields(&core, &mut config);
        if let Some(Value::Object(hum)) = config.components.get_mut("humidifier") {
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
    power.read_xform = Some(Box::new(on_off));
    power.write_xform = Some(Box::new(on_off_write));
    power.write_attach = Some(Box::new(|raw| if raw != 0 { vec![0x1f9] } else { vec![] }));
    core.add_field(config, power, false);

    let core_w = core.clone();
    let mut mode = FieldDefinition::new("humidifier", "mode").with_id(0x1f9);
    mode.read_xform = Some(Box::new(|raw| Some(label(MODES, raw))));
    mode.write_xform = Some(Box::new(move |val| {
        if val == "off" {
            core_w.set_property("humidifier-power", "OFF");
            return None;
        }
        let clip = code(MODES, val)?;
        // Powers on through the attached 0x1f7.
        core_w.set_raw(0x1f7, 1);
        Some(clip.into())
    }));
    mode.write_attach_static = Some(vec![0x1f7]);
    core.add_field(config, mode, true);

    let mut fan = FieldDefinition::new("fan_speed", "").with_id(0x1fa);
    fan.read_xform = Some(Box::new(|raw| Some(label(FANS, raw))));
    fan.write_xform = Some(Box::new(|val| code(FANS, val).map(PropertyValue::from)));
    core.add_field(config, fan, true);

    // ThinQ maps 0x336 to the current humidity; 0x1fd is a different property on this family.
    let mut current = FieldDefinition::new("humidifier", "current_humidity")
        .with_id(0x336)
        .read_only();
    current.state_topic = Some("topic".into());
    core.add_field(config, current, true);

    let core_w = core.clone();
    let mut target = FieldDefinition::new("humidifier", "target_humidity").with_id(0x253);
    target.read_callback = Some(Box::new(
        |val| matches!(val, PropertyValue::Int(n) if (30..=70).contains(n)),
    ));
    target.write_xform = Some(Box::new(move |val| {
        let humidity = crate::js::number(val)?.clamp(30.0, 70.0).round();
        core_w.set_raw(0x1f7, 1);
        Some(humidity.into())
    }));
    target.write_attach_static = Some(vec![0x1f7, 0x1f9]);
    core.add_field(config, target, true);

    for (id, comp, attach) in [
        (0x360, "ionizer", &[0x1f7, 0x1f9][..]),
        (0x2a2, "uv_nano", &[0x1f7, 0x1f9][..]),
        (0x21e, "bucket_light", &[][..]),
    ] {
        let mut f = FieldDefinition::new(comp, "").with_id(id);
        f.read_xform = Some(Box::new(on_off));
        f.write_xform = Some(Box::new(on_off_write));
        f.write_attach_static = Some(attach.to_vec());
        core.add_field(config, f, true);
    }

    // Hours in HA, minutes on the wire (as RAC timers on 0x21b); a running countdown reports the
    // minutes remaining.
    let mut timer = FieldDefinition::new("off_timer", "").with_id(0x21b);
    timer.read_xform = Some(Box::new(|raw| Some((f64::from(raw) / 60.0).ceil().into())));
    timer.write_xform = Some(Box::new(|val| {
        crate::js::number(val).map(|hours| (hours * 60.0).round().into())
    }));
    core.add_field(config, timer, true);
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

    const DEVICE_ID: &str = "test-id";
    const MODEL_ID: &str = "DHUM_231006_WW";

    // Captured from DHUM_231006_WW cf69d18f on 2026-07-27.
    const VALUES_RESPONSE_HEX: &str = "000004000000A7020470707DC17E50567E8894D02D7F503686C087008980C900D80187806150B4CD90308840CE80AB00A88187C1E801EE40618089408380EA406241F8008C901B8CD024B5D056B600B648B5D013B600B642B5D055B600B647B5D014B600B648B5D016B600B6485CF05614135D301500FFFA80CDC05DC3";
    const HIGH_RESPONSE_HEX: &str =
        "000004000000A702047D177E86B5D056B600B646B5D014B600B646B5D016B600B64648F8";

    const MODE_WIRES: &[(&str, u32)] = &[
        ("Smart Plus", 86),
        ("Silent", 19),
        ("Intensive", 20),
        ("Quick", 85),
    ];

    const FAN_WIRES: &[(&str, u32)] = &[
        ("auto", 8),
        ("low", 2),
        ("medium", 4),
        ("high", 6),
        ("turbo", 7),
    ];

    fn make_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new(MODEL_ID, "TEST DHUM", "311");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn components(ha: &MockHaConnection) -> std::collections::HashMap<String, Value> {
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
    fn discovery_exposes_only_confirmed_entities_and_derives_model_enum_options() {
        let (ha, _, _) = make_device();
        let components = components(&ha);
        let labels = |table: &[(&'static str, u32)]| -> Vec<&'static str> {
            table.iter().map(|(l, _)| *l).collect()
        };
        assert_eq!(components["humidifier"]["modes"], json!(labels(MODE_WIRES)));
        assert_eq!(components["fan_speed"]["options"], json!(labels(FAN_WIRES)));
        for key in ["ionizer", "uv_nano", "bucket_light", "off_timer"] {
            assert!(components[key].get("unique_id").is_some(), "{key}");
        }
        // Only a removal tombstone remains of it.
        assert!(components["bucket_full"].get("unique_id").is_none());
    }

    #[test]
    fn withdrawn_bucket_entities_are_removal_tombstones() {
        let (ha, _, _) = make_device();
        let components = components(&ha);
        for (key, platform) in [("bucket_full", "binary_sensor"), ("bucket_alert", "event")] {
            assert_eq!(components[key], json!({ "platform": platform }), "{key}");
        }
    }

    #[test]
    fn real_initial_response_completes_startup_and_decodes_the_confirmed_231006_state() {
        let (ha, thinq, dev) = make_device();
        thinq.emit_data(&hex_decode(VALUES_RESPONSE_HEX));

        assert!(!dev.core.waiting_caps.load(Ordering::Relaxed));
        assert!(!dev.core.waiting_values.load(Ordering::Relaxed));
        assert_eq!(prop(&ha, "humidifier-power"), Some("ON".into()));
        assert_eq!(prop(&ha, "humidifier-mode"), Some("Smart Plus".into()));
        assert_eq!(prop(&ha, "fan_speed-"), Some("auto".into()));
        assert_eq!(
            prop(&ha, "humidifier-target_humidity"),
            Some(PropertyValue::Int(45))
        );
        assert_eq!(
            prop(&ha, "humidifier-current_humidity"),
            Some(PropertyValue::Int(48))
        );
        assert_eq!(prop(&ha, "ionizer-"), Some("ON".into()));
        assert_eq!(prop(&ha, "uv_nano-"), Some("ON".into()));
        assert_eq!(prop(&ha, "bucket_light-"), Some("OFF".into()));
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(0)));

        let masking: Vec<_> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "setMaskingInfo")
            .collect();
        assert_eq!(masking.len(), 1);
        assert_eq!(masking[0].data, json!({ "blacklist_tlv": "1200" }));
    }

    #[test]
    fn real_fan_response_decodes_high_and_ignores_repeated_capability_rows_as_state() {
        let (ha, thinq, dev) = make_device();
        thinq.emit_data(&hex_decode(VALUES_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(HIGH_RESPONSE_HEX));

        assert_eq!(prop(&ha, "fan_speed-"), Some("high".into()));
        for tag in [0x2d7, 0x2d8, 0x2d9] {
            assert_eq!(dev.core.get_raw(tag), None, "{tag:#x}");
        }
    }

    #[test]
    fn fan_writes_use_the_bare_0x1fa_shape_from_captured_cloud_writes() {
        let (_, thinq, dev) = make_device();
        dev.set_property("fan_speed-", "high");
        assert_eq!(written_fields(&thinq), vec![(0x1fa, 6)]);
        thinq.reset_recorder();
        dev.set_property("fan_speed-", "auto");
        assert_eq!(written_fields(&thinq), vec![(0x1fa, 8)]);
    }

    #[test]
    fn derived_inverse_tables_encode_every_advertised_mode_and_fan() {
        let (_, thinq, dev) = make_device();
        for (label, wire) in MODE_WIRES {
            thinq.reset_recorder();
            dev.set_property("humidifier-mode", label);
            assert_eq!(
                written_fields(&thinq),
                vec![(0x1f9, *wire), (0x1f7, 1)],
                "{label}"
            );
        }
        for (label, wire) in FAN_WIRES {
            thinq.reset_recorder();
            dev.set_property("fan_speed-", label);
            assert_eq!(written_fields(&thinq), vec![(0x1fa, *wire)], "{label}");
        }
    }

    #[test]
    fn unknown_enum_values_publish_none_and_unknown_commands_are_rejected() {
        let (ha, thinq, dev) = make_device();
        dev.core.process_key_value(0x1f9, 999);
        dev.core.process_key_value(0x1fa, 999);
        assert_eq!(prop(&ha, "humidifier-mode"), Some("None".into()));
        assert_eq!(prop(&ha, "fan_speed-"), Some("None".into()));

        thinq.reset_recorder();
        dev.set_property("humidifier-mode", "not-a-mode");
        dev.set_property("fan_speed-", "not-a-fan");
        dev.set_property("fan_speed-", "6");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn labels_match_case_insensitively_and_mode_off_powers_down() {
        let (_, thinq, dev) = make_device();
        dev.set_property("humidifier-mode", "smart plus");
        assert_eq!(written_fields(&thinq), vec![(0x1f9, 86), (0x1f7, 1)]);
        thinq.reset_recorder();
        dev.set_property("fan_speed-", "TURBO");
        assert_eq!(written_fields(&thinq), vec![(0x1fa, 7)]);
        thinq.reset_recorder();
        dev.set_property("humidifier-mode", "off");
        assert_eq!(written_fields(&thinq), vec![(0x1f7, 0)]);
    }

    #[test]
    fn target_humidity_publishes_30_to_70_and_writes_clamped_with_power_and_mode() {
        let (ha, thinq, dev) = make_device();
        thinq.emit_data(&hex_decode(VALUES_RESPONSE_HEX));
        dev.core.process_key_value(0x253, 75);
        assert_eq!(
            prop(&ha, "humidifier-target_humidity"),
            Some(PropertyValue::Int(45))
        );

        for (payload, wire) in [
            ("55", 55),
            (" 52.6 ", 53),
            ("0x2d", 45),
            ("80", 70),
            ("10", 30),
        ] {
            thinq.reset_recorder();
            dev.set_property("humidifier-target_humidity", payload);
            assert_eq!(
                written_fields(&thinq),
                vec![(0x253, wire), (0x1f7, 1), (0x1f9, 86)],
                "{payload:?}"
            );
        }
        thinq.reset_recorder();
        dev.set_property("humidifier-target_humidity", "abc");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn sleep_timer_is_hours_in_ha_and_minutes_on_the_wire() {
        let (ha, thinq, dev) = make_device();
        dev.core.process_key_value(0x21b, 59);
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(1)));
        dev.core.process_key_value(0x21b, 299);
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(5)));

        dev.set_property("off_timer-", "9");
        assert_eq!(written_fields(&thinq), vec![(0x21b, 540)]);
        thinq.reset_recorder();
        dev.set_property("off_timer-", "x");
        assert!(thinq.outbox().is_empty());
    }
}
