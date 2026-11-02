//! N17 dishwasher (LG LDNPQ445S). 31-byte AABB records, class byte 0x32.
//!
//! Start is `F0 26 10`. Settings are one snapshot of every cached value. Power
//! off (`F0 26 12`) is not cancel (`F0 26 11`).

use crate::device_trait::DeviceHandler;
use crate::js::parse_int;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x32;
const RECORD_LEN: usize = 31;
const RECORD_BODY: u8 = 0x1d;

const FLAG1_DOOR: u8 = 0x02;
const FLAG1_AUTO: u8 = 0x10;
const FLAG1_TUB: u8 = 0x20;
const FLAG1_CLEAN_LIGHT: u8 = 0x40;
const OPT_EXTRA_DRY: u8 = 0x04;
const OPT_HIGH_TEMP: u8 = 0x08;
const OPT_STEAM: u8 = 0x80;
const FLAG2_REMOTE: u8 = 0x02;
const FLAG2_CHIME: u8 = 0x80;
const FLAG3_TONE: u8 = 0x04;
const STATUS_LIGHT: u8 = 0x40;

const SET_CHIME: u8 = 0x04;
const SET_TONE: u8 = 0x40;
const SET_TUB: u8 = 0x01;
const SET_CLEAN: u8 = 0x08;
const SET_AUTO: u8 = 0x20;

const STATES: &[(u8, &str)] = &[
    (0, "Off"),
    (1, "Ready"),
    (2, "Running"),
    (3, "Pause"),
    (4, "Standby"),
    (5, "End"),
];
const PROCESSES: &[(u8, &str)] = &[
    (0x00, "Idle"),
    (0x01, "Delayed Start"),
    (0x02, "Washing"),
    (0x03, "Rinsing"),
    (0x04, "Drying"),
    (0x05, "End"),
    (0x06, "Night Dry"),
    (0x63, "Cancel"),
];
const COURSES: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Auto"),
    (0x02, "Heavy"),
    (0x04, "Turbo"),
    (0x05, "Normal"),
    (0x09, "Machine Clean"),
    (0x0b, "Download Cycle"),
];
const SMART: &[(u8, &str)] = &[
    (0x01, "Tub Clean"),
    (0x02, "Express"),
    (0x03, "Rinse"),
    (0x04, "Pots and Pans"),
    (0x05, "Casseroles"),
    (0x06, "Glassware"),
    (0x07, "Night Care"),
    (0x08, "Delicate"),
    (0x09, "Refresh"),
];
const START_COURSES: &[u8] = &[0x01, 0x02, 0x04, 0x05, 0x0b];

#[derive(Clone, Copy)]
struct Settings {
    rinse: u8,
    chime: bool,
    tone: bool,
    tub: bool,
    clean_light: bool,
    automatic: bool,
    status_light: bool,
}

struct Targets {
    course: u8,
    delay: u8,
    high_temp: bool,
    extra_dry: bool,
    steam: bool,
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    settings: Mutex<Option<Settings>>,
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
            settings: Mutex::new(None),
            targets: Mutex::new(Targets {
                course: 0x01,
                delay: 0,
                high_temp: false,
                extra_dry: false,
                steam: false,
            }),
        });
        let mut base = default_config(&meta, Some(json!({"name": "LG Dishwasher"})));
        base.components = components();
        core.set_config(base);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, CLASS_BYTE, RECORD_LEN) {
            self.process_status(rec);
            return;
        }
        if buf.len() == 7 && buf.first() == Some(&CLASS_BYTE) && buf.get(1) == Some(&0x3e) {
            let energy = u16::from_be_bytes([buf[4], buf[5]]);
            self.core
                .publish_property("energy", i64::from(energy).into());
        }
    }

    fn process_status(&self, record: &[u8]) {
        if record.len() < RECORD_LEN || record[1] != RECORD_BODY {
            return;
        }
        let data = &record[2..];
        self.core
            .publish_property("status", named(STATES, data[0]).into());
        self.core
            .publish_property("process", named(PROCESSES, data[1]).into());
        let course = if data[20] != 0 {
            named(SMART, data[20])
        } else {
            named(COURSES, data[5])
        };
        self.core.publish_property("course", course.into());
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
        self.core.publish_on_off("door", data[11] & FLAG1_DOOR != 0);
        self.core
            .publish_on_off("extra_dry", data[12] & OPT_EXTRA_DRY != 0);
        self.core
            .publish_on_off("high_temp", data[12] & OPT_HIGH_TEMP != 0);
        self.core.publish_on_off("steam", data[12] & OPT_STEAM != 0);
        self.core
            .publish_on_off("remote_start", data[15] & FLAG2_REMOTE != 0);

        let settings = Settings {
            rinse: data[13],
            chime: data[15] & FLAG2_CHIME != 0,
            tone: data[16] & FLAG3_TONE != 0,
            tub: data[11] & FLAG1_TUB != 0,
            clean_light: data[11] & FLAG1_CLEAN_LIGHT != 0,
            automatic: data[11] & FLAG1_AUTO != 0,
            status_light: data[21] & STATUS_LIGHT != 0,
        };
        self.core
            .publish_property("rinse_level", i64::from(settings.rinse).into());
        self.core.publish_on_off("chime_sound", settings.chime);
        self.core.publish_on_off("end_of_cycle_tone", settings.tone);
        self.core.publish_on_off("tub_clean_reminder", settings.tub);
        self.core
            .publish_on_off("clean_indicator_light", settings.clean_light);
        self.core
            .publish_on_off("automatic_selection", settings.automatic);
        self.core
            .publish_on_off("status_indicator_light", settings.status_light);
        *self.settings.lock() = Some(settings);
    }

    fn send_settings(&self) {
        let Some(s) = *self.settings.lock() else {
            return;
        };
        let mut opts = 0u8;
        if s.chime {
            opts |= SET_CHIME;
        }
        if s.tone {
            opts |= SET_TONE;
        }
        if s.tub {
            opts |= SET_TUB;
        }
        if s.clean_light {
            opts |= SET_CLEAN;
        }
        if s.automatic {
            opts |= SET_AUTO;
        }
        self.core.send(&[
            0xf0,
            0x26,
            s.rinse,
            0x00,
            opts,
            0x40,
            0x00,
            if s.status_light { 0x01 } else { 0x00 },
            0x00,
            0x00,
        ]);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "target_course" => {
                let Some(code) = course_code(mqtt_value) else {
                    return;
                };
                if !START_COURSES.contains(&code) {
                    return;
                }
                self.targets.lock().course = code;
                self.core
                    .publish_property("target_course", mqtt_value.into());
            }
            "target_high_temp" => {
                let on = mqtt_value == "ON";
                self.targets.lock().high_temp = on;
                self.core
                    .publish_property("target_high_temp", mqtt_value.into());
            }
            "target_extra_dry" => {
                let on = mqtt_value == "ON";
                self.targets.lock().extra_dry = on;
                self.core
                    .publish_property("target_extra_dry", mqtt_value.into());
            }
            "target_steam" => {
                let on = mqtt_value == "ON";
                self.targets.lock().steam = on;
                self.core
                    .publish_property("target_steam", mqtt_value.into());
            }
            "target_delay" => {
                let Some(hours) = parse_int(mqtt_value) else {
                    return;
                };
                if !(0..=12).contains(&hours) {
                    return;
                }
                self.targets.lock().delay = hours as u8;
                self.core.publish_property("target_delay", hours.into());
            }
            "start_course" => {
                let t = self.targets.lock();
                let mut opt3 = 0u8;
                if t.high_temp {
                    opt3 |= OPT_HIGH_TEMP;
                }
                if t.extra_dry {
                    opt3 |= OPT_EXTRA_DRY;
                }
                if t.steam {
                    opt3 |= OPT_STEAM;
                }
                let pkt = [0xf0, 0x26, 0x10, t.course, t.delay, 0x00, opt3, 0x00, 0x00];
                drop(t);
                self.core.send(&pkt);
            }
            "rinse_level" => {
                if self.settings.lock().is_none() {
                    return;
                }
                let Some(level) = parse_int(mqtt_value) else {
                    return;
                };
                if !(0..=4).contains(&level) {
                    return;
                }
                self.settings.lock().as_mut().unwrap().rinse = level as u8;
                self.send_settings();
            }
            "chime_sound"
            | "end_of_cycle_tone"
            | "tub_clean_reminder"
            | "clean_indicator_light"
            | "automatic_selection"
            | "status_indicator_light" => {
                if self.settings.lock().is_none() {
                    return;
                }
                let on = mqtt_value == "ON";
                {
                    let mut guard = self.settings.lock();
                    let s = guard.as_mut().unwrap();
                    match prop {
                        "chime_sound" => s.chime = on,
                        "end_of_cycle_tone" => s.tone = on,
                        "tub_clean_reminder" => s.tub = on,
                        "clean_indicator_light" => s.clean_light = on,
                        "automatic_selection" => s.automatic = on,
                        "status_indicator_light" => s.status_light = on,
                        _ => {}
                    }
                }
                self.send_settings();
            }
            "pause" => self.core.send(&hex_decode("F02613")),
            "resume" => self.core.send(&hex_decode("F02614")),
            "cancel" => self.core.send(&hex_decode("F02611")),
            "power_off" => self.core.send(&hex_decode("F02612")),
            _ => {}
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode("F0ED1121010000001800"));
        let t = self.targets.lock();
        self.core
            .publish_property("target_course", named(COURSES, t.course).into());
        self.core.publish_on_off("target_high_temp", t.high_temp);
        self.core.publish_on_off("target_extra_dry", t.extra_dry);
        self.core.publish_on_off("target_steam", t.steam);
        self.core
            .publish_property("target_delay", i64::from(t.delay).into());
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

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| *n)
        .unwrap_or("None")
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn course_code(name: &str) -> Option<u8> {
    COURSES.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

fn components() -> std::collections::HashMap<String, Value> {
    let mut c = Map::new();
    let add = |c: &mut Map<String, Value>, k: &str, v: Value| {
        c.insert(k.into(), v);
    };
    add(
        &mut c,
        "status",
        json!({"platform":"sensor","icon":"mdi:dishwasher","device_class":"enum","options":labels(STATES),"unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status"}),
    );
    add(
        &mut c,
        "course",
        json!({"platform":"sensor","icon":"mdi:playlist-play","device_class":"enum","options":labels(COURSES).into_iter().chain(labels(SMART)).collect::<Vec<_>>(),"unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course"}),
    );
    add(
        &mut c,
        "process",
        json!({"platform":"sensor","icon":"mdi:progress-clock","device_class":"enum","options":labels(PROCESSES),"unique_id":"$deviceid-process","state_topic":"$this/process","name":"Process"}),
    );
    add(
        &mut c,
        "remaining_time",
        json!({"platform":"sensor","icon":"mdi:timer-sand","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Remaining time"}),
    );
    add(
        &mut c,
        "initial_time",
        json!({"platform":"sensor","icon":"mdi:timer","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Initial time"}),
    );
    add(
        &mut c,
        "delay_start_time",
        json!({"platform":"sensor","icon":"mdi:clock-fast","unique_id":"$deviceid-delay_start_time","state_topic":"$this/delay_start_time","device_class":"duration","unit_of_measurement":"min","name":"Delay start time"}),
    );
    add(
        &mut c,
        "door",
        json!({"platform":"binary_sensor","device_class":"door","unique_id":"$deviceid-door","state_topic":"$this/door","name":"Door"}),
    );
    add(
        &mut c,
        "energy",
        json!({"platform":"sensor","device_class":"energy","state_class":"total_increasing","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","unit_of_measurement":"Wh","icon":"mdi:lightning-bolt"}),
    );
    add(
        &mut c,
        "extra_dry",
        json!({"platform":"binary_sensor","icon":"mdi:weather-sunny","unique_id":"$deviceid-extra_dry","state_topic":"$this/extra_dry","name":"Extra dry"}),
    );
    add(
        &mut c,
        "high_temp",
        json!({"platform":"binary_sensor","icon":"mdi:thermometer-high","unique_id":"$deviceid-high_temp","state_topic":"$this/high_temp","name":"High temp"}),
    );
    add(
        &mut c,
        "steam",
        json!({"platform":"binary_sensor","icon":"mdi:kettle-steam","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam"}),
    );
    add(
        &mut c,
        "remote_start",
        json!({"platform":"binary_sensor","icon":"mdi:remote","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start"}),
    );
    add(
        &mut c,
        "rinse_level",
        json!({"platform":"number","icon":"mdi:water-plus","entity_category":"config","unique_id":"$deviceid-rinse_level","state_topic":"$this/rinse_level","command_topic":"$this/rinse_level/set","name":"Rinse aid dispenser level","min":0,"max":4,"step":1,"optimistic":true}),
    );
    add(
        &mut c,
        "chime_sound",
        json!({"platform":"switch","icon":"mdi:volume-high","entity_category":"config","unique_id":"$deviceid-chime_sound","state_topic":"$this/chime_sound","command_topic":"$this/chime_sound/set","name":"Chime sound","optimistic":true}),
    );
    add(
        &mut c,
        "end_of_cycle_tone",
        json!({"platform":"switch","icon":"mdi:music-note","entity_category":"config","unique_id":"$deviceid-end_of_cycle_tone","state_topic":"$this/end_of_cycle_tone","command_topic":"$this/end_of_cycle_tone/set","name":"End of cycle tone","optimistic":true}),
    );
    add(
        &mut c,
        "tub_clean_reminder",
        json!({"platform":"switch","icon":"mdi:bell-outline","entity_category":"config","unique_id":"$deviceid-tub_clean_reminder","state_topic":"$this/tub_clean_reminder","command_topic":"$this/tub_clean_reminder/set","name":"Tub clean reminder","optimistic":true}),
    );
    add(
        &mut c,
        "clean_indicator_light",
        json!({"platform":"switch","icon":"mdi:lightbulb","entity_category":"config","unique_id":"$deviceid-clean_indicator_light","state_topic":"$this/clean_indicator_light","command_topic":"$this/clean_indicator_light/set","name":"Clean indicator light","optimistic":true}),
    );
    add(
        &mut c,
        "automatic_selection",
        json!({"platform":"switch","icon":"mdi:auto-fix","entity_category":"config","unique_id":"$deviceid-automatic_selection","state_topic":"$this/automatic_selection","command_topic":"$this/automatic_selection/set","name":"Automatic selection","optimistic":true}),
    );
    add(
        &mut c,
        "status_indicator_light",
        json!({"platform":"switch","icon":"mdi:led-on","entity_category":"config","unique_id":"$deviceid-status_indicator_light","state_topic":"$this/status_indicator_light","command_topic":"$this/status_indicator_light/set","name":"Status indicator light","optimistic":true}),
    );
    add(
        &mut c,
        "target_course",
        json!({"platform":"select","icon":"mdi:washing-machine","unique_id":"$deviceid-target_course","state_topic":"$this/target_course","command_topic":"$this/target_course/set","name":"Target course","options":["Auto","Heavy","Turbo","Normal","Download Cycle"]}),
    );
    add(
        &mut c,
        "target_delay",
        json!({"platform":"number","icon":"mdi:clock-start","unique_id":"$deviceid-target_delay","state_topic":"$this/target_delay","command_topic":"$this/target_delay/set","name":"Target delay start","min":0,"max":12,"step":1,"unit_of_measurement":"h"}),
    );
    add(
        &mut c,
        "target_high_temp",
        json!({"platform":"switch","icon":"mdi:thermometer-high","unique_id":"$deviceid-target_high_temp","state_topic":"$this/target_high_temp","command_topic":"$this/target_high_temp/set","name":"Target high temp"}),
    );
    add(
        &mut c,
        "target_extra_dry",
        json!({"platform":"switch","icon":"mdi:weather-sunny","unique_id":"$deviceid-target_extra_dry","state_topic":"$this/target_extra_dry","command_topic":"$this/target_extra_dry/set","name":"Target extra dry"}),
    );
    add(
        &mut c,
        "target_steam",
        json!({"platform":"switch","icon":"mdi:kettle-steam","unique_id":"$deviceid-target_steam","state_topic":"$this/target_steam","command_topic":"$this/target_steam/set","name":"Target steam"}),
    );
    add(
        &mut c,
        "start_course",
        json!({
            "platform":"button","icon":"mdi:play-circle","unique_id":"$deviceid-start_course",
            "command_topic":"$this/start_course/set","name":"Start course","payload_press":"PRESS",
            "availability_mode":"all",
            "availability":[
                {"topic":"$this/availability"},
                {"topic":"$rethink/availability"},
                {"topic":"$this/remote_start","payload_available":"ON","payload_not_available":"OFF"}
            ]
        }),
    );
    add(
        &mut c,
        "pause",
        json!({"platform":"button","icon":"mdi:pause-circle","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","name":"Pause","payload_press":"PRESS"}),
    );
    add(
        &mut c,
        "resume",
        json!({"platform":"button","icon":"mdi:play-pause","unique_id":"$deviceid-resume","command_topic":"$this/resume/set","name":"Resume","payload_press":"PRESS"}),
    );
    add(
        &mut c,
        "cancel",
        json!({"platform":"button","icon":"mdi:stop-circle","unique_id":"$deviceid-cancel","command_topic":"$this/cancel/set","name":"Cancel cycle","payload_press":"PRESS"}),
    );
    add(
        &mut c,
        "power_off",
        json!({"platform":"button","icon":"mdi:power","unique_id":"$deviceid-power_off","command_topic":"$this/power_off/set","name":"Power off","payload_press":"PRESS"}),
    );
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const ID: &str = "test-id";
    const STATISTICS_ZERO: &str = "aa0b323e000000000070bb";
    const STATISTICS_TWO_WH: &str = "aa0b323e00020002017fbb";
    const TWELVE_WH: &str = "aa0b323e0000000c0064bb";
    const EC_REAL_RUNNING: &str =
        "aa4432ec001d0202000221010001030000540002008946640000004000010102030500001d020200022101000102000054000200894664000000400001010203050012bb";
    const EC_REAL_DRYING_DOOR_OPEN: &str =
        "aa4432ec001d020400022101000019000054000200894e640000004000010102030500001d020400022101000019000056000200894e640000004000010102030500ddbb";
    const EC_REAL_COMPLETE: &str =
        "aa4432ec001d020400022101000001000056000200894e640000004000010102030500001d050500022101000001000056000200894e6400000040000101020305000bbb";
    const EC_REAL_TURBO_EXTRA_DRY: &str =
        "aa4432ec001d0100000221010002210000540002008b46640000004000010102030500001d020200021d0400021d0000540402008b46640000004000010102030500d3bb";
    const EC_REAL_HEAVY_HIGH_TEMP: &str =
        "aa4432ec001d010000021d0400021d0000540402008b46640000004000010102030500001d0202000324020003240000540802008b46640000004000010102030500c2bb";
    const EC_REAL_DELAYED_START: &str =
        "aa4432ec001d010000021c0500021c0b00560102008946640000004000010102030500001d020100021c0500021c0b00540102008946640000004000010102030500c5bb";
    const EC_REAL_NORMAL_STEAM: &str =
        "aa4432ec001d010000021c0500021c0000540002008b46640000004000010102030500001d020200032b0500032b0000548002008b466400000040000101020305004ebb";
    const EC_REAL_DOWNLOAD_TUB_CLEAN: &str =
        "aa4432ec001d010000032b0500032b0000548002008b46640000004000010102030500001d020100010009010100003b540102008b4664000001400001010203050077bb";
    const EC_SETTINGS_CHIME_ON: &str =
        "aa4432ec001d010000021c0500021c0000560002008946640000004000010102030500001d010000021c0500021c000052000000894664000000400001010203050027bb";
    const EC_SETTINGS_TUB_ONLY: &str =
        "aa4432ec001d010000021c0500021c0000320000000942640000004000010102030500001d010000021c0500021c000022000000094264000000400001010203050041bb";
    const EC_SETTINGS_STATUS_LIGHT_OFF: &str =
        "aa4432ec001d010000021c0500021c0000220000000942640000004000010102030500001d010000021c0500021c000022000000094264000000000001010203050091bb";
    const EC_SETTINGS_RINSE_4: &str =
        "aa4432ec001d010000021c0500021c0000560004000942640000000000010102030500001d010000021c0500021c000056000400094264000000400001010203050061bb";
    const FOREIGN_CLASS: &str =
        "aa2330eb001b000029002900000000000100000000280000000100000064000000b6bb";
    const REAL_0A: &str =
        "aaff320a005800004100010a8500460009053230342d3100053230342d3200053230342d33000644572d312d35000644572d332d33000644572d352d3100053230342d3600063230342d313100063230342d313300dbbfbb";
    const REAL_31: &str =
        "aa4f3231030153414133303030373330310000cc7d00008000000000000253414133303030333830310000633cfff80000000000000353414134343930313030310000eb1d00004000000000006ebb";
    const REAL_72: &str = "aa093272006600e8bb";
    const EB_NORMAL_SELECTED_DOOR_OPEN: &str =
        "aa2532eb001d010000031a0500031a000056040200894664000000400001010203050071bb";
    const EB_NORMAL_SELECTED: &str =
        "aa2532eb001d010000031a0500031a000054040200894664000000400001010203050077bb";
    const EB_STANDBY_NO_COURSE: &str =
        "aa2532eb001d040000031a0000031a00005400020089446400000040000101020305004fbb";
    const EB_MARKER_08: &str =
        "aa2532eb081d040000031a0000031a000054000200894464000000400001010203050077bb";
    const EB_OFF: &str =
        "aa2532eb001d000000031a0000031a000054000200894464000000400001010203050043bb";
    const WRONG_LEN: &str =
        "aa2532eb0018010000031a0500031a000056040200894664000000400001010203050071bb";
    const BAD_LAYOUT: &str =
        "aa3a32ec00100000000000000000000000000000000000000000000000000010020000011e050001050000500c0203828400004000000000a0bb";

    fn meta() -> Metadata {
        Metadata::new("N17", "N17", "0.0.0")
    }

    fn feed(frames: &[&str]) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        DeviceHandler::start(dev.as_ref());
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    /// Everything after the connect-time status query.
    fn commands(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .outbox()
            .into_iter()
            .skip(1)
            .map(|p| hex_encode(&p).to_ascii_lowercase())
            .collect()
    }

    /// `start` seeds the target_* entities, so "decoded nothing" means no other key appeared.
    fn assert_no_status_decoded(ha: &MockHaConnection) {
        let decoded: Vec<String> = ha
            .device(ID)
            .unwrap()
            .properties
            .into_keys()
            .filter(|k| !k.starts_with("target_"))
            .collect();
        assert!(decoded.is_empty(), "{decoded:?}");
    }

    #[test]
    fn statistics_publish_accumulated_watt_hours_and_track_a_change() {
        expect(&feed(&[STATISTICS_ZERO]).0, "energy", "0");
        expect(
            &feed(&[STATISTICS_ZERO, STATISTICS_TWO_WH]).0,
            "energy",
            "2",
        );
    }

    #[test]
    fn a_repeated_sequence_number_does_not_suppress_a_changed_reading() {
        expect(&feed(&[STATISTICS_ZERO, TWELVE_WH]).0, "energy", "12");
    }

    #[test]
    fn undecoded_frame_types_the_appliance_sends_publish_nothing() {
        assert_no_status_decoded(&feed(&[REAL_0A, REAL_31, REAL_72]).0);
    }

    #[test]
    fn connect_sends_exactly_the_observed_status_query() {
        let (_, thinq, _) = feed(&[]);
        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 1);
        assert_eq!(hex_encode(&outbox[0]), "AA0EF0ED1121010000001800B5BB");
    }

    #[test]
    fn frames_from_another_appliance_class_are_ignored() {
        assert_no_status_decoded(&feed(&[FOREIGN_CLASS]).0);
    }

    #[test]
    fn eb_publishes_state_course_door_and_options() {
        let (selected, _, _) = feed(&[EB_NORMAL_SELECTED]);
        expect(&selected, "status", "Ready");
        expect(&selected, "course", "Normal");
        expect(&selected, "door", "OFF");
        expect(&selected, "extra_dry", "ON");

        let (standby, _, _) = feed(&[EB_STANDBY_NO_COURSE]);
        expect(&standby, "status", "Standby");
        expect(&standby, "course", "Off");
        expect(&standby, "extra_dry", "OFF");

        expect(&feed(&[EB_OFF]).0, "status", "Off");
    }

    #[test]
    fn eb_tracks_the_door_independently() {
        expect(&feed(&[EB_NORMAL_SELECTED_DOOR_OPEN]).0, "door", "ON");
        expect(&feed(&[EB_NORMAL_SELECTED]).0, "door", "OFF");
    }

    #[test]
    fn eb_accepts_either_record_marker() {
        let marked = feed(&[EB_MARKER_08]).0.device(ID).unwrap().properties;
        let plain = feed(&[EB_STANDBY_NO_COURSE])
            .0
            .device(ID)
            .unwrap()
            .properties;
        assert_eq!(marked, plain);
    }

    #[test]
    fn eb_record_length_other_than_0x1d_decodes_to_nothing() {
        assert_no_status_decoded(&feed(&[WRONG_LEN]).0);
    }

    #[test]
    fn real_ec_frame_decodes_the_current_record() {
        let (ha, _, _) = feed(&[EC_REAL_RUNNING]);
        expect(&ha, "status", "Running");
        expect(&ha, "initial_time", "153");
        expect(&ha, "remaining_time", "62");
        expect(&ha, "door", "OFF");
        expect(&ha, "course", "Auto");
        expect(&ha, "extra_dry", "OFF");
        expect(&ha, "delay_start_time", "0");
        expect(&ha, "remote_start", "OFF");
        expect(&ha, "process", "Washing");
    }

    #[test]
    fn drying_phase_and_auto_open_door() {
        let (ha, _, _) = feed(&[EC_REAL_DRYING_DOOR_OPEN]);
        expect(&ha, "status", "Running");
        expect(&ha, "process", "Drying");
        expect(&ha, "remaining_time", "25");
        expect(&ha, "door", "ON");
    }

    #[test]
    fn cycle_completion_reports_end_for_state_and_process() {
        let (ha, _, _) = feed(&[EC_REAL_COMPLETE]);
        expect(&ha, "status", "End");
        expect(&ha, "process", "End");
        expect(&ha, "door", "ON");
    }

    #[test]
    fn downloaded_cycle_reports_its_catalogue_number() {
        let (ha, _, _) = feed(&[EC_REAL_DOWNLOAD_TUB_CLEAN]);
        expect(&ha, "course", "Tub Clean");
        expect(&ha, "process", "Delayed Start");
        expect(&ha, "status", "Running");
        expect(&ha, "delay_start_time", "59");
    }

    #[test]
    fn commanded_delay_is_sent_in_byte_4() {
        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("target_course", "Download Cycle");
        dev.set_property("target_delay", "1");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), ["aa0df026100b0100000000bcbb"]);

        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("target_delay", "99");
        dev.set_property("start_course", "PRESS");
        assert_eq!(&commands(&thinq)[0][12..14], "00");
    }

    #[test]
    fn steam_is_decoded_and_lengthens_the_cycle() {
        let (ha, _, _) = feed(&[EC_REAL_NORMAL_STEAM]);
        expect(&ha, "course", "Normal");
        expect(&ha, "steam", "ON");
        expect(&ha, "high_temp", "OFF");
        expect(&ha, "extra_dry", "OFF");
        expect(&ha, "initial_time", "223");
    }

    #[test]
    fn the_two_option_bits_are_distinct() {
        let (turbo, _, _) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        expect(&turbo, "status", "Running");
        expect(&turbo, "course", "Turbo");
        expect(&turbo, "initial_time", "149");
        expect(&turbo, "extra_dry", "ON");
        expect(&turbo, "high_temp", "OFF");

        let (heavy, _, _) = feed(&[EC_REAL_HEAVY_HIGH_TEMP]);
        expect(&heavy, "course", "Heavy");
        expect(&heavy, "initial_time", "216");
        expect(&heavy, "extra_dry", "OFF");
        expect(&heavy, "high_temp", "ON");
    }

    #[test]
    fn start_button_is_available_only_while_remote_start_is_armed() {
        let (ha, _, _) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        let comps = ha.device(ID).unwrap().config.unwrap().components;
        assert_eq!(
            comps["start_course"]["availability"][2],
            json!({
                "topic": "$this/remote_start",
                "payload_available": "ON",
                "payload_not_available": "OFF"
            })
        );
        expect(&ha, "remote_start", "ON");
    }

    #[test]
    fn start_is_passed_through_without_an_interlock() {
        let (_, thinq, dev) = feed(&[EB_STANDBY_NO_COURSE]);
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq).len(), 1);
    }

    #[test]
    fn start_reproduces_the_captured_packets() {
        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("target_course", "Turbo");
        dev.set_property("target_extra_dry", "ON");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), ["aa0df02610040000040000b0bb"]);

        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("target_course", "Heavy");
        dev.set_property("target_high_temp", "ON");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), ["aa0df02610020000080000b2bb"]);

        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("target_course", "Normal");
        dev.set_property("target_steam", "ON");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), ["aa0df0261005000080000037bb"]);
    }

    #[test]
    fn the_three_option_bits_combine_in_one_command() {
        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("target_course", "Normal");
        dev.set_property("target_steam", "ON");
        dev.set_property("target_high_temp", "ON");
        dev.set_property("target_extra_dry", "ON");
        dev.set_property("start_course", "PRESS");
        assert_eq!(&commands(&thinq)[0][16..18], "8c");
    }

    #[test]
    fn a_course_outside_the_observed_set_is_refused() {
        let (ha, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("target_course", "Machine Clean");
        expect(&ha, "target_course", "Auto");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), ["aa0df026100100000000008bbb"]);
    }

    #[test]
    fn cancel_always_sends_0x11() {
        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("cancel", "PRESS");
        assert_eq!(commands(&thinq), ["aa07f026118dbb"]);

        let (ha, thinq, dev) = feed(&[EC_REAL_DELAYED_START]);
        expect(&ha, "process", "Delayed Start");
        dev.set_property("cancel", "PRESS");
        assert_eq!(commands(&thinq), ["aa07f026118dbb"]);
    }

    #[test]
    fn power_off_is_its_own_action() {
        let (_, thinq, dev) = feed(&[EC_REAL_TURBO_EXTRA_DRY]);
        dev.set_property("power_off", "PRESS");
        assert_eq!(commands(&thinq), ["aa07f026128cbb"]);
    }

    #[test]
    fn pause_and_resume_send_the_captured_packets() {
        let (_, thinq, dev) = feed(&[]);
        dev.set_property("pause", "PRESS");
        dev.set_property("resume", "PRESS");
        assert_eq!(commands(&thinq), ["aa07f026138fbb", "aa07f026148ebb"]);
    }

    #[test]
    fn settings_read_back_from_real_status_records() {
        let (chime_on, _, _) = feed(&[EC_SETTINGS_CHIME_ON]);
        expect(&chime_on, "rinse_level", "0");
        expect(&chime_on, "chime_sound", "ON");
        expect(&chime_on, "end_of_cycle_tone", "ON");
        expect(&chime_on, "clean_indicator_light", "ON");
        expect(&chime_on, "automatic_selection", "ON");
        expect(&chime_on, "tub_clean_reminder", "OFF");
        expect(&chime_on, "status_indicator_light", "ON");

        let (rinse4, _, _) = feed(&[EC_SETTINGS_RINSE_4]);
        expect(&rinse4, "rinse_level", "4");
        expect(&rinse4, "chime_sound", "OFF");
        expect(&rinse4, "end_of_cycle_tone", "OFF");
        expect(&rinse4, "clean_indicator_light", "ON");

        let (tub_only, _, _) = feed(&[EC_SETTINGS_TUB_ONLY]);
        expect(&tub_only, "tub_clean_reminder", "ON");
        expect(&tub_only, "clean_indicator_light", "OFF");
        expect(&tub_only, "automatic_selection", "OFF");

        let (light_off, _, _) = feed(&[EC_SETTINGS_STATUS_LIGHT_OFF]);
        expect(&light_off, "status_indicator_light", "OFF");
    }

    #[test]
    fn settings_write_snapshots_the_read_back_state() {
        let (_, thinq, dev) = feed(&[EC_SETTINGS_CHIME_ON]);
        dev.set_property("chime_sound", "OFF");
        assert_eq!(commands(&thinq), ["aa0ef026000068400001000022bb"]);

        let (_, thinq, dev) = feed(&[EC_SETTINGS_RINSE_4]);
        dev.set_property("rinse_level", "2");
        assert_eq!(commands(&thinq), ["aa0ef02602002840000100006cbb"]);

        let (_, thinq, dev) = feed(&[EC_SETTINGS_TUB_ONLY]);
        dev.set_property("status_indicator_light", "OFF");
        assert_eq!(commands(&thinq), ["aa0ef02600000140000000005abb"]);
    }

    #[test]
    fn settings_writes_are_refused_until_a_status_record_arrives() {
        let (_, thinq, dev) = feed(&[]);
        dev.set_property("chime_sound", "OFF");
        dev.set_property("rinse_level", "2");
        assert!(commands(&thinq).is_empty());

        let (_, thinq, dev) = feed(&[EC_SETTINGS_RINSE_4]);
        dev.set_property("rinse_level", "9");
        assert!(commands(&thinq).is_empty());
    }

    #[test]
    fn ec_frame_with_an_unexpected_layout_decodes_to_nothing() {
        assert_no_status_decoded(&feed(&[BAD_LAYOUT]).0);
    }

    #[test]
    fn numeric_payloads_are_parsed_like_javascript_parse_int() {
        let (ha, thinq, dev) = feed(&[EC_SETTINGS_RINSE_4]);
        for (value, hours) in [
            ("2.5", "2"),
            ("3abc", "3"),
            ("0x5", "0"),
            (" 7", "7"),
            ("+1", "1"),
        ] {
            dev.set_property("target_delay", value);
            assert_eq!(
                prop(&ha, "target_delay").as_deref(),
                Some(hours),
                "{value:?}"
            );
        }
        for value in ["", "  ", "abc", "-1", "13", "-", ".5"] {
            dev.set_property("target_delay", value);
            assert_eq!(prop(&ha, "target_delay").as_deref(), Some("1"), "{value:?}");
        }
        dev.set_property("start_course", "PRESS");
        for value in ["2.5", "9", "abc", "", " 4x"] {
            dev.set_property("rinse_level", value);
        }
        assert_eq!(
            commands(&thinq),
            [
                "aa0df026100101000000008abb",
                "aa0ef02602002840000100006cbb",
                "aa0ef02604002840000100006ebb"
            ]
        );
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("N17").is_some());
    }
}
