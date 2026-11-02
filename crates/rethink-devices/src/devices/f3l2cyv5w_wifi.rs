//! F3L2CYV5W_WIFI — LG WM3700HVA front-load washer (ThinQ1, deviceType 201, QC_Modem_1.2.75).
//!
//! Read-only. The 24-byte status frame is the F3L7CYK5W_US_WIFI record without its
//! leading 0x18 marker, so every offset is one lower and the status, course, soil,
//! spin and temperature tables are F3L7's. The two washers use different framing,
//! so this is a separate handler. The washer leaves the network when it powers
//! down and monitoring does not survive that, so `start()` sends `Mon Start` on
//! every connect.
//!
//! Port of upstream PR #288.

use crate::device_trait::DeviceHandler;
use crate::devices::f3l7cyk5w_us_wifi::{status_name, COURSE, SOIL, SPIN, STATUS_OPTIONS, TEMP};
use rethink_core::device_base::default_config;
use rethink_core::ha::{DeviceDiscovery, HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq1Device;
use rethink_util::sync::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_LENGTH: usize = 24;

const PHASE_OFFSET: usize = 0;
const TIME_HOUR_OFFSET: usize = 1;
const TIME_MIN_OFFSET: usize = 2;
const INITIAL_TIME_HOUR_OFFSET: usize = 3;
const INITIAL_TIME_MIN_OFFSET: usize = 4;
const COURSE_OFFSET: usize = 5;
const SOIL_OFFSET: usize = 7;
const SPIN_OFFSET: usize = 8;
const TEMP_OFFSET: usize = 9;
/// The low nibble is the rinse count; F3L7's high nibble is its extra-rinse setting.
const RINSE_OFFSET: usize = 10;
/// Washes since the last Tub Clean. Survives power-off, unlike the previous phase at 19.
const TCL_COUNT_OFFSET: usize = 21;

const PHASE_OFF: u8 = 0x00;
const PHASE_END: u8 = 0x3c;

fn named(table: &[(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn labels(table: &[(u8, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(_, name)| *name).collect()
}

fn hours_minutes(hours: u8, minutes: u8) -> i64 {
    i64::from(hours) * 60 + i64::from(minutes)
}

pub struct Device {
    id: String,
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq1Device>,
    config: DeviceDiscovery,
    publish_cache: Mutex<HashMap<String, String>>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq1Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let mut config = default_config(&meta, Some(json!({"name": "LG Washer"})));
        config.components = [
            ("power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:washing-machine","device_class":"running"})),
            ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":STATUS_OPTIONS})),
            ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline","device_class":"enum","options":labels(COURSE)})),
            ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"min"})),
            ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:liquid-spot","device_class":"enum","options":labels(SOIL)})),
            ("spin", json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","device_class":"enum","options":labels(SPIN)})),
            ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer","device_class":"enum","options":labels(TEMP)})),
            ("rinse_count", json!({"platform":"sensor","unique_id":"$deviceid-rinse_count","state_topic":"$this/rinse_count","name":"Rinses remaining","icon":"mdi:water-sync","state_class":"measurement"})),
            ("tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since Tub Clean","icon":"mdi:counter","entity_category":"diagnostic"})),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();

        let this = Arc::new(Self {
            id: thinq.id().to_string(),
            ha,
            thinq: thinq.clone(),
            config,
            publish_cache: Mutex::new(HashMap::new()),
        });
        this.publish_config();
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

    fn on_data(&self, buf: &[u8]) {
        if buf.len() != STATUS_LENGTH {
            return;
        }
        let phase = buf[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;
        // The raw time bytes park on a stale 1 minute at Off and at End.
        let (remaining, initial) = if is_off || phase == PHASE_END {
            (0, 0)
        } else {
            (
                hours_minutes(buf[TIME_HOUR_OFFSET], buf[TIME_MIN_OFFSET]),
                hours_minutes(buf[INITIAL_TIME_HOUR_OFFSET], buf[INITIAL_TIME_MIN_OFFSET]),
            )
        };

        self.publish_property("power", if is_off { "OFF" } else { "ON" }.into());
        self.publish_property("status", status_name(phase).into());
        self.publish_label("course", named(COURSE, buf[COURSE_OFFSET]));
        self.publish_property("remaining_time", PropertyValue::Int(remaining));
        self.publish_property("initial_time", PropertyValue::Int(initial));
        self.publish_label("soil", named(SOIL, buf[SOIL_OFFSET]));
        self.publish_label("spin", named(SPIN, buf[SPIN_OFFSET]));
        self.publish_label("temp", named(TEMP, buf[TEMP_OFFSET]));
        self.publish_property(
            "rinse_count",
            PropertyValue::Int(i64::from(buf[RINSE_OFFSET] & 0x0f)),
        );
        self.publish_property(
            "tub_clean_count",
            PropertyValue::Int(i64::from(buf[TCL_COUNT_OFFSET])),
        );
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
        self.ha
            .publish_property(&self.id, "availability", "offline".into());
    }
    fn set_property(&self, _prop: &str, _value: &str) {}
    fn publish_config(&self) {
        self.ha
            .publish_property(&self.id, "availability", "online".into());
        self.ha.publish_config(&self.id, &self.config);
    }
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

    // Samples: one empty Normal cycle on a real WM3700HVA, with the panel on Warm, High
    // spin, Normal soil. The panel read 53 minutes during SAMPLE_WASHING_0H52's run, one
    // frame earlier.
    const SAMPLE_SENSING_1H05: &str =
        "14 01 05 01 05 06 00 03 04 04 02 00 00 00 00 00 02 00 00 05 33 36 00 00";
    const SAMPLE_WASHING_0H52: &str =
        "17 00 34 00 35 06 00 03 04 04 02 00 00 00 00 00 02 00 02 14 33 36 00 01";
    const SAMPLE_RINSING_0H26: &str =
        "1e 00 1a 00 35 06 00 00 04 04 02 00 00 00 00 00 02 00 0e 17 33 36 00 01";
    const SAMPLE_SPINNING_0H11: &str =
        "28 00 0b 00 35 06 00 00 04 00 00 00 00 00 00 00 02 00 19 1e 33 36 00 01";
    const SAMPLE_COMPLETE: &str =
        "3c 00 01 00 00 fe 00 00 00 00 00 00 00 00 00 00 00 00 2a 28 33 37 00 00";
    const SAMPLE_OFF: &str =
        "00 00 01 00 00 fe 00 00 00 00 00 00 00 00 00 00 00 00 2a 3c 33 37 00 00";
    // Dial sweep, washer idle: time, course and settings read off the same panel.
    const SAMPLE_HEAVY_DUTY_2H21: &str =
        "05 02 15 02 15 05 00 05 05 04 02 00 00 00 00 00 00 00 00 00 33 37 00 00";
    const SAMPLE_BRIGHT_WHITES_1H12: &str =
        "05 01 0c 01 0c 07 00 03 04 06 03 00 00 00 00 00 00 00 00 00 33 37 00 00";
    const SAMPLE_SPEED_WASH_0H15: &str =
        "05 00 0f 00 0f 0b 00 01 05 06 01 00 00 00 00 00 00 00 00 00 33 37 00 07";
    // The first frame after a reconnect, ASCII rather than status.
    const SAMPLE_CONNECT_REPORT: &[u8] = b"RuleVer=1.3,FwVer=QC_Modem_1.2.75,regFail=N";

    fn meta() -> Metadata {
        Metadata::new("F3L2CYV5W_WIFI", "F3L2CYV5W_WIFI", "1.0")
    }

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq1Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq1Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }

    fn prop(ha: &MockHaConnection, name: &str) -> Option<String> {
        ha.device(DEVICE_ID)?
            .properties
            .get(name)
            .map(|p| p.as_string())
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    fn emit(thinq: &MockThinq1Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    #[test]
    fn a_real_cycle_walks_the_phases_with_the_countdown() {
        let (ha, thinq, _) = make();

        emit(&thinq, SAMPLE_SENSING_1H05);
        expect(&ha, "status", "Detecting");
        expect(&ha, "remaining_time", "65");

        emit(&thinq, SAMPLE_WASHING_0H52);
        expect(&ha, "status", "Washing");
        expect(&ha, "course", "Normal");
        expect(&ha, "remaining_time", "52");
        expect(&ha, "initial_time", "53");

        emit(&thinq, SAMPLE_RINSING_0H26);
        expect(&ha, "status", "Rinsing");
        expect(&ha, "remaining_time", "26");

        emit(&thinq, SAMPLE_SPINNING_0H11);
        expect(&ha, "status", "Spinning");
        expect(&ha, "remaining_time", "11");
    }

    #[test]
    fn finish_reports_end_with_the_stale_1_minute_and_no_course_sentinel_cleared() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_SPINNING_0H11);
        emit(&thinq, SAMPLE_COMPLETE);
        expect(&ha, "status", "End");
        expect(&ha, "power", "ON");
        expect(&ha, "remaining_time", "0");
        expect(&ha, "initial_time", "0");
        expect(&ha, "course", "None");
        expect(&ha, "tub_clean_count", "55");
        // Settings that no longer apply read as HA's unknown.
        expect(&ha, "soil", "None");
        expect(&ha, "spin", "None");
        expect(&ha, "temp", "None");
    }

    #[test]
    fn auto_power_off_after_a_finish_reads_as_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_OFF);
        expect(&ha, "status", "Off");
        expect(&ha, "power", "OFF");
        expect(&ha, "remaining_time", "0");
    }

    #[test]
    fn dial_positions_decode_the_course_and_its_settings() {
        let (ha, thinq, _) = make();

        emit(&thinq, SAMPLE_HEAVY_DUTY_2H21);
        expect(&ha, "status", "Standby");
        expect(&ha, "course", "Heavy Duty");
        expect(&ha, "remaining_time", "141");
        expect(&ha, "soil", "Heavy");
        expect(&ha, "spin", "Extra High");
        expect(&ha, "temp", "Warm");

        emit(&thinq, SAMPLE_BRIGHT_WHITES_1H12);
        expect(&ha, "course", "Bright Whites");
        expect(&ha, "temp", "Hot");
        expect(&ha, "rinse_count", "3");

        emit(&thinq, SAMPLE_SPEED_WASH_0H15);
        expect(&ha, "course", "Speed Wash");
        expect(&ha, "soil", "Light");
    }

    #[test]
    fn the_connect_report_does_not_disturb_the_decoded_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_0H52);
        thinq.emit_data(SAMPLE_CONNECT_REPORT);
        expect(&ha, "status", "Washing");
    }

    #[test]
    fn start_asks_the_washer_for_monitoring_frames_on_every_connect() {
        let (_, thinq, dev) = make();
        dev.start();
        assert_eq!(
            thinq.sent(),
            vec![json!({ "Cmd": "Mon", "CmdOpt": "Start" })]
        );
    }

    #[test]
    fn discovery_uses_the_f3l7_tables() {
        let (ha, _, _) = make();
        let config = ha.device(DEVICE_ID).unwrap().config.unwrap();
        let c = &config.components;
        assert_eq!(c.len(), 10);
        assert_eq!(c["status"]["options"], json!(STATUS_OPTIONS));
        assert_eq!(
            c["course"]["options"],
            json!([
                "Tub Clean",
                "Allergiene",
                "Sanitary",
                "Bedding",
                "Heavy Duty",
                "Normal",
                "Bright Whites",
                "Perm Press",
                "Delicates",
                "Towels",
                "Speed Wash",
                "Downloaded"
            ])
        );
        assert_eq!(
            c["soil"]["options"],
            json!(["Light", "Light-Normal", "Normal", "Normal-Heavy", "Heavy"])
        );
        assert_eq!(
            c["spin"]["options"],
            json!(["No Spin", "Low", "Medium", "High", "Extra High"])
        );
        assert_eq!(
            c["temp"]["options"],
            json!(["Tap Cold", "Cold", "Warm", "Hot", "Extra Hot"])
        );
        assert_eq!(c["power"]["device_class"], "running");
        assert!(c.values().all(|v| v.get("command_topic").is_none()));
    }

    /// Records availability and discovery publishes in order.
    #[derive(Default)]
    struct DiscoveryLog {
        events: Mutex<Vec<String>>,
        configs: Mutex<Vec<DeviceDiscovery>>,
    }

    impl HaConnection for DiscoveryLog {
        fn publish_config(&self, _id: &str, config: &DeviceDiscovery) {
            self.events.lock().push("config".into());
            self.configs.lock().push(config.clone());
        }
        fn publish_property(&self, _id: &str, property: &str, value: PropertyValue) {
            if property == "availability" {
                self.events
                    .lock()
                    .push(format!("availability={}", value.as_string()));
            }
        }
        fn publish_event(&self, _id: &str, _topic_suffix: &str, _payload: &str) {}
        fn is_connected(&self) -> bool {
            true
        }
    }

    #[test]
    fn publish_config_republishes_availability_and_the_discovery() {
        let log = Arc::new(DiscoveryLog::default());
        let dev = Device::new(
            log.clone(),
            MockThinq1Device::new(DEVICE_ID, meta()),
            meta(),
        );
        dev.drop_device();
        dev.publish_config();
        assert_eq!(
            *log.events.lock(),
            [
                "availability=online",
                "config",
                "availability=offline",
                "availability=online",
                "config"
            ]
        );
        let configs = log.configs.lock();
        assert_eq!(configs[1].components, configs[0].components);
        assert_eq!(configs[1].device.name.as_deref(), Some("LG Washer"));
        assert_eq!(configs[1].components.len(), 10);
    }

    #[test]
    fn registry_resolves_over_thinq1() {
        assert!(crate::registry::t1_factory("F3L2CYV5W_WIFI").is_some());
        assert!(crate::registry::t2_factory("F3L2CYV5W_WIFI").is_none());
    }
}
