//! Y_V8_Y___W.B32QEUK washer. 39-byte records inside a class `0x20` envelope.
//!
//! Course names are this dial, not the shared washer table (`0x3a` is AI Wash,
//! `0x0c` is Speed 14). Settings stay pending until Run programme sends one
//! `F0 26` frame. `auto_ack` is on so an unacked envelope cannot stall the modem.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::set_power_start_pause;
use crate::washer_common::{
    error_message, error_options, soil_level, spin_options, spin_value, state_name, state_options,
    temperature_value, SOIL_LABELS, TEMPERATURE_LABELS,
};
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x20;
const RECORD_LEN: usize = 39;

const COURSES: &[(u8, &str)] = &[
    (0x00, "Not selected"),
    (0x3a, "AI Wash"),
    (0x01, "Cotton"),
    (0x02, "Easy Care"),
    (0x04, "Eco 40-60"),
    (0x05, "Duvet"),
    (0x07, "Mix"),
    (0x08, "Sports Wear"),
    (0x0c, "Speed 14"),
    (0x12, "Tub Clean"),
    (0x1b, "Wool"),
    (0x20, "Delicate"),
    (0x2d, "Allergy SpaSteam"),
    (0x31, "Turbo Wash 39"),
    (0x0e, "Rinse + Spin"),
    (0x17, "Spin only"),
    (0x09, "Silent Wash"),
    (0x30, "Quick Tub Clean"),
    (0x3b, "MicroPlastic Care"),
];

const RINSES: &[&str] = &[
    "Not selected",
    "Normal",
    "Rinse+",
    "Rinse++",
    "Normal + Hold",
    "Rinse+ + Hold",
];

const SMART: &[(u8, &str)] = &[
    (0x34, "Kids Wear"),
    (0x35, "School Uniform"),
    (0x36, "Swimming Wear"),
    (0x37, "Rainy Season"),
    (0x38, "Gym Clothes"),
    (0x39, "Jeans"),
    (0x3a, "Blanket"),
    (0x3b, "Sweat Stain"),
    (0x3e, "Single Garment"),
    (0x3f, "Colour Protection"),
    (0x40, "Noise Minimize"),
    (0x47, "Baby Care"),
    (0x48, "Hygiene"),
    (0x49, "Small Load"),
    (0x4a, "Lingerie"),
    (0x4b, "Wool"),
    (0x4c, "Skin Care"),
    (0x4d, "Cold Wash"),
    (0x64, "Rinse + Spin"),
    (0x65, "Lightly Soiled Items"),
    (0x66, "Minimize Detergent Residue"),
    (0x67, "1hr. Wash"),
    (0x6b, "Sleeve Hems and Collars"),
    (0x6c, "Juice and Food Stains"),
    (0x6f, "Minimize Wrinkles"),
    (0x71, "Quick Tub Clean"),
    (0x79, "Drain"),
    (0x7a, "Spin"),
    (0x84, "Silent Wash"),
];

const OPT_TURBO: u8 = 0x01;
const OPT_CREASE: u8 = 0x02;
const OPT_STEAM_SOFT: u8 = 0x04;
const OPT_MEDIC: u8 = 0x10;
const OPT_PREWASH: u8 = 0x40;
const OPT_STEAM: u8 = 0x80;

#[derive(Clone, Copy, Default)]
struct Settings {
    course: u8,
    soil: u8,
    spin: u8,
    temp: u8,
    rinse: u8,
    delay_end: u8,
    options: u8,
}

#[derive(Clone, Copy, Default)]
struct Pending {
    course: Option<u8>,
    soil: Option<u8>,
    spin: Option<u8>,
    temp: Option<u8>,
    rinse: Option<u8>,
    delay_end: Option<u8>,
    options: Option<u8>,
}

pub(crate) struct Extension {
    pub courses: &'static [(u8, &'static str)],
    pub extra_courses: &'static [(u8, &'static str)],
    pub extra_components: Vec<(String, Value)>,
    pub after_state: Option<Box<dyn Fn(&AabbDeviceCore, &[u8]) + Send + Sync>>,
    pub patch_command: Option<Box<dyn Fn(&mut [u8; 18]) + Send + Sync>>,
    pub handle_set: Option<Box<dyn Fn(&AabbDeviceCore, &str, &str) -> bool + Send + Sync>>,
}

impl Extension {
    fn base() -> Self {
        Self {
            courses: COURSES,
            extra_courses: &[],
            extra_components: Vec::new(),
            after_state: None,
            patch_command: None,
            handle_set: None,
        }
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    reported: Mutex<Settings>,
    pending: Mutex<Pending>,
    ext: Extension,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        Self::with_extension(ha, thinq, meta, |_| Extension::base())
    }

    pub(crate) fn with_extension<F>(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        build: F,
    ) -> Arc<Self>
    where
        F: FnOnce(Arc<AabbDeviceCore>) -> Extension,
    {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let ext = build(core.clone());
        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        base.components = components(&ext);
        core.set_config(base);
        let this = Arc::new(Self {
            core: core.clone(),
            reported: Mutex::new(Settings::default()),
            pending: Mutex::new(Pending::default()),
            ext,
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(rec) = process_enveloped_status(inner, CLASS_BYTE, RECORD_LEN) {
                    dev.process_state(rec);
                }
            }
        }));
        this
    }

    fn course_name(&self, code: u8) -> &'static str {
        self.ext
            .courses
            .iter()
            .chain(self.ext.extra_courses.iter())
            .find(|(c, _)| *c == code)
            .map(|(_, n)| *n)
            .unwrap_or("None")
    }

    fn course_code(&self, name: &str) -> Option<u8> {
        self.ext
            .courses
            .iter()
            .chain(self.ext.extra_courses.iter())
            .find(|(_, n)| *n == name)
            .map(|(c, _)| *c)
    }

    fn process_state(&self, s: &[u8]) {
        if s.len() < RECORD_LEN {
            return;
        }
        let status = s[2];
        let options = s[16];
        let lock_status = s[17];
        self.core.publish_on_off("power", status > 0);
        self.core
            .publish_property("error_message", error_message(s[8] as usize).into());
        self.core.publish_on_off("error", s[8] != 0);
        self.core
            .publish_property("status", state_name(status as usize).into());
        self.core
            .publish_property("course", self.course_name(s[7]).into());
        self.core
            .publish_property("smart_course", named(SMART, s[22]).into());
        self.core
            .publish_property("spin", spin_value(s[10] as usize).unwrap_or("None").into());
        self.core.publish_property(
            "temp",
            temperature_value(s[11] as usize).unwrap_or("None").into(),
        );
        self.core
            .publish_property("cycles", i64::from(s[23]).into());
        self.core
            .publish_on_off("remote_start", lock_status & 0x02 != 0);
        self.core
            .publish_on_off("door_lock", lock_status & 0x40 == 0);
        self.core
            .publish_on_off("child_lock", lock_status & 0x80 == 0);
        self.core
            .publish_on_off("wrinkle_care", lock_status & 0x20 != 0);
        self.core.publish_property(
            "initial_time",
            (i64::from(s[5]) * 60 + i64::from(s[6])).into(),
        );
        self.core.publish_property(
            "remaining_time",
            (i64::from(s[3]) * 60 + i64::from(s[4])).into(),
        );
        self.core.publish_property(
            "reserve_time",
            (i64::from(s[14]) * 60 + i64::from(s[15])).into(),
        );
        self.core
            .publish_property("energy", (i64::from(s[30]) * 256 + i64::from(s[31])).into());
        self.core
            .publish_property("soil", soil_level(s[9] as usize).unwrap_or("None").into());
        self.core
            .publish_property("rinse", indexed(RINSES, s[12]).into());
        self.core
            .publish_on_off("turbowash", options & OPT_TURBO != 0);
        self.core
            .publish_on_off("crease_care", options & OPT_CREASE != 0);
        self.core
            .publish_on_off("steam_softener", options & OPT_STEAM_SOFT != 0);
        self.core
            .publish_on_off("medic_rinse", options & OPT_MEDIC != 0);
        self.core
            .publish_on_off("prewash", options & OPT_PREWASH != 0);
        self.core.publish_on_off("steam", options & OPT_STEAM != 0);
        self.core.publish_on_off("ai_dd", s[18] & 0x01 != 0);

        *self.reported.lock() = Settings {
            course: s[7],
            soil: s[9],
            spin: s[10],
            temp: s[11],
            rinse: s[12],
            delay_end: s[14],
            options,
        };
        self.publish_settings();
        if let Some(hook) = &self.ext.after_state {
            hook(&self.core, s);
        }
    }

    fn effective(&self) -> Settings {
        let reported = *self.reported.lock();
        let pending = *self.pending.lock();
        Settings {
            course: pending.course.unwrap_or(reported.course),
            soil: pending.soil.unwrap_or(reported.soil),
            spin: pending.spin.unwrap_or(reported.spin),
            temp: pending.temp.unwrap_or(reported.temp),
            rinse: pending.rinse.unwrap_or(reported.rinse),
            delay_end: pending.delay_end.unwrap_or(reported.delay_end),
            options: pending.options.unwrap_or(reported.options),
        }
    }

    fn publish_settings(&self) {
        let s = self.effective();
        self.core
            .publish_property("set_course", self.course_name(s.course).into());
        self.core.publish_property(
            "set_temp",
            temperature_value(s.temp as usize).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "set_spin",
            spin_value(s.spin as usize).unwrap_or("None").into(),
        );
        self.core
            .publish_property("set_rinse", indexed(RINSES, s.rinse).into());
        self.core.publish_property(
            "set_soil",
            soil_level(s.soil as usize).unwrap_or("None").into(),
        );
        self.core
            .publish_property("set_delay_end", i64::from(s.delay_end).into());
        self.core
            .publish_on_off("set_turbowash", s.options & OPT_TURBO != 0);
        self.core
            .publish_on_off("set_crease_care", s.options & OPT_CREASE != 0);
        self.core
            .publish_on_off("set_steam_softener", s.options & OPT_STEAM_SOFT != 0);
        self.core
            .publish_on_off("set_medic_rinse", s.options & OPT_MEDIC != 0);
        self.core
            .publish_on_off("set_prewash", s.options & OPT_PREWASH != 0);
        self.core
            .publish_on_off("set_steam", s.options & OPT_STEAM != 0);
    }

    fn programme_command(&self) -> [u8; 18] {
        let s = self.effective();
        let mut cmd = [
            0xf0,
            0x26,
            s.course,
            s.soil,
            s.spin,
            s.temp,
            s.rinse,
            0,
            s.delay_end,
            0,
            0,
            s.options,
            0x03,
            0,
            0,
            0,
            0,
            0,
        ];
        if let Some(patch) = &self.ext.patch_command {
            patch(&mut cmd);
        }
        cmd
    }

    fn set_option(&self, bit: u8, on: bool) {
        let options = self.effective().options;
        self.pending.lock().options = Some(if on { options | bit } else { options & !bit });
        self.publish_settings();
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if self
            .ext
            .handle_set
            .as_ref()
            .is_some_and(|hook| hook(&self.core, prop, mqtt_value))
        {
            return;
        }
        set_power_start_pause(&self.core, prop, mqtt_value);
        match prop {
            "run_programme" => {
                let cmd = self.programme_command();
                self.pending.lock().clear_choices();
                self.core.send(&cmd);
            }
            "set_course" => {
                if let Some(code) = self.course_code(mqtt_value) {
                    self.pending.lock().course = Some(code);
                }
                self.publish_settings();
            }
            "set_temp" => {
                if let Some(code) = index_of(TEMPERATURE_LABELS, mqtt_value) {
                    self.pending.lock().temp = Some(code);
                }
                self.publish_settings();
            }
            "set_spin" => {
                if let Some(code) = spin_code(mqtt_value) {
                    self.pending.lock().spin = Some(code);
                }
                self.publish_settings();
            }
            "set_rinse" => {
                if let Some(code) = index_of(RINSES, mqtt_value) {
                    self.pending.lock().rinse = Some(code);
                }
                self.publish_settings();
            }
            "set_soil" => {
                if let Some(code) = index_of(SOIL_LABELS, mqtt_value) {
                    self.pending.lock().soil = Some(code);
                }
                self.publish_settings();
            }
            "set_delay_end" => {
                if let Ok(hours) = mqtt_value.parse::<i64>() {
                    if hours == 0 || (3..=19).contains(&hours) {
                        self.pending.lock().delay_end = Some(hours as u8);
                    }
                }
                self.publish_settings();
            }
            "set_turbowash" => self.set_option(OPT_TURBO, mqtt_value == "ON"),
            "set_crease_care" => self.set_option(OPT_CREASE, mqtt_value == "ON"),
            "set_steam_softener" => self.set_option(OPT_STEAM_SOFT, mqtt_value == "ON"),
            "set_medic_rinse" => self.set_option(OPT_MEDIC, mqtt_value == "ON"),
            "set_prewash" => self.set_option(OPT_PREWASH, mqtt_value == "ON"),
            "set_steam" => self.set_option(OPT_STEAM, mqtt_value == "ON"),
            _ => {}
        }
    }
}

impl Pending {
    fn clear_choices(&mut self) {
        *self = Pending::default();
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode("F0ED1121010000001800"));
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

pub(crate) fn dial_courses() -> &'static [(u8, &'static str)] {
    COURSES
}

pub(crate) fn smart_course_name(code: u8) -> &'static str {
    named(SMART, code)
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> &'static str {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| *n)
        .unwrap_or("None")
}

fn indexed(table: &'static [&'static str], code: u8) -> &'static str {
    table.get(code as usize).copied().unwrap_or("None")
}

fn index_of(table: &'static [&'static str], name: &str) -> Option<u8> {
    table.iter().position(|n| *n == name).map(|i| i as u8)
}

fn spin_code(name: &str) -> Option<u8> {
    if name == "Max" {
        Some(255)
    } else {
        index_of(crate::washer_common::SPIN_LABELS, name)
    }
}

fn labels(table: &'static [(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, n)| *n).collect()
}

fn components(ext: &Extension) -> std::collections::HashMap<String, Value> {
    let course_names: Vec<&str> = ext
        .courses
        .iter()
        .chain(ext.extra_courses.iter())
        .map(|(_, n)| *n)
        .collect();
    let mut c = Map::new();
    let add = |c: &mut Map<String, Value>, k: &str, v: Value| {
        c.insert(k.into(), v);
    };
    add(
        &mut c,
        "power",
        json!({"platform":"switch","unique_id":"$deviceid-power","state_topic":"$this/power","command_topic":"$this/power/set","name":"","icon":"mdi:washing-machine"}),
    );
    add(
        &mut c,
        "start",
        json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"}),
    );
    add(
        &mut c,
        "pause",
        json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"}),
    );
    add(
        &mut c,
        "status",
        json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":state_options()}),
    );
    add(
        &mut c,
        "error",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:check-circle","device_class":"problem","entity_category":"diagnostic"}),
    );
    add(
        &mut c,
        "error_message",
        json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","device_class":"enum","entity_category":"diagnostic","options":error_options()}),
    );
    add(
        &mut c,
        "course",
        json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"}),
    );
    add(
        &mut c,
        "temp",
        json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","device_class":"enum","options":TEMPERATURE_LABELS}),
    );
    add(
        &mut c,
        "spin",
        json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":spin_options()}),
    );
    add(
        &mut c,
        "cycles",
        json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycles since Tub Clean","icon":"mdi:counter"}),
    );
    add(
        &mut c,
        "remote_start",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:play-circle-outline"}),
    );
    add(
        &mut c,
        "door_lock",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","device_class":"lock"}),
    );
    add(
        &mut c,
        "energy",
        json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"}),
    );
    add(
        &mut c,
        "initial_time",
        json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Initial time"}),
    );
    add(
        &mut c,
        "remaining_time",
        json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Remaining time"}),
    );
    add(
        &mut c,
        "reserve_time",
        json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","device_class":"duration","unit_of_measurement":"min","name":"Reserve time","icon":"mdi:timer-sand"}),
    );
    add(
        &mut c,
        "child_lock",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"}),
    );
    add(
        &mut c,
        "wrinkle_care",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle care","icon":"mdi:tshirt-crew"}),
    );
    add(
        &mut c,
        "soil",
        json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:washing-machine","device_class":"enum","options":SOIL_LABELS}),
    );
    add(
        &mut c,
        "set_course",
        json!({"platform":"select","unique_id":"$deviceid-set_course","state_topic":"$this/set_course","command_topic":"$this/set_course/set","name":"Set course","icon":"mdi:tune-vertical","options":course_names}),
    );
    add(
        &mut c,
        "set_temp",
        json!({"platform":"select","unique_id":"$deviceid-set_temp","state_topic":"$this/set_temp","command_topic":"$this/set_temp/set","name":"Set temperature","icon":"mdi:thermometer","options":TEMPERATURE_LABELS}),
    );
    add(
        &mut c,
        "set_spin",
        json!({"platform":"select","unique_id":"$deviceid-set_spin","state_topic":"$this/set_spin","command_topic":"$this/set_spin/set","name":"Set spin","icon":"mdi:autorenew","options":spin_options()}),
    );
    add(
        &mut c,
        "set_rinse",
        json!({"platform":"select","unique_id":"$deviceid-set_rinse","state_topic":"$this/set_rinse","command_topic":"$this/set_rinse/set","name":"Set rinse","icon":"mdi:water","options":RINSES}),
    );
    add(
        &mut c,
        "set_soil",
        json!({"platform":"select","unique_id":"$deviceid-set_soil","state_topic":"$this/set_soil","command_topic":"$this/set_soil/set","name":"Set soil level","icon":"mdi:washing-machine","options":SOIL_LABELS}),
    );
    add(
        &mut c,
        "set_delay_end",
        json!({"platform":"number","unique_id":"$deviceid-set_delay_end","state_topic":"$this/set_delay_end","command_topic":"$this/set_delay_end/set","name":"Set delayed end","icon":"mdi:timer-sand","unit_of_measurement":"h","min":0,"max":19,"step":1}),
    );
    add(
        &mut c,
        "run_programme",
        json!({"platform":"button","unique_id":"$deviceid-run_programme","command_topic":"$this/run_programme/set","payload_press":"","name":"Run programme","icon":"mdi:play-box-outline"}),
    );
    add(
        &mut c,
        "set_steam",
        json!({"platform":"switch","unique_id":"$deviceid-set_steam","state_topic":"$this/set_steam","command_topic":"$this/set_steam/set","name":"Set steam","icon":"mdi:kettle-steam"}),
    );
    add(
        &mut c,
        "set_prewash",
        json!({"platform":"switch","unique_id":"$deviceid-set_prewash","state_topic":"$this/set_prewash","command_topic":"$this/set_prewash/set","name":"Set pre-wash","icon":"mdi:water-sync"}),
    );
    add(
        &mut c,
        "set_turbowash",
        json!({"platform":"switch","unique_id":"$deviceid-set_turbowash","state_topic":"$this/set_turbowash","command_topic":"$this/set_turbowash/set","name":"Set TurboWash","icon":"mdi:rocket-launch"}),
    );
    add(
        &mut c,
        "set_crease_care",
        json!({"platform":"switch","unique_id":"$deviceid-set_crease_care","state_topic":"$this/set_crease_care","command_topic":"$this/set_crease_care/set","name":"Set crease care","icon":"mdi:iron-outline"}),
    );
    add(
        &mut c,
        "set_steam_softener",
        json!({"platform":"switch","unique_id":"$deviceid-set_steam_softener","state_topic":"$this/set_steam_softener","command_topic":"$this/set_steam_softener/set","name":"Set steam softener","icon":"mdi:kettle-steam-outline"}),
    );
    add(
        &mut c,
        "set_medic_rinse",
        json!({"platform":"switch","unique_id":"$deviceid-set_medic_rinse","state_topic":"$this/set_medic_rinse","command_topic":"$this/set_medic_rinse/set","name":"Set medic rinse","icon":"mdi:water-check"}),
    );
    add(
        &mut c,
        "smart_course",
        json!({"platform":"sensor","unique_id":"$deviceid-smart_course","state_topic":"$this/smart_course","name":"Downloaded course","icon":"mdi:cloud-download-outline","device_class":"enum","options":labels(SMART)}),
    );
    add(
        &mut c,
        "rinse",
        json!({"platform":"sensor","unique_id":"$deviceid-rinse","state_topic":"$this/rinse","name":"Rinse","icon":"mdi:water","device_class":"enum","options":RINSES}),
    );
    add(
        &mut c,
        "turbowash",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"}),
    );
    add(
        &mut c,
        "steam",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"}),
    );
    add(
        &mut c,
        "steam_softener",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-steam_softener","state_topic":"$this/steam_softener","name":"Steam softener","icon":"mdi:kettle-steam-outline"}),
    );
    add(
        &mut c,
        "prewash",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"}),
    );
    add(
        &mut c,
        "crease_care",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-crease_care","state_topic":"$this/crease_care","name":"Crease care","icon":"mdi:iron-outline"}),
    );
    add(
        &mut c,
        "medic_rinse",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-medic_rinse","state_topic":"$this/medic_rinse","name":"Medic rinse","icon":"mdi:water-check"}),
    );
    add(
        &mut c,
        "ai_dd",
        json!({"platform":"binary_sensor","unique_id":"$deviceid-ai_dd","state_topic":"$this/ai_dd","name":"AI DD","icon":"mdi:brain"}),
    );
    for (key, value) in &ext.extra_components {
        c.insert(key.clone(), value.clone());
    }
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_trait::DeviceHandler;
    use rethink_core::device_base::wrap_aabb;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const ID: &str = "test-id";
    const INITIAL: &str = "aaff200a0039000381000100eb0027000001032603260000000000000000000000000003000011007100000000000000000000000000974ebb";
    const RUNNING: &str = "aaff200a0039000398000100eb0027000006000e000e0c0003020201000000014220000101001100710000010000000000000000000056d9bb";
    const RINSING: &str = "aaff200a00390003c6000100eb0027000007000b000e0c000302000100000001420000010600110071000001000022000000000000004970bb";
    const SPINNING: &str = "aaff200a0039000435000100eb00270000080005000e0c00000200000000000042000001070011007100000100002600000000000000af09bb";
    const END: &str = "aaff200a0039000478000100eb002700000a0000000e0c00000000000000000040000001080011007100000100002900000000000000c901bb";
    const POWER_OFF: &str = "aaff200a0039000487000100eb00270000000000000e0c000000000000000000000000000a0011007100000100002900000000000000cb19bb";
    const PAIR_ON: &str = "aaff200a006000027f000100ec004e00000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000e0048000000000000000000000000006e4bbb";
    const PAIR_COURSE: &str = "aaff200a0060000286000100ec004e00000100000000000000000000000000000000000000000e004800000000000000000000000000000001003b003b3a00030a0401000000000001000200000e0048000004000000000000000000004282bb";
    const PAIR_STEAM: &str = "aaff200a0060000288000100ec004e000001003b003b3a00030a0401000000000001000200000e004800000400000000000000000000000001013201323a00030a0601000000800001000300000e0048000004000000000000000000000709bb";
    const PAIR_OFF: &str = "aaff200a006000028b000100ec004e000001013201323a00030a0601000000800001000300000e004800000400000000000000000000000000013201323a0000000000000000000001000301000e004800000400000000000000000000befabb";
    const PAIR_HYGIENE: &str = "aaff200a006000074f000100ec004e00000100000000000000000000000000004200000000000e004800000000000000000000000000000001023002302d00030a0601000000804200000000480e00480000000000000000000000c000b72abb";

    fn meta() -> Metadata {
        Metadata::new("Y_V8_Y___W.B32QEUK", "Y_V8_Y___W.B32QEUK", "2.11.207")
    }

    fn feed(frames: &[&str]) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(ID)?.properties.get(name).map(|p| p.as_string())
    }

    fn inner(packet: &[u8]) -> Vec<u8> {
        packet[2..packet.len() - 2].to_vec()
    }

    #[test]
    fn initial_and_running_frames_use_the_39_byte_record() {
        let (ha, thinq, _) = feed(&[INITIAL]);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("218"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("218"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("17"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
        let acks: Vec<String> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            acks,
            vec![hex_encode(&wrap_aabb(&hex_decode("f0000a040381")))]
        );
        assert!(thinq.outbox().is_empty());

        let (ha, _, _) = feed(&[RUNNING]);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Speed 14"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("20 °C"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("14"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("14"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "ai_dd").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Normal"));
    }

    #[test]
    fn spin_holds_and_energy_accumulates_across_the_cycle() {
        let (ha, thinq, _) = feed(&[RUNNING]);
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        thinq.emit_data(&hex_decode(RINSING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("11"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("34"));
        thinq.emit_data(&hex_decode(SPINNING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Spinning"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("38"));
        thinq.emit_data(&hex_decode(END));
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("41"));
        thinq.emit_data(&hex_decode(POWER_OFF));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("41"));
    }

    #[test]
    fn ec_pairs_publish_the_replacing_record() {
        let (ha, _, _) = feed(&[PAIR_ON]);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("14"));

        let (ha, _, _) = feed(&[PAIR_COURSE]);
        assert_eq!(prop(&ha, "course").as_deref(), Some("AI Wash"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("59"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("59"));
        assert_eq!(prop(&ha, "ai_dd").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Normal"));

        let (ha, _, _) = feed(&[PAIR_STEAM]);
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("AI Wash"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("110"));
        assert_eq!(prop(&ha, "crease_care").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "medic_rinse").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam_softener").as_deref(), Some("OFF"));

        let (ha, thinq, _) = feed(&[PAIR_STEAM]);
        thinq.emit_data(&hex_decode(PAIR_OFF));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));

        let (ha, thinq, _) = feed(&[]);
        thinq.emit_data(&hex_decode("001122"));
        thinq.emit_data(&hex_decode("aa08200a01020304bb"));
        thinq.emit_data(&hex_decode("aa13200a001300028b000100ec004e00010203bb"));
        assert!(ha.device(ID).unwrap().properties.is_empty());
    }

    #[test]
    fn downloaded_course_is_separate_from_its_base() {
        let (ha, _, _) = feed(&[PAIR_HYGIENE]);
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("Hygiene"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergy SpaSteam"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "rinse").as_deref(), Some("Normal"));

        let mut frame = hex_decode(PAIR_HYGIENE);
        frame[61] = 0x63;
        frame[76] = 0x5a;
        let (ha, thinq, _) = feed(&[]);
        thinq.emit_data(&frame);
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "smart_course").as_deref(), Some("None"));
    }

    #[test]
    fn commands_and_the_programme_frame_match_the_app() {
        let (_, thinq, dev) = feed(&[]);
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF0ED1121010000001800B5BB"
        );
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA08F02A010098BB");
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F0240101009CBB");
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F02404010099BB");
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F02405010098BB");
        thinq.reset_recorder();
        dev.set_property("does-not-exist", "whatever");
        assert!(thinq.outbox().is_empty());

        let (_, thinq, dev) = feed(&[]);
        dev.set_property("set_course", "Allergy SpaSteam");
        dev.set_property("set_soil", "Normal");
        dev.set_property("set_spin", "Max");
        dev.set_property("set_temp", "60 °C");
        dev.set_property("set_rinse", "Normal");
        dev.set_property("set_steam", "ON");
        dev.set_property("run_programme", "");
        assert_eq!(
            hex_encode(thinq.outbox().last().unwrap()),
            "AA16F0262D03FF06010000000080030000000000DABB"
        );
    }

    #[test]
    fn pending_settings_override_the_report_until_the_programme_runs() {
        let (_ha, thinq, dev) = feed(&[PAIR_HYGIENE]);
        thinq.reset_recorder();
        dev.set_property("run_programme", "");
        let body = inner(thinq.outbox().last().unwrap());
        assert_eq!(body[2], 0x2d);
        assert_eq!(body[4], 0x0a);
        assert_eq!(body[5], 0x06);
        assert_eq!(body[11], 0x80);
        assert_eq!(body[12], 0x03);

        let (ha, thinq, dev) = feed(&[PAIR_HYGIENE]);
        assert_eq!(prop(&ha, "set_temp").as_deref(), Some("60 °C"));
        dev.set_property("set_temp", "30 °C");
        assert_eq!(prop(&ha, "set_temp").as_deref(), Some("30 °C"));
        thinq.reset_recorder();
        dev.set_property("run_programme", "");
        assert_eq!(inner(thinq.outbox().last().unwrap())[5], 0x03);
        thinq.emit_data(&hex_decode(PAIR_HYGIENE));
        assert_eq!(prop(&ha, "set_temp").as_deref(), Some("60 °C"));

        let (_, thinq, dev) = feed(&[PAIR_HYGIENE]);
        thinq.reset_recorder();
        dev.set_property("set_prewash", "ON");
        dev.set_property("run_programme", "");
        assert_eq!(inner(thinq.outbox().last().unwrap())[11], 0x80 | 0x40);
        thinq.emit_data(&hex_decode(PAIR_HYGIENE));
        dev.set_property("set_steam", "OFF");
        dev.set_property("run_programme", "");
        assert_eq!(inner(thinq.outbox().last().unwrap())[11], 0x00);

        let (ha, _, dev) = feed(&[POWER_OFF]);
        let info = ha.device(ID).unwrap();
        let components = &info.config.as_ref().unwrap().components;
        for name in [
            "set_course",
            "set_temp",
            "set_spin",
            "set_rinse",
            "set_soil",
        ] {
            let options = components[name]["options"].as_array().unwrap();
            let published = prop(&ha, name).unwrap();
            assert!(
                options
                    .iter()
                    .any(|v| v.as_str() == Some(published.as_str())),
                "{name} published {published}"
            );
            assert!(options.iter().all(|v| v.as_str() != Some("None")));
        }
        assert!(components["status"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("Washing")));
        dev.set_property("set_delay_end", "5");
        assert_eq!(prop(&ha, "set_delay_end").as_deref(), Some("5"));
        dev.set_property("set_delay_end", "1");
        assert_eq!(prop(&ha, "set_delay_end").as_deref(), Some("5"));
        dev.set_property("set_delay_end", "0");
        assert_eq!(prop(&ha, "set_delay_end").as_deref(), Some("0"));
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("Y_V8_Y___W.B32QEUK").is_some());
    }
}
