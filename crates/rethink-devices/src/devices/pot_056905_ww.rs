//! POT_056905_WW — LG portable AC (LP1022FVSM).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::json;
use std::sync::Arc;

pub struct Device {
    pub core: Arc<TlvDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = TlvDeviceCore::new(ha, thinq);
        let this = Arc::new(Self { core: core.clone() });

        *core.is_caps_response.lock() = Some(Box::new(|tlv| tlv.iter().any(|e| e.t == 0x2da)));
        *core.is_values_response.lock() = Some(Box::new(|tlv| {
            tlv.len() >= 10 && tlv.iter().any(|e| e.t == 0x1f7)
        }));

        let mut config = default_config(&meta, Some(json!({"name": "LG Portable AC"})));
        config.components.insert(
            "climate".into(),
            json!({
                "platform": "climate",
                "unique_id": "$deviceid-climate",
                "name": null,
                "temperature_unit": "C",
                "temp_step": 1,
                "precision": 1,
                "modes": ["off", "cool", "dry", "fan_only"],
                "fan_modes": ["low", "medium", "high"],
                "swing_modes": ["on", "off"],
            }),
        );
        config.components.insert(
            "jet".into(),
            json!({
                "platform": "switch",
                "unique_id": "$deviceid-jet",
                "name": "Jet",
                "icon": "mdi:wind-power",
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
                .with_id(0x1fd)
                .read_only();
            f.state_topic = Some("topic".into());
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            core.add_field(config, f, true);
        }

        {
            let mut f = FieldDefinition::new("climate", "temperature").with_id(0x1fe);
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            f.write_xform = Some(Box::new(|val_str| {
                let val: f64 = val_str.parse().ok()?;
                let min_cel = 16.0_f64;
                let max_cel = 30.0_f64;
                let clamped = val.clamp(min_cel, max_cel);
                Some(PropertyValue::Int((clamped * 2.0).round() as i64))
            }));
            f.write_attach_static = Some(vec![0x1f9, 0x1fa]);
            core.add_field(config, f, true);
        }

        {
            let core_c = core.clone();
            let mut power = FieldDefinition::new("climate", "power").with_id(0x1f7);
            power.readable = false;
            power.write_xform = Some(Box::new(|val| {
                Some(if val == "ON" {
                    1i64.into()
                } else {
                    0i64.into()
                })
            }));
            power.write_attach = Some(Box::new(|raw| {
                if raw != 0 {
                    vec![0x1f9]
                } else {
                    vec![]
                }
            }));
            power.read_xform = Some(Box::new(|raw| {
                Some(if raw != 0 { "ON".into() } else { "OFF".into() })
            }));
            power.read_callback = Some(Box::new(move |_val| {
                if let Some(mode) = core_c.get_raw(0x1f9) {
                    core_c.process_key_value(0x1f9, mode);
                }
                false
            }));
            core.add_field(config, power, true);
        }

        {
            let core_r = core.clone();
            let core_w = core.clone();
            let core_cb = core.clone();
            let mut mode = FieldDefinition::new("climate", "mode").with_id(0x1f9);
            mode.read_xform = Some(Box::new(move |raw| {
                if core_r.get_raw(0x1f7) == Some(0) {
                    return Some("off".into());
                }
                match raw {
                    0 => Some("cool".into()),
                    1 => Some("dry".into()),
                    2 => Some("fan_only".into()),
                    _ => None,
                }
            }));
            mode.write_xform = Some(Box::new(move |val| {
                if val == "off" {
                    core_w.set_raw(0x1f7, 0);
                    return Some(PropertyValue::Int(core_w.get_raw(0x1f9).unwrap_or(0) as i64));
                }
                core_w.set_raw(0x1f7, 1);
                match val {
                    "cool" => Some(0i64.into()),
                    "dry" => Some(1i64.into()),
                    "fan_only" => Some(2i64.into()),
                    _ => None,
                }
            }));
            mode.write_callback = Some(Box::new(move |_num| {
                if core_cb.get_raw(0x1f7) == Some(0) {
                    core_cb.send(&[1, 1, 2, 1, 1], &[Tlv::new(0x1f7, 0)]);
                    return false;
                }
                true
            }));
            mode.write_attach_static = Some(vec![0x1f7, 0x1fa, 0x1fe]);
            core.add_field(config, mode, true);
        }

        {
            let mut f = FieldDefinition::new("climate", "fan_mode").with_id(0x1fa);
            f.read_xform = Some(Box::new(|raw| {
                match raw {
                    2 => Some("low".into()),
                    4 => Some("medium".into()),
                    6 => Some("high".into()),
                    _ => None,
                }
            }));
            f.write_xform = Some(Box::new(|val| {
                match val {
                    "low" => Some(2i64.into()),
                    "medium" => Some(4i64.into()),
                    "high" => Some(6i64.into()),
                    _ => None,
                }
            }));
            f.write_attach_static = Some(vec![0x1f9, 0x1fe]);
            core.add_field(config, f, true);
        }

        {
            let mut f = FieldDefinition::new("climate", "swing_mode").with_id(0x322);
            f.read_xform = Some(Box::new(|raw| {
                match raw {
                    0 => Some("off".into()),
                    100 => Some("on".into()),
                    _ => None,
                }
            }));
            f.write_xform = Some(Box::new(|val| {
                match val {
                    "off" => Some(0i64.into()),
                    "on" => Some(100i64.into()),
                    _ => None,
                }
            }));
            f.write_attach_static = Some(vec![0x1f9, 0x1fa]);
            core.add_field(config, f, true);
        }

        // Same TLV as Jet on the RAC units; the device reports 1 while Jet runs and 0 otherwise.
        {
            let mut f = FieldDefinition::new("jet", "").with_id(0x323);
            f.read_xform = Some(Box::new(|raw| {
                Some(if raw != 0 { "ON".into() } else { "OFF".into() })
            }));
            f.write_xform = Some(Box::new(|val| {
                Some(if val == "ON" {
                    1i64.into()
                } else {
                    0i64.into()
                })
            }));
            f.write_attach_static = Some(vec![0x1f9, 0x1fa]);
            core.add_field(config, f, true);
        }
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        self.core.set_property(prop, value);
    }

    pub fn get_raw(&self, id: u16) -> Option<u32> {
        self.core.get_raw(id)
    }

    pub fn set_raw(&self, id: u16, v: u32) {
        self.core.set_raw(id, v);
    }

    pub fn drop(&self) {
        self.core.drop_device();
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, PropertyValue};

    const DEVICE_ID: &str = "test-id";
    const CAPS_REQUEST_HEX: &str = "01010400000065020201027D416A0D";
    const QUERY_REQUEST_HEX: &str = "01010400000065020201027D425A6E";
    const CAPS_RESPONSE_HEX: &str = "000004000000A702019946B01011B047B09054B0C1B103B35010B4E04006B55060B6A0046FB6F0115100BD01B85020B8903CB8D020B9103CBC41BD47B5C0B61020B642B5C1B61029B642B5C2B61029B642AD3A";
    const QUERY_RESPONSE_HEX: &str = "000004000000A702049C477E417DC17E827F50277F9029C840868086C08700884089408A10188A50688A8F8CA002E88CD01BACE00258D55092D590FACAD04BCB10B0CB8CCBCFCC1048CC907C1B01C0C0AC403F51";
    // Notifications captured from an LG PA11WS (same ThinQ model) while Jet was toggled on the remote.
    // Jet on: 0x1fe=36 (18 °C), 0x321=0, 0x323=1. Jet off: 0x321=100, 0x323=0.
    const JET_ON_NOTIFY_HEX: &str = "000004000000A7020450077F9024C840C8C1B6E5";
    const JET_OFF_NOTIFY_HEX: &str = "000004000000A702045D05C85064C8C0F344";
    const WRITE_MODE_DRY_HEX: &str = "01010400000065020101097E417DC17E827F902AC337";
    const WRITE_MODE_FAN_ONLY_HEX: &str = "01010400000065020101097E427DC17E827F90293B21";
    const WRITE_POWER_OFF_HEX: &str = "01010400000065020101027DC00576";

    fn meta() -> Metadata {
        Metadata::new("POT_056905_WW", "LP1022FVSM", "115100")
    }

    fn make_device() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
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
        let (ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), QUERY_REQUEST_HEX);
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    #[test]
    fn config_exposes_expected_climate_component() {
        let (ha, _thinq, dev) = build_ready();
        let device = ha.device(DEVICE_ID).expect("HA configuration published");
        let climate = device.config.as_ref().unwrap().components.get("climate").unwrap();
        assert_eq!(climate["platform"], "climate");
        assert_eq!(
            climate["modes"],
            json!(["off", "cool", "dry", "fan_only"])
        );
        assert_eq!(climate["fan_modes"], json!(["low", "medium", "high"]));
        assert_eq!(climate["swing_modes"], json!(["on", "off"]));
        assert_eq!(climate["temp_step"], 1);
        assert_eq!(climate["precision"], 1);
        dev.drop_device();
    }

    #[test]
    fn initial_state_publishes_expected_properties() {
        let (ha, thinq, dev) = build_ready();
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));

        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some(PropertyValue::Str("dry".into()))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some(PropertyValue::Str("low".into()))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "current_temperature"),
            Some(PropertyValue::from(19.5))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "temperature_state"),
            Some(PropertyValue::from(20.5))
        );
        assert!(ha
            .get_property(DEVICE_ID, "climate", "swing_mode_state")
            .is_none());
        dev.drop_device();
    }

    #[test]
    fn write_mode_dry_from_off() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(0x1f7, 0);
        dev.set_raw(0x1f9, 0);
        dev.set_raw(0x1fa, 2);
        dev.set_raw(0x1fe, 42);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "dry");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_MODE_DRY_HEX);
        dev.drop_device();
    }

    #[test]
    fn write_mode_fan_only() {
        let (ha, thinq, dev) = build_ready();
        ha.set_property(DEVICE_ID, "climate", "mode_command", "fan_only");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_MODE_FAN_ONLY_HEX);
        dev.drop_device();
    }

    #[test]
    fn write_mode_off_sends_power_off() {
        let (ha, thinq, dev) = build_ready();
        ha.set_property(DEVICE_ID, "climate", "mode_command", "off");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_OFF_HEX);
        dev.drop_device();
    }

    #[test]
    fn jet_notifications_publish_the_jet_switch_state() {
        let (ha, thinq, dev) = build_ready();
        thinq.emit_data(&hex_decode(JET_ON_NOTIFY_HEX));
        assert_eq!(ha.get_property(DEVICE_ID, "jet", "state"), Some("ON".into()));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "temperature_state"),
            Some(PropertyValue::from(18.0))
        );
        thinq.emit_data(&hex_decode(JET_OFF_NOTIFY_HEX));
        assert_eq!(ha.get_property(DEVICE_ID, "jet", "state"), Some("OFF".into()));
        dev.drop_device();
    }

    #[test]
    fn jet_writes_attach_mode_and_fan() {
        let (ha, thinq, dev) = build_ready();
        let c = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(c["jet"]["platform"], "switch");
        ha.set_property(DEVICE_ID, "jet", "command", "ON");
        let out = thinq.outbox();
        assert_eq!(out.len(), 1);
        let sent: Vec<(u16, u32)> = rethink_util::tlv::parse(&out[0][11..out[0].len() - 2])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect();
        // the captured values: dry (1), fan low (2)
        assert_eq!(sent, [(0x323, 1), (0x1f9, 1), (0x1fa, 2)]);
        dev.drop_device();
    }

    #[test]
    fn a7_caps_triggers_values_query() {
        let (_ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), QUERY_REQUEST_HEX);
        dev.drop_device();
    }

    #[test]
    fn constructor_sends_query_caps() {
        let (_ha, thinq, dev) = make_device();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CAPS_REQUEST_HEX);
        dev.drop_device();
    }
}
