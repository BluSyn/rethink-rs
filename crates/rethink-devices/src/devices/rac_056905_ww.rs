//! RAC_056905_WW (alias RAC_0B0001_WW) — LG room air conditioner.
//! Cassette CST_570004_WW has its own handler (different mode/fan/swing tables).

use crate::ac_tables::{rac_air_temp, rac_pipe_temp};
use crate::device_trait::DeviceHandler;
use rethink_util::sync::Mutex;
use rethink_core::device_base::{default_config, FieldDefinition, TlvDeviceCore};
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::{json, Map, Value};
use std::sync::Arc;

type PowerModeChangeHook = Box<dyn Fn() + Send + Sync>;

/// EEPROM checksum at 0x2da -> the 0x21f value that means the display is on.
/// Negative cases that must stay hidden: 0x4dda, 0x4d81, 0x4dfa, 0x4d87.
fn display_light_on_value(eeprom_checksum: u32) -> Option<u32> {
    match eeprom_checksum {
        0x4e88 | 0x551d | 0x4c19 | 0x4f58 | 0x4e95 => Some(1),
        // H09S1D, firmware 0x55703: 0 = on, as the ThinQ app shows it.
        0x4eb9 => Some(0),
        _ => None,
    }
}

fn display_light_supported(cap_2d6: u32, eeprom_checksum: u32) -> bool {
    // Bit 1 is brightness control. Bit 15 is the H09S1DA display control.
    (cap_2d6 & 0x8002) != 0 && display_light_on_value(eeprom_checksum).is_some()
}

const MODE_PURIFY_ONLY: u32 = 46;
const MOMENTARY_INOUTDOOR: u32 = 0x2;
const MOMENTARY_OUTDOOR: u32 = 0x4;

#[derive(Clone)]
struct Feature {
    name: String,
    description: String,
    /// `None` is always available. `Some((text, modes))` requires the AC on,
    /// and `modes` limits that to those operating modes (46 counts as fan).
    requirement: Option<(String, Option<Vec<u32>>)>,
}

struct Inner {
    meta: Metadata,
    initial_values_received: bool,
    power_change_hooks: Vec<PowerModeChangeHook>,
    power_state_prev: Option<bool>,
    mode_change_hooks: Vec<PowerModeChangeHook>,
    mode_prev: Option<String>,
    /// `None` until a live TLV is seen. Reconnect must not write OFF for an
    /// unknown initial value (upstream master `211a0bc`).
    air_clean: Option<bool>,
    jet_mode: Option<bool>,
    energy_save: Option<bool>,
    filter_used_time: u32,
    filter_life_time: u32,
    filter_changed_date: u32,
    filter_do_reset: bool,
    /// Last published filter-exhausted condition (for device trigger edge).
    filter_needs_change: bool,
    features: Vec<Feature>,
    remembered: std::collections::HashMap<String, String>,
}

pub struct Device {
    pub core: Arc<TlvDeviceCore>,
    inner: Mutex<Inner>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = TlvDeviceCore::new(ha, thinq);
        let this = Arc::new(Self {
            core: core.clone(),
            inner: Mutex::new(Inner {
                meta,
                initial_values_received: false,
                power_change_hooks: Vec::new(),
                power_state_prev: None,
                mode_change_hooks: Vec::new(),
                mode_prev: None,
                air_clean: None,
                jet_mode: None,
                energy_save: None,
                filter_used_time: 0,
                filter_life_time: 0,
                filter_changed_date: 0,
                filter_do_reset: false,
                filter_needs_change: false,
                features: Vec::new(),
                remembered: std::collections::HashMap::new(),
            }),
        });

        *core.is_caps_response.lock() = Some(Box::new(|tlv| tlv.iter().any(|e| e.t == 0x2da)));
        *core.is_values_response.lock() = Some(Box::new(|tlv| {
            tlv.len() >= 10 && tlv.iter().any(|e| e.t == 0x1f7)
        }));

        let t_vals = this.clone();
        *core.on_values.lock() = Some(Box::new(move || t_vals.values_received()));

        let t_priv = this.clone();
        *core.on_priv_data.lock() = Some(Box::new(move |cmd, buf9, data| {
            if cmd == 0x02 {
                t_priv.process_filter_data(buf9, data);
            }
        }));

        let t_resp = this.clone();
        *core.on_priv_cmd_resp.lock() = Some(Box::new(move |success, _buf1, cmd, data| {
            if cmd == 0x02 {
                t_resp.process_filter_cmd_resp(success, data);
            }
        }));

        this
    }

    fn get_power_tlv(&self) -> Option<u32> {
        self.core.get_raw(0x1f7)
    }

    fn get_mode_tlv(&self) -> Option<u32> {
        self.core.get_raw(0x1f9)
    }

    fn get_idu_action_running_tlv_num(&self) -> Option<u16> {
        if self.core.get_raw(0x189).is_some() {
            return Some(0x189);
        }
        if self.core.get_raw(0x6c).is_some() {
            return Some(0x6c);
        }
        None
    }

    fn has_tag(&self, id: u16) -> bool {
        self.core.get_raw(id).is_some()
    }

    fn has_cap_or_tag(&self, cap_word: u16, cap_bit: u32, state_tag: u16) -> bool {
        let cap = self.core.get_raw(cap_word).unwrap_or(0);
        (cap & cap_bit) != 0 || self.has_tag(state_tag)
    }

    fn values_received(self: &Arc<Self>) {
        {
            let mut inner = self.inner.lock();
            if inner.initial_values_received {
                return;
            }
            inner.initial_values_received = true;
        }
        self.core
            .thinq
            .send("setMaskingInfo", 0, json!({ "blacklist_tlv": "1200" }));

        // Delayed work (TS: 500ms + optional 5s filter probe) runs immediately for tests/runtime ordering.
        let f1 = self.core.get_raw(0x2f1).unwrap_or(0);
        if (f1 & 1) == 0 && (f1 & 0x200) == 0 {
            self.send_filter_query();
            // Filter probe timeout path (no priv reply in unit tests)
            self.init_make_set_config();
        } else {
            self.init_make_set_config();
        }
    }

    fn send_filter_query(&self) {
        self.core.send_priv_command(0x02, 0x02, &[]);
    }

    fn process_filter_data(&self, _buf9: u8, data: &[u8]) {
        if data.len() < 1 + 3 * 4 {
            return;
        }
        let used = u32::from_le_bytes(data[1..5].try_into().unwrap_or([0; 4]));
        let life = u32::from_le_bytes(data[5..9].try_into().unwrap_or([0; 4]));
        let changed = u32::from_le_bytes(data[9..13].try_into().unwrap_or([0; 4]));
        {
            let mut inner = self.inner.lock();
            inner.filter_used_time = used;
            inner.filter_life_time = life;
            inner.filter_changed_date = changed;
        }
        // Initial config already published via timeout path in unit tests; update HA values.
        self.publish_filter_data();
        let do_reset = {
            let mut inner = self.inner.lock();
            if inner.filter_do_reset {
                inner.filter_do_reset = false;
                true
            } else {
                false
            }
        };
        if do_reset {
            self.send_filter_reset();
        }
    }

    fn send_filter_reset(&self) {
        let life = self.inner.lock().filter_life_time;
        if life == 0 {
            return;
        }
        use chrono::Datelike;
        let now = chrono::Utc::now().date_naive();
        let date = now.year() as u32 * 10000 + now.month() as u32 * 100 + now.day() as u32;
        let mut buf = vec![0u8; 12];
        buf[4..8].copy_from_slice(&life.to_be_bytes());
        buf[8..12].copy_from_slice(&date.to_be_bytes());
        self.core.send_priv_command(0x02, 0x01, &buf);
    }

    fn publish_filter_data(&self) {
        let (used, life, changed, needs, fire_filter) = {
            let mut inner = self.inner.lock();
            let used = inner.filter_used_time;
            let life = inner.filter_life_time;
            let changed = inner.filter_changed_date;
            let needs = life > 0 && used >= life;
            let fire = needs && !inner.filter_needs_change;
            inner.filter_needs_change = needs;
            (used, life, changed, needs, fire)
        };
        let changed_date = format!(
            "{:04}-{:02}-{:02}",
            changed / 10000,
            (changed / 100) % 100,
            changed % 100
        );
        self.core.ha.publish_property(
            &self.core.id,
            "filterused",
            PropertyValue::Int(used as i64),
        );
        self.core.ha.publish_property(
            &self.core.id,
            "filterlife",
            PropertyValue::Int(life as i64),
        );
        self.core
            .ha
            .publish_property(&self.core.id, "filterchangeddate", changed_date.into());
        // Sticky problem binary_sensor (ON while filter exhausted).
        self.core.ha.publish_property(
            &self.core.id,
            "filter_needs_change",
            if needs {
                "ON".into()
            } else {
                "OFF".into()
            },
        );
        // Rising-edge event for automations that want a one-shot.
        if fire_filter {
            self.core.ha.fire_notification_event(
                &self.core.id,
                "filter_changed",
                "filter_needs_change",
            );
        }
    }

    fn process_filter_cmd_resp(&self, success: bool, _data: &[u8]) {
        if success {
            self.send_filter_query();
        }
    }

    fn update_climate_action(&self) {
        let mode_tlv = self.get_mode_tlv().unwrap_or(0);
        let mut idu_running = true;
        if let Some(num) = self.get_idu_action_running_tlv_num() {
            idu_running = self.core.get_raw(num).unwrap_or(0) != 0;
        }
        let modes2ha = ["cooling", "drying", "fan", "", "heating"];
        let action: Option<&str>;
        if self.get_power_tlv() == Some(0) {
            action = Some("off");
        } else if (mode_tlv == 0 || mode_tlv == 1 || mode_tlv == 4 || mode_tlv == 6) && !idu_running
        {
            action = Some("idle");
        } else if mode_tlv == MODE_PURIFY_ONLY {
            action = Some("fan");
        } else if mode_tlv == 6 {
            action = Some("None");
        } else if (mode_tlv as usize) < modes2ha.len() && !modes2ha[mode_tlv as usize].is_empty() {
            action = Some(modes2ha[mode_tlv as usize]);
        } else {
            action = None;
        }
        if let Some(a) = action {
            self.core
                .ha
                .publish_property(&self.core.id, "climate-action", a.into());
        }
    }

    fn init_make_set_config(self: &Arc<Self>) {
        // Clear hooks from any previous config build
        {
            let mut inner = self.inner.lock();
            inner.power_change_hooks.clear();
            inner.mode_change_hooks.clear();
        }

        let meta = self.inner.lock().meta.clone();
        let mut config = default_config(&meta, Some(json!({"name": "LG Air Conditioner"})));
        config.components.insert(
            "climate".into(),
            json!({
                "platform": "climate",
                "unique_id": "$deviceid-climate",
                "name": null,
                "action_topic": "$this/climate-action",
                "temperature_unit": "C",
                "temp_step": 0.5,
                "precision": 0.5,
                "min_temp": self.temp_limit(&[0x2e1, 0x2e3, 0x2eb], false).unwrap_or(18.0),
                "max_temp": self.temp_limit(&[0x2e2, 0x2e4, 0x2ec], true).unwrap_or(30.0),
                "fan_modes": ["auto", "very low", "low", "medium", "high", "very high"],
            }),
        );

        // current temperature
        {
            let mut f = FieldDefinition::new("climate", "current_temperature")
                .with_id(0x1fd)
                .read_only();
            f.state_topic = Some("topic".into());
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            self.core.add_field(&mut config, f, true);
        }

        // humidity: the H09 capability bit publishes the raw percent on the climate
        // entity. Older units that only carry tag 0x336 keep the 0.1% sensor.
        if self.core.get_raw(0x2dd).unwrap_or(0) & 0x20 != 0 && self.has_tag(0x336) {
            if let Some(Value::Object(climate)) = config.components.get_mut("climate") {
                climate.insert(
                    "current_humidity_topic".into(),
                    json!("$this/climate-current_humidity"),
                );
            }
            config.components.insert(
                "humidity".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-humidity",
                    "state_topic": "$this/humidity",
                    "name": "Humidity",
                    "device_class": "humidity",
                    "unit_of_measurement": "%",
                    "state_class": "measurement",
                }),
            );
            let this = self.clone();
            let mut f = FieldDefinition::new("climate", "current_humidity")
                .with_id(0x336)
                .read_only();
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::Int(raw as i64))));
            f.read_callback = Some(Box::new(move |val| {
                this.core
                    .ha
                    .publish_property(&this.core.id, "humidity", val.clone());
                true
            }));
            self.core.add_field(&mut config, f, true);
        } else if self.has_tag(0x336) {
            if let Some(Value::Object(climate)) = config.components.get_mut("climate") {
                climate.insert(
                    "current_humidity_topic".into(),
                    json!("$this/humidity-"),
                );
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
                    "suggested_display_precision": 1,
                }),
            );
            let mut f = FieldDefinition::new("humidity", "").with_id(0x336).read_only();
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 10.0))));
            self.core.add_field(&mut config, f, true);
        }

        // power
        {
            let this = self.clone();
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
                    vec![0x1f9, 0x1fa, 0x1fe]
                } else {
                    vec![]
                }
            }));
            power.read_xform = Some(Box::new(|raw| {
                Some(if raw != 0 { "ON".into() } else { "OFF".into() })
            }));
            power.read_callback = Some(Box::new(move |val| {
                if let Some(mode) = this.core.get_raw(0x1f9) {
                    this.core.process_key_value(0x1f9, mode);
                }
                let power_state = matches!(val, PropertyValue::Str(s) if s == "ON");
                let changed = {
                    let mut inner = this.inner.lock();
                    let changed = inner.power_state_prev != Some(power_state);
                    inner.power_state_prev = Some(power_state);
                    changed
                };
                if changed {
                    let hooks = std::mem::take(&mut this.inner.lock().power_change_hooks);
                    for h in &hooks {
                        h();
                    }
                    this.inner.lock().power_change_hooks = hooks;
                }
                false
            }));
            self.core.add_field(&mut config, power, true);
        }

        // mode
        {
            let this_r = self.clone();
            let this_w = self.clone();
            let this_cb = self.clone();
            let mut mode = FieldDefinition::new("climate", "mode").with_id(0x1f9);
            mode.read_xform = Some(Box::new(move |raw| {
                if this_r.get_power_tlv() == Some(0) {
                    return Some("off".into());
                }
                if raw == MODE_PURIFY_ONLY {
                    return Some("fan_only".into());
                }
                let modes2ha: [Option<&str>; 7] = [
                    Some("cool"),
                    Some("dry"),
                    Some("fan_only"),
                    None,
                    Some("heat"),
                    None,
                    Some("auto"),
                ];
                modes2ha
                    .get(raw as usize)
                    .and_then(|m| *m)
                    .map(|s| s.into())
            }));
            mode.read_callback = Some(Box::new(move |val| {
                if let PropertyValue::Str(s) = val {
                    let changed = {
                        let mut inner = this_cb.inner.lock();
                        let ch = inner.mode_prev.as_deref() != Some(s.as_str());
                        if ch {
                            inner.mode_prev = Some(s.clone());
                        }
                        ch
                    };
                    if changed {
                        let hooks = std::mem::take(&mut this_cb.inner.lock().mode_change_hooks);
                        for h in &hooks {
                            h();
                        }
                        this_cb.inner.lock().mode_change_hooks = hooks;
                    }
                }
                true
            }));
            mode.write_xform = Some(Box::new(move |val| {
                if val == "off" {
                    this_w.core.set_property("climate-power", "OFF");
                    return None;
                }
                this_w.core.set_raw(0x1f7, 1);
                match val {
                    "cool" => Some(0i64.into()),
                    "dry" => Some(1i64.into()),
                    "fan_only" => Some(2i64.into()),
                    "heat" => Some(4i64.into()),
                    "auto" => Some(6i64.into()),
                    _ => None,
                }
            }));
            mode.write_attach_static = Some(vec![0x1f7, 0x1fa, 0x1fe]);
            self.core.add_field(&mut config, mode, true);
        }

        // fan_mode
        {
            let mut f = FieldDefinition::new("climate", "fan_mode").with_id(0x1fa);
            f.read_xform = Some(Box::new(|raw| {
                let modes2ha: [Option<&str>; 9] = [
                    None,
                    None,
                    Some("very low"),
                    Some("low"),
                    Some("medium"),
                    Some("high"),
                    Some("very high"),
                    None,
                    Some("auto"),
                ];
                modes2ha
                    .get(raw as usize)
                    .and_then(|m| *m)
                    .map(|s| s.into())
            }));
            f.write_xform = Some(Box::new(|val| {
                match val {
                    "very low" => Some(2i64.into()),
                    "low" => Some(3i64.into()),
                    "medium" => Some(4i64.into()),
                    "high" => Some(5i64.into()),
                    "very high" => Some(6i64.into()),
                    "auto" => Some(8i64.into()),
                    _ => None,
                }
            }));
            f.write_attach_static = Some(vec![0x1f9, 0x1fe]);
            self.core.add_field(&mut config, f, true);
        }

        // temperature
        {
            let mut f = FieldDefinition::new("climate", "temperature").with_id(0x1fe);
            f.read_xform = Some(Box::new(|raw| Some(PropertyValue::from(raw as f64 / 2.0))));
            f.write_xform = Some(Box::new(|val| {
                let n: f64 = val.parse().ok()?;
                Some(PropertyValue::Int((n * 2.0).round() as i64))
            }));
            f.write_attach_static = Some(vec![0x1f9, 0x1fa]);
            self.core.add_field(&mut config, f, true);
        }

        // vertical swing
        if self.has_cap_or_tag(0x2cd, 4, 0x321) {
            if let Some(Value::Object(climate)) = config.components.get_mut("climate") {
                climate.insert(
                    "swing_modes".into(),
                    json!(["1", "2", "3", "4", "5", "6", "on", "off"]),
                );
            }
            let mut f = FieldDefinition::new("climate", "swing_mode").with_id(0x321);
            f.read_xform = Some(Box::new(|raw| {
                if raw == 100 {
                    return Some("on".into());
                }
                let modes = ["off", "1", "2", "3", "4", "5", "6"];
                modes.get(raw as usize).map(|s| (*s).into())
            }));
            f.write_xform = Some(Box::new(|val| {
                match val {
                    "off" => Some(0i64.into()),
                    "1" => Some(1i64.into()),
                    "2" => Some(2i64.into()),
                    "3" => Some(3i64.into()),
                    "4" => Some(4i64.into()),
                    "5" => Some(5i64.into()),
                    "6" => Some(6i64.into()),
                    "on" => Some(100i64.into()),
                    _ => None,
                }
            }));
            self.core.add_field(&mut config, f, true);
        }

        // horizontal swing
        if self.has_cap_or_tag(0x2cd, 8, 0x322) {
            if let Some(Value::Object(climate)) = config.components.get_mut("climate") {
                climate.insert(
                    "swing_horizontal_modes".into(),
                    json!(["1", "2", "3", "4", "5", "1-3", "3-5", "on", "off"]),
                );
            }
            let mut f = FieldDefinition::new("climate", "swing_horizontal_mode").with_id(0x322);
            f.read_xform = Some(Box::new(|raw| {
                match raw {
                    0 => Some("off".into()),
                    1 => Some("1".into()),
                    2 => Some("2".into()),
                    3 => Some("3".into()),
                    4 => Some("4".into()),
                    5 => Some("5".into()),
                    13 => Some("1-3".into()),
                    35 => Some("3-5".into()),
                    100 => Some("on".into()),
                    _ => None,
                }
            }));
            f.write_xform = Some(Box::new(|val| {
                match val {
                    "off" => Some(0i64.into()),
                    "1" => Some(1i64.into()),
                    "2" => Some(2i64.into()),
                    "3" => Some(3i64.into()),
                    "4" => Some(4i64.into()),
                    "5" => Some(5i64.into()),
                    "1-3" => Some(13i64.into()),
                    "3-5" => Some(35i64.into()),
                    "on" => Some(100i64.into()),
                    _ => None,
                }
            }));
            self.core.add_field(&mut config, f, true);
        }

        self.add_optional_sensor_field(
            &mut config,
            &[0x221],
            "error",
            "Error code",
            Some("mdi:alert"),
            None,
            None,
        );
        self.add_optional_sensor_field(
            &mut config,
            &[0x32e],
            "capacity",
            "Capacity nominal",
            None,
            Some(json!({
                "device_class": "power",
                "unit_of_measurement": "kW",
                "suggested_display_precision": 1,
            })),
            Some(Box::new(|raw| {
                if raw != 0 {
                    Some(PropertyValue::from(((raw as f64) * 0.293 * 10.0).round() / 10.0))
                } else {
                    None
                }
            })),
        );
        self.add_optional_sensor_field(
            &mut config,
            &[0x330],
            "eev",
            "EEV opening",
            Some("mdi:valve"),
            Some(json!({
                "state_class": "measurement",
                "suggested_display_precision": 0,
            })),
            None,
        );
        self.add_optional_sensor_temp_field(
            &mut config,
            &[0x2f9],
            "pipeintemp",
            "Pipe liquid temperature",
            Some("mdi:pipe"),
            Some(Box::new(|raw| {
                let idx = 255u32.saturating_sub(raw).min(255) as u8;
                rac_pipe_temp(idx).map(PropertyValue::from)
            })),
        );
        self.add_optional_sensor_temp_field(
            &mut config,
            &[0x2fa],
            "pipeouttemp",
            "Pipe gas temperature",
            Some("mdi:pipe"),
            Some(Box::new(|raw| {
                let idx = 255u32.saturating_sub(raw).min(255) as u8;
                rac_pipe_temp(idx).map(PropertyValue::from)
            })),
        );
        self.add_optional_sensor_temp_field(
            &mut config,
            &[0x7a, 0x32c],
            "oduhextemp",
            "ODU HEX temperature",
            Some("mdi:heating-coil"),
            Some(Box::new(|raw| {
                let idx = 255u32.saturating_sub(raw).min(255) as u8;
                rac_pipe_temp(idx).map(PropertyValue::from)
            })),
        );
        self.add_optional_sensor_temp_field(
            &mut config,
            &[0x332],
            "oduairtemp",
            "ODU air temperature",
            Some("mdi:thermometer-lines"),
            Some(Box::new(|raw| {
                let idx = 255u32.saturating_sub(raw).min(255) as u8;
                rac_air_temp(idx).map(PropertyValue::from)
            })),
        );
        self.add_optional_sensor_field(
            &mut config,
            &[0x331],
            "fanrpm",
            "Fan RPM",
            Some("mdi:fan"),
            Some(json!({
                "state_class": "measurement",
                "unit_of_measurement": "rpm",
                "suggested_display_precision": 0,
            })),
            Some(Box::new(|raw| Some(PropertyValue::Int((raw as i64) * 10)))),
        );

        if self.has_cap_or_tag(0x2cc, 1, 0x20f) {
            self.add_mode_dependent_config_switch(
                &mut config,
                0x20f,
                "airclean",
                "Air purify",
                "mdi:air-purifier",
                "air_clean",
                None,
            );
        }

        let jet_bits = self.core.get_raw(0x2cd).unwrap_or(0) & 3;
        let jet_cool = (jet_bits & 1) != 0 || (self.has_tag(0x323) && jet_bits == 0);
        let jet_heat = (jet_bits & 2) != 0 || (self.has_tag(0x323) && jet_bits == 0);
        if jet_cool || jet_heat {
            self.add_jet_field(
                &mut config,
                0x323,
                "jet",
                "Jet",
                "mdi:wind-power",
                jet_cool,
                jet_heat,
            );
        }

        if self.has_cap_or_tag(0x2d3, 1, 0x21a) {
            self.add_timer_field(&mut config, 0x21a, "sleeptimer", "Sleep timer", "mdi:bed-clock", 15);
        }
        if self.has_cap_or_tag(0x2d3, 0x4 | 0x10, 0x21c) || self.has_tag(0x21b) {
            self.add_timer_field(
                &mut config,
                0x21c,
                "starttimer",
                "Turn-on timer",
                "mdi:timer-play",
                24,
            );
            self.add_timer_field(
                &mut config,
                0x21b,
                "stoptimer",
                "Turn-off timer",
                "mdi:timer-stop",
                24,
            );
        }

        let active_energy = self.core.get_raw(0x34f).unwrap_or(0) & 0x400 != 0;
        if active_energy {
            self.add_energy_limit(&mut config);
        } else if self.has_cap_or_tag(0x2cc, 2, 0x20d) {
            self.add_mode_dependent_config_switch(
                &mut config,
                0x20d,
                "energysave",
                "Energy saving",
                "mdi:flower",
                "energy_save",
                Some(Box::new(|mode| mode == 0)),
            );
        }

        // 0x21f is not in the ThinQ app. Polarity and writability differ by indoor unit,
        // so only the firmwares verified to report and accept this polarity expose it.
        let cap_2d6 = self.core.get_raw(0x2d6).unwrap_or(0);
        let eeprom = self.core.get_raw(0x2da).unwrap_or(0);
        if let Some(on) = display_light_on_value(eeprom).filter(|_| display_light_supported(cap_2d6, eeprom))
        {
            self.add_config_switch_values(
                &mut config,
                0x21f,
                "displaylight",
                "Display light",
                "mdi:lightbulb-outline",
                on,
                1 - on,
            );
        }

        let auto_dry_setting = self.core.get_raw(0x350).unwrap_or(0) & 0x10 != 0;
        if self.has_cap_or_tag(0x2cc, 4, 0x20e) && auto_dry_setting {
            config
                .components
                .insert("autodry".into(), json!({"platform": "binary_sensor"}));
            self.add_config_switch_values(
                &mut config,
                0x20e,
                "autodrysetting",
                "Auto dry",
                "mdi:hair-dryer",
                255,
                0,
            );
            config.components.insert(
                "autodryremain".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-autodryremain",
                    "name": "Auto dry remaining",
                    "icon": "mdi:hair-dryer-outline",
                    "unit_of_measurement": "%",
                    "suggested_display_precision": 0,
                    "entity_category": "diagnostic",
                }),
            );
            let f = FieldDefinition::new("autodryremain", "")
                .with_id(0x225)
                .read_only();
            self.core.add_field(&mut config, f, true);
            self.add_autodry_fan(&mut config);
        } else if self.has_cap_or_tag(0x2cc, 4, 0x20e) {
            self.add_config_switch_field(&mut config, 0x20e, "autodry", "Auto dry", "mdi:hair-dryer");
            config.components.insert(
                "autodryremain".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-autodryremain",
                    "name": "Auto dry remaining",
                    "icon": "mdi:hair-dryer-outline",
                    "unit_of_measurement": "%",
                    "suggested_display_precision": 0,
                    "entity_category": "diagnostic",
                }),
            );
            let f = FieldDefinition::new("autodryremain", "")
                .with_id(0x225)
                .read_only();
            self.core.add_field(&mut config, f, true);
        }

        if let Some(idu_id) = self.get_idu_action_running_tlv_num() {
            let this = self.clone();
            let mut f = FieldDefinition::new("climate", "action").with_id(idu_id);
            f.read_callback = Some(Box::new(move |_val| {
                this.update_climate_action();
                false
            }));
            self.core.add_field(&mut config, f, false);
        }

        {
            let this = self.clone();
            self.inner.lock().mode_change_hooks.push(Box::new(move || {
                this.update_climate_action();
            }));
        }

        let filter_life = self.inner.lock().filter_life_time;
        if filter_life != 0 {
            config.components.insert(
                "filterused".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-filterused",
                    "state_topic": "$this/filterused",
                    "name": "Filter used time",
                    "icon": "mdi:air-filter",
                    "device_class": "duration",
                    "unit_of_measurement": "h",
                    "state_class": "total_increasing",
                    "entity_category": "diagnostic",
                }),
            );
            config.components.insert(
                "filterlife".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-filterlife",
                    "state_topic": "$this/filterlife",
                    "name": "Filter life time",
                    "icon": "mdi:air-filter",
                    "device_class": "duration",
                    "unit_of_measurement": "h",
                    "entity_category": "diagnostic",
                }),
            );
            config.components.insert(
                "changeddate".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-filterchangeddate",
                    "state_topic": "$this/filterchangeddate",
                    "name": "Filter usage last reset",
                    "icon": "mdi:calendar-refresh-outline",
                    "device_class": "date",
                    "entity_category": "diagnostic",
                }),
            );
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
            let this = self.clone();
            let mut fr = FieldDefinition::new("", "");
            // registered manually under filterreset key
            fr.write_xform = Some(Box::new(|val| {
                Some(if val == "PRESS" {
                    1i64.into()
                } else {
                    0i64.into()
                })
            }));
            fr.write_callback = Some(Box::new(move |val| {
                if val == 1 {
                    this.inner.lock().filter_do_reset = true;
                    this.send_filter_query();
                }
                false
            }));
            // Use fields_by_ha with key filterreset
            let fr = Arc::new(fr);
            self.core
                .fields_by_ha
                .lock()
                .insert("filterreset".into(), fr);
        }

        let accum = self.core.get_raw(0x2d5).unwrap_or(0);
        if accum & (MOMENTARY_INOUTDOOR | MOMENTARY_OUTDOOR) != 0 {
            config.components.insert(
                "energy_current".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-energy_current",
                    "state_topic": "$this/energy_current",
                    "name": "Power",
                    "device_class": "power",
                    "unit_of_measurement": "W",
                    "state_class": "measurement",
                    "suggested_display_precision": 0,
                }),
            );
            let mut f = FieldDefinition::new("energy_current", "")
                .with_id(0x2b3)
                .read_only();
            if accum & MOMENTARY_OUTDOOR == 0 {
                f.read_xform = Some(Box::new(|raw| {
                    let w = (raw as i64) - 60;
                    Some(PropertyValue::Int(w.max(5)))
                }));
            }
            self.core.add_field(&mut config, f, true);
        }

        // Entity-based notifications (device_automation triggers are unreliable in HA UI).
        let (k, v) = rethink_core::problem_binary_sensor(
            "filter_needs_change",
            "Filter needs change",
        );
        config.components.insert(k, v);
        let (k, v) = rethink_core::notification_event(
            "filter_changed",
            "Filter alert",
            &["filter_needs_change"],
            None, // event device_class: only doorbell|button|motion
        );
        config.components.insert(k, v);

        self.add_extended_features(&mut config);
        self.describe_standard_features(&mut config, jet_cool, jet_heat);
        {
            let this = self.clone();
            self.inner.lock().mode_change_hooks.push(Box::new(move || {
                this.publish_feature_availability();
            }));
            let this = self.clone();
            self.inner.lock().power_change_hooks.push(Box::new(move || {
                this.publish_feature_availability();
            }));
        }

        self.core.set_config(config);
        self.publish_feature_info();

        if filter_life != 0 {
            self.publish_filter_data();
        }

        self.core.query();
    }

    fn add_timer_field(
        self: &Arc<Self>,
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
                "step": 0.25,
                "mode": "slider",
            }),
        );
        let mut f = FieldDefinition::new(name, "").with_id(id);
        f.read_xform = Some(Box::new(|raw| {
            let hours = ((raw as f64) / 60.0 / 0.25).ceil() * 0.25;
            Some(PropertyValue::from(hours))
        }));
        f.write_xform = Some(Box::new(|val| {
            let n: f64 = val.parse().ok()?;
            Some(PropertyValue::Int((n * 60.0).round() as i64))
        }));
        if name == "sleeptimer" {
            let this = self.clone();
            f.write_callback = Some(Box::new(move |_| this.get_power_tlv().unwrap_or(0) != 0));
        }
        self.core.add_field(config, f, true);
    }

    fn add_jet_field(
        self: &Arc<Self>,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        jet_cool: bool,
        jet_heat: bool,
    ) {
        let desc_full = format!(
            "{} {}{}{}",
            desc,
            if jet_cool { "cool" } else { "" },
            if jet_cool && jet_heat { "/" } else { "" },
            if jet_heat { "heat" } else { "" }
        );
        config.components.insert(
            name.into(),
            json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{name}"),
                "name": desc_full,
                "icon": icon,
                "entity_category": "config",
                "optimistic": true,
            }),
        );

        let this_r = self.clone();
        let this_w = self.clone();
        let this_rc = self.clone();
        let this_wc = self.clone();
        let mut f = FieldDefinition::new(name, "").with_id(id);
        f.write_xform = Some(Box::new(move |val| {
            let on = val == "ON";
            this_w.inner.lock().jet_mode = Some(on);
            if !on {
                return Some(0i64.into());
            }
            if jet_cool && this_w.get_mode_tlv() == Some(0) {
                return Some(1i64.into());
            }
            if jet_heat && this_w.get_mode_tlv() == Some(4) {
                return Some(2i64.into());
            }
            Some(0i64.into())
        }));
        f.read_xform = Some(Box::new(move |raw| {
            if jet_cool && this_r.get_mode_tlv() == Some(0) && raw == 1 {
                return Some("ON".into());
            }
            if jet_heat && this_r.get_mode_tlv() == Some(4) && raw == 2 {
                return Some("ON".into());
            }
            Some("OFF".into())
        }));
        f.read_callback = Some(Box::new(move |val| {
            let power = this_rc.get_power_tlv();
            if power == Some(0) || power.is_none() {
                return false;
            }
            let mode = this_rc.get_mode_tlv();
            if !((jet_cool && mode == Some(0)) || (jet_heat && mode == Some(4))) {
                return false;
            }
            this_rc.inner.lock().jet_mode = Some(matches!(val, PropertyValue::Str(s) if s == "ON"));
            true
        }));
        f.write_callback = Some(Box::new(move |_val| {
            let power = this_wc.get_power_tlv();
            let mode = this_wc.get_mode_tlv();
            power != Some(0)
                && ((jet_cool && mode == Some(0)) || (jet_heat && mode == Some(4)))
        }));
        self.core.add_field(config, f, true);

        let this = self.clone();
        let prop_name = format!("{name}-");
        self.inner.lock().mode_change_hooks.push(Box::new(move || {
            let Some(on) = this.inner.lock().jet_mode else {
                return;
            };
            this.core
                .set_property(&prop_name, if on { "ON" } else { "OFF" });
        }));
    }

    fn add_optional_sensor_field(
        &self,
        config: &mut DeviceDiscovery,
        ids: &[u16],
        name: &str,
        desc: &str,
        icon: Option<&str>,
        extra: Option<Value>,
        read_xform: Option<Box<dyn Fn(u32) -> Option<PropertyValue> + Send + Sync>>,
    ) {
        let id = ids.iter().copied().find(|&val| {
            if let Some(v) = self.core.get_raw(val) {
                if let Some(ref xf) = read_xform {
                    xf(v).is_some()
                } else {
                    true
                }
            } else {
                false
            }
        });
        let Some(id) = id else { return };

        let mut comp = Map::new();
        comp.insert("platform".into(), json!("sensor"));
        comp.insert("unique_id".into(), json!(format!("$deviceid-{name}")));
        comp.insert("name".into(), json!(desc));
        comp.insert("entity_category".into(), json!("diagnostic"));
        if let Some(ic) = icon {
            comp.insert("icon".into(), json!(ic));
        }
        if let Some(Value::Object(extra_map)) = extra {
            for (k, v) in extra_map {
                comp.insert(k, v);
            }
        }
        config
            .components
            .insert(name.into(), Value::Object(comp));

        let mut f = FieldDefinition::new(name, "").with_id(id).read_only();
        if let Some(xf) = read_xform {
            f.read_xform = Some(xf);
        }
        self.core.add_field(config, f, true);
    }

    fn add_optional_sensor_temp_field(
        &self,
        config: &mut DeviceDiscovery,
        ids: &[u16],
        name: &str,
        desc: &str,
        icon: Option<&str>,
        read_xform: Option<Box<dyn Fn(u32) -> Option<PropertyValue> + Send + Sync>>,
    ) {
        self.add_optional_sensor_field(
            config,
            ids,
            name,
            desc,
            icon,
            Some(json!({
                "device_class": "temperature",
                "unit_of_measurement": "°C",
                "state_class": "measurement",
                "suggested_display_precision": 2,
            })),
            read_xform,
        );
    }

    fn temp_limit(&self, tags: &[u16], max: bool) -> Option<f64> {
        let mut values = Vec::new();
        for tag in tags {
            if let Some(v) = self.core.get_raw(*tag) {
                if v != 0 {
                    values.push(v);
                }
            }
        }
        let picked = if max {
            values.into_iter().max()
        } else {
            values.into_iter().min()
        }?;
        Some(picked as f64 / 2.0)
    }

    fn requirement_met(&self, modes: Option<&[u32]>) -> bool {
        if self.get_power_tlv().unwrap_or(0) == 0 {
            return false;
        }
        let mut mode = self.get_mode_tlv().unwrap_or(0);
        if mode == MODE_PURIFY_ONLY {
            mode = 2;
        }
        match modes {
            None => true,
            Some(list) => list.contains(&mode),
        }
    }

    fn add_config_switch_field(
        &self,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
    ) {
        self.add_config_switch_values(config, id, name, desc, icon, 1, 0);
    }

    fn add_config_switch_values(
        &self,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        on_value: u32,
        off_value: u32,
    ) {
        config.components.insert(
            name.into(),
            json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{name}"),
                "name": desc,
                "icon": icon,
                "entity_category": "config",
                "optimistic": true,
            }),
        );
        let mut f = FieldDefinition::new(name, "").with_id(id);
        f.write_xform = Some(Box::new(move |val| {
            Some(if val == "ON" {
                i64::from(on_value).into()
            } else {
                i64::from(off_value).into()
            })
        }));
        f.read_xform = Some(Box::new(move |raw| {
            Some(if raw == off_value {
                "OFF".into()
            } else {
                "ON".into()
            })
        }));
        self.core.add_field(config, f, true);
    }

    fn describe_feature(
        &self,
        config: &mut DeviceDiscovery,
        name: &str,
        description: &str,
        requirement: Option<(String, Option<Vec<u32>>)>,
    ) {
        let Some(Value::Object(comp)) = config.components.get_mut(name) else {
            return;
        };
        comp.insert(
            "json_attributes_topic".into(),
            json!(format!("$this/{name}-attributes")),
        );
        if requirement.is_some() {
            comp.insert("availability_mode".into(), json!("all"));
            comp.insert(
                "availability".into(),
                json!([
                    {"topic": "$this/availability"},
                    {"topic": "$rethink/availability"},
                    {"topic": format!("$this/{name}-available")},
                ]),
            );
        }
        self.inner.lock().features.push(Feature {
            name: name.into(),
            description: description.into(),
            requirement,
        });
    }

    fn publish_feature_info(&self) {
        let features = self.inner.lock().features.clone();
        for feature in &features {
            let requires = feature
                .requirement
                .as_ref()
                .map(|(text, _)| text.as_str())
                .unwrap_or("nothing, always available");
            let body = json!({
                "description": feature.description,
                "requires": requires,
            })
            .to_string();
            self.core.ha.publish_property(
                &self.core.id,
                &format!("{}-attributes", feature.name),
                body.into(),
            );
        }
        self.publish_feature_availability();
    }

    fn publish_feature_availability(&self) {
        let features = self.inner.lock().features.clone();
        for feature in &features {
            let Some((_, modes)) = &feature.requirement else {
                continue;
            };
            let online = self.requirement_met(modes.as_deref());
            self.core.ha.publish_property(
                &self.core.id,
                &format!("{}-available", feature.name),
                (if online { "online" } else { "offline" }).into(),
            );
        }
        self.replay_remembered();
    }

    fn replay_remembered(&self) {
        let pending: Vec<(String, String, Option<Vec<u32>>)> = {
            let inner = self.inner.lock();
            inner
                .remembered
                .iter()
                .filter_map(|(name, value)| {
                    let feature = inner.features.iter().find(|f| f.name == *name)?;
                    let modes = feature.requirement.as_ref().and_then(|(_, m)| m.clone());
                    Some((name.clone(), value.clone(), modes))
                })
                .collect()
        };
        for (name, value, modes) in pending {
            if self.requirement_met(modes.as_deref()) {
                self.core.set_property(&format!("{name}-"), &value);
            }
        }
    }

    fn add_extended_features(self: &Arc<Self>, config: &mut DeviceDiscovery) {
        let caps = |tag: u16, mask: u32| self.core.get_raw(tag).unwrap_or(0) & mask != 0;
        if caps(0x2ca, 0x30000) {
            let modes = vec![0, 4];
            self.add_running_switch(config, 0x23e, "aiair", "AI Air", "mdi:robot-outline", modes.clone(), false);
            self.describe_feature(
                config,
                "aiair",
                "AI Air adjusts the fan and airflow while cooling or heating.",
                Some(("AC on, in Cooling or Heating".into(), Some(modes))),
            );
        }
        if caps(0x34f, 0x100000) && self.has_tag(0x3c9) {
            self.add_masked_switch(config, 0x3c9, "windowdetection", "Window detection", "mdi:window-open-variant", 1);
            self.describe_feature(
                config,
                "windowdetection",
                "Window Open Detection switches the AC to Energy Control when the room temperature changes suddenly.",
                None,
            );
        }
        if caps(0x2cc, 0x200) {
            let modes = vec![4];
            self.add_running_switch(config, 0x22d, "lowheating", "Low heating", "mdi:snowflake-thermometer", modes.clone(), false);
            self.describe_feature(
                config,
                "lowheating",
                "Low Heating keeps the room at about 10 °C.",
                Some(("AC on, in Heating".into(), Some(modes))),
            );
        }
        if caps(0x374, 0x4) && self.has_tag(0x3a0) {
            self.add_config_switch_values(config, 0x3a0, "beep", "Beep", "mdi:volume-high", 0, 1);
            self.describe_feature(
                config,
                "beep",
                "The beep when a setting changes. Off is the Mute function.",
                None,
            );
        }
        if caps(0x2f0, 0x8) && self.has_tag(0x12d) {
            let while_on = Some(("AC on".into(), None));
            for (id, name, desc, text) in [
                (0x12du16, "goodsleep", "GoodSleep", "Sleep+ adjusts the temperature and fan while the Sleep timer runs."),
                (0x12f, "goodsleepop", "GoodSleep adjusts operation", "Lets Sleep+ adjust the fan and operation."),
                (0x133, "goodsleeptemp", "GoodSleep adjusts temperature", "Lets Sleep+ adjust the set temperature."),
            ] {
                if !self.has_tag(id) {
                    continue;
                }
                self.add_config_switch_field(config, id, name, desc, "mdi:sleep");
                self.describe_feature(config, name, text, while_on.clone());
            }
        }
        if self.has_tag(0x351) {
            let modes = vec![0, 2];
            self.add_masked_running_switch(config, 0x351, "softair", "Soft Air", "mdi:weather-windy", 0x10000, modes.clone());
            self.describe_feature(
                config,
                "softair",
                "Soft Air directs the airflow upwards and runs the fan low.",
                Some(("AC on, in Cooling or Fan".into(), Some(modes))),
            );
        }
    }

    fn describe_standard_features(
        &self,
        config: &mut DeviceDiscovery,
        jet_cool: bool,
        jet_heat: bool,
    ) {
        self.describe_feature(
            config,
            "autodrysetting",
            "Auto Clean+ dries the indoor unit after Cooling or Dry.",
            None,
        );
        self.describe_feature(
            config,
            "autodryfan",
            "Fan speed used while Auto Clean+ dries the indoor unit.",
            None,
        );
        if config.components.contains_key("displaylight") {
            self.describe_feature(
                config,
                "displaylight",
                "The indoor unit display.",
                None,
            );
        }
        if config.components.contains_key("sleeptimer") {
            self.describe_feature(
                config,
                "sleeptimer",
                "Turns the AC off after the set time.",
                Some(("AC on".into(), None)),
            );
        }
        for name in ["starttimer", "stoptimer"] {
            self.describe_feature(
                config,
                name,
                "Turns the AC on or off after the set time (up to 24 h).",
                None,
            );
        }
        let mut jet_modes = Vec::new();
        if jet_cool {
            jet_modes.push(0);
        }
        if jet_heat {
            jet_modes.push(4);
        }
        if !jet_modes.is_empty() && config.components.contains_key("jet") {
            let text = if jet_modes.len() == 2 {
                "AC on, in Cooling or Heating"
            } else if jet_modes[0] == 0 {
                "AC on, in Cooling"
            } else {
                "AC on, in Heating"
            };
            self.describe_feature(
                config,
                "jet",
                "Jet runs strong airflow for 30 minutes.",
                Some((text.into(), Some(jet_modes))),
            );
        }
        if config.components.contains_key("energysave") {
            self.describe_feature(
                config,
                "energysave",
                "Energy saving reduces the power input.",
                Some(("AC on, in Cooling".into(), Some(vec![0]))),
            );
        }
        if config.components.contains_key("airclean") {
            self.describe_feature(
                config,
                "airclean",
                "Air purify runs the ionizer. It can be set at any time.",
                None,
            );
        }
    }

    fn add_running_switch(
        self: &Arc<Self>,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        modes: Vec<u32>,
        remember: bool,
    ) {
        self.add_gated_field(config, id, name, desc, icon, modes, remember, false, 0, None);
    }

    fn add_masked_running_switch(
        self: &Arc<Self>,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        mask: u32,
        modes: Vec<u32>,
    ) {
        self.add_gated_field(config, id, name, desc, icon, modes, false, true, mask, None);
    }

    fn add_masked_switch(
        &self,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        mask: u32,
    ) {
        config.components.insert(
            name.into(),
            json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{name}"),
                "name": desc,
                "icon": icon,
                "entity_category": "config",
                "optimistic": true,
            }),
        );
        let this_w = self.core.clone();
        let this_r = self.core.clone();
        let mut f = FieldDefinition::new(name, "").with_id(id);
        f.write_xform = Some(Box::new(move |val| {
            let current = this_w.get_raw(id).unwrap_or(0);
            let next = if val == "ON" {
                current | mask
            } else {
                current & !mask
            };
            Some(i64::from(next).into())
        }));
        f.read_xform = Some(Box::new(move |raw| {
            Some(if raw & mask != 0 { "ON".into() } else { "OFF".into() })
        }));
        let _ = this_r;
        self.core.add_field(config, f, true);
    }

    fn add_energy_limit(self: &Arc<Self>, config: &mut DeviceDiscovery) {
        config.components.insert(
            "energylimit".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-energylimit",
                "name": "Active energy control",
                "icon": "mdi:flash",
                "entity_category": "config",
                "options": ["off", "80%", "60%", "40%"],
            }),
        );
        let modes = vec![0u32];
        let this = self.clone();
        let mut f = FieldDefinition::new("energylimit", "").with_id(0x20d);
        f.write_xform = Some(Box::new(move |val| {
            this.inner.lock().remembered.insert("energylimit".into(), val.into());
            let code = match val {
                "off" => 0,
                "80%" => 80,
                "60%" => 60,
                "40%" => 40,
                _ => return None,
            };
            Some(i64::from(code).into())
        }));
        f.read_xform = Some(Box::new(|raw| {
            let label = match raw {
                0 => "off",
                80 => "80%",
                60 => "60%",
                40 => "40%",
                _ => return None,
            };
            Some(label.into())
        }));
        let this_w = self.clone();
        let modes_w = modes.clone();
        f.write_callback = Some(Box::new(move |_| this_w.requirement_met(Some(&modes_w))));
        let this_r = self.clone();
        let modes_r = modes.clone();
        f.read_callback = Some(Box::new(move |_| this_r.requirement_met(Some(&modes_r))));
        self.core.add_field(config, f, true);
        self.describe_feature(
            config,
            "energylimit",
            "Energy Control limits the power input to 80, 60 or 40 percent.",
            Some(("AC on, in Cooling".into(), Some(modes))),
        );
    }

    fn add_gated_field(
        self: &Arc<Self>,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        modes: Vec<u32>,
        remember: bool,
        masked: bool,
        mask: u32,
        _select: Option<()>,
    ) {
        config.components.insert(
            name.into(),
            json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{name}"),
                "name": desc,
                "icon": icon,
                "entity_category": "config",
                "optimistic": true,
            }),
        );
        let this_w = self.clone();
        let name_w = name.to_string();
        let mut f = FieldDefinition::new(name, "").with_id(id);
        f.write_xform = Some(Box::new(move |val| {
            if remember {
                this_w.inner.lock().remembered.insert(name_w.clone(), val.into());
            }
            if masked {
                let current = this_w.core.get_raw(id).unwrap_or(0);
                let next = if val == "ON" { current | mask } else { current & !mask };
                Some(i64::from(next).into())
            } else {
                Some(if val == "ON" { 1i64.into() } else { 0i64.into() })
            }
        }));
        f.read_xform = Some(Box::new(move |raw| {
            let on = if masked { raw & mask != 0 } else { raw != 0 };
            Some(if on { "ON".into() } else { "OFF".into() })
        }));
        let this_gate = self.clone();
        let modes_gate = modes.clone();
        f.write_callback = Some(Box::new(move |_| this_gate.requirement_met(Some(&modes_gate))));
        let this_read = self.clone();
        let modes_read = modes;
        f.read_callback = Some(Box::new(move |_| this_read.requirement_met(Some(&modes_read))));
        self.core.add_field(config, f, true);
    }

    fn add_autodry_fan(&self, config: &mut DeviceDiscovery) {
        let mask = self.core.get_raw(0x192).unwrap_or(0);
        if mask == 0 {
            return;
        }
        let table = [(2u32, "low"), (4, "medium"), (6, "high")];
        let options: Vec<&str> = table
            .iter()
            .filter(|(code, _)| mask & (1 << code) != 0)
            .map(|(_, name)| *name)
            .collect();
        if options.is_empty() {
            return;
        }
        config.components.insert(
            "autodryfan".into(),
            json!({
                "platform": "select",
                "unique_id": "$deviceid-autodryfan",
                "name": "Auto dry fan",
                "icon": "mdi:fan",
                "entity_category": "config",
                "options": options,
            }),
        );
        let mut f = FieldDefinition::new("autodryfan", "").with_id(0x1f2);
        f.read_xform = Some(Box::new(|raw| {
            let label = match raw {
                2 => "low",
                4 => "medium",
                6 => "high",
                _ => return None,
            };
            Some(label.into())
        }));
        f.write_xform = Some(Box::new(|val| {
            let code = match val {
                "low" => 2,
                "medium" => 4,
                "high" => 6,
                _ => return None,
            };
            Some(i64::from(code).into())
        }));
        self.core.add_field(config, f, true);
    }

    fn add_mode_dependent_config_switch(
        self: &Arc<Self>,
        config: &mut DeviceDiscovery,
        id: u16,
        name: &str,
        desc: &str,
        icon: &str,
        field: &str,
        check_mode: Option<Box<dyn Fn(u32) -> bool + Send + Sync>>,
    ) {
        config.components.insert(
            name.into(),
            json!({
                "platform": "switch",
                "unique_id": format!("$deviceid-{name}"),
                "name": desc,
                "icon": icon,
                "entity_category": "config",
                "optimistic": true,
            }),
        );

        let this_r = self.clone();
        let this_w = self.clone();
        let field_r = field.to_string();
        let field_w = field.to_string();
        let check_mode: Option<Arc<dyn Fn(u32) -> bool + Send + Sync>> =
            check_mode.map(|f| Arc::from(f));
        let check_r = check_mode.clone();
        let check_w = check_mode.clone();
        let has_check = check_mode.is_some();

        let mut f = FieldDefinition::new(name, "").with_id(id);
        f.write_xform = Some(Box::new(|val| {
            Some(if val == "ON" {
                1i64.into()
            } else {
                0i64.into()
            })
        }));
        f.read_xform = Some(Box::new(|raw| {
            Some(if raw != 0 { "ON".into() } else { "OFF".into() })
        }));
        f.read_callback = Some(Box::new(move |val| {
            let power = this_r.get_power_tlv();
            if power == Some(0) || power.is_none() {
                return false;
            }
            if let Some(ref check) = check_r {
                if !check(this_r.get_mode_tlv().unwrap_or(0)) {
                    return false;
                }
            }
            let on = matches!(val, PropertyValue::Str(s) if s == "ON");
            match field_r.as_str() {
                "air_clean" => this_r.inner.lock().air_clean = Some(on),
                "jet_mode" => this_r.inner.lock().jet_mode = Some(on),
                "energy_save" => this_r.inner.lock().energy_save = Some(on),
                _ => {}
            }
            true
        }));
        f.write_callback = Some(Box::new(move |val| {
            let on = val == 1;
            match field_w.as_str() {
                "air_clean" => this_w.inner.lock().air_clean = Some(on),
                "jet_mode" => this_w.inner.lock().jet_mode = Some(on),
                "energy_save" => this_w.inner.lock().energy_save = Some(on),
                _ => {}
            }
            this_w.get_power_tlv() != Some(0)
                && check_w
                    .as_ref()
                    .map(|c| c(this_w.get_mode_tlv().unwrap_or(0)))
                    .unwrap_or(true)
        }));
        self.core.add_field(config, f, true);

        let this = self.clone();
        let prop_name = format!("{name}-");
        let field_owned = field.to_string();
        if has_check {
            self.inner.lock().mode_change_hooks.push(Box::new(move || {
                let on = match field_owned.as_str() {
                    "air_clean" => this.inner.lock().air_clean,
                    "jet_mode" => this.inner.lock().jet_mode,
                    "energy_save" => this.inner.lock().energy_save,
                    _ => None,
                };
                let Some(on) = on else {
                    return;
                };
                this.core
                    .set_property(&prop_name, if on { "ON" } else { "OFF" });
            }));
        } else {
            self.inner.lock().power_change_hooks.push(Box::new(move || {
                if this.get_power_tlv() == Some(0) {
                    return;
                }
                let on = match field_owned.as_str() {
                    "air_clean" => this.inner.lock().air_clean,
                    "jet_mode" => this.inner.lock().jet_mode,
                    "energy_save" => this.inner.lock().energy_save,
                    _ => None,
                };
                let Some(on) = on else {
                    return;
                };
                this.core
                    .set_property(&prop_name, if on { "ON" } else { "OFF" });
            }));
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

    const DEVICE_ID: &str = "test-id";
    const CAPS_REQUEST_HEX: &str = "01010400000065020201027D416A0D";
    const CAPS_RESPONSE_HEX: &str = "0000040000008702010249B001B05057B0A0017CB0C1B103B306B34FB4C7B582B541B543B6A04D81B6F0690409B701BC40BD47B5C0B61024B643B5C1B600B643B5C2B600B643B5C4B61026B643B5C6B6102CB64344E1";
    const QUERY_RESPONSE_HEX: &str = "00000400000087020415777E447DC17E837F50297F9026C840C880C8C08340838083C0868086C0870087C0894088408A10118A505A8A8F8CA0C0BA8CD010ACE00164D540D580C900CAD0A0CB1040CB40CB8CCBCFCC1032CC504FCC90438B40BF600155BFE00271BFA00155C0200271BE509FBE90A01B01BED050C300C340C0C0C3803E6B";
    const WRITE_MODE_FAN_ONLY_HEX: &str = "01010400000065020101087E427DC17E837F80E609";
    const WRITE_MODE_HEAT_HEX: &str = "01010400000065020101097E447DC17E837F902AFD3D";
    const WRITE_MODE_COOL_FROM_OFF_HEX: &str = "01010400000065020101097E407DC17E887F902C8C89";
    const WRITE_POWER_OFF_HEX: &str = "01010400000065020101027DC00576";
    const WRITE_AUTODRY_ON_HEX: &str = "010104000000650201010283816D5D";
    const WRITE_AUTODRY_OFF_HEX: &str = "010104000000650201010283807D7C";
    const MINIMAL_CAPS_HEX: &str = "0000040000008702010002b6819989";
    const CST_VALUES_HEX: &str = "000004000000a70204004b7dc17e407e887f502d7f902a7f0086808840d4c0d500c84081408180c9408340838083c08fc0cd40cd00ccc0cda00352d56007f3d5a00960bc9089d5d03cd61020c9009c4087cbac40e9c1ad27";
    // S4NW12JA31A, firmware 0x690441, EEPROM checksum 0x4e88. 0x2d6 bit 1 is set.
    const DISPLAY_CAPS_RESPONSE_HEX: &str = "000004000000870201F361B001B05057B0A0017CB85024B8903CB8D020B9103CBAD024BB103CB0C1B103B306B280B347B4C7B582B541B543B6A04E88B6F0690441B701B740BC40BD4FB5C0B6102EB643B5C1B61028B643B5C2B61028B643B5C4B6102EB643B5C6B6102EB643EE35";
    const DISPLAY_OFF_QUERY_RESPONSE_HEX: &str = "00000400000087020426797E447DC17E827F502A7F9030C840C880C8C08340838083C0868086C0870087C08F80894088408A102A8A50548A8F8C90238CC2ACE0027ACA00D540D580C900CAD0B8CB1061CB40CB8CCBCFCC00CC90688B40BF600155BFC0BFA00155C000BE502DBE8CCC504F1B01BED050C340C0C0C380CCC0CD00CD4090000AA0";
    const DISPLAY_ON_NOTIFY_HEX: &str = "0000040000008702047D0287C1BF4D";
    const WRITE_DISPLAY_ON_HEX: &str = "010104000000650201010287C1E95D";
    const WRITE_DISPLAY_OFF_HEX: &str = "010104000000650201010287C0F97C";

    fn meta() -> Metadata {
        Metadata::new("RAC_056905_WW", "TEST", "1.0")
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

    #[test]
    fn display_light_follows_the_known_firmware_gate() {
        assert!(display_light_supported(2, 0x4e88));
        assert!(display_light_supported(2, 0x551d));
        assert!(display_light_supported(2, 0x4c19));
        assert!(display_light_supported(2, 0x4f58));
        assert!(display_light_supported(2, 0x4e95));
        assert!(display_light_supported(0x6, 0x4e88));
        // Negative checksums, and a positive checksum with the capability bit clear.
        assert!(!display_light_supported(2, 0x4dda));
        assert!(!display_light_supported(2, 0x4d81));
        assert!(!display_light_supported(2, 0x4dfa));
        assert!(!display_light_supported(1, 0x4e88));
        assert!(!display_light_supported(0, 0x4e88));

        let (ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(DISPLAY_CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(DISPLAY_OFF_QUERY_RESPONSE_HEX));
        // The first values packet is applied before the field exists. Replay it.
        thinq.emit_data(&hex_decode(DISPLAY_OFF_QUERY_RESPONSE_HEX));

        let device = ha.device(DEVICE_ID).expect("HA configuration published");
        let c = device.config.as_ref().unwrap();
        let component = &c.components["displaylight"];
        assert_eq!(component["platform"], "switch");
        assert_eq!(component["name"], "Display light");
        assert_eq!(component["icon"], "mdi:lightbulb-outline");
        assert_eq!(component["entity_category"], "config");
        assert!(!c.components.contains_key("light"));
        assert_eq!(
            ha.get_property(DEVICE_ID, "displaylight", "state"),
            Some("OFF".into())
        );

        thinq.emit_data(&hex_decode(DISPLAY_ON_NOTIFY_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "displaylight", "state"),
            Some("ON".into())
        );

        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "displaylight", "command", "ON");
        let out = thinq.outbox();
        assert_eq!(out.len(), 1, "display light ON write");
        assert_eq!(hex_encode(&out[0]), WRITE_DISPLAY_ON_HEX);

        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "displaylight", "command", "OFF");
        let out = thinq.outbox();
        assert_eq!(out.len(), 1, "display light OFF write");
        assert_eq!(hex_encode(&out[0]), WRITE_DISPLAY_OFF_HEX);
        let _ = dev;
    }

    fn build_ready() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let (ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    #[test]
    fn caps_and_values_trigger_config_publish() {
        let (ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS_RESPONSE_HEX));
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        let device = ha.device(DEVICE_ID).expect("HA configuration published");
        let c = &device.config.as_ref().unwrap().components;
        assert_eq!(c["climate"]["platform"], "climate");
        assert!(c.contains_key("jet"));
        assert!(c.contains_key("energysave"));
        assert!(c.contains_key("autodry"));
        assert!(c.contains_key("sleeptimer"));
        assert!(c.contains_key("starttimer"));
        assert!(c.contains_key("stoptimer"));
        assert!(c.contains_key("airclean"));
        // CAPS checksum 0x4d81 is a known negative: no display-light entity.
        assert!(!c.contains_key("light"));
        assert!(!c.contains_key("displaylight"));
        assert_eq!(
            c["climate"]["swing_modes"],
            json!(["1", "2", "3", "4", "5", "6", "on", "off"])
        );
        assert_eq!(
            c["climate"]["swing_horizontal_modes"],
            json!(["1", "2", "3", "4", "5", "1-3", "3-5", "on", "off"])
        );
        let _ = dev;
    }

    #[test]
    fn initial_state_publishes_properties() {
        let (ha, thinq, _dev) = build_ready();
        thinq.emit_data(&hex_decode(QUERY_RESPONSE_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "current_temperature"),
            Some(PropertyValue::from(20.5))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "temperature_state"),
            Some(PropertyValue::from(19.0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "fan_mode_state"),
            Some("low".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("heat".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "swing_mode_state"),
            Some("off".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "swing_horizontal_mode_state"),
            Some("off".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "autodry", "state"),
            Some("OFF".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "sleeptimer", "state"),
            Some(PropertyValue::Int(0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "starttimer", "state"),
            Some(PropertyValue::Int(0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "stoptimer", "state"),
            Some(PropertyValue::Int(0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "jet", "state"),
            Some("OFF".into())
        );
        assert!(ha.get_property(DEVICE_ID, "energysave", "state").is_none());
    }

    #[test]
    fn write_mode_fan_only() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(0x1fa, 3);
        dev.set_raw(0x1fe, 0);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "fan_only");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_MODE_FAN_ONLY_HEX);
    }

    #[test]
    fn write_mode_heat() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(0x1fa, 3);
        dev.set_raw(0x1fe, 42);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "heat");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_MODE_HEAT_HEX);
    }

    #[test]
    fn write_mode_off() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(0x1f7, 1);
        dev.set_raw(0x1f9, 0);
        dev.set_raw(0x1fa, 3);
        dev.set_raw(0x1fe, 42);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "off");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_OFF_HEX);
    }

    #[test]
    fn write_mode_cool_from_off() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(0x1f7, 0);
        dev.set_raw(0x1f9, 0);
        dev.set_raw(0x1fa, 8);
        dev.set_raw(0x1fe, 44);
        ha.set_property(DEVICE_ID, "climate", "mode_command", "cool");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_MODE_COOL_FROM_OFF_HEX);
        assert_eq!(dev.get_raw(0x1f7), Some(1));
    }

    #[test]
    fn write_autodry() {
        let (ha, thinq, dev) = build_ready();
        dev.set_raw(0x20e, 0);
        ha.set_property(DEVICE_ID, "autodry", "command", "ON");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_AUTODRY_ON_HEX);
        assert_eq!(dev.get_raw(0x20e), Some(1));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "autodry", "command", "OFF");
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_AUTODRY_OFF_HEX);
        assert_eq!(dev.get_raw(0x20e), Some(0));
    }

    #[test]
    fn constructor_sends_query_caps() {
        let (_ha, thinq, _dev) = make_device();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), CAPS_REQUEST_HEX);
    }

    #[test]
    fn cst_values_unlock_from_state_tags() {
        let (ha, thinq, _dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(MINIMAL_CAPS_HEX));
        thinq.emit_data(&hex_decode(CST_VALUES_HEX));
        let device = ha.device(DEVICE_ID).expect("config");
        let c = &device.config.as_ref().unwrap().components;
        assert!(c.contains_key("humidity"));
        assert_eq!(c["humidity"]["device_class"], "humidity");
        assert_eq!(
            c["climate"]["current_humidity_topic"],
            "$this/humidity-"
        );
        assert!(c.contains_key("autodry"));
        assert!(c.contains_key("airclean"));
        assert!(c.contains_key("energysave"));
        assert!(c.contains_key("sleeptimer"));
        assert!(c["climate"].get("swing_modes").is_some());
        assert!(c["climate"].get("swing_horizontal_modes").is_none());

        thinq.emit_data(&hex_decode(CST_VALUES_HEX));
        assert_eq!(
            ha.get_property(DEVICE_ID, "humidity", "state"),
            Some(PropertyValue::from(85.0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "current_temperature"),
            Some(PropertyValue::from(22.5))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("cool".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "autodry", "state"),
            Some("OFF".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "airclean", "state"),
            Some("OFF".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "energysave", "state"),
            Some("OFF".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "sleeptimer", "state"),
            Some(PropertyValue::Int(0))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "swing_mode_state"),
            Some("off".into())
        );
    }

    #[test]
    fn unknown_jet_airclean_energysave_not_written_on_mode_or_power_change() {
        // Master 211a0bc: reconnect must not write OFF for switches whose live
        // value has never been received.
        let (_, thinq, dev) = build_ready();
        {
            let mut inner = dev.inner.lock();
            inner.jet_mode = None;
            inner.air_clean = None;
            inner.energy_save = None;
        }
        thinq.reset_recorder();
        {
            let hooks = std::mem::take(&mut dev.inner.lock().mode_change_hooks);
            for h in &hooks {
                h();
            }
            dev.inner.lock().mode_change_hooks = hooks;
        }
        {
            let hooks = std::mem::take(&mut dev.inner.lock().power_change_hooks);
            for h in &hooks {
                h();
            }
            dev.inner.lock().power_change_hooks = hooks;
        }
        assert!(
            thinq.outbox().is_empty(),
            "reconnect must not write unknown jet/airclean/energysave; got {}",
            thinq
                .outbox()
                .iter()
                .map(|p| hex_encode(p))
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    fn h09_ready() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        const CAPS: &str = "000004000000870201139FB001E180B05077B0A0017CD41012D3F0100400B85024B8903CB8D020B9103CBAD024BB103CB0C1B1101FB3200205B2B0030000B2C0BC08B34FABC0B4B0041020B4F0045011B5A08000B54EB6A04EB9B6F0055703B701B75020BC40BD30080000EA01BD7040480BFB0FDD046440649054FA016C81B5C0B61028B642B5C1B61020B642B5C2B61020B642B5C4B61028B642B5D02EB61020B642B5C6B6102CB6429A18";
        const QUERY: &str = "00000400000087020419B27E407DC07E827F50347F90287EC17F00C840C8847BD0147C00C8C083408390FF83C0A880868086C0870087C18F8089407C82E8805940CD903688408A10FF8A408A80ADA00205ACC0AD4059C0A0406D4168816901D440F2700A50018200D540D580D5D03CD61020C900CAC0CB1092CB40CB89CBC0CC10CCCC40CC90908B40E801FA80BF40BFE00355BF80C0200355BE508FBE908E6240BEC0C340C0C0EE40C380CCC0CD00CD4090004B414BC14CC152C053404A4C";
        let (ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        thinq.emit_data(&hex_decode(CAPS));
        thinq.emit_data(&hex_decode(QUERY));
        thinq.emit_data(&hex_decode(QUERY));
        thinq.reset_recorder();
        (ha, thinq, dev)
    }

    fn notify(pairs: &[(u16, u32)]) -> Vec<u8> {
        use rethink_util::packet_codec::{encode_packet, Direction, EncodeInput, TlvEncodeInput};
        use rethink_util::tlv::Tlv;
        encode_packet(&EncodeInput::Tlv(TlvEncodeInput {
            direction: Direction::FromDevice,
            tlv: pairs.iter().copied().map(|(t, v)| Tlv::new(t, v)).collect(),
            a: None,
            s: None,
            byte5: None,
            byte6: None,
            byte7: None,
        }))
        .unwrap()
        .1
    }

    #[test]
    fn h09s1d_exposes_advertised_features_and_inverted_display_light() {
        let (ha, thinq, dev) = h09_ready();
        let info = ha.device(DEVICE_ID).unwrap();
        let c = &info.config.as_ref().unwrap().components;
        assert_eq!(c["climate"]["min_temp"].as_f64(), Some(16.0));
        assert_eq!(c["climate"]["max_temp"].as_f64(), Some(30.0));
        assert_eq!(
            c["climate"]["current_humidity_topic"],
            "$this/climate-current_humidity"
        );
        assert_eq!(c["humidity"]["device_class"], "humidity");
        assert_eq!(c["energylimit"]["platform"], "select");
        assert_eq!(c["energylimit"]["options"], json!(["off", "80%", "60%", "40%"]));
        assert_eq!(c["autodryfan"]["options"], json!(["low", "medium", "high"]));
        assert_eq!(
            c["energylimit"]["availability"][2]["topic"],
            "$this/energylimit-available"
        );
        assert!(c["beep"].get("availability").is_none());
        assert_eq!(c["beep"]["json_attributes_topic"], "$this/beep-attributes");
        assert!(c.contains_key("starttimer"));
        assert!(c.contains_key("stoptimer"));
        assert_eq!(c["autodry"], json!({"platform": "binary_sensor"}));
        assert!(!c.contains_key("energysave"));
        assert_eq!(c["energy_current"]["device_class"], "power");
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "current_humidity"),
            Some(PropertyValue::Int(54))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "humidity", "state"),
            Some(PropertyValue::Int(54))
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "windowdetection", "state"),
            Some("ON".into())
        );
        assert_eq!(ha.get_property(DEVICE_ID, "beep", "state"), Some("OFF".into()));
        assert_eq!(
            ha.get_property(DEVICE_ID, "goodsleep", "state"),
            Some("ON".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "autodrysetting", "state"),
            Some("ON".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "autodryfan", "state"),
            Some("low".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "displaylight", "state"),
            Some("OFF".into())
        );

        thinq.emit_data(&notify(&[(0x2b3, 223)]));
        assert_eq!(
            ha.get_property(DEVICE_ID, "energy_current", "state"),
            Some(PropertyValue::Int(223))
        );

        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "displaylight", "command", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "010104000000650201010287C0F97C");
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "beep", "command", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "0101040000006502010102E8012B05");
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "autodrysetting", "command", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "01010400000065020101038390FFB80D");
        let _ = dev;
    }

    #[test]
    fn h09s1d_gates_writes_and_publishes_availability() {
        let (ha, thinq, _dev) = h09_ready();
        ha.set_property(DEVICE_ID, "energylimit", "command", "60%");
        assert!(thinq.outbox().is_empty());
        thinq.emit_data(&notify(&[(0x1f7, 1), (0x1f9, 0)]));
        assert!(thinq
            .outbox()
            .iter()
            .any(|p| hex_encode(p) == "010104000000650201010383503C4776"));

        let (ha, thinq, _dev) = h09_ready();
        thinq.emit_data(&notify(&[(0x1f7, 1), (0x1f9, 46), (0x20f, 1)]));
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "mode_state"),
            Some("fan_only".into())
        );
        assert_eq!(
            ha.get_property(DEVICE_ID, "climate", "action"),
            Some("fan".into())
        );
        let props = &ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(
            props.get("aiair-available").map(|v| v.as_string()),
            Some("offline".into())
        );
        assert_eq!(
            props.get("softair-available").map(|v| v.as_string()),
            Some("online".into())
        );
        assert!(props
            .get("beep-attributes")
            .unwrap()
            .as_string()
            .contains("nothing, always available"));
        assert!(props
            .get("energylimit-attributes")
            .unwrap()
            .as_string()
            .contains("AC on, in Cooling"));

        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "softair", "command", "ON");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "0101040000006502010105D470010000C5B7"
        );

        let (ha, thinq, _dev) = h09_ready();
        thinq.emit_data(&notify(&[(0x1f7, 1), (0x1f9, 0)]));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "lowheating", "command", "ON");
        assert!(thinq.outbox().is_empty());
        thinq.emit_data(&notify(&[(0x1f9, 4)]));
        assert!(thinq.outbox().iter().all(|p| hex_encode(p) != "01010400000065020101028B413DB8"));
        ha.set_property(DEVICE_ID, "lowheating", "command", "ON");
        assert!(thinq.outbox().iter().any(|p| hex_encode(p) == "01010400000065020101028B413DB8"));

        let (ha, thinq, _dev) = h09_ready();
        ha.set_property(DEVICE_ID, "sleeptimer", "command", "1");
        assert!(thinq.outbox().is_empty());
        thinq.emit_data(&notify(&[(0x1f7, 1), (0x1f9, 0)]));
        thinq.reset_recorder();
        ha.set_property(DEVICE_ID, "sleeptimer", "command", "1");
        assert_eq!(thinq.outbox().len(), 1);
    }

    #[test]
    fn older_units_publish_jet_and_energy_saving_availability() {
        const CAPS: &str = "0000040000008702017C4EB001B05077B0A0017CB0C1B103B307B34FB4C7B582B6A04D87B6F0690409B701BC40BD46B5C0B61029B642B5C1B6102EB642B5C2B6102EB642B5C4B6102CB642B5C5B6102EB642B5C6B6102CB64294E8";
        const QUERY: &str = "00000400000087020480697E407DC07E827F50317F9029C840C881C8C08340838183C0868086C0870087C1894088408A008A408A808C808CC0ACC0D540D580C900CAC0CB108BCB40CB8CCBC0CC10CCCC40CC90968B40BF6001C7BFC0BFA001C7C000BE5089BE90881B00BEC0C300C340C0C0C3800C05";
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(CAPS));
        thinq.emit_data(&hex_decode(QUERY));
        thinq.emit_data(&hex_decode(QUERY));
        let props = &ha.device(DEVICE_ID).unwrap().properties;
        for name in ["jet", "energysave", "sleeptimer"] {
            assert_eq!(
                props.get(&format!("{name}-available")).map(|v| v.as_string()).as_deref(),
                Some("offline")
            );
        }
        assert!(props.get("airclean-available").is_none());
        thinq.emit_data(&notify(&[(0x1f7, 1), (0x1f9, 0)]));
        let props = &ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(props.get("jet-available").map(|v| v.as_string()).as_deref(), Some("online"));
        assert_eq!(props.get("energysave-available").map(|v| v.as_string()).as_deref(), Some("online"));
        thinq.emit_data(&notify(&[(0x1f9, 4)]));
        let props = &ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(props.get("energysave-available").map(|v| v.as_string()).as_deref(), Some("offline"));
        assert!(props.get("jet-attributes").unwrap().as_string().contains("AC on, in Cooling or Heating"));
        let c = ha.device(DEVICE_ID).unwrap();
        let components = &c.config.as_ref().unwrap().components;
        assert!(!components.contains_key("displaylight"));
        assert!(!components.contains_key("aiair"));
        assert_eq!(components["energysave"]["platform"], "switch");
        assert_eq!(components["autodry"]["platform"], "switch");
        assert!(!components.contains_key("autodrysetting"));
    }
}
