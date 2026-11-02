//! WIN_056905_WW — LG window air conditioner (e.g. LW1823HRSM).

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
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

        let mut config = default_config(&meta, Some(json!({"name": "LG Air Conditioner"})));
        config.components.insert(
            "climate".into(),
            json!({
                "platform": "climate",
                "unique_id": "$deviceid-climate",
                "name": null,
                "temperature_unit": "C",
                "temp_step": 0.5,
                "precision": 0.5,
                "modes": ["off", "cool", "fan_only", "heat"],
                "fan_modes": ["low", "high"],
                "swing_modes": ["on", "off"],
            }),
        );

        this.add_fields(&mut config);
        core.set_config(config);
        this
    }

    fn add_fields(self: &Arc<Self>, config: &mut DeviceDiscovery) {
        let core = &self.core;

        core.add_field(
            config,
            FieldDefinition::new("climate", "current_temperature")
                .with_id(0x1fd)
                .read_only()
                .state_topic("topic")
                .read_xform(|raw| Some(PropertyValue::from(raw as f64 / 2.0))),
            true,
        );

        core.add_field(
            config,
            FieldDefinition::new("climate", "temperature")
                .with_id(0x1fe)
                .read_xform(|raw| Some(PropertyValue::from(raw as f64 / 2.0)))
                .write_xform(|val_str| {
                    let val: f64 = val_str.parse().ok()?;
                    let min_cel = 16.0;
                    let max_cel = 30.0;
                    let clamped = if val < min_cel {
                        min_cel
                    } else if val > max_cel {
                        max_cel
                    } else {
                        val
                    };
                    Some(PropertyValue::Int((clamped * 2.0).round() as i64))
                })
                .write_attach_static(vec![0x1f9, 0x1fa]),
            true,
        );

        {
            let core_c = core.clone();
            let mut power = FieldDefinition::new("climate", "power")
                .with_id(0x1f7)
                .write_only()
                .write_xform(|val| Some(if val == "ON" { 1i64.into() } else { 0i64.into() }))
                .write_attach(|raw| if raw != 0 { vec![0x1f9] } else { vec![] })
                .read_xform(|raw| Some(if raw != 0 { "ON".into() } else { "OFF".into() }));
            power.read_callback = Some(Box::new(move |_val| {
                if let Some(mode) = core_c.get_raw(0x1f9) {
                    core_c.process_key_value(0x1f9, mode);
                }
                false
            }));
            // write_only sets readable=false; restore readable for read_callback path via process_key_value
            // but power should not publish — keep readable false and use read_callback on reprocess of mode.
            // Actually TS has readable:false and read_callback that reprocesses mode. read path:
            // doRead = read_callback(...); if doRead && readable → publish. So readable false is fine.
            // But read_callback only runs if field is found — and readable false still runs callback:
            //   if doRead && def.readable → publish. Callback still runs. Good.
            // Wait — write_only sets readable=false. Field is still in fields_by_id. process_key_value
            // runs read_xform then read_callback. Good.
            power.readable = false;
            power.writable = true;
            core.add_field(config, power, true);
        }

        {
            let core_c = core.clone();
            let core_w = core.clone();
            let mut mode = FieldDefinition::new("climate", "mode").with_id(0x1f9);
            mode.read_xform = Some(Box::new(move |raw| {
                if core_c.get_raw(0x1f7) == Some(0) {
                    return Some("off".into());
                }
                let modes2ha: [Option<&str>; 9] = [
                    Some("cool"),
                    None,
                    Some("fan_only"),
                    None,
                    Some("heat"),
                    None,
                    None,
                    None,
                    None,
                ];
                modes2ha.get(raw as usize).and_then(|m| *m).map(|s| s.into())
            }));
            mode.write_xform = Some(Box::new(move |val| {
                if val == "off" {
                    core_w.set_property("climate-power", "OFF");
                } else {
                    core_w.set_property("climate-power", "ON");
                }
                let modes2clip: &[(&str, i64)] =
                    &[("cool", 0), ("fan_only", 2), ("heat", 4), ("dry", 8)];
                modes2clip
                    .iter()
                    .find(|(k, _)| *k == val)
                    .map(|(_, v)| PropertyValue::Int(*v))
            }));
            mode.write_attach_static = Some(vec![0x1f7, 0x1fa, 0x1fe, 0x322]);
            core.add_field(config, mode, true);
        }

        core.add_field(
            config,
            FieldDefinition::new("climate", "fan_mode")
                .with_id(0x1fa)
                .read_xform(|raw| {
                    match raw {
                        2 => Some("low".into()),
                        6 => Some("high".into()),
                        _ => None,
                    }
                })
                .write_xform(|val| {
                    match val {
                        "low" => Some(2i64.into()),
                        "high" => Some(6i64.into()),
                        _ => None,
                    }
                })
                .write_attach_static(vec![0x1f9, 0x1fe]),
            true,
        );

        core.add_field(
            config,
            FieldDefinition::new("climate", "swing_mode")
                .with_id(0x322)
                .read_xform(|raw| {
                    match raw {
                        0 => Some("off".into()),
                        100 => Some("on".into()),
                        _ => None,
                    }
                })
                .write_xform(|val| {
                    match val {
                        "off" => Some(0i64.into()),
                        "on" => Some(100i64.into()),
                        _ => None,
                    }
                })
                .write_attach_static(vec![0x1f9, 0x1fa]),
            true,
        );
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

/// Fluent helpers for FieldDefinition (local extension).
trait FieldDefExt {
    fn state_topic(self, topic: &str) -> Self;
    fn read_xform(self, f: impl Fn(u32) -> Option<PropertyValue> + Send + Sync + 'static) -> Self;
    fn write_xform(self, f: impl Fn(&str) -> Option<PropertyValue> + Send + Sync + 'static) -> Self;
    fn write_attach(self, f: impl Fn(u32) -> Vec<u16> + Send + Sync + 'static) -> Self;
    fn write_attach_static(self, ids: Vec<u16>) -> Self;
}

impl FieldDefExt for FieldDefinition {
    fn state_topic(mut self, topic: &str) -> Self {
        self.state_topic = Some(topic.into());
        self
    }
    fn read_xform(mut self, f: impl Fn(u32) -> Option<PropertyValue> + Send + Sync + 'static) -> Self {
        self.read_xform = Some(Box::new(f));
        self
    }
    fn write_xform(mut self, f: impl Fn(&str) -> Option<PropertyValue> + Send + Sync + 'static) -> Self {
        self.write_xform = Some(Box::new(f));
        self
    }
    fn write_attach(mut self, f: impl Fn(u32) -> Vec<u16> + Send + Sync + 'static) -> Self {
        self.write_attach = Some(Box::new(f));
        self
    }
    fn write_attach_static(mut self, ids: Vec<u16>) -> Self {
        self.write_attach_static = Some(ids);
        self
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
    use rethink_core::{hex_encode, MockHaConnection, MockThinq2Device};
    use rethink_util::tlv;

    const DEVICE_ID: &str = "test-id";

    fn meta() -> Metadata {
        Metadata::new("WIN_056905_WW", "LW1823HRSM", "1.0")
    }

    #[test]
    fn constructor_sends_query_caps() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let _dev = Device::new(ha, thinq.clone(), meta());
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "01010400000065020201027D416A0D"
        );
    }

    #[test]
    fn config_exposes_climate() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let _dev = Device::new(ha.clone(), thinq, meta());
        let device = ha.device(DEVICE_ID).expect("config");
        let climate = device.config.as_ref().unwrap().components.get("climate").unwrap();
        assert_eq!(climate["platform"], "climate");
        assert_eq!(climate["modes"], json!(["off", "cool", "fan_only", "heat"]));
        assert_eq!(climate["fan_modes"], json!(["low", "high"]));
    }

    /// A unit in cool, fan low, 22 °C, swing off; powered as given.
    fn make_device(power: u32) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        let d = dev.clone();
        ha.on_set_property(move |id, prop, value| {
            if id == DEVICE_ID {
                d.set_property(prop, value);
            }
        });
        for (id, v) in [(0x1f7, power), (0x1f9, 0), (0x1fa, 2), (0x1fe, 44), (0x322, 0)] {
            dev.set_raw(id, v);
        }
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn sent_tlvs(thinq: &MockThinq2Device) -> Vec<Vec<(u16, u32)>> {
        thinq
            .outbox()
            .iter()
            .map(|p| tlv::parse(&p[11..p.len() - 2]).into_iter().map(|e| (e.t, e.v)).collect())
            .collect()
    }

    #[test]
    fn mode_off_turns_the_unit_off() {
        let (ha, thinq, _dev) = make_device(1);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "off");
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(sent, ["01010400000065020101027DC00576"]); // 0x1f7=0
    }

    #[test]
    fn mode_from_off_turns_the_unit_on() {
        let (ha, thinq, _dev) = make_device(0);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "heat");
        assert_eq!(
            sent_tlvs(&thinq),
            [
                vec![(0x1f7, 1), (0x1f9, 0)],
                vec![(0x1f9, 4), (0x1f7, 1), (0x1fa, 2), (0x1fe, 44), (0x322, 0)],
            ]
        );
    }
}
