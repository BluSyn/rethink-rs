//! CSL_306406_WW — LG console air conditioner.
//!
//! Speaks the `RAC_056905_WW` TLV protocol with console-specific fan speeds, a Jet shortcut
//! button (instead of tag `0x323`), filter hours from `0x355`/`0x356`, and unadvertised display
//! light (`0x21f`), beep (`0x3a7`), plasma ionizer (`0x20f`), and auto dry (`0x20e`) switches.
//!
//! Port of upstream PR #343.

use super::rac_056905_ww::{Device, FanModes};
use crate::device_trait::DeviceHandler;
use rethink_core::device_base::FieldDefinition;
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::json;
use std::sync::Arc;

const CSL_FAN_MODES: FanModes = &[
    ("very low", 1),
    ("low", 2),
    ("medium", 4),
    ("high", 6),
    ("very high", 7),
    ("auto", 8),
];

const MODE_COOL: u32 = 0;
const MODE_HEAT: u32 = 4;
const FAN_VERY_HIGH: u32 = 7;

fn add_console_features(dev: &Arc<Device>, config: &mut DeviceDiscovery) {
    // 0x21f: 11 = LED off, 12 = LED on, 13 = auto.
    dev.add_config_switch_values(
        config,
        0x21f,
        "displaylight",
        "Display light",
        "mdi:lightbulb-outline",
        12,
        11,
    );
    // 0x3a7: 0 = beeps on, 1 = muted.
    dev.add_config_switch_values(config, 0x3a7, "beep", "Beep", "mdi:volume-high", 0, 1);
    dev.describe_feature(
        config,
        "beep",
        "The beep when a setting changes. Off is the \"Mute\" function.",
        None,
    );
    dev.add_config_switch_values(
        config,
        0x20f,
        "plasma",
        "Plasma ionizer",
        "mdi:air-purifier",
        1,
        0,
    );
    dev.add_config_switch_values(
        config,
        0x20e,
        "autodrysetting",
        "Auto dry",
        "mdi:hair-dryer",
        255,
        0,
    );

    add_filter_sensors(dev, config);
    add_jet_button(dev, config);
}

/// 0x356 is total filter life (e.g. 2400 h) and 0x355 is remaining hours.
fn add_filter_sensors(dev: &Arc<Device>, config: &mut DeviceDiscovery) {
    dev.add_optional_sensor_field(
        config,
        &[0x356],
        "filterlifetime",
        "Filter life time",
        Some("mdi:air-filter"),
        Some(json!({
            "device_class": "duration",
            "unit_of_measurement": "h",
            "suggested_display_precision": 0,
        })),
        None,
    );
    if dev.get_raw(0x356).is_none() || dev.get_raw(0x355).is_none() {
        return;
    }
    config.components.insert(
        "filterusedtime".into(),
        json!({
            "platform": "sensor",
            "unique_id": "$deviceid-filterusedtime",
            "name": "Filter used time",
            "icon": "mdi:air-filter",
            "device_class": "duration",
            "unit_of_measurement": "h",
            "suggested_display_precision": 0,
            "entity_category": "diagnostic",
            "state_topic": "$this/filterusedtime-",
        }),
    );

    let publish_used = |d: &Device| {
        if let (Some(total), Some(remaining)) = (d.get_raw(0x356), d.get_raw(0x355)) {
            let used = i64::from(total) - i64::from(remaining);
            d.core
                .ha
                .publish_property(&d.core.id, "filterusedtime-", PropertyValue::Int(used));
        }
    };

    // Re-register 0x356 with a read_callback so either tag arriving updates filterusedtime.
    let d1 = dev.clone();
    let mut life = FieldDefinition::new("filterlifetime", "")
        .with_id(0x356)
        .read_only();
    life.read_callback = Some(Box::new(move |_| {
        publish_used(&d1);
        true
    }));
    dev.core.add_field(config, life, true);

    let d2 = dev.clone();
    let mut rem = FieldDefinition::new("filterusedtime", "").with_id(0x355);
    rem.readable = false;
    rem.writable = false;
    rem.read_callback = Some(Box::new(move |_| {
        publish_used(&d2);
        false
    }));
    dev.core.add_field(config, rem, false);
}

/// The remote's Jet button switches to maximum fan and the extreme target temperature.
fn add_jet_button(dev: &Arc<Device>, config: &mut DeviceDiscovery) {
    config.components.insert(
        "jet".into(),
        json!({
            "platform": "button",
            "unique_id": "$deviceid-jet",
            "command_topic": "$this/jet/set",
            "name": "Jet",
            "icon": "mdi:wind-power",
        }),
    );
    let d = dev.clone();
    let mut f = FieldDefinition::new("", "");
    f.write_xform = Some(Box::new(|val| {
        Some(PropertyValue::Int(if val == "PRESS" { 1 } else { 0 }))
    }));
    f.write_callback = Some(Box::new(move |val| {
        if val == 1 {
            run_jet(&d);
        }
        false
    }));
    dev.core
        .fields_by_ha
        .lock()
        .insert("jet".into(), Arc::new(f));
}

fn run_jet(dev: &Device) {
    let mode = dev.get_mode_tlv();
    if dev.get_power_tlv().unwrap_or(0) == 0 || !matches!(mode, Some(MODE_COOL | MODE_HEAT)) {
        return;
    }
    let mode = mode.unwrap();
    let limit = if mode == MODE_HEAT {
        dev.temp_limit(&[0x2e2, 0x2e4, 0x2ec], true).unwrap_or(30.0)
    } else {
        dev.temp_limit(&[0x2e1, 0x2e3, 0x2eb], false)
            .unwrap_or(18.0)
    };
    let target = (limit * 2.0).round() as u32;
    dev.set_raw(0x1fa, FAN_VERY_HIGH);
    dev.set_raw(0x1fe, target);
    dev.core.send(
        &[1, 1, 2, 1, 1],
        &[
            Tlv::new(0x1f9, mode),
            Tlv::new(0x1fa, FAN_VERY_HIGH),
            Tlv::new(0x1fe, target),
        ],
    );
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::with_options(
        ha,
        thinq,
        meta,
        CSL_FAN_MODES,
        false,
        Some(Box::new(add_console_features)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    // Standard RAC caps + CSL_306406_WW live captures from upstream PR #343.
    const CAPS_HEX: &str = "0000040000008702010249B001B05057B0A0017CB0C1B103B306B34FB4C7B582B541B543B6A04D81B6F0690409B701BC40BD47B5C0B61024B643B5C1B600B643B5C2B600B643B5C4B61026B643B5C6B6102CB64344E1";
    const CSL_STATUS_HEX: &str = "000004000000A7020400687DC17E447E887F50277F90267F0086808840D4C0D500C84081408180C9408340838083C18FC0CD40CD00CCC0CDA00212AD901AACC0AD5019B54ED560090ED5A00960BC88D5D03CD61020C9009C4087CCAC40E9C06640668066C067006740678067C068008F80F7404F71";
    const CSL_FAN_HIGH_HEX: &str = "000004000000a70204000c7dc17e447f903c7e867f50279cc5";
    const CSL_FAN_VERY_HIGH_HEX: &str = "000004000000a70204000c7dc17e447f903c7e877f50278ce4";
    const CSL_BEEP_ON_HEX: &str = "000004000000A702040002E9C0924C";
    const CSL_BEEP_OFF_HEX: &str = "000004000000A702040002E9C1826D";
    const CSL_PLASMA_ON_HEX: &str = "000004000000A70204000283C1668C";
    const CSL_PLASMA_OFF_HEX: &str = "000004000000A70204000283C076AD";

    fn make_ready() -> (
        Arc<MockHaConnection>,
        Arc<MockThinq2Device>,
        Arc<dyn DeviceHandler>,
    ) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("CSL_306406_WW", "CSL_306406_WW", "1.0");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        let dev = create(ha.clone(), thinq.clone(), meta);
        let d = dev.clone();
        ha.on_set_property(move |id, prop, val| {
            if id == DEVICE_ID {
                d.set_property(prop, val);
            }
        });
        thinq.emit_data(&hex_decode(CAPS_HEX));
        thinq.emit_data(&hex_decode(CSL_STATUS_HEX));
        thinq.emit_data(&hex_decode(CSL_STATUS_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    #[test]
    fn fan_modes_switches_filter_hours_and_jet_button() {
        let (ha, thinq, _dev) = make_ready();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["jet"]["platform"], "button");

        // Filter life 2400 h, remaining 2318 h -> 82 h used.
        assert_eq!(
            ha.get_property(DEVICE_ID, "filterlifetime", "state"),
            Some(PropertyValue::Int(2400))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "filterusedtime", "state"),
            Some(PropertyValue::Int(82))
        );

        // Fan speed codes (6 = high, 7 = very high).
        thinq.emit_data(&hex_decode(CSL_FAN_HIGH_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some("high".into())
        );
        thinq.emit_data(&hex_decode(CSL_FAN_VERY_HIGH_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some("very high".into())
        );
        ha.set_property(DEVICE_ID, "climate", "fan_mode_command", "high");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "01010400000065020101077E867E447F903C5C38"
        );

        // Beep (0x3a7) and Plasma (0x20f).
        thinq.emit_data(&hex_decode(CSL_BEEP_ON_HEX));
        assert_eq!(ha.get_property(DEVICE_ID, "beep", "state"), Some("ON".into()));
        thinq.emit_data(&hex_decode(CSL_BEEP_OFF_HEX));
        assert_eq!(ha.get_property(DEVICE_ID, "beep", "state"), Some("OFF".into()));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "beep", "command", "OFF");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "0101040000006502010102E9C1C178"
        );

        thinq.emit_data(&hex_decode(CSL_PLASMA_ON_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "plasma", "state"),
            Some("ON".into())
        );
        thinq.emit_data(&hex_decode(CSL_PLASMA_OFF_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "plasma", "state"),
            Some("OFF".into())
        );
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "plasma", "command", "ON");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "010104000000650201010283C12599"
        );

        // Auto dry (0x20e).
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "autodrysetting", "command", "ON");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "01010400000065020101038390FFB80D"
        );

        // Jet button while heating sets fan=7 and temp=30 C (60).
        thinq.emit_data(&hex_decode(CSL_STATUS_HEX));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "jet", "command", "PRESS");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "01010400000065020101077E447E877F903C4130"
        );
    }
}
