//! WTDN3 — ThinQ1 washer.

use crate::device_trait::DeviceHandler;
use crate::washer_common::{
    course_name, drying_mode, spin_options, spin_value, temperature_value, thinq1_washer_name,
    washer_status_options, ERRORS, THINQ1_WASHER,
    TEMPERATURE_LABELS,
};
use rethink_util::sync::Mutex;
use rethink_core::device_base::default_config;
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq1Device;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

pub struct Device {
    id: String,
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    publish_cache: Mutex<HashMap<String, String>>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq1Device>, meta: Metadata) -> Arc<Self> {
        let id = thinq.id().to_string();
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));

        let status_options = washer_status_options(THINQ1_WASHER);
        let error_options: Vec<&str> = ERRORS.iter().filter_map(|s| *s).collect();

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
                    "options": status_options,
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
                    "options": error_options,
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
                }),
            ),
            (
                "temp".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-temp",
                    "state_topic": "$this/temp",
                    "name": "Temperature",
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
                "drying_mode".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-drying-mode",
                    "state_topic": "$this/drying_mode",
                    "name": "Drying mode",
                    "icon": "mdi:tumble-dryer",
                }),
            ),
            (
                "cycles".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-cycles",
                    "state_topic": "$this/cycles",
                    "name": "Cycle count",
                    "icon": "mdi:counter",
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

        let t = this.clone();
        thinq.on_data(Box::new(move |buf| t.process_data(buf)));
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

    fn publish_on_off(&self, prop: &str, on: bool) {
        let s = if on { "ON" } else { "OFF" };
        {
            let mut cache = self.publish_cache.lock();
            if cache.get(prop).map(|c| c.as_str() == s).unwrap_or(false) {
                return;
            }
            cache.insert(prop.to_string(), s.to_string());
        }
        self.ha.publish_property(&self.id, prop, s.into());
    }

    fn process_data(&self, buf: &[u8]) {
        if buf.len() != 28 {
            return;
        }
        let status = buf[0] as usize;
        let time_remain = buf[1] as i64 * 60 + buf[2] as i64;
        let time_initial = buf[3] as i64 * 60 + buf[4] as i64;
        let native_course = buf[5] as u32;
        let error = buf[6] as usize;
        let spin = buf[8] as usize;
        let temp = buf[9] as usize;
        let drying = buf[11] as u32;
        let lock_status = buf[15];
        let custom_course = buf[20] as u32;
        let cycles = buf[21] as i64;

        self.publish_on_off("power", status > 0);

        let err_msg = ERRORS
            .get(error)
            .and_then(|e| *e)
            .unwrap_or("unknown");
        self.publish_property("error_message", err_msg.into());
        self.publish_on_off("error", error != 0);

        self.publish_property("status", thinq1_washer_name(status as u8).into());

        let course = course_name(custom_course)
            .or_else(|| course_name(native_course))
            .unwrap_or("unknown");
        self.publish_property("course", course.into());

        self.publish_property("spin", spin_value(spin).unwrap_or("None").into());
        self.publish_property("temp", temperature_value(temp).unwrap_or("None").into());

        let dry = drying_mode(drying).unwrap_or("unknown");
        self.publish_property("drying_mode", dry.into());

        self.publish_property("cycles", PropertyValue::Int(cycles));
        self.publish_on_off("remote_start", lock_status & 2 != 0);
        // inverted logic, off=locked
        self.publish_on_off("door_lock", lock_status & 0x40 == 0);
        self.publish_property("initial_time", PropertyValue::Int(time_initial));
        self.publish_property("remaining_time", PropertyValue::Int(time_remain));
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

    pub fn drop(&self) {
        self.ha
            .publish_property(&self.id, "availability", "offline".into());
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.id
    }
    fn start(&self) {
        self.thinq.send(json!({ "Cmd": "Mon", "CmdOpt": "Start" }));
    }
    fn drop_device(&self) {
        self.drop();
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
        Metadata::new("WTDN3", "WTDN3", "1.0")
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
    fn config_exposes_expected_components() {
        let (ha, _thinq, _dev) = make_device();
        let cfg = ha.device(DEVICE_ID).unwrap().config.unwrap();
        for c in [
            "power",
            "start",
            "pause",
            "status",
            "error",
            "error_message",
            "course",
            "temp",
            "spin",
            "drying_mode",
            "cycles",
            "remote_start",
            "door_lock",
            "initial_time",
            "remaining_time",
        ] {
            assert!(cfg.components.contains_key(c), "component {c}");
        }
        let opts = cfg.components["status"]["options"].as_array().unwrap();
        assert!(opts.iter().any(|v| v == "Washing"));
        assert!(opts.iter().any(|v| v == "Error"));
        assert_eq!(cfg.components["temp"]["device_class"], "enum");
        assert_eq!(cfg.components["spin"]["device_class"], "enum");
        assert!(cfg.components["temp"]["options"].as_array().unwrap().iter().any(|v| v == "Cold"));
        assert!(cfg.components["spin"]["options"].as_array().unwrap().iter().any(|v| v == "No spin"));
    }

    #[test]
    fn off_state() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "00000000000000000000000000000000000000000006006400000000",
        ));
        assert_eq!(prop(&ha, "power"), "OFF".into());
        assert_eq!(prop(&ha, "status"), "Off".into());
        assert_eq!(prop(&ha, "error"), "OFF".into());
        assert_eq!(prop(&ha, "error_message"), "OK".into());
        assert_eq!(prop(&ha, "cycles"), PropertyValue::Int(6));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(0));
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(0));
    }

    #[test]
    fn ready_cotton_1200_40c() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "01030403040100030904010000000000000003000006006400000400",
        ));
        assert_eq!(prop(&ha, "power"), "ON".into());
        assert_eq!(prop(&ha, "status"), "Standby".into());
        assert_eq!(prop(&ha, "course"), "Cotton".into());
        assert_eq!(prop(&ha, "spin"), PropertyValue::Str("1200 rpm".into()));
        assert_eq!(prop(&ha, "temp"), PropertyValue::Str("40 °C".into()));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(184));
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(184));
        assert_eq!(prop(&ha, "error"), "OFF".into());
        assert_eq!(prop(&ha, "drying_mode"), "Off".into());
    }

    #[test]
    fn wash_door_lock_and_remaining() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "06011401140100030904010000000040000001040006006400000400",
        ));
        assert_eq!(prop(&ha, "status"), "Washing".into());
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(80));
        assert_eq!(prop(&ha, "door_lock"), "OFF".into());
        assert_eq!(prop(&ha, "remote_start"), "OFF".into());
    }

    #[test]
    fn fast30_running() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "06001e001e2200030502010000000040000001010006006400000200",
        ));
        assert_eq!(prop(&ha, "status"), "Washing".into());
        assert_eq!(prop(&ha, "course"), "Quick 30".into());
        assert_eq!(prop(&ha, "spin"), PropertyValue::Str("800 rpm".into()));
        assert_eq!(prop(&ha, "temp"), PropertyValue::Str("20 °C".into()));
        assert_eq!(prop(&ha, "initial_time"), PropertyValue::Int(30));
        assert_eq!(prop(&ha, "remaining_time"), PropertyValue::Int(30));
    }

    #[test]
    fn custom_course_overrides_native() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "01030003000100030904010000000242000003006f08006f00000400",
        ));
        assert_eq!(prop(&ha, "course"), "Reducing Wrinkles".into());
    }

    #[test]
    fn error_state() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "12030403040102030904010000000000000003010008006400000400",
        ));
        assert_eq!(prop(&ha, "status"), "Error".into());
        assert_eq!(prop(&ha, "error"), "ON".into());
        assert_eq!(
            prop(&ha, "error_message"),
            "Door open error (DE1)".into()
        );
    }

    #[test]
    fn remote_start_on() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode(
            "01010e010e0000000000000000000042000003000007006400000000",
        ));
        assert_eq!(prop(&ha, "remote_start"), "ON".into());
        assert_eq!(prop(&ha, "door_lock"), "OFF".into());
    }

    #[test]
    fn short_frames_ignored() {
        let (ha, thinq, _dev) = make_device();
        thinq.emit_data(&hex_decode("AABBCC"));
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());
    }

    #[test]
    fn start_sends_mon() {
        let (_ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(thinq.sent().len(), 1);
        assert_eq!(thinq.sent()[0]["Cmd"], "Mon");
        assert_eq!(thinq.sent()[0]["CmdOpt"], "Start");
    }

    #[test]
    fn write_power_on_off() {
        let (_ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(thinq.sent()[0]["Value"], "On");
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(thinq.sent()[0]["Value"], "Off");
    }

    #[test]
    fn write_pause() {
        let (_ha, thinq, dev) = make_device();
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(thinq.sent()[0]["CmdOpt"], "Operation");
        assert_eq!(thinq.sent()[0]["Value"], "Stop");
    }
}
