//! WLSEL6333F — LG LSEL6333F slide-in electric range (AABB, class `0x40`, 75-byte status).
//!
//! Port of upstream PR #329 (adapting PR #304 with the full-preference clock sync
//! fix that preserves Auto Remote). Decodes 75-byte `40 EB`/`40 EC` records and
//! 73-byte `40 CF` live preheat temperature frames; supports remote Bake start,
//! cancel, preferences, beeper volumes, temperature adjustment, and clock sync.

use crate::device_trait::DeviceHandler;
use chrono::Timelike;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{hex_decode, HaConnection, Metadata, PropertyValue, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LENGTH: usize = 75;
const OVEN_STATE: usize = 14;
const OVEN_MODE: usize = 15;
const TARGET_TEMPERATURE: usize = 21;
const CURRENT_TEMPERATURE: usize = 23;
const DOOR_FLAGS: usize = 27;
const DOOR_OPEN: u8 = 0x04;
const CONTROL_LOCK: usize = 11;
const COOKTOP_STATE: usize = 60;

const PREFERENCE_TEMPLATE: &str =
    "8080808080808080808080ff8080800000808080808000000000000080";
const CF_MARKER: &[u8] = &[0x00, 0x45, 0x01, 0x03, 0x04];

fn state_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Idle"),
        1 => Some("Preheating"),
        2 => Some("Cooking"),
        _ => None,
    }
}

fn mode_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("None"),
        1 => Some("Bake"),
        _ => None,
    }
}

fn clock_format_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("24-hour"),
        1 => Some("12-hour"),
        _ => None,
    }
}

fn temp_unit_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("°F"),
        1 => Some("°C"),
        _ => None,
    }
}

fn beeper_name(code: u8) -> Option<&'static str> {
    match code {
        2 => Some("High"),
        1 => Some("Low"),
        0 => Some("Mute"),
        _ => None,
    }
}

fn components() -> HashMap<String, Value> {
    [
        ("oven_status", json!({"platform":"sensor","device_class":"enum","icon":"mdi:stove","unique_id":"$deviceid-oven_status","state_topic":"$this/oven_status","name":"Oven Status","options":["Idle","Preheating","Cooking"]})),
        ("oven_mode", json!({"platform":"sensor","device_class":"enum","icon":"mdi:chef-hat","unique_id":"$deviceid-oven_mode","state_topic":"$this/oven_mode","name":"Oven Mode","options":["None","Bake"]})),
        ("oven_set_temperature", json!({"platform":"sensor","device_class":"temperature","icon":"mdi:thermometer-chevron-up","unique_id":"$deviceid-oven_set_temperature","state_topic":"$this/oven_set_temperature","name":"Oven Set Temperature","unit_of_measurement":"°F"})),
        ("oven_temperature", json!({"platform":"sensor","device_class":"temperature","icon":"mdi:thermometer","unique_id":"$deviceid-oven_temperature","state_topic":"$this/oven_temperature","name":"Oven Temperature","unit_of_measurement":"°F"})),
        ("oven_door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-oven_door","state_topic":"$this/oven_door","name":"Oven Door"})),
        ("control_lock", json!({"platform":"binary_sensor","icon":"mdi:lock","unique_id":"$deviceid-control_lock","state_topic":"$this/control_lock","name":"Control Lock"})),
        ("cooktop_status", json!({"platform":"binary_sensor","icon":"mdi:stove","unique_id":"$deviceid-cooktop_status","state_topic":"$this/cooktop_status","name":"Cooktop"})),
        ("preheat_alarm_light", json!({"platform":"switch","icon":"mdi:lightbulb-alert-outline","unique_id":"$deviceid-preheat_alarm_light","state_topic":"$this/preheat_alarm_light","command_topic":"$this/preheat_alarm_light/set","name":"Preheat Alarm Light"})),
        ("auto_conversion", json!({"platform":"switch","icon":"mdi:fan-auto","unique_id":"$deviceid-auto_conversion","state_topic":"$this/auto_conversion","command_topic":"$this/auto_conversion/set","name":"Convection Auto Conversion"})),
        ("auto_remote", json!({"platform":"switch","icon":"mdi:remote","unique_id":"$deviceid-auto_remote","state_topic":"$this/auto_remote","command_topic":"$this/auto_remote/set","name":"Auto Remote"})),
        ("remote_start", json!({"platform":"binary_sensor","icon":"mdi:remote","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote Start"})),
        ("remote_started", json!({"platform":"binary_sensor","icon":"mdi:remote-tv","unique_id":"$deviceid-remote_started","state_topic":"$this/remote_started","name":"Remote Started"})),
        ("keep_warm", json!({"platform":"binary_sensor","icon":"mdi:heat-wave","unique_id":"$deviceid-keep_warm","state_topic":"$this/keep_warm","name":"Keep Warm"})),
        ("remaining_time", json!({"platform":"sensor","device_class":"duration","unit_of_measurement":"s","icon":"mdi:timer-outline","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining Time"})),
        ("cook_time", json!({"platform":"sensor","device_class":"duration","unit_of_measurement":"s","icon":"mdi:timer-sand","unique_id":"$deviceid-cook_time","state_topic":"$this/cook_time","name":"Cook Time"})),
        ("remote_temperature", json!({"platform":"number","icon":"mdi:thermometer","unique_id":"$deviceid-remote_temperature","state_topic":"$this/remote_temperature","command_topic":"$this/remote_temperature/set","name":"Remote Bake Temperature","unit_of_measurement":"°F","min":170,"max":550,"step":5})),
        ("remote_cook_time", json!({"platform":"number","icon":"mdi:timer-cog-outline","unique_id":"$deviceid-remote_cook_time","state_topic":"$this/remote_cook_time","command_topic":"$this/remote_cook_time/set","name":"Remote Bake Time","unit_of_measurement":"min","min":0,"max":719,"step":1})),
        ("remote_keep_warm", json!({"platform":"switch","icon":"mdi:heat-wave","unique_id":"$deviceid-remote_keep_warm","state_topic":"$this/remote_keep_warm","command_topic":"$this/remote_keep_warm/set","name":"Remote Bake Keep Warm"})),
        ("start", json!({"platform":"button","icon":"mdi:play","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"PRESS","name":"Start Bake","availability":[{"topic":"$this/remote_start","payload_available":"ON","payload_not_available":"OFF"}]})),
        ("cancel", json!({"platform":"button","icon":"mdi:stop","unique_id":"$deviceid-cancel","command_topic":"$this/cancel/set","payload_press":"PRESS","name":"Cancel"})),
        ("temperature_adjustment", json!({"platform":"number","icon":"mdi:thermometer-plus","unique_id":"$deviceid-temperature_adjustment","state_topic":"$this/temperature_adjustment","command_topic":"$this/temperature_adjustment/set","name":"Temperature Adjustment","unit_of_measurement":"°F","min":-35,"max":35,"step":1})),
        ("clock_format", json!({"platform":"sensor","device_class":"enum","icon":"mdi:clock-outline","unique_id":"$deviceid-clock_format","state_topic":"$this/clock_format","name":"Clock format","options":["24-hour","12-hour"],"entity_category":"diagnostic"})),
        ("temperature_unit", json!({"platform":"select","icon":"mdi:thermometer","unique_id":"$deviceid-temperature_unit","state_topic":"$this/temperature_unit","command_topic":"$this/temperature_unit/set","options":["°F","°C"],"name":"Temperature Unit"})),
        ("oven_beeper_volume", json!({"platform":"select","icon":"mdi:volume-high","unique_id":"$deviceid-oven_beeper_volume","state_topic":"$this/oven_beeper_volume","command_topic":"$this/oven_beeper_volume/set","name":"Oven Beeper Volume","options":["High","Low","Mute"]})),
        ("cooktop_beeper_volume", json!({"platform":"select","icon":"mdi:volume-high","unique_id":"$deviceid-cooktop_beeper_volume","state_topic":"$this/cooktop_beeper_volume","command_topic":"$this/cooktop_beeper_volume/set","name":"Cooktop Beeper Volume","options":["High","Low","Mute"]})),
        ("clock_sync", json!({"platform":"button","unique_id":"$deviceid-clock_sync","command_topic":"$this/clock_sync/set","payload_press":"PRESS","name":"Sync Clock","icon":"mdi:clock-check-outline"})),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

struct State {
    clock_format: Option<u8>,
    temperature_unit: Option<u8>,
    auto_remote: Option<u8>,
    remote_temperature: u16,
    remote_cook_time: u16,
    remote_keep_warm: bool,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    state: Mutex<State>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let comps = components();
        let mut config = default_config(&meta, Some(json!({"name": "LG Electric Range"})));
        config.components = comps.clone();
        core.set_config(config);
        for (prop, comp) in &comps {
            if comp.get("state_topic").is_some() {
                core.publish_property(prop, "None".into());
            }
        }
        core.publish_property("remote_temperature", PropertyValue::Int(350));
        core.publish_property("remote_cook_time", PropertyValue::Int(15));
        core.publish_on_off("remote_keep_warm", false);

        let this = Arc::new(Self {
            core: core.clone(),
            state: Mutex::new(State {
                clock_format: None,
                temperature_unit: None,
                auto_remote: None,
                remote_temperature: 350,
                remote_cook_time: 15,
                remote_keep_warm: false,
            }),
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(cur) = process_common_status(buf, 0x40, STATUS_LENGTH) {
            self.process_status(cur);
            return;
        }
        if buf.len() == 73 && buf[0] == 0x40 && buf[1] == 0xcf && &buf[2..7] == CF_MARKER {
            let target = u16::from_be_bytes([buf[12], buf[13]]);
            let temp = u16::from_be_bytes([buf[14], buf[15]]);
            let unit = self.state.lock().temperature_unit;
            let val = if (target & 0x8000) == 0 && unit != Some(1) && temp != 0 {
                PropertyValue::Int(i64::from(temp))
            } else {
                "None".into()
            };
            self.core.publish_property("oven_temperature", val);
        }
    }

    fn process_status(&self, cur: &[u8]) {
        let clock_fmt = (cur[0] <= 1).then_some(cur[0]);
        let temp_unit = (cur[2] <= 1).then_some(cur[2]);
        let auto_remote = (cur[10] <= 1).then_some(cur[10]);
        {
            let mut st = self.state.lock();
            st.clock_format = clock_fmt;
            st.temperature_unit = temp_unit;
            st.auto_remote = auto_remote;
        }

        self.core.publish_property(
            "clock_format",
            clock_format_name(cur[0]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "preheat_alarm_light",
            match cur[5] {
                1 => "ON",
                0 => "OFF",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "auto_conversion",
            match cur[1] {
                1 => "ON",
                0 => "OFF",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "auto_remote",
            match cur[10] {
                1 => "ON",
                0 => "OFF",
                _ => "None",
            }
            .into(),
        );
        self.core
            .publish_on_off("keep_warm", cur[DOOR_FLAGS] & 0x08 != 0);
        self.core
            .publish_on_off("remote_started", cur[OVEN_MODE] & 0x80 != 0);
        let rem = i64::from(cur[16]) + i64::from(cur[17]) * 60 + i64::from(cur[18]) * 3600;
        let cook = i64::from(cur[19]) * 60 + i64::from(cur[20]) * 3600;
        self.core
            .publish_property("remaining_time", PropertyValue::Int(rem));
        self.core
            .publish_property("cook_time", PropertyValue::Int(cook));
        self.core.publish_on_off("remote_start", cur[31] & 1 != 0);
        self.core.publish_property(
            "temperature_adjustment",
            if temp_unit == Some(0) {
                PropertyValue::Int(i64::from(cur[3] as i8))
            } else {
                "None".into()
            },
        );
        self.core.publish_property(
            "temperature_unit",
            temp_unit_name(cur[2]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "oven_beeper_volume",
            beeper_name(cur[6]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "cooktop_beeper_volume",
            beeper_name(cur[9]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "oven_status",
            state_name(cur[OVEN_STATE]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "oven_mode",
            mode_name(cur[OVEN_MODE] & 0x7f).unwrap_or("None").into(),
        );
        let target = u16::from_be_bytes([cur[TARGET_TEMPERATURE], cur[TARGET_TEMPERATURE + 1]]);
        let temp = u16::from_be_bytes([cur[CURRENT_TEMPERATURE], cur[CURRENT_TEMPERATURE + 1]]);
        let fahrenheit = (target & 0x8000) == 0 && temp_unit == Some(0);
        self.core.publish_property(
            "oven_set_temperature",
            if fahrenheit && target != 0 {
                PropertyValue::Int(i64::from(target))
            } else {
                "None".into()
            },
        );
        self.core.publish_property(
            "oven_temperature",
            if fahrenheit && temp != 0 {
                PropertyValue::Int(i64::from(temp))
            } else {
                "None".into()
            },
        );
        self.core
            .publish_on_off("oven_door", cur[DOOR_FLAGS] & DOOR_OPEN != 0);
        self.core.publish_property(
            "control_lock",
            match cur[CONTROL_LOCK] {
                1 => "ON",
                0 => "OFF",
                _ => "None",
            }
            .into(),
        );
        self.core.publish_property(
            "cooktop_status",
            match cur[COOKTOP_STATE] {
                1 => "ON",
                0 | 3 => "OFF",
                _ => "None",
            }
            .into(),
        );
    }

    fn send_preference(&self, payload: &[u8]) {
        let mut cmd = Vec::with_capacity(4 + payload.len());
        cmd.extend_from_slice(&[0xf0, 0x43, 0x21, 0x0e]);
        cmd.extend_from_slice(payload);
        self.core.send(&cmd);
    }

    pub fn send_clock(&self, hours: u8, minutes: u8) {
        let (clock_format, auto_remote) = {
            let st = self.state.lock();
            match (st.clock_format, st.auto_remote) {
                (Some(cf), Some(ar)) => (cf, ar),
                _ => return,
            }
        };
        let mut payload = hex_decode(PREFERENCE_TEMPLATE);
        payload[0] = if clock_format == 1 {
            let h = hours % 12;
            if h == 0 { 12 } else { h }
        } else {
            hours
        };
        payload[1] = minutes;
        payload[2] = clock_format;
        payload[13] = auto_remote;
        payload[28] = auto_remote;
        self.send_preference(&payload);
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
        match prop {
            "remote_keep_warm" if value == "ON" || value == "OFF" => {
                self.state.lock().remote_keep_warm = value == "ON";
                self.core.publish_property(prop, value.into());
            }
            "remote_temperature" | "remote_cook_time" => {
                let s = value.trim();
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return;
                }
                let Ok(n) = s.parse::<u16>() else { return };
                if prop == "remote_temperature" {
                    if !(170..=550).contains(&n) || n % 5 != 0 {
                        return;
                    }
                    self.state.lock().remote_temperature = n;
                } else {
                    if n > 719 {
                        return;
                    }
                    self.state.lock().remote_cook_time = n;
                }
                self.core.publish_property(prop, PropertyValue::Int(i64::from(n)));
            }
            "start" if value == "PRESS" => {
                let (temp, mins, keep_warm) = {
                    let st = self.state.lock();
                    (st.remote_temperature, st.remote_cook_time, st.remote_keep_warm)
                };
                let mut cmd = vec![0xf0, 0x43, 0x20, 0x0b];
                let mut payload = [0u8; 15];
                payload[0] = 1;
                payload[4] = if keep_warm { 2 } else { 0 };
                payload[5..7].copy_from_slice(&temp.to_be_bytes());
                payload[7] = (mins / 60) as u8;
                payload[8] = (mins % 60) as u8;
                cmd.extend_from_slice(&payload);
                self.core.send(&cmd);
            }
            "cancel" if value == "PRESS" => {
                self.core.send(&[0xf0, 0x44, 0x00]);
            }
            "preheat_alarm_light" | "auto_conversion" | "auto_remote"
                if value == "ON" || value == "OFF" =>
            {
                let mut payload = hex_decode(PREFERENCE_TEMPLATE);
                let enabled = u8::from(value == "ON");
                let idx = match prop {
                    "preheat_alarm_light" => 7,
                    "auto_conversion" => 3,
                    _ => 13,
                };
                payload[idx] = enabled;
                if prop == "auto_remote" {
                    payload[28] = enabled;
                }
                self.send_preference(&payload);
            }
            "temperature_adjustment" => {
                let s = value.trim();
                let Ok(adj) = s.parse::<i8>() else { return };
                if !(-35..=35).contains(&adj) {
                    return;
                }
                let mut payload = hex_decode(PREFERENCE_TEMPLATE);
                payload[4] = u8::from(adj == 0);
                payload[5] = adj as u8;
                self.send_preference(&payload);
            }
            "temperature_unit" => {
                let unit = match value {
                    "°F" => 0,
                    "°C" => 1,
                    _ => return,
                };
                let mut payload = hex_decode(PREFERENCE_TEMPLATE);
                payload[9] = unit;
                self.send_preference(&payload);
            }
            "oven_beeper_volume" | "cooktop_beeper_volume" => {
                let vol = match value {
                    "High" => 2,
                    "Low" => 1,
                    "Mute" => 0,
                    _ => return,
                };
                let mut payload = hex_decode(PREFERENCE_TEMPLATE);
                payload[if prop == "oven_beeper_volume" { 8 } else { 12 }] = vol;
                self.send_preference(&payload);
            }
            "clock_sync" if value == "PRESS" => {
                let now = chrono::Local::now();
                self.send_clock(now.hour() as u8, now.minute() as u8);
            }
            _ => {}
        }
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

    const ID: &str = "test-id";
    const IDLE: &str = "AA5140EB0101000000010200FF020000000000000000000000000000000000000000000E000000000000000000000000000000000000000000000000000000000000000000000000000000000000006FBB";
    const BAKE350: &str = "AA9C40EC0101000000010200FF0200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000101000000010200FF020000000001010000000000015E0064000020000000020000000000000000000000000000000000000000000000000000000000000000000000000000000000000030BB";
    const PREHEAT_PROGRESS: &str = "AA0040CF004501030401010000000163009B00000000000022000000000700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000065BB";

    fn prop(ha: &MockHaConnection, key: &str) -> Option<String> {
        ha.device(ID)?.properties.get(key).map(|v| v.as_string())
    }

    #[test]
    fn decodes_status_cf_preheat_and_emits_captured_commands() {
        let meta = Metadata::new("WLSEL6333F", "WLSEL6333F", "1.0");
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta.clone());
        let dev = Device::new(ha.clone(), thinq.clone(), meta);

        thinq.emit_data(&hex_decode(IDLE));
        assert_eq!(prop(&ha, "oven_status").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "clock_format").as_deref(), Some("12-hour"));
        assert_eq!(prop(&ha, "temperature_unit").as_deref(), Some("°F"));
        assert_eq!(prop(&ha, "auto_remote").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(BAKE350));
        assert_eq!(prop(&ha, "oven_status").as_deref(), Some("Preheating"));
        assert_eq!(prop(&ha, "oven_mode").as_deref(), Some("Bake"));
        assert_eq!(prop(&ha, "oven_set_temperature").as_deref(), Some("350"));
        assert_eq!(prop(&ha, "oven_temperature").as_deref(), Some("100"));

        thinq.emit_data(&hex_decode(PREHEAT_PROGRESS));
        assert_eq!(prop(&ha, "oven_temperature").as_deref(), Some("155"));

        thinq.reset_recorder();
        dev.set_property("auto_remote", "ON");
        dev.set_property("temperature_adjustment", "-5");
        dev.set_property("cooktop_beeper_volume", "Low");
        let sent: Vec<String> = thinq.outbox().iter().map(|b| hex_encode(b)).collect();
        assert_eq!(
            sent,
            vec![
                "AA25F043210E8080808080808080808080FF800180000080808080800000000000000167BB",
                "AA25F043210E8080808000FB8080808080FF80808000008080808080000000000000807EBB",
                "AA25F043210E8080808080808080808080FF0180800000808080808000000000000080E4BB",
            ]
        );
    }
}
