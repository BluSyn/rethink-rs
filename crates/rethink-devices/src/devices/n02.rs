//! N02 dishwasher (LG LDPH7972S). 31-byte AABB records, class byte 0x32.
//!
//! N17's family with a richer settings screen: Time Indicator is new, the
//! other settings reuse N17's bits. The record's own length byte reads 0x18,
//! so only the frame length gates a record. Start is
//! `F0 26 10 <course> <delay> 00 <opt3> <opt4> 00`, with Dry Boost and Night
//! Dry in opt4. Settings are one `F0 26 <rinse> 00 <opts> 00 00 00 00 00`
//! snapshot of every cached value. Cancel (`F0 26 11`) drains for about a
//! minute as Running/Cancel before Standby. Download Cycle reports the course
//! loaded in its slot; loading one (`F0 25 03`) is not implemented. `auto_ack`
//! stays off.
//!
//! Port of upstream PR #227.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x32;
const RECORD_LEN: usize = 31;

const FLAG1_DOOR: u8 = 0x02;
const FLAG1_RINSE_REFILL: u8 = 0x04;
const FLAG1_AUTO_SELECT: u8 = 0x10;
const FLAG1_MACHINE_CLEAN: u8 = 0x20;
const FLAG1_CLEAN_LIGHT: u8 = 0x40;
const FLAG1_NIGHT_DRY: u8 = 0x80;
const OPT_HIGH_TEMP: u8 = 0x08;
const OPT_FLEX_ZONE: u8 = 0x70;
const OPT_STEAM: u8 = 0x80;
const FLAG2_REMOTE: u8 = 0x02;
const FLAG2_TIME_INDICATOR: u8 = 0x08;
const FLAG2_DRY_BOOST: u8 = 0x30;
const FLAG2_CHIME: u8 = 0x80;
const FLAG3_TONE: u8 = 0x04;

const OPT4_DRY_BOOST_LOW: u8 = 0x01;
const OPT4_DRY_BOOST_HIGH: u8 = 0x03;
const OPT4_NIGHT_DRY: u8 = 0x04;

const SET_MACHINE_CLEAN: u8 = 0x01;
const SET_CHIME: u8 = 0x04;
const SET_CLEAN_LIGHT: u8 = 0x08;
const SET_TIME_INDICATOR: u8 = 0x10;
const SET_AUTO_SELECT: u8 = 0x20;
const SET_TONE: u8 = 0x40;

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
    (0x03, "Delicate"),
    (0x05, "Normal"),
    (0x06, "Rinse"),
    (0x07, "Refresh"),
    (0x09, "Machine Clean"),
    (0x0b, "Download Cycle"),
    (0x12, "1 Hour"),
];
// Machine Clean was only ever reached through Download Cycle, never started directly.
const START_COURSES: &[u8] = &[0x01, 0x02, 0x03, 0x05, 0x07, 0x0b, 0x12];
// The start command's opt3 uses the readback's bits.
const FLEX_ZONES: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x10, "Dual"),
    (0x20, "Lower"),
    (0x40, "Upper"),
];
// flags2 readback, where High stacks on Low's bit; opt4 encodes them as 0x01/0x03.
const DRY_BOOST_LOW: u8 = 0x10;
const DRY_BOOST_HIGH: u8 = 0x30;
const DRY_BOOSTS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (DRY_BOOST_LOW, "Low"),
    (DRY_BOOST_HIGH, "High"),
];

#[derive(Clone, Copy)]
struct Settings {
    rinse: u8,
    machine_clean: bool,
    chime: bool,
    clean_light: bool,
    time_indicator: bool,
    auto_select: bool,
    tone: bool,
}

impl Settings {
    fn packet(&self) -> [u8; 10] {
        let mut opts = 0u8;
        for (on, bit) in [
            (self.machine_clean, SET_MACHINE_CLEAN),
            (self.chime, SET_CHIME),
            (self.clean_light, SET_CLEAN_LIGHT),
            (self.time_indicator, SET_TIME_INDICATOR),
            (self.auto_select, SET_AUTO_SELECT),
            (self.tone, SET_TONE),
        ] {
            if on {
                opts |= bit;
            }
        }
        [
            0xf0, 0x26, self.rinse, 0x00, opts, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]
    }
}

struct Targets {
    course: u8,
    delay: u8,
    high_temp: bool,
    steam: bool,
    flex_zone: u8,
    dry_boost: u8,
    night_dry: bool,
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
                course: 0x05,
                delay: 0,
                high_temp: false,
                steam: false,
                flex_zone: 0x00,
                dry_boost: 0x00,
                night_dry: false,
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
        let data = &record[2..];
        self.core
            .publish_property("status", named(STATES, data[0]).into());
        self.core
            .publish_property("process", named(PROCESSES, data[1]).into());
        self.core
            .publish_property("course", named(COURSES, data[5]).into());

        // The last tick before Off, Standby, or End reads 0:01, never 0:00.
        let cycle_gone = matches!(data[0], 0x00 | 0x04 | 0x05);
        self.core.publish_property(
            "initial_time",
            (i64::from(data[3]) * 60 + i64::from(data[4])).into(),
        );
        let remaining = if cycle_gone {
            0
        } else {
            i64::from(data[7]) * 60 + i64::from(data[8])
        };
        self.core
            .publish_property("remaining_time", remaining.into());
        self.core.publish_property(
            "delay_start_time",
            (i64::from(data[9]) * 60 + i64::from(data[10])).into(),
        );
        self.core.publish_on_off("door", data[11] & FLAG1_DOOR != 0);
        self.core
            .publish_on_off("remote_start", data[15] & FLAG2_REMOTE != 0);
        self.core.publish_property(
            "flex_zone",
            named(FLEX_ZONES, data[12] & OPT_FLEX_ZONE).into(),
        );
        self.core
            .publish_on_off("night_dry", data[11] & FLAG1_NIGHT_DRY != 0);
        self.core
            .publish_on_off("rinse_refill", data[11] & FLAG1_RINSE_REFILL != 0);
        self.core.publish_property(
            "dry_boost",
            named(DRY_BOOSTS, data[15] & FLAG2_DRY_BOOST).into(),
        );

        let settings = Settings {
            rinse: data[13],
            machine_clean: data[11] & FLAG1_MACHINE_CLEAN != 0,
            chime: data[15] & FLAG2_CHIME != 0,
            clean_light: data[11] & FLAG1_CLEAN_LIGHT != 0,
            time_indicator: data[15] & FLAG2_TIME_INDICATOR != 0,
            auto_select: data[11] & FLAG1_AUTO_SELECT != 0,
            tone: data[16] & FLAG3_TONE != 0,
        };
        self.core
            .publish_property("rinse_level", i64::from(settings.rinse).into());
        self.core
            .publish_on_off("machine_clean_reminder", settings.machine_clean);
        self.core.publish_on_off("chime_sound", settings.chime);
        self.core
            .publish_on_off("clean_light_reminder", settings.clean_light);
        self.core
            .publish_on_off("time_indicator", settings.time_indicator);
        self.core
            .publish_on_off("auto_select", settings.auto_select);
        self.core.publish_on_off("end_of_cycle_tone", settings.tone);
        *self.settings.lock() = Some(settings);
    }

    /// A settings write is a full snapshot, so it needs the read-back values first.
    fn write_setting(&self, prop: &str, mqtt_value: &str) {
        let pkt = {
            let mut guard = self.settings.lock();
            let Some(s) = guard.as_mut() else {
                tracing::debug!("N02 refusing settings write: no status record received yet");
                return;
            };
            let on = mqtt_value == "ON";
            match prop {
                "rinse_level" => {
                    let Ok(level) = mqtt_value.parse::<i64>() else {
                        return;
                    };
                    if !(0..=4).contains(&level) {
                        return;
                    }
                    s.rinse = level as u8;
                }
                "machine_clean_reminder" => s.machine_clean = on,
                "chime_sound" => s.chime = on,
                "clean_light_reminder" => s.clean_light = on,
                "time_indicator" => s.time_indicator = on,
                "auto_select" => s.auto_select = on,
                "end_of_cycle_tone" => s.tone = on,
                _ => return,
            }
            s.packet()
        };
        self.core.send(&pkt);
    }

    fn send_start(&self) {
        let pkt = {
            let t = self.targets.lock();
            let mut opt3 = t.flex_zone;
            if t.high_temp {
                opt3 |= OPT_HIGH_TEMP;
            }
            if t.steam {
                opt3 |= OPT_STEAM;
            }
            let mut opt4 = match t.dry_boost {
                DRY_BOOST_HIGH => OPT4_DRY_BOOST_HIGH,
                DRY_BOOST_LOW => OPT4_DRY_BOOST_LOW,
                _ => 0,
            };
            if t.night_dry {
                opt4 |= OPT4_NIGHT_DRY;
            }
            [0xf0, 0x26, 0x10, t.course, t.delay, 0x00, opt3, opt4, 0x00]
        };
        self.core.send(&pkt);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "target_course" => {
                let Some(code) = code_of(COURSES, mqtt_value) else {
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
                self.targets.lock().high_temp = mqtt_value == "ON";
                self.core
                    .publish_property("target_high_temp", mqtt_value.into());
            }
            "target_steam" => {
                self.targets.lock().steam = mqtt_value == "ON";
                self.core
                    .publish_property("target_steam", mqtt_value.into());
            }
            "target_flex_zone" => {
                let Some(code) = code_of(FLEX_ZONES, mqtt_value) else {
                    return;
                };
                self.targets.lock().flex_zone = code;
                self.core
                    .publish_property("target_flex_zone", mqtt_value.into());
            }
            "target_dry_boost" => {
                let Some(code) = code_of(DRY_BOOSTS, mqtt_value) else {
                    return;
                };
                self.targets.lock().dry_boost = code;
                self.core
                    .publish_property("target_dry_boost", mqtt_value.into());
            }
            "target_night_dry" => {
                self.targets.lock().night_dry = mqtt_value == "ON";
                self.core
                    .publish_property("target_night_dry", mqtt_value.into());
            }
            "target_delay" => {
                let Ok(hours) = mqtt_value.parse::<i64>() else {
                    return;
                };
                if !(0..=12).contains(&hours) {
                    return;
                }
                self.targets.lock().delay = hours as u8;
                self.core.publish_property("target_delay", hours.into());
            }
            "start_course" => self.send_start(),
            "rinse_level"
            | "machine_clean_reminder"
            | "chime_sound"
            | "clean_light_reminder"
            | "time_indicator"
            | "auto_select"
            | "end_of_cycle_tone" => self.write_setting(prop, mqtt_value),
            "pause" => self.core.send(&hex_decode("F02613")),
            "resume" => self.core.send(&hex_decode("F02614")),
            "cancel" => self.core.send(&hex_decode("F02611")),
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
        self.core.publish_on_off("target_steam", t.steam);
        self.core
            .publish_property("target_flex_zone", named(FLEX_ZONES, t.flex_zone).into());
        self.core
            .publish_property("target_dry_boost", named(DRY_BOOSTS, t.dry_boost).into());
        self.core.publish_on_off("target_night_dry", t.night_dry);
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

fn code_of(table: &'static [(u8, &'static str)], name: &str) -> Option<u8> {
    table.iter().find(|(_, n)| *n == name).map(|(c, _)| *c)
}

fn components() -> std::collections::HashMap<String, Value> {
    let start_courses: Vec<&str> = START_COURSES.iter().map(|&c| named(COURSES, c)).collect();
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
        json!({"platform":"sensor","icon":"mdi:playlist-play","device_class":"enum","options":labels(COURSES),"unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course"}),
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
        "remote_start",
        json!({"platform":"binary_sensor","icon":"mdi:remote","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start"}),
    );
    add(
        &mut c,
        "flex_zone",
        json!({"platform":"sensor","icon":"mdi:view-split-horizontal","device_class":"enum","options":labels(FLEX_ZONES),"unique_id":"$deviceid-flex_zone","state_topic":"$this/flex_zone","name":"Flex zone"}),
    );
    add(
        &mut c,
        "dry_boost",
        json!({"platform":"sensor","icon":"mdi:tumble-dryer","device_class":"enum","options":labels(DRY_BOOSTS),"unique_id":"$deviceid-dry_boost","state_topic":"$this/dry_boost","name":"Dry boost"}),
    );
    add(
        &mut c,
        "night_dry",
        json!({"platform":"binary_sensor","icon":"mdi:weather-night","unique_id":"$deviceid-night_dry","state_topic":"$this/night_dry","name":"Night dry"}),
    );
    add(
        &mut c,
        "rinse_refill",
        json!({"platform":"binary_sensor","icon":"mdi:cup-water","device_class":"problem","unique_id":"$deviceid-rinse_refill","state_topic":"$this/rinse_refill","name":"Rinse aid refill"}),
    );
    add(
        &mut c,
        "rinse_level",
        json!({"platform":"number","icon":"mdi:water-plus","entity_category":"config","unique_id":"$deviceid-rinse_level","state_topic":"$this/rinse_level","command_topic":"$this/rinse_level/set","name":"Rinse aid dispenser level","min":0,"max":4,"step":1,"optimistic":true}),
    );
    add(
        &mut c,
        "machine_clean_reminder",
        json!({"platform":"switch","icon":"mdi:bell-outline","entity_category":"config","unique_id":"$deviceid-machine_clean_reminder","state_topic":"$this/machine_clean_reminder","command_topic":"$this/machine_clean_reminder/set","name":"Machine clean reminder","optimistic":true}),
    );
    add(
        &mut c,
        "chime_sound",
        json!({"platform":"switch","icon":"mdi:volume-high","entity_category":"config","unique_id":"$deviceid-chime_sound","state_topic":"$this/chime_sound","command_topic":"$this/chime_sound/set","name":"Chime sound","optimistic":true}),
    );
    add(
        &mut c,
        "clean_light_reminder",
        json!({"platform":"switch","icon":"mdi:lightbulb","entity_category":"config","unique_id":"$deviceid-clean_light_reminder","state_topic":"$this/clean_light_reminder","command_topic":"$this/clean_light_reminder/set","name":"Clean light reminder","optimistic":true}),
    );
    add(
        &mut c,
        "time_indicator",
        json!({"platform":"switch","icon":"mdi:clock-outline","entity_category":"config","unique_id":"$deviceid-time_indicator","state_topic":"$this/time_indicator","command_topic":"$this/time_indicator/set","name":"Time indicator","optimistic":true}),
    );
    add(
        &mut c,
        "auto_select",
        json!({"platform":"switch","icon":"mdi:auto-fix","entity_category":"config","unique_id":"$deviceid-auto_select","state_topic":"$this/auto_select","command_topic":"$this/auto_select/set","name":"Auto select","optimistic":true}),
    );
    add(
        &mut c,
        "end_of_cycle_tone",
        json!({"platform":"switch","icon":"mdi:music-note","entity_category":"config","unique_id":"$deviceid-end_of_cycle_tone","state_topic":"$this/end_of_cycle_tone","command_topic":"$this/end_of_cycle_tone/set","name":"End of cycle tone","optimistic":true}),
    );
    add(
        &mut c,
        "target_course",
        json!({"platform":"select","icon":"mdi:dishwasher","unique_id":"$deviceid-target_course","state_topic":"$this/target_course","command_topic":"$this/target_course/set","name":"Target course","options":start_courses}),
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
        "target_flex_zone",
        json!({"platform":"select","icon":"mdi:view-split-horizontal","unique_id":"$deviceid-target_flex_zone","state_topic":"$this/target_flex_zone","command_topic":"$this/target_flex_zone/set","name":"Target flex zone","options":labels(FLEX_ZONES)}),
    );
    add(
        &mut c,
        "target_dry_boost",
        json!({"platform":"select","icon":"mdi:tumble-dryer","unique_id":"$deviceid-target_dry_boost","state_topic":"$this/target_dry_boost","command_topic":"$this/target_dry_boost/set","name":"Target dry boost","options":labels(DRY_BOOSTS)}),
    );
    add(
        &mut c,
        "target_night_dry",
        json!({"platform":"switch","icon":"mdi:weather-night","unique_id":"$deviceid-target_night_dry","state_topic":"$this/target_night_dry","command_topic":"$this/target_night_dry/set","name":"Target night dry"}),
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
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const ID: &str = "test-id";
    const EC_POWER_ON: &str = "aa4432ec001800000002190000000100000200000080006300000000000101020305000018010000021905000219000052000200880463000000000001010203050081bb";
    const EC_DOOR_CLOSED: &str = "aa4432ec001804000002190000021900005200020088046300000000000101020305000018040000021900000219000050000200880463000000000001010203050019bb";
    const EC_REMOTE_START_ARMED: &str = "aa4432ec00180100000219050002190000520002008a0463000000000001010203050000180100000219050002190000500002008a0463000000000001010203050001bb";
    const EC_FLEX_ZONE_SELECTED: &str = "aa4432ec0018010000021905000219000022000100000063000000000001010203050000180100000219050002190000221001000000630000000000010102030500bdbb";
    const EC_FLEX_ZONE_RUNNING: &str = "aa4432ec0018010000021905000219000022100100000063000000000001010203050000180202000219050002190000201001000000630000000000010102030500acbb";
    const CMD_START_FLEX_ZONE_UPPER: &str = "aa0df0261005000040000077bb";
    const EC_FLEX_ZONE_UPPER_RUNNING: &str = "aa4432ec001801000001320300013200002000010002006300000000000101020305000018020200020c0500020c00002040010002006300000000000101020305007abb";
    const CMD_START_FLEX_ZONE_LOWER: &str = "aa0df0261005000020000057bb";
    const EC_FLEX_ZONE_LOWER_RUNNING: &str = "aa4432ec0018010000020c0500020c00002040010002006300000000000101020305000018020200021305000213000020200100020063000000000001010203050040bb";
    const EC_DRY_BOOST_LOW_RUNNING: &str = "aa4432ec001801000002280500022800002200010010006300000000000101020305000018020200022805000228000020000100100063000000000001010203050060bb";
    const EC_DRY_BOOST_HIGH_RUNNING: &str = "aa4432ec0018010000023205000232000022000100300063000000000001010203050000180202000232050002320000200001003000630000000000010102030500c8bb";
    const EC_NIGHT_DRY_SELECTED: &str = "aa4432ec0018010000023205000232000022000100300063000000000001010203050000180100000232050002320000a2000100300063000000000001010203050049bb";
    const EC_NIGHT_DRY_RUNNING: &str = "aa4432ec00180100000232050002320000a2000100300063000000000001010203050000180202000232050002320000a00001003000630000000000010102030500c8bb";
    const CMD_START_DRY_BOOST_HIGH: &str = "aa0df02610050000000300b0bb";
    const CMD_START_NIGHT_DRY: &str = "aa0df02610050000000400b3bb";
    const CMD_START_STEAM: &str = "aa0df0261005000080000037bb";
    const CMD_START_HIGH_TEMP: &str = "aa0df02610050000080000bfbb";
    const EC_ONE_HOUR_SELECTED: &str = "aa4432ec0018010000021905000219000022000100000063000000000001010203050000180100000100120001000000220001000000630000000000010102030500e4bb";
    const EC_ONE_HOUR_RUNNING: &str = "aa4432ec0018010000010012000100000022000100000063000000000001010203050000180202000100120001000000200001000000630000000000010102030500debb";
    const CMD_RINSE_LEVEL_1: &str = "aa0ef02601007c00000000001ebb";
    const EC_RINSE_LEVEL_1: &str = "aa4432ec00180100000219050002190000500002008a0463000000000001010203050000180100000219050002190000500001008a0463000000000001010203050004bb";
    const CMD_MC_REMINDER_ON: &str = "aa0ef02601007d000000000019bb";
    const EC_MC_REMINDER_ON: &str = "aa4432ec00180100000219050002190000500001008a0463000000000001010203050000180100000219050002190000700001008a0463000000000001010203050025bb";
    const CMD_TIME_INDICATOR_OFF: &str = "aa0ef02601006d000000000069bb";
    const EC_TIME_INDICATOR_OFF: &str = "aa4432ec00180100000219050002190000700001008a0463000000000001010203050000180100000219050002190000700001008204630000000000010102030500ddbb";
    const CMD_CHIME_OFF: &str = "aa0ef02601006900000000006dbb";
    const EC_CHIME_OFF: &str = "aa4432ec001801000002190500021900007000010082046300000000000101020305000018010000021905000219000070000100020463000000000001010203050055bb";
    const CMD_AUTO_SELECT_OFF: &str = "aa0ef02601004900000000004dbb";
    const EC_AUTO_SELECT_OFF: &str = "aa4432ec001801000002190500021900007000010002046300000000000101020305000018010000021905000219000060000100020463000000000001010203050025bb";
    const CMD_CLEAN_LIGHT_OFF: &str = "aa0ef026010041000000000045bb";
    const EC_CLEAN_LIGHT_OFF: &str = "aa4432ec001801000002190500021900006000010002046300000000000101020305000018010000021905000219000020000100020463000000000001010203050075bb";
    const CMD_START_NORMAL: &str = "aa0df02610050000000000b7bb";
    const CMD_START_AUTO: &str = "aa0df026100100000000008bbb";
    const CMD_START_HEAVY_DELAYED: &str = "aa0df02610020100000000b5bb";
    const EC_HEAVY_DELAYED_RUNNING: &str = "aa4432ec001801000002220100022200002000010002006300000000000101020305000018020100022f0200022f003b20010100020063000000000001010203050018bb";
    const CMD_START_DELICATE_DELAYED: &str = "aa0df02610030a00000000bfbb";
    const EC_DELICATE_DELAYED_RUNNING: &str = "aa4432ec001801000002130500021300002020010002006300000000000101020305000018020100013203000132093b20010100020063000000000001010203050034bb";
    const CMD_START_REFRESH: &str = "aa0df026100700008004003dbb";
    const EC_REFRESH_RUNNING: &str = "aa4432ec0018010000013203000132000020000100020063000000000001010203050000180203000039070000390000a080010002006300000000000101020305001dbb";
    const CMD_START_DOWNLOAD_CYCLE: &str = "aa0df026100b0000000000bdbb";
    const EC_DOWNLOAD_CYCLE_RUNNING: &str = "aa4432ec00180100000039070000390000a080010002006300000000000101020305000018020200010e0901010e000020000100020063000001000001010203050052bb";
    const EC_DOWNLOAD_SLOT_NOW_RINSE: &str = "aa4432ec0018010000010e0901010e0000600002000a046300000100000101020305000018010000000c0601000c0000600002000a046300000300000301020305001cbb";
    const EC_DOWNLOAD_CYCLE_RINSE_RUNNING: &str = "aa4432ec0018010000000c0601000c0000600002000a046300000300000303020305000018020300000c0601000c0000600002000a0463000003000003030203050019bb";
    const EC_RUNNING: &str = "aa4432ec00180100000219050002190000200001000200630000000000010102030500001802020002190500021900002000010002006300000000000101020305008ebb";
    const CMD_PAUSE: &str = "aa07f026138fbb";
    const EC_PAUSED: &str = "aa4432ec001802020002190500021800002000010002006300000000000101020305000018030200021905000218000020000100020063000000000001010203050088bb";
    const CMD_CANCEL: &str = "aa07f026118dbb";
    const CMD_RESUME: &str = "aa07f026148ebb";
    const EC_CANCEL_DRAINING: &str = "aa4432ec001803020002190500021800002000010002006300000000000101020305000018026300021900000001000000000000020064000000000001010203050055bb";
    const EC_STANDBY_AFTER_CANCEL: &str = "aa4432ec001802630002190000000100000000000002006400000000000101020305000818040000021900000001000000000000000064000000000001010203050092bb";
    const EC_NATURAL_COMPLETION: &str = "aa4432ec0018020400000c060100010000600002000a046300000300000303020305000018050500000c060100010000600002000a0463000003000003030203050015bb";
    const EC_RINSE_REFILL_ON: &str = "aa4432ec00180100000219050002190000620002000a0463000000000003030203050000180100000219050002190000660002000a04630000000000030302030500d7bb";
    const STATISTICS_ZERO: &str = "aa0b323e000000000070bb";
    const REAL_0A: &str = "aaff320a005f00028100010a86004d0a053230342d3100053230342d3200053230342d33000644572d312d35000644572d332d33000644572d352d3100053230342d3600063230342d313100063230342d313200063230342d313300feeabb";
    const REAL_00_ACK: &str = "aa08320026005fbb";
    const REAL_27_MARKER: &str = "aa073227005fbb";
    const REAL_D8: &str = "aa0732d800eebb";
    // Synthetic: class byte 0x30, never seen from this device.
    const FOREIGN: &str = "aa2330eb001b000029002900000000000100000000280000000100000064000000b6bb";

    fn meta() -> Metadata {
        Metadata::new("N02", "N02", "1")
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

    /// Everything after the connect-time status query.
    fn commands(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .outbox()
            .into_iter()
            .skip(1)
            .map(|p| hex_encode(&p).to_ascii_lowercase())
            .collect()
    }

    fn status_keys(ha: &MockHaConnection) -> Vec<String> {
        let mut keys: Vec<String> = ha
            .device(ID)
            .unwrap()
            .properties
            .keys()
            .filter(|k| !k.starts_with("target_"))
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn connect_sends_the_family_wide_status_query() {
        let (_, thinq, _) = feed(&[]);
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]).to_ascii_lowercase(),
            "aa0ef0ed1121010000001800b5bb"
        );
    }

    #[test]
    fn power_on_reports_ready_the_default_course_and_an_open_door() {
        let (ha, _, _) = feed(&[EC_POWER_ON]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "rinse_level").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn the_door_bit_is_isolated_by_a_real_close() {
        let (ha, _, _) = feed(&[EC_POWER_ON]);
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        let (ha, _, _) = feed(&[EC_DOOR_CLOSED]);
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn remote_start_arms_at_the_panel_and_is_read_back_independently_of_the_door() {
        let (ha, _, _) = feed(&[EC_REMOTE_START_ARMED]);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn the_start_button_is_available_only_while_remote_start_is_armed() {
        let (ha, _, _) = feed(&[EC_REMOTE_START_ARMED]);
        let info = ha.device(ID).unwrap();
        let components = &info.config.as_ref().unwrap().components;
        assert_eq!(
            components["start_course"]["availability"][2],
            json!({"topic":"$this/remote_start","payload_available":"ON","payload_not_available":"OFF"})
        );
    }

    #[test]
    fn flex_zone_dual_is_read_from_the_options_byte_selected_before_start_and_carried_through() {
        let (ha, _, _) = feed(&[EC_FLEX_ZONE_SELECTED]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "flex_zone").as_deref(), Some("Dual"));

        let (ha, _, _) = feed(&[EC_FLEX_ZONE_RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "flex_zone").as_deref(), Some("Dual"));
    }

    #[test]
    fn flex_zone_dual_reproduces_the_confirmed_options_bit_in_the_start_command() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_flex_zone", "Dual");
        dev.set_property("start_course", "PRESS");
        // aa 0d f0 26 10 <course> <delay> 00 <opt3>: opt3 is hex chars 16-18.
        assert_eq!(&commands(&thinq)[0][16..18], "10");
    }

    #[test]
    fn flex_zone_upper_reproduces_a_real_captured_command_and_readback() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_flex_zone", "Upper");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_FLEX_ZONE_UPPER]);

        let (ha, _, _) = feed(&[EC_FLEX_ZONE_UPPER_RUNNING]);
        assert_eq!(prop(&ha, "flex_zone").as_deref(), Some("Upper"));
    }

    #[test]
    fn flex_zone_lower_reproduces_a_real_captured_command_and_readback() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_flex_zone", "Lower");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_FLEX_ZONE_LOWER]);

        let (ha, _, _) = feed(&[EC_FLEX_ZONE_LOWER_RUNNING]);
        assert_eq!(prop(&ha, "flex_zone").as_deref(), Some("Lower"));
    }

    #[test]
    fn dry_boost_reads_off_low_high_from_flags2_confirmed_against_the_display_at_each_step() {
        let (ha, _, _) = feed(&[EC_DRY_BOOST_LOW_RUNNING]);
        assert_eq!(prop(&ha, "dry_boost").as_deref(), Some("Low"));
        let (ha, _, _) = feed(&[EC_DRY_BOOST_HIGH_RUNNING]);
        assert_eq!(prop(&ha, "dry_boost").as_deref(), Some("High"));
    }

    #[test]
    fn night_dry_is_read_from_flags1_selected_before_start_and_carried_through() {
        let (ha, _, _) = feed(&[EC_NIGHT_DRY_SELECTED]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));

        let (ha, _, _) = feed(&[EC_NIGHT_DRY_RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));
        // Dry Boost High was still selected in the same session; flags1/flags2 are independent.
        assert_eq!(prop(&ha, "dry_boost").as_deref(), Some("High"));
    }

    #[test]
    fn rinse_refill_is_read_from_flags1_isolated_from_a_real_live_use_capture() {
        let (ha, _, _) = feed(&[EC_RINSE_REFILL_ON]);
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("ON"));
        let (ha, _, _) = feed(&[EC_NIGHT_DRY_SELECTED]);
        assert_eq!(prop(&ha, "rinse_refill").as_deref(), Some("OFF"));
    }

    #[test]
    fn dry_boost_night_dry_steam_and_high_temp_each_reproduce_a_real_isolated_start() {
        for (target, value, expected) in [
            ("target_dry_boost", "High", CMD_START_DRY_BOOST_HIGH),
            ("target_night_dry", "ON", CMD_START_NIGHT_DRY),
            ("target_steam", "ON", CMD_START_STEAM),
            ("target_high_temp", "ON", CMD_START_HIGH_TEMP),
        ] {
            let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
            dev.set_property(target, value);
            dev.set_property("start_course", "PRESS");
            assert_eq!(commands(&thinq), vec![expected], "{target}");
        }
    }

    #[test]
    fn a_settings_write_snapshots_the_read_back_state_byte_for_byte_as_the_app_sent_it() {
        let steps = [
            (
                EC_REMOTE_START_ARMED,
                "rinse_level",
                "1",
                CMD_RINSE_LEVEL_1,
                EC_RINSE_LEVEL_1,
            ),
            (
                EC_RINSE_LEVEL_1,
                "machine_clean_reminder",
                "ON",
                CMD_MC_REMINDER_ON,
                EC_MC_REMINDER_ON,
            ),
            (
                EC_MC_REMINDER_ON,
                "time_indicator",
                "OFF",
                CMD_TIME_INDICATOR_OFF,
                EC_TIME_INDICATOR_OFF,
            ),
            (
                EC_TIME_INDICATOR_OFF,
                "chime_sound",
                "OFF",
                CMD_CHIME_OFF,
                EC_CHIME_OFF,
            ),
            (
                EC_CHIME_OFF,
                "auto_select",
                "OFF",
                CMD_AUTO_SELECT_OFF,
                EC_AUTO_SELECT_OFF,
            ),
            (
                EC_AUTO_SELECT_OFF,
                "clean_light_reminder",
                "OFF",
                CMD_CLEAN_LIGHT_OFF,
                EC_CLEAN_LIGHT_OFF,
            ),
        ];
        for (status, setting, value, expected, readback) in steps {
            let (_, thinq, dev) = feed(&[status]);
            dev.set_property(setting, value);
            assert_eq!(commands(&thinq), vec![expected], "{setting}");
            let (ha, _, _) = feed(&[readback]);
            assert_eq!(prop(&ha, setting).as_deref(), Some(value), "{setting}");
        }
    }

    #[test]
    fn settings_writes_are_refused_until_a_status_record_has_been_received() {
        let (_, thinq, dev) = feed(&[]);
        dev.set_property("rinse_level", "1");
        dev.set_property("chime_sound", "OFF");
        assert!(commands(&thinq).is_empty());

        let (_, thinq, dev) = feed(&[EC_RINSE_LEVEL_1]);
        dev.set_property("rinse_level", "9");
        assert!(commands(&thinq).is_empty());
    }

    #[test]
    fn a_start_reproduces_byte_for_byte_the_command_lg_was_captured_sending() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_NORMAL]);

        let (ha, _, _) = feed(&[EC_RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
    }

    #[test]
    fn one_hour_is_also_selectable_and_matches_a_real_panel_session() {
        let (ha, _, _) = feed(&[EC_ONE_HOUR_SELECTED]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("1 Hour"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("60"));

        let (ha, _, _) = feed(&[EC_ONE_HOUR_RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("1 Hour"));

        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_course", "1 Hour");
        dev.set_property("start_course", "PRESS");
        // aa 0d f0 26 10 <course>: the course is hex chars 10-12.
        assert_eq!(&commands(&thinq)[0][10..12], "12");
    }

    #[test]
    fn auto_reproduces_the_real_app_driven_start_command() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_course", "Auto");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_AUTO]);
    }

    #[test]
    fn heavy_with_a_real_delay_reproduces_the_command_and_the_delayed_start_readback() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_course", "Heavy");
        dev.set_property("target_delay", "1");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_HEAVY_DELAYED]);

        let (ha, _, _) = feed(&[EC_HEAVY_DELAYED_RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Delayed Start"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Heavy"));
        assert_eq!(prop(&ha, "delay_start_time").as_deref(), Some("59"));
    }

    #[test]
    fn delicate_with_a_10_hour_delay_reproduces_the_real_command_and_readback() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_course", "Delicate");
        dev.set_property("target_delay", "10");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_DELICATE_DELAYED]);

        let (ha, _, _) = feed(&[EC_DELICATE_DELAYED_RUNNING]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Delicate"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Delayed Start"));
        // 9:59, just under the commanded 10:00.
        assert_eq!(prop(&ha, "delay_start_time").as_deref(), Some("599"));
    }

    #[test]
    fn refresh_reproduces_a_real_start_steam_included_as_the_app_sent_it() {
        // The app sends Steam explicitly for Refresh; rethink does not add or enforce it.
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_course", "Refresh");
        dev.set_property("target_steam", "ON");
        dev.set_property("target_night_dry", "ON");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_REFRESH]);

        let (ha, _, _) = feed(&[EC_REFRESH_RUNNING]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Refresh"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "night_dry").as_deref(), Some("ON"));
    }

    #[test]
    fn download_cycle_reproduces_the_real_command_and_the_readback_reports_the_underlying_course() {
        let (_, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_course", "Download Cycle");
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_DOWNLOAD_CYCLE]);

        let (ha, _, _) = feed(&[EC_DOWNLOAD_CYCLE_RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Machine Clean"));
    }

    #[test]
    fn a_different_downloaded_cycle_reports_its_own_course_once_the_slot_changes() {
        let (ha, _, _) = feed(&[EC_DOWNLOAD_SLOT_NOW_RINSE]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Rinse"));

        let (ha, _, _) = feed(&[EC_DOWNLOAD_CYCLE_RINSE_RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Rinse"));
    }

    #[test]
    fn a_course_outside_the_observed_set_is_refused_rather_than_started() {
        // Machine Clean is a real course here, but only ever ran through Download Cycle.
        let (ha, thinq, dev) = feed(&[EC_REMOTE_START_ARMED]);
        dev.set_property("target_course", "Machine Clean");
        assert_eq!(prop(&ha, "target_course").as_deref(), Some("Normal"));
        dev.set_property("start_course", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_START_NORMAL]);
    }

    #[test]
    fn pause_and_resume_send_the_captured_packets() {
        let (_, thinq, dev) = feed(&[EC_RUNNING]);
        dev.set_property("pause", "PRESS");
        dev.set_property("resume", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_PAUSE, CMD_RESUME]);
        let (ha, _, _) = feed(&[EC_PAUSED]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Pause"));
    }

    #[test]
    fn cancel_sends_the_captured_packet_and_the_drain_reports_running_with_process_cancel() {
        let (_, thinq, dev) = feed(&[EC_PAUSED]);
        dev.set_property("cancel", "PRESS");
        assert_eq!(commands(&thinq), vec![CMD_CANCEL]);

        let (ha, _, _) = feed(&[EC_CANCEL_DRAINING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Running"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("Cancel"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Off"));
    }

    #[test]
    fn the_drain_finishes_on_its_own_into_standby_and_clears_remote_start() {
        let (ha, _, _) = feed(&[EC_STANDBY_AFTER_CANCEL]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        // The appliance's own reading is stuck at 0:01 here; gated to 0.
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
    }

    #[test]
    fn a_natural_completion_is_gated_the_same_way_with_no_stuck_minute_at_end() {
        let (ha, _, _) = feed(&[EC_NATURAL_COMPLETION]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "process").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        // initial_time is not gated: it still says how long that cycle was.
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("12"));
    }

    #[test]
    fn statistics_publish_accumulated_watt_hours() {
        let (ha, _, _) = feed(&[STATISTICS_ZERO]);
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
    }

    #[test]
    fn the_undecoded_frame_types_this_appliance_really_sends_publish_nothing() {
        let (ha, _, _) = feed(&[REAL_0A, REAL_00_ACK, REAL_27_MARKER, REAL_D8]);
        assert!(status_keys(&ha).is_empty());
    }

    #[test]
    fn a_frame_from_another_appliance_class_is_ignored() {
        let (ha, _, _) = feed(&[FOREIGN]);
        assert!(status_keys(&ha).is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("N02").is_some());
    }
}
