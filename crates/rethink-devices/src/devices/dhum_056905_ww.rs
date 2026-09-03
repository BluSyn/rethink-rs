//! DHUM_056905_WW — LG dehumidifier (deviceType 403).

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
const SILENT_MODES: &[u32] = &[2, 19];
const HA_MODES: &[&str] = &["Smart", "Jet", "Silent", "Spot", "Laundry"];

struct ModeFanCap {
    mode: u32,
    fixed_fan: Option<u32>,
}

const MODE_FAN_CAPS: &[ModeFanCap] = &[
    ModeFanCap {
        mode: 17,
        fixed_fan: None,
    },
    ModeFanCap {
        mode: 18,
        fixed_fan: None,
    },
    ModeFanCap {
        mode: 20,
        fixed_fan: None,
    },
    ModeFanCap {
        mode: 21,
        fixed_fan: Some(6),
    }, // Laundry
    ModeFanCap {
        mode: 22,
        fixed_fan: None,
    },
];

fn clip_to_ha_mode(raw: u32) -> String {
    match raw {
        0 | 17 => "Smart".into(),
        1 | 18 => "Jet".into(),
        2 | 19 => "Silent".into(),
        4 | 20 => "Spot".into(),
        5 | 21 => "Laundry".into(),
        n => format!("mode{n}"),
    }
}

fn ha_to_clip_mode(mode: &str) -> Option<u32> {
    match mode {
        "Smart" => Some(17),
        "Jet" => Some(18),
        "Silent" => Some(19),
        "Spot" => Some(20),
        "Laundry" => Some(21),
        _ => None,
    }
}

fn normalize_ha_mode(val: &str) -> String {
    let mut c = val.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
    }
}

struct Inner {
    power_state_prev: Option<bool>,
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
                power_state_prev: None,
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
                "options": ["low", "high"],
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
        // Bucket full is already a problem binary_sensor — automate on state.
        // Rising-edge event for one-shot automations (bucket just full / emptied).
        let (k, v) = rethink_core::notification_event(
            "bucket_alert",
            "Bucket alert",
            &["bucket_full", "bucket_ok"],
            None, // event device_class: only doorbell|button|motion
        );
        config.components.insert(k, v);

        this.add_fields(&mut config);

        // Wire bare humidifier state/command to power property
        if let Some(serde_json::Value::Object(hum)) = config.components.get_mut("humidifier") {
            hum.insert("state_topic".into(), json!("$this/humidifier-power"));
            hum.insert("command_topic".into(), json!("$this/humidifier-power/set"));
        }

        core.set_config(config);
        this
    }

    fn add_fields(self: &Arc<Self>, config: &mut DeviceDiscovery) {
        let core = &self.core;

        // power (0x1f7) — autoreg false
        {
            let this = self.clone();
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
            power.read_callback = Some(Box::new(move |val| {
                let power_state = matches!(val, PropertyValue::Str(s) if s == "ON");
                let mut inner = this.inner.lock();
                inner.power_state_prev = Some(power_state);
                true
            }));
            core.add_field(config, power, false);
        }

        // mode
        {
            let this_r = self.clone();
            let this_w = self.clone();
            let mut mode = FieldDefinition::new("humidifier", "mode").with_id(0x1f9);
            mode.read_xform = Some(Box::new(|raw| Some(clip_to_ha_mode(raw).into())));
            mode.read_callback = Some(Box::new(move |_val| {
                let mode = this_r.core.get_raw(0x1f9);
                if let Some(mode) = mode {
                    let mut inner = this_r.inner.lock();
                    let was_silent = inner
                        .mode_clip_prev
                        .map(|m| SILENT_MODES.contains(&m))
                        .unwrap_or(false);
                    if SILENT_MODES.contains(&mode) && !was_silent {
                        drop(inner);
                        this_r.publish_fan_speed_state(Some("low"));
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
                let mode = normalize_ha_mode(val);
                let clip = ha_to_clip_mode(&mode).unwrap_or_else(|| val.parse().unwrap_or(0));
                {
                    let mut inner = this_w.inner.lock();
                    let was_silent = inner
                        .mode_clip_prev
                        .map(|m| SILENT_MODES.contains(&m))
                        .unwrap_or(false);
                    if mode == "Silent" && !was_silent {
                        this_w.core.set_raw(0x1fa, 2);
                        drop(inner);
                        this_w.publish_fan_speed_state(Some("low"));
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

        // fan_speed
        {
            let this_r = self.clone();
            let this_w = self.clone();
            let mut fan = FieldDefinition::new("fan_speed", "").with_id(0x1fa);
            fan.read_xform = Some(Box::new(|raw| {
                Some(match raw {
                    2 => "low".into(),
                    6 => "high".into(),
                    n => PropertyValue::Str(n.to_string()),
                })
            }));
            fan.read_callback = Some(Box::new(move |val| {
                let s = match val {
                    PropertyValue::Str(s) => s.clone(),
                    other => other.as_string(),
                };
                this_r.publish_fan_speed_state(Some(&s));
                false
            }));
            fan.write_xform = Some(Box::new(|val| {
                Some(match val {
                    "low" => 2i64.into(),
                    "high" => 6i64.into(),
                    other => other.parse::<i64>().ok()?.into(),
                })
            }));
            fan.write_callback = Some(Box::new(move |val| {
                if val != 2 && val != 6 {
                    return false;
                }
                this_w.send_fan_speed_tlvs(val as u32);
                false
            }));
            core.add_field(config, fan, true);
        }

        // current humidity 0x336
        {
            let mut f = FieldDefinition::new("humidifier", "current_humidity")
                .with_id(0x336)
                .read_only();
            f.state_topic = Some("topic".into());
            core.add_field(config, f, true);
        }

        // target humidity 0x253
        {
            let core_w = core.clone();
            let mut f = FieldDefinition::new("humidifier", "target_humidity").with_id(0x253);
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::Int(raw as i64))));
            f.read_callback = Some(Box::new(|val| {
                let n = match val {
                    PropertyValue::Int(i) => *i,
                    PropertyValue::Num(n) => *n as i64,
                    PropertyValue::Str(s) => s.parse().unwrap_or(0),
                };
                (30..=70).contains(&n)
            }));
            f.write_xform = Some(Box::new(move |val_str| {
                let mut val: f64 = val_str.parse().ok()?;
                if val < 30.0 {
                    val = 30.0;
                }
                if val > 70.0 {
                    val = 70.0;
                }
                val = val.round();
                core_w.set_raw(0x1f7, 1);
                Some(PropertyValue::Int(val as i64))
            }));
            f.write_attach_static = Some(vec![0x1f7, 0x1f9]);
            core.add_field(config, f, true);
        }

        // ionizer 0x360
        {
            let mut f = FieldDefinition::new("ionizer", "").with_id(0x360);
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
            f.write_attach_static = Some(vec![0x1f7, 0x1f9]);
            core.add_field(config, f, true);
        }

        // uv_nano 0x2a2
        {
            let mut f = FieldDefinition::new("uv_nano", "").with_id(0x2a2);
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
            f.write_attach_static = Some(vec![0x1f7, 0x1f9]);
            core.add_field(config, f, true);
        }

        // bucket light 0x21e
        {
            let mut f = FieldDefinition::new("bucket_light", "").with_id(0x21e);
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
            core.add_field(config, f, true);
        }

        self.add_timer_field(config, 0x21b, "off_timer", "Sleep timer", "mdi:bed-clock", 9);
    }

    fn add_timer_field(
        &self,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        max: i64,
    ) {
        config.components.insert(
            name.into(),
            json!({
                "platform": "number",
                "unique_id": format!("$deviceid-{name}"),
                "name": desc,
                "icon": icon,
                "device_class": "duration",
                "unit_of_measurement": "h",
                "min": 0,
                "max": max,
                "step": 1,
                "mode": "slider",
            }),
        );
        let step = 1.0_f64;
        let mut f = FieldDefinition::new(name, "").with_id(id);
        f.read_xform = Some(Box::new(move |raw| {
            let hours = ((raw as f64) / 60.0 / step).ceil() * step;
            Some(PropertyValue::from(hours))
        }));
        f.write_xform = Some(Box::new(|val| {
            let n: f64 = val.parse().ok()?;
            Some(PropertyValue::Int((n * 60.0).round() as i64))
        }));
        self.core.add_field(config, f, true);
    }

    fn publish_fan_speed_state(&self, override_s: Option<&str>) {
        let state = if let Some(s) = override_s {
            s.to_string()
        } else {
            match self.core.get_raw(0x1fa) {
                Some(6) => "high".into(),
                Some(2) => "low".into(),
                Some(v) => v.to_string(),
                None => "low".into(),
            }
        };
        self.core
            .ha
            .publish_property(&self.core.id, "fan_speed-", state.into());
    }

    fn build_fan_speed_tlvs(&self, fan: u32) -> Vec<Tlv> {
        let mut tlvs = vec![Tlv::new(0x1fa, fan)];
        for cap in MODE_FAN_CAPS {
            let fan_speed = cap.fixed_fan.unwrap_or(fan);
            tlvs.push(Tlv::new(0x2d7, cap.mode));
            tlvs.push(Tlv::new(0x2d8, 0));
            tlvs.push(Tlv::new(0x2d9, fan_speed));
        }
        self.core.set_raw(0x1fa, fan);
        tlvs
    }

    fn send_fan_speed_tlvs(&self, fan: u32) {
        let tlvs = self.build_fan_speed_tlvs(fan);
        self.core.send(&[1, 1, 2, 1, 1], &tlvs);
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
        // One-shot event entity (binary_sensor state already published above).
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
            return true; // do not store
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

    pub fn get_raw(&self, id: u16) -> Option<u32> {
        self.core.get_raw(id)
    }

    pub fn set_raw(&self, id: u16, v: u32) {
        self.core.set_raw(id, v);
    }

    /// Test helper: set previous mode clip value (for silent-mode transition tests).
    pub fn set_mode_clip_prev(&self, v: Option<u32>) {
        self.inner.lock().mode_clip_prev = v;
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};
    use rethink_util::tlv;

    const DEVICE_ID: &str = "test-id";
    const CAPS_RESPONSE_HEX: &str = "000004000000A70201000AB6A00A7CB541B5A004023220";
    const QUERY_RESPONSE_HEX: &str =
        "00000400000087020400117DC17E50117E827F503094D023D801A8803F5E";
    const IONIZER_OFF_NOTIFY_HEX: &str = "000004000000A702043F02D80085A3";
    const UV_ON_NOTIFY_HEX: &str = "000004000000A702044B02A88184D7";
    const UV_OFF_NOTIFY_HEX: &str = "000004000000A702044A08A8808C90388CD041E991";
    const BUCKET_LIGHT_ON_NOTIFY_HEX: &str = "000004000000A702048C0287817086";
    const BUCKET_LIGHT_OFF_NOTIFY_HEX: &str = "000004000000A702048A028780473E";
    const SLEEP_TIMER_59M_NOTIFY_HEX: &str = "000004000000A70204ED0386D03BB028";
    const SLEEP_TIMER_299M_NOTIFY_HEX: &str = "000004000000A70204F30486E0012BC8DA";
    const SLEEP_TIMER_OFF_NOTIFY_HEX: &str = "000004000000A70204EE0286C0AFE8";
    const BUCKET_EMPTIED_NOTIFY_HEX: &str = "000004000000A702046706AC600100AC807407";
    const BUCKET_FULL_NOTIFY_HEX: &str = "000004000000A70204EE02AC811E20";

    fn meta() -> Metadata {
        Metadata::new("DHUM_056905_WW", "TEST DHUM", "9439")
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
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn current_humidity_48_notify() -> Vec<u8> {
        let tlv_bytes = tlv::build(&[Tlv::new(0x336, 48)]);
        let mut body = vec![0x04, 0x00, 0x00, 0x00, 0xa7, 0x02, 0x04, 0x00, tlv_bytes.len() as u8];
        body.extend_from_slice(&tlv_bytes);
        let mut out = vec![0x00, 0x00];
        out.extend_from_slice(&body);
        out.extend_from_slice(&[0x00, 0x00]);
        out
    }

    fn parse_sent_tlvs(packet: &[u8]) -> Vec<(u16, u32)> {
        tlv::parse(&packet[11..packet.len() - 2])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect()
    }

    fn expected_fan_speed_tlvs(fan: u32) -> Vec<(u16, u32)> {
        let mut out = vec![(0x1fa, fan)];
        for mode in [17u32, 18, 20, 21, 22] {
            let fs = if mode == 21 { 6 } else { fan };
            out.push((0x2d7, mode));
            out.push((0x2d8, 0));
            out.push((0x2d9, fs));
        }
        out
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<PropertyValue> {
        ha.device(DEVICE_ID)?.properties.get(name).cloned()
    }

    #[test]
    fn caps_and_values_publish_humidifier_config() {
        let (ha, thinq, _dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        let device = ha.device(DEVICE_ID).expect("HA configuration published");
        let c = &device.config.as_ref().unwrap().components;
        let hum = c.get("humidifier").unwrap();
        assert_eq!(hum["device_class"], "dehumidifier");
        assert_eq!(
            hum["modes"],
            json!(["Smart", "Jet", "Silent", "Spot", "Laundry"])
        );
        assert_eq!(c["fan_speed"]["options"], json!(["low", "high"]));
        assert_eq!(c["off_timer"]["name"], "Sleep timer");
        assert_eq!(c["off_timer"]["platform"], "number");
        assert_eq!(c["off_timer"]["device_class"], "duration");
        assert_eq!(c["off_timer"]["unit_of_measurement"], "h");
        assert_eq!(c["off_timer"]["mode"], "slider");
        assert_eq!(c["off_timer"]["min"], 0);
        assert_eq!(c["off_timer"]["max"], 9);
        assert_eq!(c["off_timer"]["step"], 1);
        assert!(c.contains_key("ionizer"));
        assert!(c.contains_key("uv_nano"));
        assert!(c.contains_key("bucket_light"));
        assert_eq!(c["bucket_full"]["device_class"], "problem");
        assert_eq!(c["bucket_full"]["state_topic"], "$this/bucket_full-");
        assert_eq!(c["bucket_alert"]["platform"], "event");
        assert_eq!(
            c["bucket_alert"]["event_types"],
            json!(["bucket_full", "bucket_ok"])
        );
        assert_eq!(c["current_humidity"]["platform"], "sensor");
        assert_eq!(c["current_humidity"]["device_class"], "humidity");
        assert!(hum.get("target_humidity_state_topic").is_some());
        assert_eq!(
            hum["current_humidity_topic"],
            "$this/humidifier-current_humidity"
        );
        assert_eq!(
            c["current_humidity"]["state_topic"],
            "$this/humidifier-current_humidity"
        );
    }

    #[test]
    fn values_packet_publishes_target_humidity() {
        let (ha, _thinq, _dev) = build_ready();
        assert_eq!(
            prop(&ha, "humidifier-target_humidity"),
            Some(PropertyValue::Int(35))
        );
        assert_eq!(prop(&ha, "ionizer-"), Some("ON".into()));
        assert_eq!(prop(&ha, "uv_nano-"), Some("OFF".into()));
        assert_eq!(prop(&ha, "fan_speed-"), Some("low".into()));
    }

    #[test]
    fn current_humidity_from_0x336() {
        let (ha, thinq, dev) = build_ready();
        thinq.emit_data(&current_humidity_48_notify());
        assert_eq!(
            prop(&ha, "humidifier-current_humidity"),
            Some(PropertyValue::Int(48))
        );
        dev.process_key_value(0x336, 52);
        assert_eq!(
            prop(&ha, "humidifier-current_humidity"),
            Some(PropertyValue::Int(52))
        );
    }

    #[test]
    fn uv_on_off_notify() {
        let (ha, thinq, _dev) = build_ready();
        thinq.emit_data(&hex_decode(UV_ON_NOTIFY_HEX));
        assert_eq!(prop(&ha, "uv_nano-"), Some("ON".into()));
        thinq.emit_data(&hex_decode(UV_OFF_NOTIFY_HEX));
        assert_eq!(prop(&ha, "uv_nano-"), Some("OFF".into()));
    }

    #[test]
    fn ionizer_off_notify() {
        let (ha, thinq, _dev) = build_ready();
        thinq.emit_data(&hex_decode(IONIZER_OFF_NOTIFY_HEX));
        assert_eq!(prop(&ha, "ionizer-"), Some("OFF".into()));
    }

    #[test]
    fn bucket_full_logic() {
        let (ha, thinq, dev) = build_ready();
        dev.process_key_value(0x2b2, 1);
        assert_eq!(prop(&ha, "bucket_full-"), Some("ON".into()));
        {
            let events = &ha.device(DEVICE_ID).unwrap().events;
            assert!(
                events
                    .iter()
                    .any(|(t, p)| {
                        t == "events/bucket_alert" && p.contains("bucket_full")
                    }),
                "device trigger event on full: {events:?}"
            );
        }
        dev.process_key_value(0x336, 50);
        assert_eq!(prop(&ha, "bucket_full-"), Some("ON".into()));
        assert_eq!(
            prop(&ha, "humidifier-current_humidity"),
            Some(PropertyValue::Int(50))
        );
        thinq.emit_data(&hex_decode(BUCKET_EMPTIED_NOTIFY_HEX));
        assert_eq!(prop(&ha, "bucket_full-"), Some("OFF".into()));
        {
            let events = &ha.device(DEVICE_ID).unwrap().events;
            assert!(
                events
                    .iter()
                    .any(|(t, p)| {
                        t == "events/bucket_alert" && p.contains("bucket_ok")
                    }),
                "bucket_ok event on emptied: {events:?}"
            );
        }
        thinq.emit_data(&hex_decode(BUCKET_FULL_NOTIFY_HEX));
        assert_eq!(prop(&ha, "bucket_full-"), Some("ON".into()));
    }

    #[test]
    fn bucket_light_notify() {
        let (ha, thinq, _dev) = build_ready();
        thinq.emit_data(&hex_decode(BUCKET_LIGHT_ON_NOTIFY_HEX));
        assert_eq!(prop(&ha, "bucket_light-"), Some("ON".into()));
        thinq.emit_data(&hex_decode(BUCKET_LIGHT_OFF_NOTIFY_HEX));
        assert_eq!(prop(&ha, "bucket_light-"), Some("OFF".into()));
    }

    #[test]
    fn target_humidity_write() {
        let (_ha, thinq, dev) = build_ready();
        dev.set_property("humidifier-target_humidity", "45");
        let pkt = hex_encode(&thinq.outbox().last().unwrap());
        assert!(pkt.contains("94D02D"), "target humidity 45 as 0x253");
    }

    #[test]
    fn ionizer_write() {
        let (_ha, thinq, dev) = build_ready();
        dev.set_property("ionizer-", "ON");
        let pkt = hex_encode(&thinq.outbox().last().unwrap());
        assert!(pkt.contains("D801"));
        assert!(pkt.contains("7DC1"));
    }

    #[test]
    fn uv_write() {
        let (_ha, thinq, dev) = build_ready();
        dev.set_property("uv_nano-", "ON");
        let pkt = hex_encode(&thinq.outbox().last().unwrap());
        assert!(pkt.contains("A881"));
        assert!(pkt.contains("7DC1"));
    }

    #[test]
    fn bucket_light_write_no_attach() {
        let (_ha, thinq, dev) = build_ready();
        dev.set_property("bucket_light-", "ON");
        let pkt = hex_encode(&thinq.outbox().last().unwrap());
        assert!(pkt.contains("8781"));
        assert!(!pkt.contains("7DC1"));
    }

    #[test]
    fn fan_speed_write_table() {
        let (_ha, thinq, dev) = build_ready();
        dev.set_property("fan_speed-", "low");
        assert_eq!(thinq.outbox().len(), 1);
        let low = parse_sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(low, expected_fan_speed_tlvs(2));
        assert_eq!(dev.get_raw(0x1fa), Some(2));
        assert_eq!(dev.get_raw(0x2d7), None);
        assert_eq!(dev.get_raw(0x2d8), None);
        assert_eq!(dev.get_raw(0x2d9), None);

        thinq.reset_recorder();
        dev.set_property("fan_speed-", "high");
        let high = parse_sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(high, expected_fan_speed_tlvs(6));
        assert_eq!(dev.get_raw(0x1fa), Some(6));

        dev.process_key_value(0x2d7, 17);
        dev.process_key_value(0x2d8, 0);
        dev.process_key_value(0x2d9, 2);
        assert_eq!(dev.get_raw(0x2d7), None);
        assert_eq!(dev.get_raw(0x2d9), None);
    }

    #[test]
    fn sleep_timer_countdown() {
        let (ha, thinq, _dev) = build_ready();
        thinq.emit_data(&hex_decode(SLEEP_TIMER_59M_NOTIFY_HEX));
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(1)));
        thinq.emit_data(&hex_decode(SLEEP_TIMER_299M_NOTIFY_HEX));
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(5)));
        thinq.emit_data(&hex_decode(SLEEP_TIMER_OFF_NOTIFY_HEX));
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(0)));
    }

    #[test]
    fn sleep_timer_setpoint_read() {
        let (ha, _thinq, dev) = build_ready();
        dev.process_key_value(0x21b, 540);
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(9)));
        dev.process_key_value(0x21b, 180);
        assert_eq!(prop(&ha, "off_timer-"), Some(PropertyValue::Int(3)));
    }

    #[test]
    fn sleep_timer_write() {
        let (_ha, thinq, dev) = build_ready();
        for (hours, needle) in [
            ("9", "86E0021C"),
            ("5", "86E0012C"),
            ("2", "86D078"),
            ("1", "86D03C"),
            ("0", "86C0"),
        ] {
            thinq.reset_recorder();
            dev.set_property("off_timer-", hours);
            let pkt = hex_encode(&thinq.outbox().last().unwrap());
            assert!(pkt.contains(needle), "{hours}h → {needle}, got {pkt}");
        }
    }

    #[test]
    fn silent_mode_defaults_fan_low() {
        let (ha, thinq, dev) = build_ready();
        dev.set_mode_clip_prev(Some(17));
        dev.process_key_value(0x1f9, 19);
        assert_eq!(prop(&ha, "fan_speed-"), Some("low".into()));
        dev.process_key_value(0x1fa, 6);
        assert_eq!(prop(&ha, "fan_speed-"), Some("high".into()));
        thinq.reset_recorder();
        dev.set_property("fan_speed-", "high");
        assert!(!thinq.outbox().is_empty());
    }
}
