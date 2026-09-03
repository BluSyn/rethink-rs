//! DHUM_231006_WW — Korean dehumidifier (PR #113), same TLV map as DHUM_056905_WW
//! with different mode/fan tables verified against LG cloud decode.

use crate::device_trait::DeviceHandler;
use rethink_util::sync::Mutex;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::tlv::Tlv;
use serde_json::json;
use std::sync::Arc;

const CAPS_ONLY_TAGS: &[u16] = &[0x2d5, 0x2d6, 0x336, 0x2e5, 0x2e6, 0x2da];
const BUCKET_EMPTIED_EVENT: u32 = 256;
const HA_MODES: &[&str] = &["Smart Plus", "Silent", "Intensive", "Quick"];
const SILENT_MODES: &[u32] = &[19];
const FAN_CLIP_VALUES: &[u32] = &[2, 4, 6, 7, 8];

fn clip_to_ha_mode(raw: u32) -> String {
    match raw {
        19 => "Silent".into(),
        20 => "Intensive".into(),
        85 => "Quick".into(),
        86 => "Smart Plus".into(),
        n => format!("mode{n}"),
    }
}

fn ha_to_clip_mode(mode: &str) -> Option<u32> {
    match mode {
        "Smart Plus" => Some(86),
        "Silent" => Some(19),
        "Intensive" => Some(20),
        "Quick" => Some(85),
        _ => None,
    }
}

fn fan_to_ha(raw: u32) -> String {
    match raw {
        2 => "Low".into(),
        4 => "Mid".into(),
        6 => "High".into(),
        7 => "Turbo".into(),
        8 => "Auto".into(),
        n => n.to_string(),
    }
}

fn fan_to_clip(val: &str) -> Option<u32> {
    match val {
        "Low" => Some(2),
        "Mid" => Some(4),
        "High" => Some(6),
        "Turbo" => Some(7),
        "Auto" => Some(8),
        other => other.parse().ok(),
    }
}

struct Inner {
    mode_clip_prev: Option<u32>,
    initial_values_received: bool,
    bucket_full_ha_state: Option<bool>,
}

pub struct Device {
    pub core: Arc<TlvDeviceCore>,
    inner: Mutex<Inner>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = TlvDeviceCore::new(ha.clone(), thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            inner: Mutex::new(Inner {
                mode_clip_prev: None,
                initial_values_received: false,
                bucket_full_ha_state: None,
            }),
        });

        *core.is_caps_response.lock() = Some(Box::new(|tlv| tlv.iter().any(|e| e.t == 0x2da)));
        *core.is_values_response.lock() = Some(Box::new(|tlv| {
            tlv.iter().any(|e| {
                matches!(
                    e.t,
                    0x1f7 | 0x1f9 | 0x1fa | 0x21b | 0x21e | 0x2b2 | 0x253 | 0x2a2 | 0x336 | 0x360
                )
            })
        }));

        let t_vals = this.clone();
        *core.on_values.lock() = Some(Box::new(move || t_vals.values_received()));

        let t_kv = this.clone();
        *core.key_value_intercept.lock() = Some(Box::new(move |k, v| t_kv.intercept_key_value(k, v)));

        let mut config = default_config(&meta, Some(json!({"name": "LG Dehumidifier"})));
        config.components.insert(
            "humidifier".into(),
            json!({
                "platform": "humidifier",
                "unique_id": "$deviceid-humidifier",
                "name": null,
                "device_class": "dehumidifier",
                "modes": HA_MODES,
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
        config.components.insert(
            "fan_speed".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-fan_speed",
                "name": "Fan speed",
                "icon": "mdi:fan",
                "options": ["Auto", "Low", "Mid", "High", "Turbo"],
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
            "bucket_full".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-bucket_full",
                "name": "Bucket full",
                "icon": "mdi:water-alert",
                "device_class": "problem",
                "payload_on": "ON",
                "payload_off": "OFF",
                "state_topic": "$this/bucket_full-",
            }),
        );
        let (k, v) = rethink_core::notification_event(
            "bucket_alert",
            "Bucket alert",
            &["bucket_full", "bucket_ok"],
            None, // event device_class: only doorbell|button|motion
        );
        config.components.insert(k, v);

        this.add_fields(&mut config);
        if let Some(serde_json::Value::Object(hum)) = config.components.get_mut("humidifier") {
            hum.insert("state_topic".into(), json!("$this/humidifier-power"));
            hum.insert(
                "command_topic".into(),
                json!("$this/humidifier-power/set"),
            );
        }
        core.set_config(config);
        this
    }

    fn add_fields(self: &Arc<Self>, config: &mut DeviceDiscovery) {
        let core = &self.core;

        {
            let mut power = FieldDefinition::new("humidifier", "power").with_id(0x1f7);
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
            core.add_field(config, power, false);
        }

        {
            let this_r = self.clone();
            let this_w = self.clone();
            let mut mode = FieldDefinition::new("humidifier", "mode").with_id(0x1f9);
            mode.read_xform = Some(Box::new(|raw| Some(clip_to_ha_mode(raw).into())));
            mode.read_callback = Some(Box::new(move |_val| {
                if let Some(mode) = this_r.core.get_raw(0x1f9) {
                    let mut inner = this_r.inner.lock();
                    let was_silent = inner
                        .mode_clip_prev
                        .map(|m| SILENT_MODES.contains(&m))
                        .unwrap_or(false);
                    if SILENT_MODES.contains(&mode) && !was_silent {
                        drop(inner);
                        this_r.publish_fan_speed_state(Some("Low"));
                        this_r.inner.lock().mode_clip_prev = Some(mode);
                    } else {
                        inner.mode_clip_prev = Some(mode);
                    }
                }
                true
            }));
            mode.write_xform = Some(Box::new(move |val| {
                if val == "off" || val.is_empty() {
                    this_w.core.set_property("humidifier-power", "OFF");
                    return None;
                }
                this_w.core.set_raw(0x1f7, 1);
                let clip = ha_to_clip_mode(val).unwrap_or_else(|| val.parse().unwrap_or(0));
                {
                    let mut inner = this_w.inner.lock();
                    let was_silent = inner
                        .mode_clip_prev
                        .map(|m| SILENT_MODES.contains(&m))
                        .unwrap_or(false);
                    if val == "Silent" && !was_silent {
                        this_w.core.set_raw(0x1fa, 2);
                        drop(inner);
                        this_w.publish_fan_speed_state(Some("Low"));
                        this_w.inner.lock().mode_clip_prev = Some(clip);
                    } else {
                        inner.mode_clip_prev = Some(clip);
                    }
                }
                Some(PropertyValue::Int(clip as i64))
            }));
            mode.write_attach_static = Some(vec![0x1f7]);
            core.add_field(config, mode, true);
        }

        {
            let this_r = self.clone();
            let this_w = self.clone();
            let mut fan = FieldDefinition::new("fan_speed", "").with_id(0x1fa);
            fan.read_xform = Some(Box::new(|raw| Some(fan_to_ha(raw).into())));
            fan.read_callback = Some(Box::new(move |val| {
                let s = match val {
                    PropertyValue::Str(s) => s.clone(),
                    other => other.as_string(),
                };
                this_r.publish_fan_speed_state(Some(&s));
                false
            }));
            fan.write_xform = Some(Box::new(|val| {
                fan_to_clip(val).map(|w| PropertyValue::Int(w as i64))
            }));
            fan.write_callback = Some(Box::new(move |val| {
                if !FAN_CLIP_VALUES.contains(&(val as u32)) {
                    return false;
                }
                // PR #113: send 0x1fa alone (no per-mode fan table)
                this_w
                    .core
                    .send(&[1, 1, 2, 1, 1], &[Tlv::new(0x1fa, val as u32)]);
                this_w.core.set_raw(0x1fa, val as u32);
                false
            }));
            core.add_field(config, fan, true);
        }

        {
            let mut f = FieldDefinition::new("humidifier", "current_humidity")
                .with_id(0x336)
                .read_only();
            f.state_topic = Some("topic".into());
            core.add_field(config, f, true);
        }

        {
            let core_w = core.clone();
            let mut f = FieldDefinition::new("humidifier", "target_humidity").with_id(0x253);
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::Int(raw as i64))));
            f.write_xform = Some(Box::new(move |val_str| {
                let mut val: f64 = val_str.parse().ok()?;
                val = val.clamp(30.0, 70.0).round();
                core_w.set_raw(0x1f7, 1);
                Some(PropertyValue::Int(val as i64))
            }));
            f.write_attach_static = Some(vec![0x1f7, 0x1f9]);
            core.add_field(config, f, true);
        }

        for (id, comp) in [(0x360, "ionizer"), (0x2a2, "uv_nano"), (0x21e, "bucket_light")] {
            let mut f = FieldDefinition::new(comp, "").with_id(id);
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
            if id != 0x21e {
                f.write_attach_static = Some(vec![0x1f7, 0x1f9]);
            }
            core.add_field(config, f, true);
        }
    }

    fn publish_fan_speed_state(&self, override_s: Option<&str>) {
        let state = if let Some(s) = override_s {
            s.to_string()
        } else {
            self.core
                .get_raw(0x1fa)
                .map(fan_to_ha)
                .unwrap_or_else(|| "Low".into())
        };
        self.core
            .ha
            .publish_property(&self.core.id, "fan_speed-", state.into());
    }

    fn publish_bucket_full_state(&self, full: bool) {
        {
            let mut inner = self.inner.lock();
            if inner.bucket_full_ha_state == Some(full) {
                return;
            }
            inner.bucket_full_ha_state = Some(full);
        }
        self.core.ha.publish_property(
            &self.core.id,
            "bucket_full-",
            if full { "ON".into() } else { "OFF".into() },
        );
        self.core.ha.fire_notification_event(
            &self.core.id,
            "bucket_alert",
            if full { "bucket_full" } else { "bucket_ok" },
        );
    }

    fn intercept_key_value(&self, k: u16, v: u32) -> bool {
        let waiting_caps = self.core.waiting_caps.load(std::sync::atomic::Ordering::Relaxed);
        if waiting_caps && CAPS_ONLY_TAGS.contains(&k) {
            self.core.set_raw(k, v);
            return true;
        }
        if k == 0x2d7 || k == 0x2d8 || k == 0x2d9 {
            return true;
        }
        if k == 0x2b1 {
            self.core.set_raw(k, v);
            if v == BUCKET_EMPTIED_EVENT {
                self.publish_bucket_full_state(false);
            }
            return true;
        }
        if k == 0x2b2 {
            self.core.set_raw(k, v);
            self.publish_bucket_full_state(v != 0);
            return true;
        }
        false
    }

    fn values_received(&self) {
        let mut inner = self.inner.lock();
        if inner.initial_values_received {
            return;
        }
        inner.initial_values_received = true;
        drop(inner);
        self.core
            .thinq
            .send("setMaskingInfo", 0, json!({ "blacklist_tlv": "1200" }));
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        self.core.set_property(prop, value);
    }

    pub fn process_key_value(&self, k: u16, v: u32) {
        self.core.process_key_value(k, v);
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
    use rethink_core::{MockHaConnection, MockThinq2Device};
    use rethink_util::tlv;

    const DEVICE_ID: &str = "test-id";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("DHUM_231006_WW", "TEST DHUM KR", "1");
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

    fn written_fields(thinq: &MockThinq2Device) -> Vec<(u16, u32)> {
        let packet = thinq.outbox().last().cloned().expect("packet");
        tlv::parse(&packet[11..packet.len() - 2])
            .into_iter()
            .map(|t| (t.t, t.v))
            .collect()
    }

    #[test]
    fn config_korean_modes_and_five_fans() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(
            comps["humidifier"]["modes"],
            json!(["Smart Plus", "Silent", "Intensive", "Quick"])
        );
        assert_eq!(
            comps["fan_speed"]["options"],
            json!(["Auto", "Low", "Mid", "High", "Turbo"])
        );
    }

    #[test]
    fn mode_codes_decode_and_write() {
        let (ha, thinq, dev) = make();
        for (label, wire) in [
            ("Silent", 19u32),
            ("Intensive", 20),
            ("Quick", 85),
            ("Smart Plus", 86),
        ] {
            dev.process_key_value(0x1f9, wire);
            assert_eq!(
                ha.device(DEVICE_ID)
                    .unwrap()
                    .properties
                    .get("humidifier-mode")
                    .map(|x| x.as_string())
                    .as_deref(),
                Some(label)
            );
            thinq.reset_recorder();
            // need initial raw for attach fields
            dev.core.set_raw(0x1f7, 1);
            dev.set_property("humidifier-mode", label);
            let fields = written_fields(&thinq);
            assert!(fields.contains(&(0x1f9, wire)), "{label}");
            assert!(fields.contains(&(0x1f7, 1)), "power for {label}");
        }
    }

    #[test]
    fn fan_mid_turbo_auto_write_1fa_alone() {
        let (ha, thinq, dev) = make();
        for (label, wire) in [
            ("Low", 2u32),
            ("Mid", 4),
            ("High", 6),
            ("Turbo", 7),
            ("Auto", 8),
        ] {
            dev.process_key_value(0x1fa, wire);
            assert_eq!(
                ha.device(DEVICE_ID)
                    .unwrap()
                    .properties
                    .get("fan_speed-")
                    .map(|x| x.as_string())
                    .as_deref(),
                Some(label)
            );
            thinq.reset_recorder();
            dev.set_property("fan_speed-", label);
            assert_eq!(written_fields(&thinq), vec![(0x1fa, wire)], "{label}");
        }
    }

    #[test]
    fn humidity_and_power_roundtrip() {
        let (ha, thinq, dev) = make();
        // 0x336 is treated as caps-only while waiting_caps; clear that gate.
        dev.core
            .waiting_caps
            .store(false, std::sync::atomic::Ordering::Relaxed);
        dev.core
            .waiting_values
            .store(false, std::sync::atomic::Ordering::Relaxed);
        dev.process_key_value(0x1f7, 1);
        dev.process_key_value(0x253, 45);
        dev.process_key_value(0x336, 55);
        let p = ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(
            p.get("humidifier-power").map(|x| x.as_string()).as_deref(),
            Some("ON")
        );
        assert_eq!(
            p.get("humidifier-target_humidity")
                .map(|x| x.as_string())
                .as_deref(),
            Some("45")
        );
        assert_eq!(
            p.get("humidifier-current_humidity")
                .map(|x| x.as_string())
                .as_deref(),
            Some("55")
        );
        thinq.reset_recorder();
        dev.set_property("humidifier-target_humidity", "55");
        let fields = written_fields(&thinq);
        assert!(fields.iter().any(|(t, v)| *t == 0x253 && *v == 55));
    }
}
