//! PAC_910604_WW — LG stand (floor-standing) air conditioner.
//!
//! Port of upstream PR #122. TLV climate on header 0xA7 (already accepted by
//! `TlvDeviceCore`). This model's capability bitmaps are empty, so modes, fans,
//! and extra switches come from the 557-frame captures in issue #105 — not from
//! 0x2cc/0x2cd.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use rethink_util::sync::Mutex;
use rethink_util::tlv::Tlv;
use serde_json::json;
use std::sync::Arc;

const TAG_POWER: u16 = 0x1f7;
const TAG_MODE: u16 = 0x1f9;
const TAG_FAN: u16 = 0x1fa;
const TAG_TEMP_STEP: u16 = 0x1fb;
const TAG_CURRENT_TEMP: u16 = 0x1fd;
const TAG_TARGET_TEMP: u16 = 0x1fe;
const TAG_AUTO_DRY_ENABLE: u16 = 0x20e;
const TAG_AUTO_DRY_LEVEL: u16 = 0x1f2;
const TAG_AUTO_DRY_REMAIN: u16 = 0x225;
const TAG_SWING_H: u16 = 0x2a3;
const TAG_POWER_W: u16 = 0x2b3;
const TAG_HUMIDITY: u16 = 0x336;
const TAG_FILTER_REMAIN: u16 = 0x355;
const TAG_FILTER_LIFE: u16 = 0x356;

const A8_FRAME_LEN: usize = 307;
const COMPRESSOR_RUNNING_OFFSET: usize = 160;

const MIN_TEMP_C: f64 = 18.0;
const MAX_TEMP_C: f64 = 30.0;

fn mode_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        0 => Some("cool"),
        1 => Some("dry"),
        5 => Some("fan_only"),
        _ => None,
    }
}

fn ha_to_mode(val: &str) -> Option<u32> {
    match val {
        "cool" => Some(0),
        "dry" => Some(1),
        "fan_only" => Some(5),
        _ => None,
    }
}

fn fan_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        2 => Some("very low"),
        3 => Some("low"),
        4 => Some("medium"),
        5 => Some("high"),
        6 => Some("very high"),
        7 | 8 => Some("auto"),
        _ => None,
    }
}

fn ha_to_fan(val: &str) -> Option<u32> {
    match val {
        "very low" => Some(2),
        "low" => Some(3),
        "medium" => Some(4),
        "high" => Some(5),
        "very high" => Some(6),
        "auto" => Some(8),
        _ => None,
    }
}

fn swing_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        1 => Some("focus"),
        2 => Some("wide"),
        3 => Some("left"),
        4 => Some("right"),
        5 => Some("split"),
        _ => None,
    }
}

fn ha_to_swing(val: &str) -> Option<u32> {
    match val {
        "focus" => Some(1),
        "wide" => Some(2),
        "left" => Some(3),
        "right" => Some(4),
        "split" => Some(5),
        _ => None,
    }
}

fn dry_level_to_ha(raw: u32) -> Option<&'static str> {
    match raw {
        2 => Some("1"),
        3 => Some("2"),
        4 => Some("3"),
        5 => Some("4"),
        6 => Some("5"),
        _ => None,
    }
}

fn ha_to_dry_level(val: &str) -> Option<u32> {
    match val {
        "1" => Some(2),
        "2" => Some(3),
        "3" => Some(4),
        "4" => Some(5),
        "5" => Some(6),
        _ => None,
    }
}

struct Inner {
    compressor_running: Option<bool>,
    last_power: Option<u32>,
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
                compressor_running: None,
                last_power: None,
            }),
        });

        *core.is_caps_response.lock() = Some(Box::new(|tlv| tlv.iter().any(|e| e.t == 0x2da)));
        *core.is_values_response.lock() = Some(Box::new(|tlv| {
            tlv.len() >= 10 && tlv.iter().any(|e| e.t == TAG_POWER)
        }));

        let t_kv = this.clone();
        *core.on_key_value_extra.lock() = Some(Box::new(move |k, v| t_kv.on_key_value(k, v)));

        let t_a8 = this.clone();
        thinq.on_data(Box::new(move |data| t_a8.process_a8(data)));

        let mut config = default_config(&meta, Some(json!({"name": "LG Stand Air Conditioner"})));
        config.components.insert(
            "climate".into(),
            json!({
                "platform": "climate",
                "unique_id": "$deviceid-climate",
                "name": null,
                "temperature_unit": "C",
                "temp_step": 0.5,
                "precision": 0.5,
                "min_temp": 18,
                "max_temp": 30,
                "modes": ["off", "cool", "dry", "fan_only"],
                "fan_modes": ["very low", "low", "medium", "high", "very high", "auto"],
                "swing_horizontal_modes": ["focus", "wide", "left", "right", "split"],
                "action_topic": "$this/climate-action",
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
                .with_id(TAG_CURRENT_TEMP)
                .read_only();
            f.state_topic = Some("topic".into());
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            core.add_field(config, f, true);
        }

        {
            let mut f = FieldDefinition::new("climate", "temperature").with_id(TAG_TARGET_TEMP);
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            f.write_xform = Some(Box::new(|val_str| {
                let val: f64 = val_str.parse().ok()?;
                let clamped = val.clamp(MIN_TEMP_C, MAX_TEMP_C);
                Some(PropertyValue::Int((clamped * 2.0).round() as i64))
            }));
            f.write_attach_static = Some(vec![TAG_MODE, TAG_FAN]);
            core.add_field(config, f, true);
        }

        {
            let core_c = core.clone();
            let mut power = FieldDefinition::new("climate", "power").with_id(TAG_POWER);
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
                    vec![TAG_MODE]
                } else {
                    vec![]
                }
            }));
            power.read_xform = Some(Box::new(|raw| {
                Some(if raw != 0 { "ON".into() } else { "OFF".into() })
            }));
            power.read_callback = Some(Box::new(move |_val| {
                if let Some(mode) = core_c.get_raw(TAG_MODE) {
                    core_c.process_key_value(TAG_MODE, mode);
                }
                false
            }));
            core.add_field(config, power, true);
        }

        {
            let core_r = core.clone();
            let core_w = core.clone();
            let core_cb = core.clone();
            let mut mode = FieldDefinition::new("climate", "mode").with_id(TAG_MODE);
            mode.read_xform = Some(Box::new(move |raw| {
                if core_r.get_raw(TAG_POWER) == Some(0) {
                    return Some("off".into());
                }
                mode_to_ha(raw).map(|s| s.into())
            }));
            mode.write_xform = Some(Box::new(move |val| {
                if val == "off" {
                    core_w.set_raw(TAG_POWER, 0);
                    return Some(PropertyValue::Int(core_w.get_raw(TAG_MODE).unwrap_or(0) as i64));
                }
                core_w.set_raw(TAG_POWER, 1);
                ha_to_mode(val).map(|n| PropertyValue::Int(n as i64))
            }));
            mode.write_callback = Some(Box::new(move |_num| {
                if core_cb.get_raw(TAG_POWER) == Some(0) {
                    core_cb.send(&[1, 1, 2, 1, 1], &[Tlv::new(TAG_POWER, 0)]);
                    return false;
                }
                true
            }));
            mode.write_attach_static = Some(vec![TAG_POWER, TAG_FAN, TAG_TARGET_TEMP]);
            core.add_field(config, mode, true);
        }

        {
            let mut f = FieldDefinition::new("climate", "fan_mode").with_id(TAG_FAN);
            f.read_xform = Some(Box::new(|raw| fan_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| ha_to_fan(val).map(|n| PropertyValue::Int(n as i64))));
            f.write_attach_static = Some(vec![TAG_MODE, TAG_TARGET_TEMP]);
            core.add_field(config, f, true);
        }

        {
            let mut f = FieldDefinition::new("climate", "swing_horizontal_mode").with_id(TAG_SWING_H);
            f.read_xform = Some(Box::new(|raw| swing_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| ha_to_swing(val).map(|n| PropertyValue::Int(n as i64))));
            core.add_field(config, f, true);
        }

        {
            let mut f = FieldDefinition::new("energy_current", "").with_id(TAG_POWER_W).read_only();
            config.components.insert(
                "energy_current".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-energy_current",
                    "state_topic": "$this/energy_current-",
                    "name": "Power",
                    "device_class": "power",
                    "unit_of_measurement": "W",
                    "state_class": "measurement",
                    "suggested_display_precision": 1,
                }),
            );
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 10.0))));
            core.add_field(config, f, true);
        }

        self.add_switch(config, 0x236, "jet", "Jet cool", "mdi:wind-power", 1, 0, 1, None);
        self.add_switch(config, 0x29d, "quiet", "Quiet mode", "mdi:volume-off", 1, 0, 1, None);
        self.add_switch(config, 0x2a2, "uvnano", "UVnano", "mdi:bacteria", 1, 0, 1, None);
        self.add_switch(config, 0x3a9, "childlock", "Child lock", "mdi:lock", 1, 0, 1, None);
        self.add_switch(config, 0x23e, "smartcare", "Smart care", "mdi:auto-fix", 1, 0, 1, None);
        self.add_switch(
            config,
            0x1be,
            "spacefit",
            "Space-fit wind",
            "mdi:arrow-expand-horizontal",
            1,
            0,
            1,
            None,
        );
        self.add_switch(config, 0x20f, "airclean", "Air purify", "mdi:air-purifier", 1, 0, 1, None);
        self.add_switch(
            config,
            0x21f,
            "display",
            "Display Light",
            "mdi:television-ambient-light",
            0,
            1,
            0,
            Some("config"),
        );
        self.add_switch(
            config,
            0x3a0,
            "beep",
            "Beep Sound",
            "mdi:volume-high",
            0,
            1,
            0,
            Some("config"),
        );
        self.add_switch(
            config,
            0x3a2,
            "hxclean",
            "Cleaning - Heat exchanger",
            "mdi:heating-coil",
            1,
            0,
            1,
            Some("diagnostic"),
        );
        self.add_switch(
            config,
            0x165,
            "allclean",
            "Cleaning - ALL",
            "mdi:spray-bottle",
            100,
            0,
            2,
            Some("diagnostic"),
        );

        self.add_switch(
            config,
            TAG_AUTO_DRY_ENABLE,
            "autodry",
            "Auto dry",
            "mdi:hair-dryer",
            255,
            0,
            255,
            None,
        );

        config.components.insert(
            "autodrylevel".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-autodrylevel",
                "name": "Auto dry level",
                "icon": "mdi:tune",
                "options": ["1", "2", "3", "4", "5"],
            }),
        );
        {
            let mut f = FieldDefinition::new("autodrylevel", "").with_id(TAG_AUTO_DRY_LEVEL);
            f.read_xform = Some(Box::new(|raw| dry_level_to_ha(raw).map(|s| s.into())));
            f.write_xform = Some(Box::new(|val| {
                ha_to_dry_level(val).map(|n| PropertyValue::Int(n as i64))
            }));
            core.add_field(config, f, true);
        }

        config.components.insert(
            "autodryremain".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-autodryremain",
                "name": "Auto dry remaining",
                "icon": "mdi:timer-sand",
                "unit_of_measurement": "min",
                "device_class": "duration",
                "entity_category": "diagnostic",
            }),
        );
        {
            let f = FieldDefinition::new("autodryremain", "")
                .with_id(TAG_AUTO_DRY_REMAIN)
                .read_only();
            core.add_field(config, f, true);
        }

        config.components.insert(
            "autodryrunning".into(),
            json!({
                "platform": "binary_sensor",
                "unique_id": "$deviceid-autodryrunning",
                "state_topic": "$this/autodryrunning",
                "name": "Auto dry running",
                "icon": "mdi:hair-dryer",
                "entity_category": "diagnostic",
            }),
        );
        config.components.insert(
            "autodrycancel".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-autodrycancel",
                "command_topic": "$this/autodrycancel/set",
                "name": "Cancel auto dry",
                "icon": "mdi:hair-dryer-outline",
                "entity_category": "diagnostic",
            }),
        );
        {
            let this = self.clone();
            let mut f = FieldDefinition::new("autodrycancel", "");
            f.readable = false;
            f.write_xform = Some(Box::new(|val| {
                if val == "PRESS" {
                    Some(0i64.into())
                } else {
                    None
                }
            }));
            f.write_callback = Some(Box::new(move |_num| {
                this.core
                    .send(&[1, 1, 2, 1, 1], &[Tlv::new(TAG_AUTO_DRY_REMAIN, 0)]);
                false
            }));
            let f = Arc::new(f);
            core.fields_by_ha
                .lock()
                .insert("autodrycancel".into(), f);
        }

        config.components.insert(
            "onesidewind".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-onesidewind",
                "name": "One-side wind",
                "icon": "mdi:arrow-left-right",
                "options": ["off", "left", "right"],
            }),
        );
        {
            let mut f = FieldDefinition::new("onesidewind", "").with_id(0x2a8);
            f.read_xform = Some(Box::new(|raw| {
                Some(
                    match raw {
                        1 => "left",
                        2 => "right",
                        _ => "off",
                    }
                    .into(),
                )
            }));
            f.write_xform = Some(Box::new(|val| {
                Some(PropertyValue::Int(match val {
                    "left" => 1,
                    "right" => 2,
                    _ => 0,
                }))
            }));
            core.add_field(config, f, true);
        }

        config.components.insert(
            "humidity".into(),
            json!({
                "platform": "sensor",
                "unique_id": "$deviceid-humidity",
                "name": "Humidity",
                "device_class": "humidity",
                "unit_of_measurement": "%",
                "state_class": "measurement",
                "suggested_display_precision": 0,
            }),
        );
        {
            let f = FieldDefinition::new("humidity", "").with_id(TAG_HUMIDITY).read_only();
            core.add_field(config, f, true);
        }

        config.components.insert(
            "humiditydisplay".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-humiditydisplay",
                "name": "Humidity display",
                "icon": "mdi:water-percent",
                "options": ["while running", "always"],
            }),
        );
        {
            let mut f = FieldDefinition::new("humiditydisplay", "").with_id(0x337);
            f.read_xform = Some(Box::new(|raw| {
                Some(if raw == 1 {
                    "always".into()
                } else {
                    "while running".into()
                })
            }));
            f.write_xform = Some(Box::new(|val| {
                Some(PropertyValue::Int(if val == "always" { 1 } else { 0 }))
            }));
            core.add_field(config, f, true);
        }

        config.components.insert(
            "tempstep".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-tempstep",
                "name": "Temperature step",
                "icon": "mdi:thermometer-lines",
                "entity_category": "diagnostic",
                "options": ["0.5", "1"],
            }),
        );
        {
            let this = self.clone();
            let mut f = FieldDefinition::new("tempstep", "").with_id(TAG_TEMP_STEP);
            f.read_xform = Some(Box::new(|raw| {
                Some(if raw == 1 { "1".into() } else { "0.5".into() })
            }));
            f.write_xform = Some(Box::new(|val| {
                match val {
                    "1" => Some(1i64.into()),
                    "0.5" => Some(0i64.into()),
                    _ => None,
                }
            }));
            f.read_callback = Some(Box::new(move |_val| {
                this.update_temp_step();
                true
            }));
            core.add_field(config, f, true);
        }

        for (key, name, topic) in [
            ("filter_life", "Filter life", "filter_life"),
            ("filter_used", "Filter used", "filter_used"),
            ("filter_remaining", "Filter remaining", "filter_remaining"),
        ] {
            config.components.insert(
                key.into(),
                json!({
                    "platform": "sensor",
                    "unique_id": format!("$deviceid-{key}"),
                    "state_topic": format!("$this/{topic}"),
                    "name": name,
                    "entity_category": "diagnostic",
                    "icon": "mdi:air-filter",
                }),
            );
        }
        if let Some(life) = config.components.get_mut("filter_life") {
            life["unit_of_measurement"] = json!("h");
        }
        if let Some(used) = config.components.get_mut("filter_used") {
            used["unit_of_measurement"] = json!("h");
        }
        if let Some(rem) = config.components.get_mut("filter_remaining") {
            rem["unit_of_measurement"] = json!("%");
        }
        config.components.insert(
            "filterreset".into(),
            json!({
                "platform": "button",
                "unique_id": "$deviceid-filterreset",
                "command_topic": "$this/filterreset/set",
                "name": "Reset filter usage",
                "icon": "mdi:calendar-refresh-outline",
                "entity_category": "diagnostic",
            }),
        );
        {
            let this = self.clone();
            let mut f = FieldDefinition::new("filterreset", "");
            f.readable = false;
            f.write_xform = Some(Box::new(|val| {
                if val == "PRESS" {
                    Some(0i64.into())
                } else {
                    None
                }
            }));
            f.write_callback = Some(Box::new(move |_num| {
                this.core
                    .send(&[1, 1, 2, 1, 1], &[Tlv::new(TAG_FILTER_REMAIN, 0)]);
                false
            }));
            core.fields_by_ha
                .lock()
                .insert("filterreset".into(), Arc::new(f));
        }
    }

    fn add_switch(
        self: &Arc<Self>,
        config: &mut DeviceDiscovery,
        id: u16,
        key: &str,
        name: &str,
        icon: &str,
        on_write: u32,
        off_write: u32,
        on_read: u32,
        entity_category: Option<&str>,
    ) {
        let mut body = json!({
            "platform": "switch",
            "unique_id": format!("$deviceid-{key}"),
            "name": name,
            "icon": icon,
        });
        if let Some(cat) = entity_category {
            body["entity_category"] = json!(cat);
        }
        config.components.insert(key.into(), body);
        let mut f = FieldDefinition::new(key, "").with_id(id);
        f.read_xform = Some(Box::new(move |raw| {
            Some(if raw == on_read { "ON".into() } else { "OFF".into() })
        }));
        f.write_xform = Some(Box::new(move |val| {
            Some(PropertyValue::Int(if val == "ON" {
                on_write as i64
            } else {
                off_write as i64
            }))
        }));
        self.core.add_field(config, f, true);
    }

    fn on_key_value(&self, k: u16, v: u32) {
        match k {
            TAG_POWER => {
                let mut inner = self.inner.lock();
                let prev = inner.last_power;
                inner.last_power = Some(v);
                if prev == Some(0) && v == 1 {
                    inner.compressor_running = None;
                }
                drop(inner);
                self.update_climate_action();
            }
            TAG_MODE => self.update_climate_action(),
            TAG_AUTO_DRY_REMAIN => {
                self.core.ha.publish_property(
                    &self.core.id,
                    "autodryrunning",
                    if v > 0 { "ON" } else { "OFF" }.into(),
                );
            }
            TAG_FILTER_LIFE | TAG_FILTER_REMAIN => self.publish_filter(),
            _ => {}
        }
    }

    fn publish_filter(&self) {
        let Some(life) = self.core.get_raw(TAG_FILTER_LIFE) else {
            return;
        };
        let remain = self.core.get_raw(TAG_FILTER_REMAIN).unwrap_or(0);
        let used = life.saturating_sub(remain);
        let pct = if life == 0 { 0 } else { remain * 100 / life };
        self.core
            .ha
            .publish_property(&self.core.id, "filter_life", (life as i64).into());
        self.core
            .ha
            .publish_property(&self.core.id, "filter_used", (used as i64).into());
        self.core.ha.publish_property(
            &self.core.id,
            "filter_remaining",
            (pct as i64).into(),
        );
    }

    fn process_a8(&self, buf: &[u8]) {
        if buf.len() != A8_FRAME_LEN
            || buf[2] != 0x04
            || buf[3] != 0x00
            || buf[4] != 0x00
            || buf[5] != 0x00
            || buf[6] != 0xa8
            || buf[10] != 0xff
        {
            return;
        }
        self.inner.lock().compressor_running = Some(buf[COMPRESSOR_RUNNING_OFFSET] != 0);
        self.update_climate_action();
    }

    fn update_temp_step(&self) {
        let raw = self.core.get_raw(TAG_TEMP_STEP).unwrap_or(0);
        let step = if raw == 1 { 1.0 } else { 0.5 };
        let mut cfg = self.core.config.lock();
        let Some(config) = cfg.as_mut() else {
            return;
        };
        let Some(climate) = config.components.get_mut("climate") else {
            return;
        };
        let current = climate.get("temp_step").and_then(|v| v.as_f64());
        if current == Some(step) {
            return;
        }
        climate["temp_step"] = json!(step);
        climate["precision"] = json!(step);
        let published = config.clone();
        drop(cfg);
        self.core.set_config(published);
    }

    fn update_climate_action(&self) {
        let power = self.core.get_raw(TAG_POWER);
        let mode = self.core.get_raw(TAG_MODE);
        let running = self.inner.lock().compressor_running;
        let action = match power {
            Some(0) => Some("off"),
            Some(1) => {
                if mode == Some(5) {
                    Some("fan")
                } else if running == Some(false) {
                    Some("idle")
                } else if running == Some(true) {
                    match mode {
                        Some(0) => Some("cooling"),
                        Some(1) => Some("drying"),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(action) = action {
            self.core
                .ha
                .publish_property(&self.core.id, "climate-action", action.into());
        }
    }

    pub fn set_property(&self, prop: &str, value: &str) {
        let before = self.core.get_raw(TAG_POWER);
        self.core.set_property(prop, value);
        let after = self.core.get_raw(TAG_POWER);
        if before == Some(0) && after == Some(1) {
            self.inner.lock().compressor_running = None;
        }
        self.update_climate_action();
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
    use std::collections::HashMap;

    const DEVICE_ID: &str = "test-id";

    const CAPS_RESPONSE_HEX: &str = concat!(
        "000004000000A7020161A1",
        "6400644F64907C6C6018036C816D01B000B05023B0B001F07CB0C0B10DB23004F800B290C0B2E0084",
        "3B340B4F0045013B500B5601213B5A0C002B76001A0B780B7C0B801B85024B8903CB8D020B9103CBC",
        "600201BD1010BD60580FB240D3F0020000BC08D42001FFEA1021E1901CDD05FB102FB4A00200B6B03",
        "53028B6F0310917EFC0F010FFF06003FEFA01B5C0B61030B644B5C1B61030B648B5C5B61030B643",
        "2A9C"
    );
    const QUERY_RESPONSE_HEX: &str = concat!(
        "000004000000A7020440",
        "D091009283918091C0A300A3407DC17E407F902A7E847F5032820081408180808080C082408D8082",
        "C0830083408390FF83C19080868086C087009F4087C08780938193C09440948090409000D2008940",
        "8840ACE00DAD8E808E408E0090C0CCC0CD00CD40CD9039D5A00BB8D560098ACDC1C8C08F808FC08F",
        "41AB40AB00D1009780A801A840A881A8C1E8807EC0E3D065E800FA80F140E480EB103CEA80EAC0EA",
        "40EEC2EE407C86EE80698169C16D406E416CD03C5DF0B405A0A740AA0064F00201016F8059404B40",
        "4BC14CC15F41C490CD",
        "5D7F"
    );
    const STATE_FAN_1_HEX: &str = "000004000000A7020441047E82C4829AD1";
    const STATE_FAN_5_HEX: &str = "000004000000A7020447027E860483";
    const STATE_MODE_COOL_HEX: &str = "000004000000A70204920B7E407F902A7E86D200C489366F";
    const STATE_MODE_DRY_HEX: &str = "000004000000A70204850B7E417F90307E88D201C489B93B";
    const STATE_MODE_AIRCLEAN_HEX: &str = "000004000000A702048E0B7E457F90347E82D205C4898B59";
    const STATE_WIND_CONCENTRATED_HEX: &str = "000004000000A702049A02A8C112A4";
    const STATE_WIND_SPLIT_HEX: &str = "000004000000A70204940F8CA001F08CD045ACE00129A8C5C48D2B51";
    const STATE_CHILDLOCK_ON_HEX: &str = "000004000000A70204EE04EA41C482CD33";
    const STATE_DISPLAY_OFF_HEX: &str = "000004000000A70204F70487C1C482C45B";
    const STATE_DISPLAY_ON_HEX: &str = "000004000000A70204F90487C0C48273C8";
    const STATE_UVNANO_ON_HEX: &str = "000004000000A702046204A881C482B7F3";
    const STATE_AIDRY_ON_HEX: &str = "000004000000A702046E038390FF03DE";
    const STATE_AIDRY_END_HEX: &str = "000004000000A7020404068940A8C1C48485B6";
    const STATE_POWER_OFF_HEX: &str = "000004000000A70204AC117DC07E407F90327E86D2008F40E880C48F64F0";
    const A8_COOLING_HEX: &str = concat!(
        "000004000000A8670301FF0B0101035601010000040000000000000000000001003C350000000000000000000100000B",
        "B80BB801000500000000000044000000000100000100003C0001000000001800000202010156142D1E00320000000000",
        "00000000006100001919000226024E023A000002E4030702F8000099009A180000000005250004800005D300020F4618",
        "4646009F080015021702180000000000010004000026170002EE000000014B51CA3735CE000055093209F00420036C00",
        "0000000101010100FF000000000000000000000000006400DE0003DE3900030186018601000000010000000012010000",
        "000000000ACE000100030000000A00005600024D00335F00000000000000000000000000000000000A0A3300FFFFFF00",
        "020000000000000000000000000000000530AD"
    );
    const A8_STOPPED_HEX: &str = concat!(
        "000004000000A8666501FF0B0101655801010000040000000000000000000001003C350000000000000000000100000B",
        "B80BB801000500000000000044000000000100000100003C0001000000001800000202010156142D1E00320000000000",
        "00000000006100001919000226024E023A0000022B0253023F0000900091180000000005250004800005D300000A2800",
        "00000000000016020000000000000000000005002800000002EE000000004B5FCB0035D3000000098209F00458036C00",
        "00000001010100001C000000000000000000000000006400E20001CC050000012C012C00000000010000000012010000",
        "000000000ACE000100030000000A00005700026C0003F0000000000000000000000000000000000004033300FFFFFF00",
        "02000000000000000000000000000000051BA1"
    );
    const A8_SHORT_HEX: &str = "000004000000a8180201024ec1abda";

    fn meta() -> Metadata {
        Metadata::new("PAC_910604_WW", "PAC_910604_WW", "310917")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
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
        let (ha, thinq, dev) = make();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn components(ha: &MockHaConnection) -> serde_json::Map<String, serde_json::Value> {
        ha.device(DEVICE_ID)
            .unwrap()
            .config
            .unwrap()
            .components
            .into_iter()
            .collect()
    }

    fn raw_prop(ha: &MockHaConnection, name: &str) -> Option<PropertyValue> {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .get(name)
            .cloned()
    }

    fn sent_tlvs(pkt: &[u8]) -> HashMap<u16, u32> {
        tlv::parse(&pkt[11..pkt.len() - 2])
            .into_iter()
            .map(|e| (e.t, e.v))
            .collect()
    }

    #[test]
    fn a7_caps_triggers_values_query() {
        let (_ha, thinq, _dev) = make();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        assert_eq!(thinq.outbox().len(), 1, "values query sent in response to caps");
    }

    #[test]
    fn climate_exposes_this_models_modes_and_speeds() {
        let (ha, _, _) = build_ready();
        let climate = &components(&ha)["climate"];
        assert_eq!(climate["modes"], json!(["off", "cool", "dry", "fan_only"]));
        assert_eq!(
            climate["fan_modes"],
            json!(["very low", "low", "medium", "high", "very high", "auto"])
        );
        assert_eq!(
            climate["swing_horizontal_modes"],
            json!(["focus", "wide", "left", "right", "split"])
        );
        assert!(climate.get("swing_modes").is_none());
        assert_eq!(climate["min_temp"], 18);
        assert_eq!(climate["max_temp"], 30);
    }

    #[test]
    fn comprehensive_dump_publishes_climate_state() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "current_temperature"),
            Some(PropertyValue::from(25.0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "temperature_state"),
            Some(PropertyValue::from(21.0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some(PropertyValue::Str("cool".into()))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some(PropertyValue::Str("medium".into()))
        );
    }

    #[test]
    fn mode_notifications_round_trip() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(STATE_MODE_DRY_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("dry".into())
        );
        thinq.emit_data(&hex_decode(STATE_MODE_AIRCLEAN_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("fan_only".into())
        );
        thinq.emit_data(&hex_decode(STATE_MODE_COOL_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("cool".into())
        );
        thinq.emit_data(&hex_decode(STATE_POWER_OFF_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("off".into())
        );
    }

    #[test]
    fn fan_speeds_map_onto_named_steps() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(STATE_FAN_1_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some("very low".into())
        );
        thinq.emit_data(&hex_decode(STATE_FAN_5_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some("very high".into())
        );
    }

    #[test]
    fn sideways_aim_is_horizontal_swing() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(STATE_WIND_CONCENTRATED_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "swing_horizontal_mode_state"),
            Some("focus".into())
        );
        thinq.emit_data(&hex_decode(STATE_WIND_SPLIT_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "swing_horizontal_mode_state"),
            Some("split".into())
        );
    }

    #[test]
    fn power_is_tenths_of_a_watt() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(STATE_WIND_SPLIT_HEX));
        assert_eq!(raw_prop(&ha, "energy_current-"), Some(PropertyValue::from(29.7)));
    }

    #[test]
    fn panel_switches_including_inverted_display() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(STATE_CHILDLOCK_ON_HEX));
        assert_eq!(raw_prop(&ha, "childlock-").map(|p| p.as_string()).as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(STATE_UVNANO_ON_HEX));
        assert_eq!(raw_prop(&ha, "uvnano-").map(|p| p.as_string()).as_deref(), Some("ON"));
        thinq.emit_data(&hex_decode(STATE_DISPLAY_OFF_HEX));
        assert_eq!(raw_prop(&ha, "display-").map(|p| p.as_string()).as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(STATE_DISPLAY_ON_HEX));
        assert_eq!(raw_prop(&ha, "display-").map(|p| p.as_string()).as_deref(), Some("ON"));
    }

    #[test]
    fn inverted_switch_writes_stored_polarity() {
        let (ha, thinq, _) = build_ready();
        ha.emit_set_property(DEVICE_ID, "display-", "OFF");
        let tlvs = sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(tlvs.get(&0x21f).copied(), Some(1));
    }

    #[test]
    fn ai_dry_enable_strength_and_countdown() {
        let (ha, thinq, _) = build_ready();
        let c = components(&ha);
        assert_eq!(c["autodry"]["platform"], "switch");
        assert_eq!(c["autodrylevel"]["platform"], "select");
        assert_eq!(c["autodrylevel"]["options"], json!(["1", "2", "3", "4", "5"]));
        assert_eq!(c["autodryremain"]["unit_of_measurement"], "min");

        thinq.emit_data(&hex_decode(STATE_AIDRY_ON_HEX));
        assert_eq!(raw_prop(&ha, "autodry-").map(|p| p.as_string()).as_deref(), Some("ON"));

        thinq.reset_recorder();
        ha.emit_set_property(DEVICE_ID, "autodry-", "ON");
        let tlvs = sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(tlvs.get(&0x20e).copied(), Some(255));
    }

    #[test]
    fn running_ai_dry_can_be_cancelled() {
        let (ha, thinq, _) = build_ready();
        assert_eq!(
            raw_prop(&ha, "autodryrunning").map(|p| p.as_string()).as_deref(),
            Some("OFF")
        );
        thinq.reset_recorder();
        ha.emit_set_property(DEVICE_ID, "autodrycancel", "PRESS");
        assert_eq!(thinq.outbox().len(), 1);
        let tlvs = sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(tlvs.get(&0x225).copied(), Some(0));
        thinq.emit_data(&hex_decode(STATE_AIDRY_END_HEX));
        assert_eq!(
            raw_prop(&ha, "autodryrunning").map(|p| p.as_string()).as_deref(),
            Some("OFF")
        );
    }

    #[test]
    fn filter_counters_are_value_tags_and_reset() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        assert_eq!(raw_prop(&ha, "filter_life"), Some(3000i64.into()));
        assert_eq!(raw_prop(&ha, "filter_used"), Some(558i64.into()));
        assert_eq!(raw_prop(&ha, "filter_remaining"), Some(81i64.into()));
        assert!(components(&ha).contains_key("filterreset"));
        thinq.reset_recorder();
        ha.emit_set_property(DEVICE_ID, "filterreset", "PRESS");
        let tlvs = sent_tlvs(&thinq.outbox()[0]);
        assert_eq!(tlvs.get(&0x355).copied(), Some(0));
    }

    #[test]
    fn hvac_action_needs_compressor_record() {
        let (ha, thinq, _) = build_ready();
        assert!(raw_prop(&ha, "climate-action").is_none());
        thinq.emit_data(&hex_decode(A8_COOLING_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("cooling")
        );
        thinq.emit_data(&hex_decode(A8_STOPPED_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("idle")
        );
        thinq.emit_data(&hex_decode(A8_COOLING_HEX));
        thinq.emit_data(&hex_decode(STATE_MODE_AIRCLEAN_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("fan")
        );
        thinq.emit_data(&hex_decode(A8_COOLING_HEX));
        thinq.emit_data(&hex_decode(STATE_POWER_OFF_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("off")
        );
    }

    #[test]
    fn compressor_reading_discarded_on_power_up() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(A8_COOLING_HEX));
        thinq.emit_data(&hex_decode(STATE_POWER_OFF_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("off")
        );
        thinq.emit_data(&hex_decode(STATE_MODE_COOL_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("off"),
            "still the last thing known, not a stale cooling"
        );
    }

    #[test]
    fn short_a8_is_not_telemetry() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(A8_STOPPED_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("idle")
        );
        thinq.emit_data(&hex_decode(A8_SHORT_HEX));
        assert_eq!(
            raw_prop(&ha, "climate-action").map(|p| p.as_string()).as_deref(),
            Some("idle")
        );
    }

    #[test]
    fn temperature_step_tracks_1fb() {
        let (ha, thinq, _) = build_ready();
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        assert_eq!(
            raw_prop(&ha, "tempstep-").map(|p| p.as_string()).as_deref(),
            Some("0.5")
        );
        assert_eq!(components(&ha)["climate"]["temp_step"], 0.5);

        // Synthesize a 0xA7 values frame carrying only 0x1fb = 1 (no capture on file).
        let tlv = tlv::build(&[Tlv::new(TAG_TEMP_STEP, 1)]);
        let mut pkt = vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0xa7, 0x02, 0x04, 0x00, tlv.len() as u8];
        pkt.extend_from_slice(&tlv);
        pkt.extend_from_slice(&[0x00, 0x00]);
        thinq.emit_data(&pkt);
        assert_eq!(
            raw_prop(&ha, "tempstep-").map(|p| p.as_string()).as_deref(),
            Some("1")
        );
        assert_eq!(components(&ha)["climate"]["temp_step"], 1.0);
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("PAC_910604_WW").is_some());
    }
}
