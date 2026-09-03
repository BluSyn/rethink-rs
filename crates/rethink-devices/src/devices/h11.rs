//! H11 dishwasher (AABB) — LG DUE2BG / ThinQ modelId H11.
//!
//! Port of upstream PR #139. Status frames are 0x32/0xEC with dual half-payload;
//! current state is the second half, marker 0x00 0x18 then 24-byte data.
//! Full remote control: settings via F0 26 [rinse][salt][opt…], start_course via F0 26 10.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Arc;

fn dishwasher_states() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "INITIAL"),
        (2, "RUNNING"),
        (3, "PAUSE"),
        (4, "STANDBY"),
    ])
}
fn courses() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "OFF"),
        (0x01, "AUTO"),
        (0x12, "ONE_HOUR"),
        (0x05, "NORMAL/ECO"),
        (0x02, "HEAVY/INTENSIVE"),
        (0x10, "SILENT_NIGHT"),
        (0x08, "EXPRESS"),
        (0x0b, "DOWNLOAD_CYCLE"),
        (0x09, "MACHINE_CLEAN"),
    ])
}
fn smart_courses() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x05, "GREASY_TABLEWARE"),
        (0x0d, "MACHINE_CLEAN"),
        (0x0f, "PLASTIC_WASH"),
    ])
}

fn course_code(name: &str) -> Option<u8> {
    match name {
        "AUTO" => Some(0x01),
        "ONE_HOUR" => Some(0x12),
        "NORMAL/ECO" => Some(0x05),
        "HEAVY/INTENSIVE" => Some(0x02),
        "SILENT_NIGHT" => Some(0x10),
        "EXPRESS" => Some(0x08),
        "DOWNLOAD_CYCLE" => Some(0x0b),
        "MACHINE_CLEAN" => Some(0x09),
        _ => None,
    }
}

struct CachedSettings {
    rinse_level: u8,
    salt_level: u8,
    buzzer_level: String,
    end_alarm_sound: bool,
    clean_reminder: bool,
    auto_dry: bool,
    brightness: bool,
    remote_start_mode: String,
}

struct Targets {
    course: u8,
    delay: u8,
    high_temp: bool,
    extra_dry: bool,
    extra_rinse: u8,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    settings: Mutex<CachedSettings>,
    targets: Mutex<Targets>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            settings: Mutex::new(CachedSettings {
                rinse_level: 0,
                salt_level: 0,
                buzzer_level: "LOW".into(),
                end_alarm_sound: false,
                clean_reminder: false,
                auto_dry: false,
                brightness: false,
                remote_start_mode: "ONE_TIME".into(),
            }),
            targets: Mutex::new(Targets {
                course: 0x01,
                delay: 0,
                high_temp: false,
                extra_dry: false,
                extra_rinse: 0,
            }),
        });

        let mut base = default_config(&meta, Some(json!({"name": "LG Dishwasher"})));
        let comps = [
            ("power", json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"Power","icon":"mdi:power"})),
            ("state", json!({"platform":"sensor","icon":"mdi:washing-machine","unique_id":"$deviceid-state","state_topic":"$this/state","name":"State"})),
            ("course", json!({"platform":"sensor","icon":"mdi:dishwasher","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course"})),
            ("remain_time", json!({"platform":"sensor","icon":"mdi:timer-sand","unique_id":"$deviceid-remain_time","state_topic":"$this/remain_time","name":"Remain Time","unit_of_measurement":"min"})),
            ("course_time", json!({"platform":"sensor","icon":"mdi:timer","unique_id":"$deviceid-course_time","state_topic":"$this/course_time","name":"Course Time","unit_of_measurement":"min"})),
            ("door", json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door","payload_on":"OPEN","payload_off":"CLOSE"})),
            ("high_temp_dry", json!({"platform":"binary_sensor","icon":"mdi:weather-sunny","unique_id":"$deviceid-high_temp_dry","state_topic":"$this/high_temp_dry","name":"High Temp Dry","payload_on":"ON","payload_off":"OFF"})),
            ("sterilize", json!({"platform":"binary_sensor","icon":"mdi:thermometer-high","unique_id":"$deviceid-sterilize","state_topic":"$this/sterilize","name":"Sterilize","payload_on":"ON","payload_off":"OFF"})),
            ("rinse_level", json!({"platform":"number","icon":"mdi:water-plus","unique_id":"$deviceid-rinse_level","state_topic":"$this/rinse_level","command_topic":"$this/rinse_level/set","name":"Rinse Level","min":0,"max":4,"step":1})),
            ("salt_level", json!({"platform":"number","icon":"mdi:shaker","unique_id":"$deviceid-salt_level","state_topic":"$this/salt_level","command_topic":"$this/salt_level/set","name":"Salt Level","min":0,"max":4,"step":1})),
            ("buzzer_level", json!({"platform":"select","icon":"mdi:volume-high","unique_id":"$deviceid-buzzer_level","state_topic":"$this/buzzer_level","command_topic":"$this/buzzer_level/set","name":"Buzzer Level","options":["OFF","LOW","HIGH"]})),
            ("end_alarm_sound", json!({"platform":"switch","icon":"mdi:music-note","unique_id":"$deviceid-end_alarm_sound","state_topic":"$this/end_alarm_sound","command_topic":"$this/end_alarm_sound/set","name":"End Alarm Sound","payload_on":"ON","payload_off":"OFF"})),
            ("clean_reminder", json!({"platform":"switch","icon":"mdi:lightbulb","unique_id":"$deviceid-clean_reminder","state_topic":"$this/clean_reminder","command_topic":"$this/clean_reminder/set","name":"Clean Reminder Light","payload_on":"ON","payload_off":"OFF"})),
            ("auto_dry", json!({"platform":"switch","icon":"mdi:weather-sunny","unique_id":"$deviceid-auto_dry","state_topic":"$this/auto_dry","command_topic":"$this/auto_dry/set","name":"Auto Dry","payload_on":"ON","payload_off":"OFF"})),
            ("brightness", json!({"platform":"switch","icon":"mdi:brightness-6","unique_id":"$deviceid-brightness","state_topic":"$this/brightness","command_topic":"$this/brightness/set","name":"Time Indicator Brightness","payload_on":"HIGH","payload_off":"LOW"})),
            ("remote_start_mode", json!({"platform":"select","icon":"mdi:remote","unique_id":"$deviceid-remote_start_mode","state_topic":"$this/remote_start_mode","command_topic":"$this/remote_start_mode/set","name":"Remote Start Mode","options":["PERMANENT","ONE_TIME","OFF"]})),
            ("delay_start", json!({"platform":"sensor","icon":"mdi:clock-fast","unique_id":"$deviceid-delay_start","state_topic":"$this/delay_start","name":"Delay Start (Hours)","unit_of_measurement":"h"})),
            ("remote_start", json!({"platform":"binary_sensor","icon":"mdi:remote","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote Start","payload_on":"ON","payload_off":"OFF"})),
            ("pause", json!({"platform":"button","icon":"mdi:pause","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","name":"Pause","payload_press":"PRESS"})),
            ("resume", json!({"platform":"button","icon":"mdi:play","unique_id":"$deviceid-resume","command_topic":"$this/resume/set","name":"Resume","payload_press":"PRESS"})),
            ("cancel", json!({"platform":"button","icon":"mdi:stop","unique_id":"$deviceid-cancel","command_topic":"$this/cancel/set","name":"Cancel / Drain Stop","payload_press":"PRESS"})),
            ("target_course", json!({"platform":"select","icon":"mdi:washing-machine","unique_id":"$deviceid-target_course","state_topic":"$this/target_course","command_topic":"$this/target_course/set","name":"Target Course","options":["AUTO","ONE_HOUR","NORMAL/ECO","HEAVY/INTENSIVE","SILENT_NIGHT","EXPRESS","DOWNLOAD_CYCLE","MACHINE_CLEAN"]})),
            ("target_delay", json!({"platform":"number","icon":"mdi:clock-start","unique_id":"$deviceid-target_delay","state_topic":"$this/target_delay","command_topic":"$this/target_delay/set","name":"Delay Start Hour","min":0,"max":12,"step":1})),
            ("target_high_temp", json!({"platform":"switch","icon":"mdi:thermometer-high","unique_id":"$deviceid-target_high_temp","state_topic":"$this/target_high_temp","command_topic":"$this/target_high_temp/set","name":"High Temp","payload_on":"ON","payload_off":"OFF"})),
            ("target_extra_dry", json!({"platform":"switch","icon":"mdi:weather-sunny","unique_id":"$deviceid-target_extra_dry","state_topic":"$this/target_extra_dry","command_topic":"$this/target_extra_dry/set","name":"Extra Dry","payload_on":"ON","payload_off":"OFF"})),
            ("target_extra_rinse", json!({"platform":"select","icon":"mdi:water-plus","unique_id":"$deviceid-target_extra_rinse","state_topic":"$this/target_extra_rinse","command_topic":"$this/target_extra_rinse/set","name":"Extra Rinse","options":["0","1","2","3"]})),
            ("start_course", json!({"platform":"button","icon":"mdi:play-circle","unique_id":"$deviceid-start_course","command_topic":"$this/start_course/set","name":"Start Course","payload_press":"PRESS"})),
        ];
        let mut components = Map::new();
        for (k, v) in comps {
            components.insert(k.into(), v);
        }
        base.components = components.into_iter().collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    /// Build and send F0 26 settings packet from cached values (PR #139 sendSettings).
    fn send_settings(&self) {
        let s = self.settings.lock();
        let mut opt1: u8 = 0x00;
        if s.end_alarm_sound {
            opt1 |= 0x40;
        }
        if s.auto_dry {
            opt1 |= 0x20;
        }
        if s.clean_reminder {
            opt1 |= 0x08;
        }
        if s.buzzer_level == "HIGH" {
            opt1 |= 0x04;
        } else if s.buzzer_level == "LOW" {
            opt1 |= 0x02;
        }

        let opt2: u8 = match s.remote_start_mode.as_str() {
            "OFF" => 0xc0,
            "PERMANENT" => 0x80,
            "ONE_TIME" => 0x40,
            _ => 0x00,
        };

        let mut opt3: u8 = 0x00;
        if s.brightness {
            opt3 |= 0x40;
        }

        let pkt = [
            0xf0,
            0x26,
            s.rinse_level,
            s.salt_level,
            opt1,
            opt2,
            opt3,
            0x00,
            0x00,
            0x00,
        ];
        self.core.send(&pkt);
    }

    /// F0 26 10 start course with target options (PR #139 start_course).
    fn send_start_course(&self) {
        let t = self.targets.lock();
        let mut opt3: u8 = 0;
        if t.high_temp {
            opt3 |= 0x08;
        }
        if t.extra_dry {
            opt3 |= 0x04;
        }

        let mut opt4: u8 = 0;
        match t.extra_rinse {
            1 => opt4 |= 0x08,
            2 => opt4 |= 0x10,
            3 => opt4 |= 0x18,
            _ => {}
        }
        if t.course == 0x0b {
            // Download cycle flag
            opt4 |= 0x40;
        }

        let pkt = [
            0xf0,
            0x26,
            0x10,
            t.course,
            t.delay,
            0x00,
            opt3,
            opt4,
            0x00,
        ];
        self.core.send(&pkt);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 4 || buf[0] != 0x32 || buf[1] != 0xec {
            return;
        }
        let payload_len = buf.len() - 2;
        let half_len = payload_len / 2;
        if half_len <= 10 {
            return;
        }
        let cur = &buf[2 + half_len..];
        self.process_status(cur);
    }

    fn process_status(&self, cur: &[u8]) {
        if cur.len() < 26 || cur[0] != 0x00 || cur[1] != 0x18 {
            return;
        }
        let data = &cur[2..26];
        let state_code = data[0];
        let process_code = data[1];
        let state_str = dishwasher_states()
            .get(&state_code)
            .copied()
            .unwrap_or("UNKNOWN");
        let is_power_off = state_code == 4 || process_code == 0x63;

        self.core.publish_property("state", state_str.into());
        self.core.publish_on_off("power", !is_power_off);

        let base_course = data[5];
        let smart_course = data[20];
        let course_str = if smart_course != 0 {
            smart_courses()
                .get(&smart_course)
                .copied()
                .unwrap_or("DOWNLOAD_COURSE")
        } else {
            courses()
                .get(&base_course)
                .copied()
                .unwrap_or("UNKNOWN")
        };
        self.core.publish_property("course", course_str.into());
        self.core.publish_property(
            "course_time",
            (data[3] as i64 * 60 + data[4] as i64).into(),
        );
        self.core.publish_property(
            "remain_time",
            (data[7] as i64 * 60 + data[8] as i64).into(),
        );
        self.core
            .publish_property("delay_start", (data[9] as i64).into());
        self.core.publish_property(
            "door",
            if data[11] & 0x02 != 0 {
                "OPEN"
            } else {
                "CLOSE"
            }
            .into(),
        );
        self.core.publish_on_off("high_temp_dry", data[12] & 0x04 != 0);
        self.core.publish_on_off("sterilize", data[12] & 0x08 != 0);
        self.core.publish_on_off("remote_start", data[15] & 0x02 != 0);

        let mut s = self.settings.lock();
        s.rinse_level = data[13];
        s.salt_level = data[14];
        self.core
            .publish_property("rinse_level", (s.rinse_level as i64).into());
        self.core
            .publish_property("salt_level", (s.salt_level as i64).into());
        s.auto_dry = data[11] & 0x10 != 0;
        s.clean_reminder = data[11] & 0x40 != 0;
        self.core.publish_on_off("auto_dry", s.auto_dry);
        self.core.publish_on_off("clean_reminder", s.clean_reminder);
        s.buzzer_level = if data[15] & 0x80 != 0 {
            "HIGH".into()
        } else if data[15] & 0x40 != 0 {
            "LOW".into()
        } else {
            "OFF".into()
        };
        self.core
            .publish_property("buzzer_level", s.buzzer_level.clone().into());
        let remote_bits = data[16] & 0xc0;
        s.remote_start_mode = match remote_bits {
            0xc0 => "OFF".into(),
            0x80 => "PERMANENT".into(),
            0x40 => "ONE_TIME".into(),
            _ => s.remote_start_mode.clone(),
        };
        self.core
            .publish_property("remote_start_mode", s.remote_start_mode.clone().into());
        s.end_alarm_sound = data[16] & 0x04 != 0;
        self.core.publish_on_off("end_alarm_sound", s.end_alarm_sound);
        s.brightness = data[19] & 0x40 != 0;
        self.core.publish_property(
            "brightness",
            if s.brightness { "HIGH" } else { "LOW" }.into(),
        );
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "power" => {
                if mqtt_value == "ON" {
                    self.core.send(&hex_decode("F02616"));
                } else if mqtt_value == "OFF" {
                    self.core.send(&hex_decode("F02612"));
                }
            }
            "pause" => self.core.send(&hex_decode("F02613")),
            "resume" => self.core.send(&hex_decode("F02614")),
            "cancel" => self.core.send(&hex_decode("F02611")),
            "target_course" => {
                if let Some(c) = course_code(mqtt_value) {
                    self.targets.lock().course = c;
                    self.core
                        .publish_property("target_course", mqtt_value.into());
                }
            }
            "target_delay" => {
                if let Ok(v) = mqtt_value.parse::<u8>() {
                    self.targets.lock().delay = v;
                    self.core
                        .publish_property("target_delay", (v as i64).into());
                }
            }
            "target_high_temp" => {
                self.targets.lock().high_temp = mqtt_value == "ON";
                self.core
                    .publish_property("target_high_temp", mqtt_value.into());
            }
            "target_extra_dry" => {
                self.targets.lock().extra_dry = mqtt_value == "ON";
                self.core
                    .publish_property("target_extra_dry", mqtt_value.into());
            }
            "target_extra_rinse" => {
                if let Ok(v) = mqtt_value.parse::<u8>() {
                    self.targets.lock().extra_rinse = v;
                    self.core
                        .publish_property("target_extra_rinse", mqtt_value.into());
                }
            }
            "rinse_level" => {
                if let Ok(v) = mqtt_value.parse::<u8>() {
                    self.settings.lock().rinse_level = v;
                    self.send_settings();
                }
            }
            "salt_level" => {
                if let Ok(v) = mqtt_value.parse::<u8>() {
                    self.settings.lock().salt_level = v;
                    self.send_settings();
                }
            }
            "buzzer_level" => {
                self.settings.lock().buzzer_level = mqtt_value.into();
                self.send_settings();
            }
            "end_alarm_sound" => {
                self.settings.lock().end_alarm_sound = mqtt_value == "ON";
                self.send_settings();
            }
            "clean_reminder" => {
                self.settings.lock().clean_reminder = mqtt_value == "ON";
                self.send_settings();
            }
            "auto_dry" => {
                self.settings.lock().auto_dry = mqtt_value == "ON";
                self.send_settings();
            }
            "brightness" => {
                self.settings.lock().brightness = mqtt_value == "HIGH";
                self.send_settings();
            }
            "remote_start_mode" => {
                self.settings.lock().remote_start_mode = mqtt_value.into();
                self.send_settings();
            }
            "start_course" => self.send_start_course(),
            _ => {}
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        // Family status request + default target properties (PR #139 start()).
        crate::devices::washer_ctrl::request_status(&self.core);
        self.core.publish_property("target_course", "AUTO".into());
        self.core.publish_property("target_delay", 0i64.into());
        self.core.publish_property("target_high_temp", "OFF".into());
        self.core.publish_property("target_extra_dry", "OFF".into());
        self.core.publish_property("target_extra_rinse", "0".into());
    }
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device, Metadata, Thinq2Device};
    use crate::device_trait::DeviceHandler;

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata {
        Metadata::new("H11", "DUE2BG.AKOR", "1.0")
    }
    fn make() -> (
        std::sync::Arc<MockHaConnection>,
        std::sync::Arc<MockThinq2Device>,
        std::sync::Arc<Device>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    /// Match AABB-wrapped outbox packets by inner payload bytes after AA/len.
    fn has_inner(sent: &[Vec<u8>], inner: &[u8]) -> bool {
        sent.iter()
            .any(|p| p.len() >= 4 + inner.len() && p[2..2 + inner.len()] == *inner)
    }

    /// Build a minimal 0x32/0xEC dual-half frame with current status in the second half.
    fn build_ec_status(data: &[u8; 24]) -> Vec<u8> {
        let mut half = vec![0x00, 0x18];
        half.extend_from_slice(data);
        while half.len() < 26 {
            half.push(0);
        }
        let old = half.clone();
        let mut inner = vec![0x32, 0xec];
        inner.extend(old);
        inner.extend(half);
        let mut pkt = vec![0xaa, (inner.len() + 4) as u8];
        pkt.extend(inner);
        pkt.push(0);
        pkt.push(0xbb);
        pkt
    }

    #[test]
    fn config_has_full_control_surface() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;

        // Status / sensors
        for k in [
            "power",
            "state",
            "course",
            "remain_time",
            "door",
            "remote_start",
        ] {
            assert!(comps.contains_key(k), "missing {k}");
        }

        // Writable settings (must have command_topic — not demoted sensors)
        for k in [
            "rinse_level",
            "salt_level",
            "buzzer_level",
            "end_alarm_sound",
            "clean_reminder",
            "auto_dry",
            "brightness",
            "remote_start_mode",
        ] {
            assert!(comps.contains_key(k), "missing settings entity {k}");
            assert!(
                comps[k].get("command_topic").is_some(),
                "{k} must have command_topic for HA control"
            );
        }
        assert_eq!(comps["rinse_level"]["platform"], "number");
        assert_eq!(comps["buzzer_level"]["platform"], "select");
        assert_eq!(comps["auto_dry"]["platform"], "switch");
        assert_eq!(comps["brightness"]["platform"], "switch");

        // Target + start_course (PR #139 control surface)
        for k in [
            "target_course",
            "target_delay",
            "target_high_temp",
            "target_extra_dry",
            "target_extra_rinse",
            "start_course",
        ] {
            assert!(comps.contains_key(k), "missing control entity {k}");
            assert!(
                comps[k].get("command_topic").is_some(),
                "{k} must have command_topic"
            );
        }
        assert_eq!(comps["start_course"]["platform"], "button");
        assert_eq!(comps["target_course"]["platform"], "select");
        assert_eq!(comps["target_delay"]["platform"], "number");
    }

    #[test]
    fn start_publishes_default_targets() {
        let (ha, _, dev) = make();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(prop(&ha, "target_course").as_deref(), Some("AUTO"));
        assert_eq!(prop(&ha, "target_delay").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "target_high_temp").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "target_extra_dry").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "target_extra_rinse").as_deref(), Some("0"));
    }

    #[test]
    fn standby_power_off() {
        let (ha, thinq, _) = make();
        let mut data = [0u8; 24];
        data[0] = 4; // STANDBY
        data[5] = 0x01; // AUTO
        thinq.emit_data(&build_ec_status(&data));
        assert_eq!(prop(&ha, "state").as_deref(), Some("STANDBY"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("AUTO"));
    }

    #[test]
    fn running_with_times_and_door() {
        let (ha, thinq, _) = make();
        let mut data = [0u8; 24];
        data[0] = 2; // RUNNING
        data[1] = 0x01;
        data[3] = 1;
        data[4] = 30;
        data[5] = 0x05; // NORMAL/ECO
        data[7] = 0;
        data[8] = 45;
        data[11] = 0x02; // door open
        data[12] = 0x0c; // extra dry + sterilize
        data[15] = 0x02; // remote start
        thinq.emit_data(&build_ec_status(&data));
        assert_eq!(prop(&ha, "state").as_deref(), Some("RUNNING"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("NORMAL/ECO"));
        assert_eq!(prop(&ha, "course_time").as_deref(), Some("90"));
        assert_eq!(prop(&ha, "remain_time").as_deref(), Some("45"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OPEN"));
        assert_eq!(prop(&ha, "high_temp_dry").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "sterilize").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
    }

    #[test]
    fn smart_course_overrides_base() {
        let (ha, thinq, _) = make();
        let mut data = [0u8; 24];
        data[0] = 1;
        data[5] = 0x01;
        data[20] = 0x05; // GREASY_TABLEWARE
        thinq.emit_data(&build_ec_status(&data));
        assert_eq!(prop(&ha, "course").as_deref(), Some("GREASY_TABLEWARE"));
    }

    #[test]
    fn power_pause_commands() {
        let (_ha, thinq, dev) = make();
        dev.set_property("power", "ON");
        dev.set_property("power", "OFF");
        dev.set_property("pause", "PRESS");
        let sent = thinq.outbox();
        assert!(has_inner(&sent, &hex_decode("F02616")), "wake: {sent:?}");
        assert!(has_inner(&sent, &hex_decode("F02612")), "off: {sent:?}");
        assert!(has_inner(&sent, &hex_decode("F02613")), "pause: {sent:?}");
    }

    /// start_course builds F0 26 10 [course][delay][0][opt3][opt4][0] from targets.
    #[test]
    fn start_course_sends_f02610_payload() {
        let (ha, thinq, dev) = make();
        thinq.reset_recorder();

        // Configure targets then press start_course
        dev.set_property("target_course", "HEAVY/INTENSIVE"); // 0x02
        dev.set_property("target_delay", "3");
        dev.set_property("target_high_temp", "ON"); // opt3 |= 0x08
        dev.set_property("target_extra_dry", "ON"); // opt3 |= 0x04
        dev.set_property("target_extra_rinse", "2"); // opt4 |= 0x10
        assert_eq!(prop(&ha, "target_course").as_deref(), Some("HEAVY/INTENSIVE"));
        assert_eq!(prop(&ha, "target_delay").as_deref(), Some("3"));
        assert_eq!(prop(&ha, "target_high_temp").as_deref(), Some("ON"));

        thinq.reset_recorder();
        dev.set_property("start_course", "PRESS");

        // f0 26 10 course=0x02 delay=3 opt2=0 opt3=0x0c opt4=0x10 opt5=0
        let expected = [0xf0, 0x26, 0x10, 0x02, 0x03, 0x00, 0x0c, 0x10, 0x00];
        let sent = thinq.outbox();
        assert!(
            has_inner(&sent, &expected),
            "start_course payload missing, outbox={sent:?}"
        );

        // Download cycle sets opt4 bit 0x40
        thinq.reset_recorder();
        dev.set_property("target_course", "DOWNLOAD_CYCLE");
        dev.set_property("target_high_temp", "OFF");
        dev.set_property("target_extra_dry", "OFF");
        dev.set_property("target_extra_rinse", "0");
        dev.set_property("target_delay", "0");
        thinq.reset_recorder();
        dev.set_property("start_course", "PRESS");
        let expected_dl = [0xf0, 0x26, 0x10, 0x0b, 0x00, 0x00, 0x00, 0x40, 0x00];
        assert!(
            has_inner(&thinq.outbox(), &expected_dl),
            "download start missing: {:?}",
            thinq.outbox()
        );
    }

    /// Settings write goes through send_settings → F0 26 [rinse][salt][opt1][opt2][opt3]…
    #[test]
    fn settings_write_sends_f026_packet() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();

        // Seed from a status frame so cache matches device (rinse/salt/buzzer bits)
        let mut data = [0u8; 24];
        data[0] = 1; // INITIAL
        data[13] = 1; // rinse
        data[14] = 2; // salt
        data[15] = 0x40; // LOW buzzer
        data[16] = 0x40; // ONE_TIME remote mode
        thinq.emit_data(&build_ec_status(&data));
        thinq.reset_recorder();

        // Change rinse_level → must call sendSettings with new rinse
        dev.set_property("rinse_level", "3");
        // opt1: LOW buzzer only → 0x02; opt2 ONE_TIME → 0x40; opt3 no brightness → 0
        let expected = [0xf0, 0x26, 0x03, 0x02, 0x02, 0x40, 0x00, 0x00, 0x00, 0x00];
        let sent = thinq.outbox();
        assert!(
            has_inner(&sent, &expected),
            "rinse_level settings write missing: {sent:?}"
        );

        // auto_dry ON adds opt1 bit 0x20
        thinq.reset_recorder();
        dev.set_property("auto_dry", "ON");
        let expected_ad = [0xf0, 0x26, 0x03, 0x02, 0x22, 0x40, 0x00, 0x00, 0x00, 0x00];
        assert!(
            has_inner(&thinq.outbox(), &expected_ad),
            "auto_dry settings: {:?}",
            thinq.outbox()
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("H11").is_some());
    }
}
