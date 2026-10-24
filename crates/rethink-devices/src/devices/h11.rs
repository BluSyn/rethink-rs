//! LG H11 dishwasher (modelId `H11`).
//!
//! Local parser from upstream `c3b6f42`. AABB framing is removed before
//! `process_aabb`. Status bodies are `32 EC` (94 bytes, previous + current
//! 46-byte records; only the current record is applied) or `32 EB` (48 bytes,
//! one record). Energy is `32 3E` (7 bytes). Controls are `F0 26`.
//!
//! `auto_ack` stays off: this handler acks itself only if a future device opts in.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const STATUS_RECORD_LEN: usize = 46;

const STATUS_OPTIONS: &[&str] = &["Off", "Ready", "Running", "Pause", "Standby", "End"];
const PROCESS_OPTIONS: &[&str] = &[
    "Idle",
    "Delayed Start",
    "Washing",
    "Rinsing",
    "Drying",
    "End",
    "Night Dry",
    "Cancel",
];
const COURSE_OPTIONS: &[&str] = &[
    "Off",
    "Auto",
    "One hour",
    "Normal/Eco",
    "Heavy/Intensive",
    "Silent night",
    "Express",
    "Download cycle",
    "Machine clean",
    "Pots and pans",
    "Glassware and wine glasses",
    "Grilled meat",
    "Greasy tableware",
    "Dishes with baked-on food",
    "Fish dishes",
    "Delicate",
    "Rinse only",
    "Plastic wash",
];
const TARGET_COURSES: &[&str] = &[
    "Auto",
    "One hour",
    "Normal/Eco",
    "Heavy/Intensive",
    "Silent night",
    "Express",
    "Download cycle",
    "Machine clean",
];
const BUZZER_OPTIONS: &[&str] = &["Off", "Low", "High"];
const REMOTE_OPTIONS: &[&str] = &["Off", "Permanent", "One-time"];

struct Settings {
    rinse: Option<u8>,
    salt: Option<u8>,
    buzzer: Option<u8>,
    end_alarm: Option<u8>,
    clean_reminder: Option<u8>,
    auto_dry: Option<u8>,
    brightness: Option<u8>,
    remote: Option<u8>,
}

impl Settings {
    fn empty() -> Self {
        Self {
            rinse: None,
            salt: None,
            buzzer: None,
            end_alarm: None,
            clean_reminder: None,
            auto_dry: None,
            brightness: None,
            remote: None,
        }
    }
}

struct Targets {
    course: u8,
    delay: u8,
    extra_rinse: u8,
    high_temp: u8,
    extra_dry: u8,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    settings: Mutex<Settings>,
    targets: Mutex<Targets>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            settings: Mutex::new(Settings::empty()),
            targets: Mutex::new(Targets {
                course: 0x01,
                delay: 0,
                extra_rinse: 0,
                high_temp: 0,
                extra_dry: 0,
            }),
        });
        core.set_config(discovery(&meta));

        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn reject(&self, prop: &str, value: &str) {
        tracing::debug!(prop, value, "H11 ignoring invalid value");
    }

    fn publish_enum(&self, prop: &str, value: Option<&'static str>) {
        self.core
            .publish_property(prop, value.unwrap_or("None").into());
    }

    fn send_settings(&self) {
        let pkt = {
            let s = self.settings.lock();
            let (
                Some(rinse),
                Some(salt),
                Some(buzzer),
                Some(end_alarm),
                Some(clean),
                Some(auto_dry),
                Some(brightness),
                Some(remote),
            ) = (
                s.rinse,
                s.salt,
                s.buzzer,
                s.end_alarm,
                s.clean_reminder,
                s.auto_dry,
                s.brightness,
                s.remote,
            )
            else {
                tracing::debug!("H11 ignoring settings command, cached settings are incomplete");
                return;
            };
            // Opt1: end alarm 0x40, auto dry 0x20, clean reminder 0x08, buzzer 0x02/0x04.
            // Opt2: remote start mode. Opt3: brightness 0x40.
            [
                0xf0,
                0x26,
                rinse,
                salt,
                (end_alarm << 6) | (auto_dry << 5) | (clean << 3) | (buzzer << 1),
                remote << 6,
                brightness << 6,
                0x00,
                0x00,
                0x00,
            ]
        };
        self.core.send(&pkt);
    }

    fn send_start(&self) {
        let t = self.targets.lock();
        // Opt3: high temp 0x08, extra dry 0x04.
        // Opt4: extra rinse 0x08/0x10/0x18, plus 0x40 for Download cycle.
        let mut opt4 = t.extra_rinse << 3;
        if t.course == 0x0b {
            opt4 |= 0x40;
        }
        let pkt = [
            0xf0,
            0x26,
            0x10,
            t.course,
            t.delay,
            0x00,
            (t.high_temp << 3) | (t.extra_dry << 2),
            opt4,
            0x00,
        ];
        drop(t);
        self.core.send(&pkt);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "power" => match mqtt_value {
                "ON" => self.core.send(&[0xf0, 0x26, 0x16]),
                "OFF" => self.core.send(&[0xf0, 0x26, 0x12]),
                _ => self.reject(prop, mqtt_value),
            },
            "pause" => {
                if mqtt_value == "PRESS" {
                    self.core.send(&[0xf0, 0x26, 0x13]);
                } else {
                    self.reject(prop, mqtt_value);
                }
            }
            "resume" => {
                if mqtt_value == "PRESS" {
                    self.core.send(&[0xf0, 0x26, 0x14]);
                } else {
                    self.reject(prop, mqtt_value);
                }
            }
            "cancel" => {
                if mqtt_value == "PRESS" {
                    self.core.send(&[0xf0, 0x26, 0x11]);
                } else {
                    self.reject(prop, mqtt_value);
                }
            }
            "target_course" => match course_code(mqtt_value) {
                Some(code) => {
                    self.targets.lock().course = code;
                    self.core
                        .publish_property("target_course", mqtt_value.into());
                }
                None => self.reject(prop, mqtt_value),
            },
            "target_delay" => match parse_integer_in_range(mqtt_value, 0, 12) {
                Some(value) => {
                    self.targets.lock().delay = value;
                    self.core
                        .publish_property("target_delay", i64::from(value).into());
                }
                None => self.reject(prop, mqtt_value),
            },
            "target_high_temp" => match switch_code(mqtt_value) {
                Some(code) => {
                    self.targets.lock().high_temp = code;
                    self.core
                        .publish_property("target_high_temp", mqtt_value.into());
                }
                None => self.reject(prop, mqtt_value),
            },
            "target_extra_dry" => match switch_code(mqtt_value) {
                Some(code) => {
                    self.targets.lock().extra_dry = code;
                    self.core
                        .publish_property("target_extra_dry", mqtt_value.into());
                }
                None => self.reject(prop, mqtt_value),
            },
            "target_extra_rinse" => match parse_integer_in_range(mqtt_value, 0, 3) {
                Some(value) => {
                    self.targets.lock().extra_rinse = value;
                    self.core
                        .publish_property("target_extra_rinse", i64::from(value).into());
                }
                None => self.reject(prop, mqtt_value),
            },
            "rinse_level" => match parse_integer_in_range(mqtt_value, 0, 4) {
                Some(value) => {
                    self.settings.lock().rinse = Some(value);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "salt_level" => match parse_integer_in_range(mqtt_value, 0, 4) {
                Some(value) => {
                    self.settings.lock().salt = Some(value);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "buzzer_level" => match buzzer_code(mqtt_value) {
                Some(code) => {
                    self.settings.lock().buzzer = Some(code);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "end_of_cycle_tone" => match switch_code(mqtt_value) {
                Some(code) => {
                    self.settings.lock().end_alarm = Some(code);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "clean_reminder" => match switch_code(mqtt_value) {
                Some(code) => {
                    self.settings.lock().clean_reminder = Some(code);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "auto_dry" => match switch_code(mqtt_value) {
                Some(code) => {
                    self.settings.lock().auto_dry = Some(code);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "brightness" => match brightness_code(mqtt_value) {
                Some(code) => {
                    self.settings.lock().brightness = Some(code);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "remote_start_mode" => match remote_mode_code(mqtt_value) {
                Some(code) => {
                    self.settings.lock().remote = Some(code);
                    self.send_settings();
                }
                None => self.reject(prop, mqtt_value),
            },
            "start_course" => {
                if mqtt_value == "PRESS" {
                    self.send_start();
                } else {
                    self.reject(prop, mqtt_value);
                }
            }
            _ => {}
        }
    }

    fn process_aabb(&self, buf: &[u8]) {
        if process_common_status(buf, 0x32, STATUS_RECORD_LEN).is_some_and(|rec| {
            self.process_status(rec);
            true
        }) {
            return;
        }
        if buf.len() == 7 && buf.first() == Some(&0x32) && buf.get(1) == Some(&0x3e) {
            self.process_statistics(buf);
        }
    }

    fn process_statistics(&self, buf: &[u8]) {
        if buf.len() != 7 {
            return;
        }
        // 32 3e [delta Wh] [accumulated Wh] [seq]. A repeated sequence still
        // carries a new total, so the sequence byte is not a dedupe key.
        let energy = u16::from_be_bytes([buf[4], buf[5]]);
        self.core
            .publish_property("energy", i64::from(energy).into());
    }

    fn process_status(&self, record: &[u8]) {
        // Length byte only. The flag nibble is not part of the shared layout.
        if record.len() < 26 || record[1] != 0x18 {
            return;
        }
        let data = &record[2..26];
        let state = state_name(data[0]);
        let process = process_name(data[1]);
        self.publish_enum("status", state);
        self.publish_enum("process", process);
        // Ready and Running/Cancel stay powered. Only Standby is off.
        self.core.publish_property(
            "power",
            if state == Some("Standby") {
                "OFF"
            } else {
                "ON"
            }
            .into(),
        );

        let course = if data[20] != 0 {
            download_course_name(data[20])
        } else {
            course_name(data[5])
        };
        self.publish_enum("course", course);
        self.core.publish_property(
            "initial_time",
            (i64::from(data[3]) * 60 + i64::from(data[4])).into(),
        );
        self.core.publish_property(
            "remaining_time",
            (i64::from(data[7]) * 60 + i64::from(data[8])).into(),
        );
        self.core.publish_property(
            "delay_start_time",
            (i64::from(data[9]) * 60 + i64::from(data[10])).into(),
        );
        self.core.publish_property(
            "door",
            if data[11] & 0x02 != 0 { "ON" } else { "OFF" }
            .into(),
        );
        self.core.publish_property(
            "extra_dry",
            if data[12] & 0x04 != 0 { "ON" } else { "OFF" }.into(),
        );
        self.core.publish_property(
            "high_temp",
            if data[12] & 0x08 != 0 { "ON" } else { "OFF" }.into(),
        );
        self.core.publish_property(
            "remote_start",
            if data[15] & 0x02 != 0 { "ON" } else { "OFF" }.into(),
        );

        let rinse = (data[13] <= 4).then_some(data[13]);
        let salt = (data[14] <= 4).then_some(data[14]);
        let auto_dry = (data[11] >> 4) & 1;
        let clean = (data[11] >> 6) & 1;
        let buzzer_bits = (data[15] & 0xc0) >> 6;
        let buzzer = buzzer_name(buzzer_bits).map(|_| buzzer_bits);
        let remote_bits = (data[16] & 0xc0) >> 6;
        let remote = remote_mode_name(remote_bits).map(|_| remote_bits);
        let end_alarm = (data[16] >> 2) & 1;
        let brightness = (data[19] >> 6) & 1;
        {
            let mut cached = self.settings.lock();
            cached.rinse = rinse;
            cached.salt = salt;
            cached.auto_dry = Some(auto_dry);
            cached.clean_reminder = Some(clean);
            // An unlisted code clears the cache so a later settings write cannot
            // resend a level the appliance never reported.
            cached.buzzer = buzzer;
            cached.remote = remote;
            cached.end_alarm = Some(end_alarm);
            cached.brightness = Some(brightness);
        }
        if let Some(level) = rinse {
            self.core
                .publish_property("rinse_level", i64::from(level).into());
        }
        if let Some(level) = salt {
            self.core
                .publish_property("salt_level", i64::from(level).into());
        }
        self.core
            .publish_property("auto_dry", on_off(auto_dry).into());
        self.core
            .publish_property("clean_reminder", on_off(clean).into());
        self.publish_enum("buzzer_level", buzzer_name(buzzer_bits));
        self.publish_enum("remote_start_mode", remote_mode_name(remote_bits));
        self.core
            .publish_property("end_of_cycle_tone", on_off(end_alarm).into());
        self.core
            .publish_property("brightness", brightness_name(brightness).into());

        let extra_rinse = data[21] >> 4;
        if extra_rinse <= 3 {
            self.core
                .publish_property("extra_rinse", i64::from(extra_rinse).into());
        } else {
            self.core.publish_property("extra_rinse", "None".into());
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }

    fn start(&self) {
        self.core.publish_property("target_course", "Auto".into());
        self.core.publish_property("target_delay", 0i64.into());
        self.core.publish_property("target_high_temp", "OFF".into());
        self.core.publish_property("target_extra_dry", "OFF".into());
        self.core
            .publish_property("target_extra_rinse", 0i64.into());
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

fn discovery(meta: &Metadata) -> rethink_core::DeviceDiscovery {
    let mut base = default_config(meta, Some(json!({"name": "LG Dishwasher"})));
    let mut components = Map::new();
    let add = |components: &mut Map<String, Value>, key: &str, body: Value| {
        components.insert(key.into(), body);
    };
    add(
        &mut components,
        "power",
        json!({
            "platform": "switch",
            "unique_id": "$deviceid-power",
            "state_topic": "$this/power",
            "command_topic": "$this/power/set",
            "name": "Power",
            "icon": "mdi:power"
        }),
    );
    add(
        &mut components,
        "status",
        json!({
            "platform": "sensor",
            "device_class": "enum",
            "icon": "mdi:state-machine",
            "unique_id": "$deviceid-status",
            "state_topic": "$this/status",
            "name": "Status",
            "options": STATUS_OPTIONS
        }),
    );
    add(
        &mut components,
        "process",
        json!({
            "platform": "sensor",
            "device_class": "enum",
            "icon": "mdi:progress-clock",
            "unique_id": "$deviceid-process",
            "state_topic": "$this/process",
            "name": "Process",
            "options": PROCESS_OPTIONS
        }),
    );
    add(
        &mut components,
        "course",
        json!({
            "platform": "sensor",
            "device_class": "enum",
            "icon": "mdi:pin-outline",
            "unique_id": "$deviceid-course",
            "state_topic": "$this/course",
            "name": "Course",
            "options": COURSE_OPTIONS
        }),
    );
    add(
        &mut components,
        "remaining_time",
        json!({
            "platform": "sensor",
            "icon": "mdi:timer-sand",
            "unique_id": "$deviceid-remaining_time",
            "state_topic": "$this/remaining_time",
            "name": "Remaining time",
            "device_class": "duration",
            "unit_of_measurement": "min"
        }),
    );
    add(
        &mut components,
        "initial_time",
        json!({
            "platform": "sensor",
            "icon": "mdi:timer",
            "unique_id": "$deviceid-initial_time",
            "state_topic": "$this/initial_time",
            "name": "Initial cycle duration",
            "device_class": "duration",
            "unit_of_measurement": "min"
        }),
    );
    add(
        &mut components,
        "door",
        json!({
            "platform": "binary_sensor",
            "device_class": "door",
            "unique_id": "$deviceid-door",
            "state_topic": "$this/door",
            "name": "Door"
        }),
    );
    add(
        &mut components,
        "energy",
        json!({
            "platform": "sensor",
            "device_class": "energy",
            "state_class": "total_increasing",
            "unique_id": "$deviceid-energy",
            "state_topic": "$this/energy",
            "name": "Cycle energy consumption",
            "unit_of_measurement": "Wh",
            "icon": "mdi:lightning-bolt"
        }),
    );
    add(
        &mut components,
        "extra_dry",
        json!({
            "platform": "binary_sensor",
            "icon": "mdi:weather-sunny",
            "unique_id": "$deviceid-extra_dry",
            "state_topic": "$this/extra_dry",
            "name": "High heat drying active"
        }),
    );
    add(
        &mut components,
        "high_temp",
        json!({
            "platform": "binary_sensor",
            "icon": "mdi:thermometer-high",
            "unique_id": "$deviceid-high_temp",
            "state_topic": "$this/high_temp",
            "name": "Sanitizing wash active"
        }),
    );
    add(
        &mut components,
        "extra_rinse",
        json!({
            "platform": "sensor",
            "icon": "mdi:water-plus",
            "unique_id": "$deviceid-extra_rinse",
            "state_topic": "$this/extra_rinse",
            "name": "Extra rinse count"
        }),
    );
    add(
        &mut components,
        "rinse_level",
        json!({
            "platform": "number",
            "icon": "mdi:water-plus",
            "unique_id": "$deviceid-rinse_level",
            "state_topic": "$this/rinse_level",
            "command_topic": "$this/rinse_level/set",
            "name": "Rinse aid level",
            "min": 0, "max": 4, "step": 1
        }),
    );
    add(
        &mut components,
        "salt_level",
        json!({
            "platform": "number",
            "icon": "mdi:shaker",
            "unique_id": "$deviceid-salt_level",
            "state_topic": "$this/salt_level",
            "command_topic": "$this/salt_level/set",
            "name": "Salt dispensing level",
            "min": 0, "max": 4, "step": 1
        }),
    );
    add(
        &mut components,
        "buzzer_level",
        json!({
            "platform": "select",
            "icon": "mdi:volume-high",
            "unique_id": "$deviceid-buzzer_level",
            "state_topic": "$this/buzzer_level",
            "command_topic": "$this/buzzer_level/set",
            "name": "Buzzer volume",
            "options": BUZZER_OPTIONS
        }),
    );
    add(
        &mut components,
        "end_of_cycle_tone",
        json!({
            "platform": "switch",
            "icon": "mdi:music-note",
            "unique_id": "$deviceid-end_of_cycle_tone",
            "state_topic": "$this/end_of_cycle_tone",
            "command_topic": "$this/end_of_cycle_tone/set",
            "name": "End of cycle tone"
        }),
    );
    add(
        &mut components,
        "clean_reminder",
        json!({
            "platform": "switch",
            "icon": "mdi:lightbulb",
            "unique_id": "$deviceid-clean_reminder",
            "state_topic": "$this/clean_reminder",
            "command_topic": "$this/clean_reminder/set",
            "name": "Machine clean reminder"
        }),
    );
    add(
        &mut components,
        "auto_dry",
        json!({
            "platform": "switch",
            "icon": "mdi:weather-sunny",
            "unique_id": "$deviceid-auto_dry",
            "state_topic": "$this/auto_dry",
            "command_topic": "$this/auto_dry/set",
            "name": "Auto dry"
        }),
    );
    add(
        &mut components,
        "brightness",
        json!({
            "platform": "switch",
            "icon": "mdi:brightness-6",
            "unique_id": "$deviceid-brightness",
            "state_topic": "$this/brightness",
            "command_topic": "$this/brightness/set",
            "name": "Time indicator brightness",
            "payload_on": "HIGH",
            "payload_off": "LOW"
        }),
    );
    add(
        &mut components,
        "remote_start_mode",
        json!({
            "platform": "select",
            "icon": "mdi:remote",
            "unique_id": "$deviceid-remote_start_mode",
            "state_topic": "$this/remote_start_mode",
            "command_topic": "$this/remote_start_mode/set",
            "name": "Remote start mode",
            "options": REMOTE_OPTIONS
        }),
    );
    add(
        &mut components,
        "delay_start_time",
        json!({
            "platform": "sensor",
            "icon": "mdi:clock-fast",
            "unique_id": "$deviceid-delay_start_time",
            "state_topic": "$this/delay_start_time",
            "name": "Delay start time",
            "device_class": "duration",
            "unit_of_measurement": "min"
        }),
    );
    add(
        &mut components,
        "remote_start",
        json!({
            "platform": "binary_sensor",
            "icon": "mdi:remote",
            "unique_id": "$deviceid-remote_start",
            "state_topic": "$this/remote_start",
            "name": "Remote start enabled"
        }),
    );
    add(
        &mut components,
        "pause",
        json!({
            "platform": "button",
            "icon": "mdi:pause",
            "unique_id": "$deviceid-pause",
            "command_topic": "$this/pause/set",
            "name": "Pause",
            "payload_press": "PRESS"
        }),
    );
    add(
        &mut components,
        "resume",
        json!({
            "platform": "button",
            "icon": "mdi:play",
            "unique_id": "$deviceid-resume",
            "command_topic": "$this/resume/set",
            "name": "Resume",
            "payload_press": "PRESS"
        }),
    );
    add(
        &mut components,
        "cancel",
        json!({
            "platform": "button",
            "icon": "mdi:stop",
            "unique_id": "$deviceid-cancel",
            "command_topic": "$this/cancel/set",
            "name": "Cancel / drain",
            "payload_press": "PRESS"
        }),
    );
    add(
        &mut components,
        "target_course",
        json!({
            "platform": "select",
            "icon": "mdi:washing-machine",
            "unique_id": "$deviceid-target_course",
            "state_topic": "$this/target_course",
            "command_topic": "$this/target_course/set",
            "name": "Target course",
            "options": TARGET_COURSES
        }),
    );
    add(
        &mut components,
        "target_delay",
        json!({
            "platform": "number",
            "icon": "mdi:clock-start",
            "unique_id": "$deviceid-target_delay",
            "state_topic": "$this/target_delay",
            "command_topic": "$this/target_delay/set",
            "name": "Start delay setting",
            "device_class": "duration",
            "unit_of_measurement": "h",
            "min": 0, "max": 12, "step": 1
        }),
    );
    add(
        &mut components,
        "target_high_temp",
        json!({
            "platform": "switch",
            "icon": "mdi:thermometer-high",
            "unique_id": "$deviceid-target_high_temp",
            "state_topic": "$this/target_high_temp",
            "command_topic": "$this/target_high_temp/set",
            "name": "Target sanitizing wash"
        }),
    );
    add(
        &mut components,
        "target_extra_dry",
        json!({
            "platform": "switch",
            "icon": "mdi:weather-sunny",
            "unique_id": "$deviceid-target_extra_dry",
            "state_topic": "$this/target_extra_dry",
            "command_topic": "$this/target_extra_dry/set",
            "name": "Target high heat drying"
        }),
    );
    add(
        &mut components,
        "target_extra_rinse",
        json!({
            "platform": "number",
            "icon": "mdi:water-plus",
            "unique_id": "$deviceid-target_extra_rinse",
            "state_topic": "$this/target_extra_rinse",
            "command_topic": "$this/target_extra_rinse/set",
            "name": "Target extra rinse count",
            "min": 0, "max": 3, "step": 1
        }),
    );
    add(
        &mut components,
        "start_course",
        json!({
            "platform": "button",
            "icon": "mdi:play-circle",
            "unique_id": "$deviceid-start_course",
            "command_topic": "$this/start_course/set",
            "name": "Start course",
            "payload_press": "PRESS"
        }),
    );
    base.components = components.into_iter().collect();
    base
}

fn state_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Off"),
        1 => Some("Ready"),
        2 => Some("Running"),
        3 => Some("Pause"),
        4 => Some("Standby"),
        5 => Some("End"),
        _ => None,
    }
}

fn process_name(code: u8) -> Option<&'static str> {
    match code {
        0x00 => Some("Idle"),
        0x01 => Some("Delayed Start"),
        0x06 => Some("Night Dry"),
        0x02 => Some("Washing"),
        0x03 => Some("Rinsing"),
        0x04 => Some("Drying"),
        0x05 => Some("End"),
        0x63 => Some("Cancel"),
        _ => None,
    }
}

fn course_name(code: u8) -> Option<&'static str> {
    match code {
        0x00 => Some("Off"),
        0x01 => Some("Auto"),
        0x02 => Some("Heavy/Intensive"),
        0x05 => Some("Normal/Eco"),
        0x08 => Some("Express"),
        0x09 => Some("Machine clean"),
        0x0b => Some("Download cycle"),
        0x10 => Some("Silent night"),
        0x12 => Some("One hour"),
        _ => None,
    }
}

fn course_code(name: &str) -> Option<u8> {
    match name {
        "Auto" => Some(0x01),
        "Heavy/Intensive" => Some(0x02),
        "Normal/Eco" => Some(0x05),
        "Express" => Some(0x08),
        "Machine clean" => Some(0x09),
        "Download cycle" => Some(0x0b),
        "Silent night" => Some(0x10),
        "One hour" => Some(0x12),
        _ => None,
    }
}

fn download_course_name(code: u8) -> Option<&'static str> {
    match code {
        0x02 => Some("Pots and pans"),
        0x03 => Some("Glassware and wine glasses"),
        0x04 => Some("Grilled meat"),
        0x05 => Some("Greasy tableware"),
        0x06 => Some("Dishes with baked-on food"),
        0x07 => Some("Fish dishes"),
        0x08 => Some("Delicate"),
        0x0a => Some("Rinse only"),
        0x0d => Some("Machine clean"),
        0x0f => Some("Plastic wash"),
        _ => None,
    }
}

fn buzzer_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("Off"),
        1 => Some("Low"),
        2 => Some("High"),
        _ => None,
    }
}

fn buzzer_code(name: &str) -> Option<u8> {
    match name {
        "Off" => Some(0),
        "Low" => Some(1),
        "High" => Some(2),
        _ => None,
    }
}

fn remote_mode_name(code: u8) -> Option<&'static str> {
    match code {
        3 => Some("Off"),
        2 => Some("Permanent"),
        1 => Some("One-time"),
        _ => None,
    }
}

fn remote_mode_code(name: &str) -> Option<u8> {
    match name {
        "Off" => Some(3),
        "Permanent" => Some(2),
        "One-time" => Some(1),
        _ => None,
    }
}

fn switch_code(name: &str) -> Option<u8> {
    match name {
        "OFF" => Some(0),
        "ON" => Some(1),
        _ => None,
    }
}

fn brightness_code(name: &str) -> Option<u8> {
    match name {
        "LOW" => Some(0),
        "HIGH" => Some(1),
        _ => None,
    }
}

fn brightness_name(code: u8) -> &'static str {
    if code == 0 {
        "LOW"
    } else {
        "HIGH"
    }
}

fn on_off(bit: u8) -> &'static str {
    if bit == 0 {
        "OFF"
    } else {
        "ON"
    }
}

/// `Number(value)` plus `Number.isInteger` in `[min, max]`. Blank input is rejected.
fn parse_integer_in_range(value: &str, min: i64, max: i64) -> Option<u8> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: f64 = trimmed.parse().ok()?;
    if !parsed.is_finite() || parsed.fract() != 0.0 {
        return None;
    }
    let n = parsed as i64;
    if (min..=max).contains(&n) {
        Some(n as u8)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::ha::{DeviceDiscovery, PropertyValue};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};
    use std::sync::Arc;

    const DEVICE_ID: &str = "test-id";

    const EXPRESS_STARTS_RUNNING: &str = "aa6232ec0018010000012e0800012e0000100c00014146640240001000050d050f00020103ce003e0103ce003e0103ce003e0018020200012e0800012e0000100c00014146640240001000050d050f00020103ce003e0103ce003e0103ce003edabb";
    const EXPRESS_STARTS_RINSING: &str = "aa6232ec0018020200003b0800002f00001000000141466402400020000f0d050f00040103ce003e0103ce003e0103ce003e0018020300003b0800002f00001000000141466402400020000f0d050f00040103ce003e0103ce003e0103ce003e9abb";
    const EXPRESS_STARTS_DRYING: &str = "aa6232ec0018020300003b0800000100001000000141466402400020000f0d050f00040103ce003e0103ce003e0103ce003e0018020400003b08000001000010000001414e6402400020000f0d050f00040103ce003e0103ce003e0103ce003e2cbb";
    const EXPRESS_ENDS: &str = "aa6232ec0018020400003b08000001000012000001414e6402400020000f0d050f00040103ce003e0103ce003e0103ce003e0018050500003b08000001000012000001414e6402400020000f0d050f00040103ce003e0103ce003e0103ce003edfbb";
    const EXPRESS_RETURNS_TO_STANDBY: &str = "aa6232ec0018050500003b08000001000012000001414e6402400020000f0d050f00040103ce003e0103ce003e0103ce003e0818040000003b00000001000012000001414c6402400000000f0d050f00040103ce003e0103ce003e0103ce003e33bb";
    const ONE_HOUR_SELECTED: &str = "aa6232ec0018040000011e0000000100001000000141446402400000000f0d050f00040103ce003e0103ce003e0103ce003e0818010000011e1200011e00001000000141466402400020000f0d050f00040103ce003e0103ce003e0103ce003e6cbb";
    const STANDBY_SNAPSHOT: &str = "aa3432eb0018040000012e0000012e0000100000014144640240000000050d050f00020103ce003e0103ce003e0103ce003e5cbb";
    const SETTINGS_BASELINE: &str = "aa6232ec0018010000013a0800013a0000140c010143466402400020000f0d050f00040103ce003e0103ce003e0103ce003e0018010000013a0800013a0000100c000143466402400020000f0d050f00040103ce003e0103ce003e0103ce003ea8bb";
    const MACHINE_CLEAN_RESERVED: &str = "aa6232ec0018020100012209010122003b1001000141466402400d00000d0d050f00000103ce003e0103ce003e0103ce003e0018020100012209010122003a1001000141466402400d00000d0d050f00000103ce003e0103ce003e0103ce003e9cbb";
    const CANCEL_STARTS_DRAINING: &str = "aa6232ec0018020200002f0800002e00001000000143466402400010000d0d050f00000103ce003e0103ce003e0103ce003e0018026300002f0000000100001000000143446402400000000d0d050f00000103ce003e0103ce003e0103ce003ef7bb";
    const CANCEL_STOPS_DRAINING: &str = "aa6232ec0018026300002f0000000100001000000143446402400000000d0d050f00000103ce003e0103ce003e0103ce003e0018010000002f0800002f00001000000143466402400010000d0d050f00000103ce003e0103ce003e0103ce003ef5bb";
    const ENERGY_TOTAL_314: &str = "aa0b323e0092013a02a1bb";
    const ENERGY_TOTAL_346: &str = "aa0b323e00c1015a0216bb";

    fn meta() -> Metadata {
        Metadata::new("H11", "H11", "1")
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?
            .properties
            .get(name)
            .map(|p| p.as_string())
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        (ha, thinq, dev)
    }

    struct RecHa {
        inner: Arc<MockHaConnection>,
        pubs: Mutex<Vec<(String, String)>>,
    }

    impl HaConnection for RecHa {
        fn publish_config(&self, id: &str, config: &DeviceDiscovery) {
            self.inner.publish_config(id, config);
        }
        fn publish_property(&self, id: &str, property: &str, value: PropertyValue) {
            if property != "availability" {
                self.pubs
                    .lock()
                    .push((property.to_string(), value.as_string()));
            }
            self.inner.publish_property(id, property, value);
        }
        fn publish_event(&self, id: &str, topic_suffix: &str, payload: &str) {
            self.inner.publish_event(id, topic_suffix, payload);
        }
        fn is_connected(&self) -> bool {
            true
        }
    }

    fn make_rec() -> (Arc<RecHa>, Arc<MockThinq2Device>, Arc<Device>) {
        let rec = Arc::new(RecHa {
            inner: MockHaConnection::new(),
            pubs: Mutex::new(Vec::new()),
        });
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(rec.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        rec.pubs.lock().clear();
        (rec, thinq, dev)
    }

    fn assert_outbox(thinq: &MockThinq2Device, expected_lower: &[&str]) {
        let got: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        let want: Vec<String> = expected_lower
            .iter()
            .map(|s| s.to_ascii_uppercase())
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn publishes_detailed_state_and_process_across_the_captured_cycle() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert_eq!(
            comps["status"]["options"],
            json!(["Off", "Ready", "Running", "Pause", "Standby", "End"])
        );
        assert_eq!(
            comps["process"]["options"],
            json!([
                "Idle",
                "Delayed Start",
                "Washing",
                "Rinsing",
                "Drying",
                "End",
                "Night Dry",
                "Cancel"
            ])
        );

        thinq.emit_data(&hex_decode(EXPRESS_STARTS_RUNNING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));

        thinq.emit_data(&hex_decode(EXPRESS_STARTS_RINSING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Rinsing"));

        thinq.emit_data(&hex_decode(EXPRESS_STARTS_DRYING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Drying"));

        thinq.emit_data(&hex_decode(EXPRESS_ENDS));
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("End"));

        thinq.emit_data(&hex_decode(EXPRESS_RETURNS_TO_STANDBY));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
    }

    #[test]
    fn updates_state_course_and_time_from_the_current_08_18_record_only() {
        let (rec, thinq, _) = make_rec();
        thinq.emit_data(&hex_decode(ONE_HOUR_SELECTED));
        let ha = &rec.inner;
        assert_eq!(prop(ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(ha, "course").as_deref(), Some("One hour"));
        assert_eq!(prop(ha, "remaining_time").as_deref(), Some("90"));
        assert_eq!(prop(ha, "initial_time").as_deref(), Some("90"));
        assert_eq!(prop(ha, "power").as_deref(), Some("ON"));
        let pubs = rec.pubs.lock();
        assert_eq!(
            pubs.iter()
                .filter(|(p, _)| p == "status")
                .cloned()
                .collect::<Vec<_>>(),
            vec![("status".into(), "Ready".into())]
        );
        assert_eq!(
            pubs.iter()
                .filter(|(p, _)| p == "course")
                .cloned()
                .collect::<Vec<_>>(),
            vec![("course".into(), "One hour".into())]
        );
    }

    #[test]
    fn updates_state_and_course_from_the_real_eb_reconnect_snapshot() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STANDBY_SNAPSHOT));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("106"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
    }

    #[test]
    fn updates_state_from_the_current_00_18_record_only() {
        let (rec, thinq, _) = make_rec();
        thinq.emit_data(&hex_decode(EXPRESS_STARTS_RUNNING));
        let statuses: Vec<String> = rec
            .pubs
            .lock()
            .iter()
            .filter(|(p, _)| p == "status")
            .map(|(_, v)| v.clone())
            .collect();
        assert_eq!(statuses, vec!["Running".to_string()]);
        assert_eq!(prop(&rec.inner, "course").as_deref(), Some("Express"));
        assert_eq!(prop(&rec.inner, "remaining_time").as_deref(), Some("106"));
    }

    #[test]
    fn reports_the_captured_delayed_machine_clean_cycle_in_minutes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(MACHINE_CLEAN_RESERVED));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Delayed Start"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Machine clean"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("94"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("94"));
        assert_eq!(prop(&ha, "delay_start_time").as_deref(), Some("58"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
    }

    #[test]
    fn reports_cancel_draining_as_powered_on_and_returns_to_ready_when_it_stops() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(CANCEL_STARTS_DRAINING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Cancel"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(CANCEL_STOPS_DRAINING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("47"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
    }

    #[test]
    fn does_not_overwrite_staged_remote_start_options_with_current_cycle_status() {
        let (ha, thinq, dev) = make();
        dev.set_property("target_extra_rinse", "2");
        thinq.emit_data(&hex_decode(EXPRESS_STARTS_RUNNING));
        assert_eq!(prop(&ha, "target_extra_rinse").as_deref(), Some("2"));
    }

    #[test]
    fn ec_processes_only_the_fixed_offset_current_record() {
        let (rec, thinq, _) = make_rec();
        thinq.emit_data(&hex_decode(EXPRESS_STARTS_RUNNING));
        let ha = &rec.inner;
        assert_eq!(prop(ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(ha, "course").as_deref(), Some("Express"));
        assert_eq!(prop(ha, "initial_time").as_deref(), Some("106"));
        assert_eq!(prop(ha, "remaining_time").as_deref(), Some("106"));
        assert_eq!(prop(ha, "delay_start_time").as_deref(), Some("0"));
        assert_eq!(prop(ha, "door").as_deref(), Some("OFF"));
        assert_eq!(prop(ha, "extra_dry").as_deref(), Some("ON"));
        assert_eq!(prop(ha, "high_temp").as_deref(), Some("ON"));
        assert_eq!(prop(ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(ha, "rinse_level").as_deref(), Some("0"));
        assert_eq!(prop(ha, "salt_level").as_deref(), Some("1"));
        assert_eq!(prop(ha, "auto_dry").as_deref(), Some("ON"));
        assert_eq!(prop(ha, "clean_reminder").as_deref(), Some("OFF"));
        assert_eq!(prop(ha, "buzzer_level").as_deref(), Some("Low"));
        assert_eq!(prop(ha, "remote_start_mode").as_deref(), Some("One-time"));
        assert_eq!(prop(ha, "end_of_cycle_tone").as_deref(), Some("ON"));
        assert_eq!(prop(ha, "brightness").as_deref(), Some("HIGH"));
        assert_eq!(prop(ha, "extra_rinse").as_deref(), Some("1"));
        assert_eq!(prop(ha, "target_extra_rinse").as_deref(), Some("0"));
        let pubs = rec.pubs.lock();
        assert_eq!(
            pubs.iter()
                .filter(|(p, _)| p == "status")
                .cloned()
                .collect::<Vec<_>>(),
            vec![("status".into(), "Running".into())]
        );
    }

    #[test]
    fn eb_parses_the_real_single_record_reconnect_snapshot() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STANDBY_SNAPSHOT));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn rejects_unobserved_lengths_and_malformed_fixed_record_headers() {
        let (ha, thinq, _) = make();
        let full = hex_decode(EXPRESS_STARTS_RUNNING);
        let mut wrong_length = full[..full.len() - 3].to_vec();
        wrong_length.extend_from_slice(&full[full.len() - 2..]);
        thinq.emit_data(&wrong_length);

        let mut unobserved = vec![0u8; 58];
        unobserved[0] = 0xaa;
        unobserved[1] = 58;
        unobserved[2] = 0x32;
        unobserved[3] = 0xec;
        unobserved[30] = 0x00;
        unobserved[31] = 0x18;
        unobserved[57] = 0xbb;
        thinq.emit_data(&unobserved);

        assert_eq!(prop(&ha, "status"), None);
    }

    #[test]
    fn unrecognised_codes_are_published_as_unavailable() {
        let (ha, thinq, _) = make();
        let mut unknown = hex_decode(EXPRESS_STARTS_RUNNING);
        let data_offset = 2 + 48 + 2;
        unknown[data_offset] = 0x7f;
        unknown[data_offset + 1] = 0x7f;
        unknown[data_offset + 5] = 0x7f;
        unknown[data_offset + 15] |= 0xc0;
        unknown[data_offset + 16] = 0x00;
        unknown[data_offset + 20] = 0x7f;
        unknown[data_offset + 21] = 0x40;
        thinq.emit_data(&unknown);

        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "buzzer_level").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "remote_start_mode").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "target_extra_rinse").as_deref(), Some("0"));
    }

    #[test]
    fn does_not_discard_a_changed_energy_total_when_its_sequence_repeats() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(ENERGY_TOTAL_314));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("314"));
        thinq.emit_data(&hex_decode(ENERGY_TOTAL_346));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("346"));
    }

    #[test]
    fn generates_captured_simple_control_packets() {
        let cases = [
            ("power", "ON", "aa07f0261688bb"),
            ("power", "OFF", "aa07f026128cbb"),
            ("pause", "PRESS", "aa07f026138fbb"),
            ("resume", "PRESS", "aa07f026148ebb"),
            ("cancel", "PRESS", "aa07f026118dbb"),
        ];
        for (property, value, expected) in cases {
            let (_ha, thinq, dev) = make();
            dev.set_property(property, value);
            assert_outbox(&thinq, &[expected]);
        }
    }

    #[test]
    fn preserves_all_other_captured_settings_when_changing_one_setting() {
        let cases = [
            ("rinse_level", "0", "aa0ef0260001624040000000e4bb"),
            ("rinse_level", "3", "aa0ef0260301624040000000e1bb"),
            ("rinse_level", "4", "aa0ef0260401624040000000e0bb"),
            ("salt_level", "0", "aa0ef0260000624040000000e5bb"),
            ("salt_level", "2", "aa0ef0260002624040000000e7bb"),
            ("salt_level", "4", "aa0ef0260004624040000000e1bb"),
            ("buzzer_level", "High", "aa0ef0260001644040000000e6bb"),
            ("buzzer_level", "Off", "aa0ef0260001604040000000fabb"),
            ("end_of_cycle_tone", "OFF", "aa0ef026000122404000000024bb"),
            ("clean_reminder", "ON", "aa0ef02600016a4040000000ecbb"),
            ("auto_dry", "OFF", "aa0ef0260001424040000000c4bb"),
            ("brightness", "LOW", "aa0ef026000162400000000024bb"),
            (
                "remote_start_mode",
                "Permanent",
                "aa0ef0260001628040000000a4bb",
            ),
        ];
        for (property, value, expected) in cases {
            let (_ha, thinq, dev) = make();
            thinq.emit_data(&hex_decode(SETTINGS_BASELINE));
            thinq.reset_recorder();
            dev.set_property(property, value);
            assert_outbox(&thinq, &[expected]);
        }
    }

    #[test]
    fn does_not_send_settings_before_a_real_status_frame() {
        let (_ha, thinq, dev) = make();
        dev.set_property("rinse_level", "3");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn does_not_reuse_a_stale_remote_start_mode_after_an_unknown_status_value() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SETTINGS_BASELINE));
        let mut unknown = hex_decode(SETTINGS_BASELINE);
        unknown[2 + 48 + 2 + 16] = 0x06;
        thinq.emit_data(&unknown);
        thinq.reset_recorder();
        dev.set_property("rinse_level", "3");
        assert_eq!(prop(&ha, "remote_start_mode").as_deref(), Some("None"));
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn does_not_reuse_a_stale_buzzer_level_after_an_invalid_status_bit_combination() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SETTINGS_BASELINE));
        let mut unknown = hex_decode(SETTINGS_BASELINE);
        unknown[2 + 48 + 2 + 15] |= 0xc0;
        thinq.emit_data(&unknown);
        thinq.reset_recorder();
        dev.set_property("rinse_level", "3");
        assert_eq!(prop(&ha, "buzzer_level").as_deref(), Some("None"));
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn rejects_invalid_number_select_switch_and_button_inputs_without_sending() {
        let (_ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SETTINGS_BASELINE));
        thinq.reset_recorder();
        let invalid = [
            ("power", "MAYBE"),
            ("pause", "ON"),
            ("target_course", "Off"),
            ("target_delay", "13"),
            ("target_delay", "1.5"),
            ("target_high_temp", "YES"),
            ("target_extra_rinse", "4"),
            ("rinse_level", "5"),
            ("salt_level", "-1"),
            ("buzzer_level", "LOUD"),
            ("remote_start_mode", "Disabled"),
            ("auto_dry", "YES"),
            ("brightness", "MEDIUM"),
            ("start_course", "ON"),
        ];
        for (property, value) in invalid {
            dev.set_property(property, value);
        }
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn generates_captured_remote_start_packets_from_staged_public_controls() {
        let (_ha, thinq, dev) = make();
        dev.set_property("target_course", "One hour");
        dev.set_property("target_high_temp", "ON");
        dev.set_property("target_extra_dry", "ON");
        dev.set_property("target_extra_rinse", "1");
        dev.set_property("start_course", "PRESS");
        dev.set_property("target_delay", "1");
        dev.set_property("start_course", "PRESS");
        assert_outbox(
            &thinq,
            &["aa0df026101200000c080056bb", "aa0df026101201000c080051bb"],
        );

        let (_ha, thinq, dev) = make();
        dev.set_property("target_course", "Download cycle");
        dev.set_property("start_course", "PRESS");
        assert_outbox(&thinq, &["aa0df026100b00000040007dbb"]);
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("H11").is_some());
    }
}
