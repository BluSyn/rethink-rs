//! WTWN3 — ThinQ1 front-load washer (F4J7TN1W).
//!
//! 28-byte status, same skeleton as WTDN3, with wash/rinse/option fields and a
//! local course table. Byte 11 is unused on this model.

use crate::device_trait::DeviceHandler;
use crate::washer_common::{
    error_options, spin_options, spin_value, temperature_value, thinq1_washer_name,
    washer_status_options, ERRORS, THINQ1_WASHER,
    TEMPERATURE_LABELS,
};
use rethink_core::device_base::default_config;
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq1Device;
use rethink_util::sync::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const COURSES: &[(u8, &str)] = &[
    (0x01, "Cotton"),
    (0x02, "Easy Care"),
    (0x04, "Cotton+"),
    (0x05, "Duvet"),
    (0x06, "Stain Care"),
    (0x07, "Mix"),
    (0x08, "Sports Wear"),
    (0x09, "Silent Wash"),
    (0x0b, "Gentle Care"),
    (0x0c, "Speed 14"),
    (0x12, "Tub Clean"),
    (0x20, "Delicate"),
    (0x2c, "Baby Steam Care"),
    (0x2d, "Allergy SpaSteam"),
    (0x34, "Kids Wear"),
    (0x35, "School Uniform"),
    (0x36, "Swimming Wear"),
    (0x37, "Rainy Season"),
    (0x38, "Gym Clothes"),
    (0x39, "Jeans"),
    (0x3a, "Blanket"),
    (0x3b, "Sweat Stain"),
    (0x3e, "Single Garment"),
    (0x3f, "Color Protection"),
    (0x40, "Noise Minimize"),
    (0x49, "Small Load"),
    (0x4a, "Lingerie"),
    (0x4b, "Wool"),
    (0x4d, "Cold Wash"),
    (0x64, "Rinse + Spin"),
    (0x65, "Lightly Soiled Items"),
    (0x66, "Minimize Detergent Residue"),
    (0x67, "1hr. Wash"),
    (0x6b, "Sleeve Hems and Collars"),
    (0x6c, "Juice and Food Stains"),
    (0x6f, "Minimize Wrinkles"),
];

const WASH: &[(u8, &str)] = &[
    (1, "Turbo Wash"),
    (2, "Time Save"),
    (3, "Normal"),
    (4, "Intensive"),
];

const RINSES: &[&str] = &[
    "Not selected",
    "Normal",
    "Rinse+",
    "Rinse++",
    "Normal + Hold",
    "Rinse+ + Hold",
];

const OPT1_TURBO_WASH: u8 = 0x01;
const OPT1_CREASE_CARE: u8 = 0x02;
const OPT1_MEDIC_RINSE: u8 = 0x10;
const OPT1_PRE_WASH: u8 = 0x40;
const OPT1_STEAM: u8 = 0x80;

const OPT2_REMOTE_START: u8 = 0x02;
const OPT2_DOOR_LOCK: u8 = 0x40;
const OPT2_CHILD_LOCK: u8 = 0x80;

pub struct Device {
    id: String,
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    publish_cache: Mutex<HashMap<String, String>>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq1Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let id = thinq.id().to_string();
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        let course_options: Vec<&str> = COURSES.iter().map(|(_, name)| *name).collect();
        let wash_options: Vec<&str> = WASH.iter().map(|(_, name)| *name).collect();

        config.components = [
            (
                "power".into(),
                json!({
                    "platform": "switch",
                    "unique_id": "$deviceid-power",
                    "state_topic": "$this/power",
                    "command_topic": "$this/power/set",
                    "name": "",
                    "icon": "mdi:washing-machine",
                }),
            ),
            (
                "start".into(),
                json!({
                    "platform": "button",
                    "unique_id": "$deviceid-start",
                    "command_topic": "$this/start/set",
                    "payload_press": "",
                    "name": "Start",
                    "icon": "mdi:play-circle-outline",
                }),
            ),
            (
                "pause".into(),
                json!({
                    "platform": "button",
                    "unique_id": "$deviceid-pause",
                    "command_topic": "$this/pause/set",
                    "payload_press": "",
                    "name": "Pause",
                    "icon": "mdi:pause-circle-outline",
                }),
            ),
            (
                "status".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-status",
                    "state_topic": "$this/status",
                    "name": "Status",
                    "icon": "mdi:state-machine",
                    "device_class": "enum",
                    "options": washer_status_options(THINQ1_WASHER),
                }),
            ),
            (
                "error".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-error",
                    "state_topic": "$this/error",
                    "name": "Error",
                    "icon": "mdi:check-circle",
                    "device_class": "problem",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "error_message".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-error-message",
                    "state_topic": "$this/error_message",
                    "name": "Error message",
                    "icon": "mdi:alert-circle-outline",
                    "device_class": "enum",
                    "entity_category": "diagnostic",
                    "options": error_options(),
                }),
            ),
            (
                "course".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-course",
                    "state_topic": "$this/course",
                    "name": "Course",
                    "icon": "mdi:pin-outline",
                    "device_class": "enum",
                    "options": course_options,
                }),
            ),
            (
                "wash".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-wash",
                    "state_topic": "$this/wash",
                    "name": "Wash",
                    "icon": "mdi:waves",
                    "device_class": "enum",
                    "options": wash_options,
                }),
            ),
            (
                "temp".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-temp",
                    "state_topic": "$this/temp",
                    "name": "Temperature",
                    "icon": "mdi:thermometer",
                    "device_class": "enum",
                    "options": TEMPERATURE_LABELS,
                }),
            ),
            (
                "spin".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-spin",
                    "state_topic": "$this/spin",
                    "name": "Spin",
                    "icon": "mdi:autorenew",
                    "device_class": "enum",
                    "options": spin_options(),
                }),
            ),
            (
                "rinse".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-rinse",
                    "state_topic": "$this/rinse",
                    "name": "Rinse",
                    "icon": "mdi:water",
                    "device_class": "enum",
                    "options": RINSES,
                }),
            ),
            (
                "turbo_wash".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-turbo_wash",
                    "state_topic": "$this/turbo_wash",
                    "name": "TurboWash",
                    "icon": "mdi:speedometer",
                }),
            ),
            (
                "pre_wash".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-pre_wash",
                    "state_topic": "$this/pre_wash",
                    "name": "Pre-wash",
                    "icon": "mdi:water-plus-outline",
                }),
            ),
            (
                "steam".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-steam",
                    "state_topic": "$this/steam",
                    "name": "Steam",
                    "icon": "mdi:weather-fog",
                }),
            ),
            (
                "crease_care".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-crease_care",
                    "state_topic": "$this/crease_care",
                    "name": "Crease care",
                    "icon": "mdi:iron-outline",
                }),
            ),
            (
                "medic_rinse".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-medic_rinse",
                    "state_topic": "$this/medic_rinse",
                    "name": "Medic rinse",
                    "icon": "mdi:medical-bag",
                }),
            ),
            (
                "tub_clean_count".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-tub_clean_count",
                    "state_topic": "$this/tub_clean_count",
                    "name": "Washes since tub clean",
                    "icon": "mdi:counter",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "remote_start".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-remote_start",
                    "state_topic": "$this/remote_start",
                    "name": "Remote start",
                    "icon": "mdi:play-circle-outline",
                }),
            ),
            (
                "door_lock".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-door_lock",
                    "state_topic": "$this/door_lock",
                    "name": "Door lock",
                    "device_class": "lock",
                }),
            ),
            (
                "child_lock".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-child_lock",
                    "state_topic": "$this/child_lock",
                    "name": "Child lock",
                    "icon": "mdi:lock-outline",
                }),
            ),
            (
                "standby".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-standby",
                    "state_topic": "$this/standby",
                    "name": "Standby",
                    "icon": "mdi:power-sleep",
                    "entity_category": "diagnostic",
                }),
            ),
            (
                "initial_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-initial_time",
                    "state_topic": "$this/initial_time",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "name": "Initial time",
                }),
            ),
            (
                "remaining_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-remaining_time",
                    "state_topic": "$this/remaining_time",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "name": "Remaining time",
                }),
            ),
            (
                "reserve_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-reserve_time",
                    "state_topic": "$this/reserve_time",
                    "device_class": "duration",
                    "unit_of_measurement": "min",
                    "name": "Delay start",
                    "icon": "mdi:timer-sand",
                }),
            ),
        ]
        .into_iter()
        .collect();

        ha.publish_property(&id, "availability", "online".into());
        ha.publish_config(&id, &config);

        let this = Arc::new(Self {
            id,
            ha,
            thinq: thinq.clone(),
            publish_cache: Mutex::new(HashMap::new()),
        });
        let handler = this.clone();
        thinq.on_data(Box::new(move |buf| handler.on_data(buf)));
        this
    }

    fn publish_property(&self, prop: &str, value: PropertyValue) {
        {
            let mut cache = self.publish_cache.lock();
            if let Some(prev) = cache.get(prop) {
                if value.eq_str(prev) {
                    return;
                }
            }
            cache.insert(prop.to_string(), value.as_string());
        }
        self.ha.publish_property(&self.id, prop, value);
    }

    fn publish_label(&self, prop: &str, label: Option<&str>) {
        self.publish_property(prop, label.unwrap_or("None").into());
    }

    fn publish_on_off(&self, prop: &str, on: bool) {
        self.publish_property(prop, if on { "ON" } else { "OFF" }.into());
    }

    fn on_data(&self, buf: &[u8]) {
        if buf.len() != 28 {
            return;
        }
        let status = buf[0] as usize;
        let time_remain = buf[1] as i64 * 60 + buf[2] as i64;
        let time_initial = buf[3] as i64 * 60 + buf[4] as i64;
        let native_course = buf[5];
        let error = buf[6] as usize;
        let wash = buf[7];
        let spin = buf[8] as usize;
        let temp = buf[9] as usize;
        let rinse = buf[10] as usize;
        let reserve = buf[12] as i64 * 60 + buf[13] as i64;
        let option1 = buf[14];
        let option2 = buf[15];
        let smart_course = buf[20];
        let tub_clean_count = buf[21] as i64;
        let standby = buf[27];

        self.publish_on_off("power", status > 0);
        self.publish_label("error_message", hole(ERRORS, error));
        self.publish_on_off("error", error != 0);
        self.publish_property("status", thinq1_washer_name(buf[0]).into());
        let course = course_label(smart_course).or_else(|| course_label(native_course));
        self.publish_label("course", course);
        self.publish_label("wash", named(WASH, wash));
        self.publish_label("spin", spin_value(spin));
        self.publish_label("temp", temperature_value(temp));
        self.publish_label("rinse", RINSES.get(rinse).copied());
        self.publish_on_off("turbo_wash", option1 & OPT1_TURBO_WASH != 0);
        self.publish_on_off("crease_care", option1 & OPT1_CREASE_CARE != 0);
        self.publish_on_off("medic_rinse", option1 & OPT1_MEDIC_RINSE != 0);
        self.publish_on_off("pre_wash", option1 & OPT1_PRE_WASH != 0);
        self.publish_on_off("steam", option1 & OPT1_STEAM != 0);
        self.publish_property("tub_clean_count", PropertyValue::Int(tub_clean_count));
        self.publish_on_off("remote_start", option2 & OPT2_REMOTE_START != 0);
        // Lock class: ON means unlocked.
        self.publish_on_off("door_lock", option2 & OPT2_DOOR_LOCK == 0);
        self.publish_on_off("child_lock", option2 & OPT2_CHILD_LOCK != 0);
        self.publish_on_off("standby", standby != 0);
        self.publish_property("initial_time", PropertyValue::Int(time_initial));
        self.publish_property("remaining_time", PropertyValue::Int(time_remain));
        self.publish_property("reserve_time", PropertyValue::Int(reserve));
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if prop == "power" {
            if mqtt_value == "ON" {
                self.thinq.send(json!({
                    "Cmd": "Control",
                    "CmdOpt": "Power",
                    "Value": "On",
                    "Format": "B64",
                    "Data": ""
                }));
            } else if mqtt_value == "OFF" {
                self.thinq.send(json!({
                    "Cmd": "Control",
                    "CmdOpt": "Power",
                    "Value": "Off",
                    "Format": "B64",
                    "Data": ""
                }));
            }
        }
        if prop == "pause" {
            self.thinq.send(json!({
                "Cmd": "Control",
                "CmdOpt": "Operation",
                "Value": "Stop",
                "Format": "B64",
                "Data": ""
            }));
        }
        if prop == "start" {
            self.thinq.send(json!({
                "Cmd": "Control",
                "CmdOpt": "Operation",
                "Value": "Start",
                "Format": "B64",
                "Data": mqtt_value
            }));
        }
    }
}

fn course_label(code: u8) -> Option<&'static str> {
    named(COURSES, code)
}

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn hole<'a>(table: &'a [Option<&'a str>], code: usize) -> Option<&'a str> {
    table.get(code).copied().flatten()
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.id
    }
    fn start(&self) {
        self.thinq.send(json!({ "Cmd": "Mon", "CmdOpt": "Start" }));
    }
    fn drop_device(&self) {
        self.ha
            .publish_property(&self.id, "availability", "offline".into());
    }
    fn set_property(&self, prop: &str, value: &str) {
        Device::set_property(self, prop, value);
    }
    fn publish_config(&self) {}
}

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    Device::new(ha, thinq, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rethink_core::{hex_decode, MockHaConnection, MockThinq1Device};

    const DEVICE_ID: &str = "test-id";

    fn meta() -> Metadata {
        Metadata::new("WTWN3", "WTWN3", "1.0")
    }

    fn make_device() -> (Arc<MockHaConnection>, Arc<MockThinq1Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq1Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> PropertyValue {
        ha.device(DEVICE_ID)
            .unwrap()
            .properties
            .get(name)
            .cloned()
            .unwrap()
    }

    #[test]
    fn washing_mix_decodes_course_options_and_times() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "06012101210700030704020000000040000001030039006400000400",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Washing".into());
        assert_eq!(prop(&ha, "course"), "Mix".into());
        assert_eq!(prop(&ha, "wash"), "Normal".into());
        assert_eq!(prop(&ha, "spin"), "1000 rpm".into());
        assert_eq!(prop(&ha, "temp"), "40 °C".into());
        assert_eq!(prop(&ha, "rinse"), "Rinse+".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(93));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(93));
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "error"), "OFF".into());
        assert_eq!(prop(&ha, "error_message"), "OK".into());
        assert_eq!(prop(&ha, "door_lock"), "OFF".into());
        assert_eq!(prop(&ha, "child_lock"), "OFF".into());
        assert_eq!(prop(&ha, "remote_start"), "OFF".into());
        assert_eq!(prop(&ha, "tub_clean_count"), PropertyValue::Int(57));
        assert_eq!(prop(&ha, "standby"), "OFF".into());
    }

    #[test]
    fn tub_clean_measuring_door_locked() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "04 01 0e 01 0e 12 00 03 01 06 01 00 00 00 00 40 00 00 01 01 00 29 00 4b 00 00 02 00",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Detecting".into());
        assert_eq!(prop(&ha, "course"), "Tub Clean".into());
        assert_eq!(prop(&ha, "wash"), "Normal".into());
        assert_eq!(prop(&ha, "spin"), "No spin".into());
        assert_eq!(prop(&ha, "temp"), "60 °C".into());
        assert_eq!(prop(&ha, "rinse"), "Normal".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(74));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(74));
        assert_eq!(prop(&ha, "door_lock"), "OFF".into());
        assert_eq!(prop(&ha, "child_lock"), "OFF".into());
        assert_eq!(prop(&ha, "tub_clean_count"), PropertyValue::Int(41));
    }

    #[test]
    fn rinse_and_spin_follow_the_state_byte() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "07002f01210700030700020000000040000001060039006400000400",
        ));
        assert_eq!(prop(&ha, "status"), "Rinsing".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(47));
        assert_eq!(prop(&ha, "temp"), "Not selected".into());

        thinq.emit_data(&hex_decode(
            "08000b01210700000700000000000040000001070039006400000400",
        ));
        assert_eq!(prop(&ha, "status"), "Spinning".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(11));
        assert_eq!(prop(&ha, "spin"), "1000 rpm".into());
    }

    #[test]
    fn end_keeps_power_on_and_unlocks_the_door() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "0a000001210700000000000000000000000001080039006400000400",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "End".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "door_lock"), "ON".into());
    }

    #[test]
    fn idle_is_power_off_and_keeps_the_course() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "000000012107000000000000000000000000020a0039006400000400",
        ));
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "status"), "Off".into());
        assert_eq!(prop(&ha, "course"), "Mix".into());
        assert_eq!(prop(&ha, "tub_clean_count"), PropertyValue::Int(57));
    }

    #[test]
    fn option_bits_delay_and_locks() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "01 02 0a 02 0a 01 00 03 09 04 01 00 03 00 c1 82 00 00 00 00 00 39 00 64 00 00 04 00",
        ));
        assert_eq!(prop(&ha, "status"), "Standby".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "spin"), "1200 rpm".into());
        assert_eq!(prop(&ha, "temp"), "40 °C".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(130));
        assert_eq!(prop(&ha, "reserve_time"), PropertyValue::Int(180));
        assert_eq!(prop(&ha, "turbo_wash"), "ON".into());
        assert_eq!(prop(&ha, "pre_wash"), "ON".into());
        assert_eq!(prop(&ha, "steam"), "ON".into());
        assert_eq!(prop(&ha, "crease_care"), "OFF".into());
        assert_eq!(prop(&ha, "medic_rinse"), "OFF".into());
        assert_eq!(prop(&ha, "remote_start"), "ON".into());
        assert_eq!(prop(&ha, "child_lock"), "ON".into());
        assert_eq!(prop(&ha, "door_lock"), "ON".into());
    }

    #[test]
    fn downloaded_course_overrides_base_and_error_publishes() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "12 01 0e 01 0e 03 02 03 07 02 01 00 00 00 00 00 00 00 00 00 39 39 00 64 00 00 04 00",
        ));
        assert_eq!(prop(&ha, "status"), "Error".into());
        assert_eq!(prop(&ha, "error"), "ON".into());
        assert_eq!(prop(&ha, "error_message"), "Door open error (DE1)".into());
        assert_eq!(prop(&ha, "course"), "Jeans".into());
    }

    #[test]
    fn short_frames_are_ignored() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode("0102"));
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());
    }

    #[test]
    fn power_and_buttons_send_control_commands() {
        let (_ha, thinq, dev) = make_device();
        dev.set_property("power", "OFF");
        dev.set_property("pause", "");
        dev.set_property("start", "");
        assert_eq!(
            thinq.sent(),
            vec![
                json!({"Cmd":"Control","CmdOpt":"Power","Value":"Off","Format":"B64","Data":""}),
                json!({"Cmd":"Control","CmdOpt":"Operation","Value":"Stop","Format":"B64","Data":""}),
                json!({"Cmd":"Control","CmdOpt":"Operation","Value":"Start","Format":"B64","Data":""}),
            ]
        );
    }

    #[test]
    fn factory_starts_monitoring() {
        let thinq = MockThinq1Device::new(DEVICE_ID, meta());
        let ha = MockHaConnection::new();
        let factory = crate::t1_factory("WTWN3").unwrap();
        let dev = factory(ha, thinq.clone(), meta());
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.sent(), vec![json!({"Cmd":"Mon","CmdOpt":"Start"})]);
    }
}
