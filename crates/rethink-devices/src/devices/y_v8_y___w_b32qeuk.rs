//! Y_V8_Y___W.B32QEUK washer. 39-byte records inside a class `0x20` envelope.
//!
//! Course names are this dial, not the shared washer table (`0x3a` is AI Wash,
//! `0x0c` is Speed 14). Settings stay pending until Run programme sends one
//! `F0 26` frame. `auto_ack` is on so an unacked envelope cannot stall the modem.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::set_power_start_pause;
use crate::js::number;
use crate::washer_common::{
    error_message, error_options, fy_steam_name, soil_level, spin_options, spin_value,
    temperature_value, washer_status_options, FY_STEAM, SOIL_LABELS, TEMPERATURE_LABELS,
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
    (0x86, "MicroPlastic Care"),
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
    /// Hours as `Number()` parsed them: 4.5 is kept and published, and sent as 4.
    delay_end: f64,
    options: u8,
    /// The selected downloaded course, which a start repeats unless a course was picked.
    smart_course: u8,
}

/// Settings chosen but not yet run, as wire codes. Cleared once a programme runs.
#[derive(Clone, Copy, Default)]
pub(crate) struct Pending {
    pub course: Option<u8>,
    pub soil: Option<u8>,
    pub spin: Option<u8>,
    pub temp: Option<u8>,
    pub rinse: Option<u8>,
    pub delay_end: Option<f64>,
    pub options: Option<u8>,
}

/// What a sibling model changes, in place of upstream's subclass overrides. Each hook
/// defaults to this model's own behaviour.
pub(crate) trait Extension: Send + Sync + 'static {
    /// Courses beyond this dial's, decoded and offered by Set course.
    fn extra_courses(&self) -> &'static [(u8, &'static str)] {
        &[]
    }

    /// Adds, replaces or tombstones discovery components.
    fn components(&self, _components: &mut Map<String, Value>) {}

    /// Every AABB payload, after any status record in it has been handled.
    fn process_aabb(&self, _dev: &Device, _inner: &[u8]) {}

    /// A status record, after this model's own fields and settings are published.
    fn process_state(&self, _dev: &Device, _s: &[u8]) {}

    /// The Set course label, in place of the chosen or reported course's.
    fn course_setting(&self, _dev: &Device) -> Option<&'static str> {
        None
    }

    /// Edits a Run programme frame before it is sent.
    fn programme_command(&self, _dev: &Device, _cmd: &mut [u8; 18]) {}

    /// Sees every write first. Returns true when this model must not handle it too.
    fn set_property(&self, _dev: &Device, _prop: &str, _value: &str) -> bool {
        false
    }
}

struct Base;

impl Extension for Base {}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    reported: Mutex<Settings>,
    pending: Mutex<Pending>,
    ext: Box<dyn Extension>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        Self::with_extension(ha, thinq, meta, Base)
    }

    pub(crate) fn with_extension(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
        ext: impl Extension,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        base.components = components(&ext);
        core.set_config(base);
        let this = Arc::new(Self {
            core: core.clone(),
            reported: Mutex::new(Settings::default()),
            pending: Mutex::new(Pending::default()),
            ext: Box::new(ext),
        });
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                if let Some(rec) = process_enveloped_status(inner, CLASS_BYTE, RECORD_LEN) {
                    dev.process_state(rec);
                }
                dev.ext.process_aabb(&dev, inner);
            }
        }));
        this
    }

    pub(crate) fn core(&self) -> &AabbDeviceCore {
        &self.core
    }

    pub(crate) fn pending(&self) -> Pending {
        *self.pending.lock()
    }

    pub(crate) fn drop_pending_course(&self) {
        self.pending.lock().course = None;
    }

    fn courses(&self) -> impl Iterator<Item = &'static (u8, &'static str)> {
        COURSES.iter().chain(self.ext.extra_courses())
    }

    fn course_name(&self, code: u8) -> &'static str {
        self.courses()
            .find(|(c, _)| *c == code)
            .map(|(_, n)| *n)
            .unwrap_or("None")
    }

    fn course_code(&self, name: &str) -> Option<u8> {
        self.courses().find(|(_, n)| *n == name).map(|(c, _)| *c)
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
            .publish_property("status", fy_steam_name(status).into());
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
            delay_end: f64::from(s[14]),
            options,
            smart_course: s[22],
        };
        self.publish_settings();
        self.ext.process_state(self, s);
    }

    pub(crate) fn reported_smart_course(&self) -> u8 {
        self.reported.lock().smart_course
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
            smart_course: if pending.course.is_some() {
                0
            } else {
                reported.smart_course
            },
        }
    }

    /// Republishes every set_* entity, after each state record and each write.
    pub(crate) fn publish_settings(&self) {
        let s = self.effective();
        let course = self
            .ext
            .course_setting(self)
            .unwrap_or_else(|| self.course_name(s.course));
        self.core.publish_property("set_course", course.into());
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
            .publish_property("set_delay_end", s.delay_end.into());
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

    /// `F0 26 <course> <soil> <spin> <temp> <rinse> <dry> <reserve h> <reserve min> <load>
    /// <options> <flags> 00 <downloaded course> 00 00 00`, as the LG app sends it.
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
            s.delay_end as u8,
            0,
            0,
            s.options,
            0x03,
            0,
            s.smart_course,
            0,
            0,
            0,
        ];
        self.ext.programme_command(self, &mut cmd);
        cmd
    }

    /// Sends the programme and drops the pending choices: the appliance is the authority again.
    pub(crate) fn run_programme(&self) {
        let cmd = self.programme_command();
        self.pending.lock().clear_choices();
        self.core.send(&cmd);
    }

    fn set_option(&self, bit: u8, on: bool) {
        let options = self.effective().options;
        self.pending.lock().options = Some(if on { options | bit } else { options & !bit });
        self.publish_settings();
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if self.ext.set_property(self, prop, mqtt_value) {
            return;
        }
        set_power_start_pause(&self.core, prop, mqtt_value);
        match prop {
            "run_programme" => self.run_programme(),
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
                // modelJson allows 3..19; 0 is no delayed end.
                if let Some(hours) =
                    number(mqtt_value).filter(|h| *h == 0.0 || (3.0..=19.0).contains(h))
                {
                    self.pending.lock().delay_end = Some(hours);
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

pub(crate) fn smart_courses() -> &'static [(u8, &'static str)] {
    SMART
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

fn components(ext: &dyn Extension) -> std::collections::HashMap<String, Value> {
    let course_names: Vec<&str> = COURSES
        .iter()
        .chain(ext.extra_courses())
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
        json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":washer_status_options(FY_STEAM)}),
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
        json!({"platform":"select","unique_id":"$deviceid-set_course","state_topic":"$this/set_course","command_topic":"$this/set_course/set","name":"Set course","icon":"mdi:tune-vertical","options":course_names,"entity_category":"config"}),
    );
    add(
        &mut c,
        "set_temp",
        json!({"platform":"select","unique_id":"$deviceid-set_temp","state_topic":"$this/set_temp","command_topic":"$this/set_temp/set","name":"Set temperature","icon":"mdi:thermometer","options":TEMPERATURE_LABELS,"entity_category":"config"}),
    );
    add(
        &mut c,
        "set_spin",
        json!({"platform":"select","unique_id":"$deviceid-set_spin","state_topic":"$this/set_spin","command_topic":"$this/set_spin/set","name":"Set spin","icon":"mdi:autorenew","options":spin_options(),"entity_category":"config"}),
    );
    add(
        &mut c,
        "set_rinse",
        json!({"platform":"select","unique_id":"$deviceid-set_rinse","state_topic":"$this/set_rinse","command_topic":"$this/set_rinse/set","name":"Set rinse","icon":"mdi:water","options":RINSES,"entity_category":"config"}),
    );
    add(
        &mut c,
        "set_soil",
        json!({"platform":"select","unique_id":"$deviceid-set_soil","state_topic":"$this/set_soil","command_topic":"$this/set_soil/set","name":"Set soil level","icon":"mdi:washing-machine","options":SOIL_LABELS,"entity_category":"config"}),
    );
    add(
        &mut c,
        "set_delay_end",
        json!({"platform":"number","unique_id":"$deviceid-set_delay_end","state_topic":"$this/set_delay_end","command_topic":"$this/set_delay_end/set","name":"Set delayed end","icon":"mdi:timer-sand","unit_of_measurement":"h","min":0,"max":19,"step":1,"entity_category":"config"}),
    );
    add(
        &mut c,
        "run_programme",
        json!({"platform":"button","unique_id":"$deviceid-run_programme","command_topic":"$this/run_programme/set","payload_press":"","name":"Run programme","icon":"mdi:play-box-outline"}),
    );
    add(
        &mut c,
        "set_steam",
        json!({"platform":"switch","unique_id":"$deviceid-set_steam","state_topic":"$this/set_steam","command_topic":"$this/set_steam/set","name":"Set steam","icon":"mdi:kettle-steam","entity_category":"config"}),
    );
    add(
        &mut c,
        "set_prewash",
        json!({"platform":"switch","unique_id":"$deviceid-set_prewash","state_topic":"$this/set_prewash","command_topic":"$this/set_prewash/set","name":"Set pre-wash","icon":"mdi:water-sync","entity_category":"config"}),
    );
    add(
        &mut c,
        "set_turbowash",
        json!({"platform":"switch","unique_id":"$deviceid-set_turbowash","state_topic":"$this/set_turbowash","command_topic":"$this/set_turbowash/set","name":"Set TurboWash","icon":"mdi:rocket-launch","entity_category":"config"}),
    );
    add(
        &mut c,
        "set_crease_care",
        json!({"platform":"switch","unique_id":"$deviceid-set_crease_care","state_topic":"$this/set_crease_care","command_topic":"$this/set_crease_care/set","name":"Set crease care","icon":"mdi:iron-outline","entity_category":"config"}),
    );
    add(
        &mut c,
        "set_steam_softener",
        json!({"platform":"switch","unique_id":"$deviceid-set_steam_softener","state_topic":"$this/set_steam_softener","command_topic":"$this/set_steam_softener/set","name":"Set steam softener","icon":"mdi:kettle-steam-outline","entity_category":"config"}),
    );
    add(
        &mut c,
        "set_medic_rinse",
        json!({"platform":"switch","unique_id":"$deviceid-set_medic_rinse","state_topic":"$this/set_medic_rinse","command_topic":"$this/set_medic_rinse/set","name":"Set medic rinse","icon":"mdi:water-check","entity_category":"config"}),
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
    ext.components(&mut c);
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_trait::DeviceHandler;
    use rethink_core::device_base::wrap_aabb;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const ID: &str = "test-id";
    // Captures from a Y_V8_Y___W.B32QEUK running a 14-minute programme (upstream issue #11).
    const INITIAL: &str = "aaff200a0039000381000100eb0027000001032603260000000000000000000000000003000011007100000000000000000000000000974ebb";
    const RUNNING: &str = "aaff200a0039000398000100eb0027000006000e000e0c0003020201000000014220000101001100710000010000000000000000000056d9bb";
    const RINSING: &str = "aaff200a00390003c6000100eb0027000007000b000e0c000302000100000001420000010600110071000001000022000000000000004970bb";
    const SPINNING: &str = "aaff200a0039000435000100eb00270000080005000e0c00000200000000000042000001070011007100000100002600000000000000af09bb";
    const END: &str = "aaff200a0039000478000100eb002700000a0000000e0c00000000000000000040000001080011007100000100002900000000000000c901bb";
    const POWER_OFF: &str = "aaff200a0039000487000100eb00270000000000000e0c000000000000000000000000000a0011007100000100002900000000000000cb19bb";
    // An FSR7A04PG switched on from Home Assistant, then its temperature and steam changed on the
    // panel. Each 0xec envelope carries the record being replaced, then the one replacing it.
    const PAIR_ON: &str = "aaff200a006000027f000100ec004e00000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000e0048000000000000000000000000006e4bbb";
    const PAIR_COURSE: &str = "aaff200a0060000286000100ec004e00000100000000000000000000000000000000000000000e004800000000000000000000000000000001003b003b3a00030a0401000000000001000200000e0048000004000000000000000000004282bb";
    const PAIR_STEAM: &str = "aaff200a0060000288000100ec004e000001003b003b3a00030a0401000000000001000200000e004800000400000000000000000000000001013201323a00030a0601000000800001000300000e0048000004000000000000000000000709bb";
    const PAIR_OFF: &str = "aaff200a006000028b000100ec004e000001013201323a00030a0601000000800001000300000e004800000400000000000000000000000000013201323a0000000000000000000001000301000e004800000400000000000000000000befabb";
    // The downloaded Hygiene programme (0x48) selected, on its Allergy SpaSteam base (0x2d).
    const PAIR_HYGIENE: &str = "aaff200a006000074f000100ec004e00000100000000000000000000000000004200000000000e004800000000000000000000000000000001023002302d00030a0601000000804200000000480e00480000000000000000000000c000b72abb";

    const WRITE_INIT: &str = "AA0EF0ED1121010000001800B5BB";
    const WRITE_POWER_ON: &str = "AA08F02A010098BB";
    const WRITE_POWER_OFF: &str = "AA09F0240101009CBB";
    const WRITE_PAUSE: &str = "AA09F02404010099BB";
    const WRITE_START: &str = "AA09F02405010098BB";
    // The LG app starting Allergy SpaSteam at 60 °C with steam, captured through bridge mode.
    const APP_START: &str = "AA16F0262D03FF06010000000080030000000000DABB";

    fn meta() -> Metadata {
        Metadata::new("Y_V8_Y___W.B32QEUK", "Y_V8_Y___W.B32QEUK", "2.11.207")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn feed(frames: &[&str]) -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let (ha, thinq, dev) = make();
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

    fn components(ha: &MockHaConnection) -> std::collections::HashMap<String, Value> {
        ha.device(ID).unwrap().config.unwrap().components
    }

    fn options(component: &Value) -> Vec<String> {
        component["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    fn last_packet(thinq: &MockThinq2Device) -> String {
        hex_encode(thinq.outbox().last().unwrap())
    }

    fn last_inner(thinq: &MockThinq2Device) -> Vec<u8> {
        let packet = thinq.outbox().last().unwrap().clone();
        packet[2..packet.len() - 2].to_vec()
    }

    #[test]
    fn config_is_published_on_construction() {
        let (ha, _, _) = make();
        assert!(ha.device(ID).unwrap().config.is_some(), "config published");
        let status = options(&components(&ha)["status"]);
        assert!(status.iter().any(|o| o == "Washing"));
        assert!(status.iter().any(|o| o == "Error"));
    }

    #[test]
    fn initial_state_push_decodes_status_time_and_cycles() {
        let (ha, _, _) = feed(&[INITIAL]);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "error", "OFF");
        expect(&ha, "error_message", "OK");
        expect(&ha, "remaining_time", "218");
        expect(&ha, "initial_time", "218");
        expect(&ha, "cycles", "17");
        expect(&ha, "energy", "0");
        // Lock byte 0x00: remote start off, door and child lock ON (unlocked, inverted).
        expect(&ha, "remote_start", "OFF");
        expect(&ha, "door_lock", "ON");
        expect(&ha, "child_lock", "ON");
        expect(&ha, "reserve_time", "0");
        expect(&ha, "turbowash", "OFF");
    }

    #[test]
    fn running_state_with_door_locked_and_remote_start_active() {
        let (ha, _, _) = feed(&[RUNNING]);
        expect(&ha, "status", "Washing");
        expect(&ha, "course", "Speed 14");
        expect(&ha, "spin", "400 rpm");
        expect(&ha, "temp", "20 °C");
        expect(&ha, "initial_time", "14");
        expect(&ha, "remaining_time", "14");
        // Lock byte 0x42: door locked (0x40) and remote start (0x02).
        expect(&ha, "remote_start", "ON");
        expect(&ha, "door_lock", "OFF");
        expect(&ha, "child_lock", "ON");
        expect(&ha, "wrinkle_care", "OFF");
        expect(&ha, "turbowash", "ON");
        expect(&ha, "steam", "OFF");
        expect(&ha, "prewash", "OFF");
        expect(&ha, "soil", "Normal");
        expect(&ha, "ai_dd", "OFF");
        expect(&ha, "rinse", "Normal");
        expect(&ha, "reserve_time", "0");
    }

    #[test]
    fn spin_holds_for_the_whole_cycle_and_energy_accumulates() {
        let (ha, thinq, _) = feed(&[RUNNING]);
        expect(&ha, "spin", "400 rpm");
        expect(&ha, "energy", "0");

        thinq.emit_data(&hex_decode(RINSING));
        expect(&ha, "spin", "400 rpm");
        expect(&ha, "energy", "34");

        thinq.emit_data(&hex_decode(SPINNING));
        expect(&ha, "spin", "400 rpm");
        expect(&ha, "energy", "38");

        // The total holds once the cycle is over.
        thinq.emit_data(&hex_decode(END));
        expect(&ha, "energy", "41");
        thinq.emit_data(&hex_decode(POWER_OFF));
        expect(&ha, "energy", "41");
    }

    #[test]
    fn rinsing_state() {
        let (ha, _, _) = feed(&[RINSING]);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "remaining_time", "11");
    }

    #[test]
    fn spinning_state() {
        let (ha, _, _) = feed(&[SPINNING]);
        expect(&ha, "status", "Spinning");
    }

    #[test]
    fn end_state() {
        let (ha, _, _) = feed(&[END]);
        expect(&ha, "status", "End");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "door_lock", "OFF");
        expect(&ha, "remote_start", "OFF");
    }

    #[test]
    fn power_off_transition() {
        let (ha, _, _) = feed(&[POWER_OFF]);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
    }

    #[test]
    fn a_status_envelope_is_acked_so_the_appliance_stops_retransmitting_it() {
        let (_, thinq, _) = make();
        thinq.reset_recorder();

        thinq.emit_data(&hex_decode(INITIAL));

        // INITIAL carries sequence 0x0381 at inner[5..7], acked under the `ack` command.
        let acks: Vec<String> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(acks, [hex_encode(&wrap_aabb(&hex_decode("f0000a040381")))]);
        assert!(thinq.outbox().is_empty(), "the ack is not sent as a packet");
    }

    #[test]
    fn frames_not_matching_the_aa_bb_envelope_are_ignored() {
        let (ha, thinq, _) = make();
        let before = prop(&ha, "power");
        thinq.emit_data(&hex_decode("001122"));
        assert_eq!(prop(&ha, "power"), before);
    }

    #[test]
    fn frames_with_wrong_inner_length_are_ignored() {
        let (ha, thinq, _) = make();
        let before = prop(&ha, "power");
        thinq.emit_data(&hex_decode("AA08200A01020304BB"));
        assert_eq!(prop(&ha, "power"), before);
    }

    #[test]
    fn ec_pair_switching_on_is_reported_so_the_power_switch_holds() {
        let (ha, _, _) = feed(&[PAIR_ON]);
        expect(&ha, "power", "ON");
        expect(&ha, "status", "Standby");
        expect(&ha, "cycles", "14");
    }

    #[test]
    fn ec_pair_a_course_selected_on_the_panel_reaches_ha() {
        let (ha, _, _) = feed(&[PAIR_COURSE]);
        expect(&ha, "course", "AI Wash");
        expect(&ha, "spin", "1400 rpm");
        expect(&ha, "temp", "40 °C");
        expect(&ha, "initial_time", "59");
        expect(&ha, "remaining_time", "59");
        expect(&ha, "ai_dd", "ON");
        expect(&ha, "steam", "OFF");
        expect(&ha, "soil", "Normal");
    }

    #[test]
    fn ec_pair_the_replacing_record_wins_not_the_one_being_replaced() {
        let (ha, _, _) = feed(&[PAIR_STEAM]);
        expect(&ha, "temp", "60 °C");
        expect(&ha, "steam", "ON");
        expect(&ha, "spin", "1400 rpm");
        expect(&ha, "course", "AI Wash");
        expect(&ha, "initial_time", "110");
        expect(&ha, "crease_care", "OFF");
        expect(&ha, "prewash", "OFF");
        expect(&ha, "medic_rinse", "OFF");
        expect(&ha, "steam_softener", "OFF");
    }

    #[test]
    fn ec_pair_switching_off_is_reported() {
        let (ha, _, _) = feed(&[PAIR_STEAM, PAIR_OFF]);
        expect(&ha, "power", "OFF");
        expect(&ha, "status", "Off");
        expect(&ha, "steam", "OFF");
    }

    #[test]
    fn ec_pair_an_envelope_whose_payload_is_shorter_than_it_claims_is_ignored() {
        let (ha, thinq, _) = make();
        let before = prop(&ha, "power");
        // 0x00ec announcing 78 bytes, carrying 4
        thinq.emit_data(&hex_decode("AA13200A001300028B000100EC004E00010203BB"));
        assert_eq!(prop(&ha, "power"), before);
    }

    #[test]
    fn a_downloaded_course_is_reported_separately_from_the_base_course_it_is_built_on() {
        let (ha, _, _) = feed(&[PAIR_HYGIENE]);
        expect(&ha, "smart_course", "Hygiene");
        expect(&ha, "course", "Allergy SpaSteam");
        expect(&ha, "temp", "60 °C");
        expect(&ha, "steam", "ON");
        expect(&ha, "spin", "1400 rpm");
        expect(&ha, "rinse", "Normal");
    }

    #[test]
    fn an_unmapped_course_is_not_published() {
        let mut frame = hex_decode(PAIR_HYGIENE);
        // Replacing-record offsets 7 (course) and 22 (downloaded course).
        frame[61] = 0x63;
        frame[76] = 0x5a;
        let (ha, thinq, _) = make();
        thinq.emit_data(&frame);
        expect(&ha, "course", "None");
        expect(&ha, "smart_course", "None");
    }

    #[test]
    fn smart_course_0x86_is_microplastic_care() {
        let mut frame = hex_decode(PAIR_HYGIENE);
        frame[76] = 0x86;
        let (ha, thinq, _) = make();
        thinq.emit_data(&frame);
        expect(&ha, "smart_course", "MicroPlastic Care");
        let smart = options(&components(&ha)["smart_course"]);
        assert!(smart.iter().any(|o| o == "MicroPlastic Care"));
    }

    #[test]
    fn start_sends_the_f0ed_initialisation_packet() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_INIT);
    }

    #[test]
    fn ha_write_power_on() {
        let (_, thinq, dev) = make();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_ON);
    }

    #[test]
    fn ha_write_power_off() {
        let (_, thinq, dev) = make();
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_OFF);
    }

    #[test]
    fn ha_write_pause_button() {
        let (_, thinq, dev) = make();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_PAUSE);
    }

    #[test]
    fn ha_write_start_button() {
        let (_, thinq, dev) = make();
        dev.set_property("start", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_START);
    }

    #[test]
    fn programme_the_assembled_command_matches_the_one_the_lg_app_sends() {
        let (_, thinq, dev) = make();
        dev.set_property("set_course", "Allergy SpaSteam");
        dev.set_property("set_soil", "Normal");
        dev.set_property("set_spin", "Max");
        dev.set_property("set_temp", "60 °C");
        dev.set_property("set_rinse", "Normal");
        dev.set_property("set_steam", "ON");
        dev.set_property("run_programme", "");
        assert_eq!(last_packet(&thinq), APP_START);
    }

    #[test]
    fn programme_an_untouched_start_runs_what_the_appliance_already_reports() {
        let (_, thinq, dev) = feed(&[PAIR_HYGIENE]);
        thinq.reset_recorder();

        dev.set_property("run_programme", "");

        let inner = last_inner(&thinq);
        assert_eq!(inner[2], 0x2d, "course as reported");
        assert_eq!(inner[4], 0x0a, "spin as reported, 1400 rather than Max");
        assert_eq!(inner[5], 0x06, "temp as reported");
        assert_eq!(inner[11], 0x80, "steam as reported");
        assert_eq!(inner[12], 0x03, "initialBit | remoteStart");
    }

    #[test]
    fn programme_a_chosen_setting_overrides_the_reported_one_and_is_released_once_it_has_run() {
        let (ha, thinq, dev) = feed(&[PAIR_HYGIENE]);
        expect(&ha, "set_temp", "60 °C");

        dev.set_property("set_temp", "30 °C");
        expect(&ha, "set_temp", "30 °C");

        thinq.reset_recorder();
        dev.set_property("run_programme", "");
        assert_eq!(last_inner(&thinq)[5], 0x03, "the chosen 30 °C is sent");

        thinq.emit_data(&hex_decode(PAIR_HYGIENE));
        expect(&ha, "set_temp", "60 °C");
    }

    #[test]
    fn programme_option_switches_set_and_clear_their_own_bit_only() {
        let (_, thinq, dev) = feed(&[PAIR_HYGIENE]);
        thinq.reset_recorder();

        dev.set_property("set_prewash", "ON");
        dev.set_property("run_programme", "");
        assert_eq!(
            last_inner(&thinq)[11],
            0x80 | 0x40,
            "steam kept, pre-wash added"
        );

        thinq.emit_data(&hex_decode(PAIR_HYGIENE));
        dev.set_property("set_steam", "OFF");
        dev.set_property("run_programme", "");
        assert_eq!(last_inner(&thinq)[11], 0x00, "steam cleared");
    }

    #[test]
    fn programme_an_idle_appliance_leaves_every_select_in_a_state_ha_will_accept() {
        let (ha, _, _) = feed(&[POWER_OFF]);
        let components = components(&ha);
        for name in [
            "set_course",
            "set_temp",
            "set_spin",
            "set_rinse",
            "set_soil",
        ] {
            let published = prop(&ha, name).unwrap();
            assert!(
                options(&components[name]).contains(&published),
                "{name} published {published}, which is not one of its options"
            );
        }
    }

    #[test]
    fn programme_a_delayed_end_outside_the_range_the_appliance_accepts_is_ignored() {
        let (ha, _, dev) = make();
        dev.set_property("set_delay_end", "5");
        expect(&ha, "set_delay_end", "5");
        dev.set_property("set_delay_end", "1");
        expect(&ha, "set_delay_end", "5");
        dev.set_property("set_delay_end", "0");
        expect(&ha, "set_delay_end", "0");
    }

    #[test]
    fn programme_a_delayed_end_parses_like_javascript_number() {
        let (ha, thinq, dev) = make();
        for (payload, published) in [
            ("5.0", "5"),
            ("", "0"),
            ("0x5", "5"),
            (" 7 ", "7"),
            ("1e1", "10"),
            ("4.5", "4.5"),
        ] {
            dev.set_property("set_delay_end", payload);
            assert_eq!(
                prop(&ha, "set_delay_end").as_deref(),
                Some(published),
                "{payload:?}"
            );
        }
        for payload in ["abc", "Infinity", "19.5", "2.99", "0x"] {
            dev.set_property("set_delay_end", payload);
            assert_eq!(
                prop(&ha, "set_delay_end").as_deref(),
                Some("4.5"),
                "{payload:?}"
            );
        }
        dev.set_property("run_programme", "");
        assert_eq!(last_inner(&thinq)[8], 4, "4.5 h goes out as hour 4");
    }

    #[test]
    fn no_option_list_uses_the_payload_ha_reserves_for_unknown() {
        let (ha, _, _) = make();
        for (name, component) in components(&ha) {
            if component.get("options").is_some() {
                assert!(
                    !options(&component).iter().any(|o| o == "None"),
                    "{name} offers \"None\", which HA treats as unknown"
                );
            }
        }
    }

    #[test]
    fn set_entities_are_configuration_and_run_programme_is_not() {
        let (ha, _, _) = make();
        let components = components(&ha);
        let category = |name: &str| components[name].get("entity_category").cloned();
        let set: Vec<&String> = components
            .keys()
            .filter(|k| k.starts_with("set_"))
            .collect();
        assert_eq!(set.len(), 12);
        for name in set {
            assert_eq!(category(name), Some(json!("config")), "{name}");
        }
        assert_eq!(category("run_programme"), None);
    }

    #[test]
    fn ha_write_to_unknown_property_emits_no_packet() {
        let (_, thinq, dev) = make();
        dev.set_property("does-not-exist", "whatever");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("Y_V8_Y___W.B32QEUK").is_some());
    }
}
