//! STUDIO_HOOD — LG range hood (deviceType 304), AABB 0x43 status (PR #120).

use crate::device_trait::DeviceHandler;
use rethink_util::sync::Mutex;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::ha::{HaConnection, PropertyValue};
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use serde_json::json;
use std::sync::Arc;

pub struct Device {
    core: Arc<AabbDeviceCore>,
    fan_speed: Mutex<u8>,
    light_level: Mutex<u8>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            fan_speed: Mutex::new(0),
            light_level: Mutex::new(0),
        });

        let mut config = default_config(&meta, Some(json!({"name": "LG Range Hood"})));
        config.components = [
            (
                "fan_power".into(),
                json!({
                    "platform": "fan",
                    "unique_id": "$deviceid-fan",
                    "state_topic": "$this/fan_power",
                    "command_topic": "$this/fan_power/set",
                    "percentage_state_topic": "$this/fan_speed",
                    "percentage_command_topic": "$this/fan_speed/set",
                    "speed_range_min": 1,
                    "speed_range_max": 5,
                    "name": "Fan",
                    "icon": "mdi:fan",
                }),
            ),
            (
                "light_power".into(),
                json!({
                    "platform": "light",
                    "unique_id": "$deviceid-light",
                    "state_topic": "$this/light_power",
                    "command_topic": "$this/light_power/set",
                    "brightness_state_topic": "$this/light_level",
                    "brightness_command_topic": "$this/light_level/set",
                    "brightness_scale": 2,
                    "name": "Light",
                    "icon": "mdi:lightbulb",
                }),
            ),
        ]
        .into_iter()
        .collect();
        core.set_config(config);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_status(&self, cur: &[u8]) {
        if cur.len() < 6 {
            return;
        }
        let fan = cur[1];
        let light = cur[5];
        *self.fan_speed.lock() = fan;
        *self.light_level.lock() = light;
        self.core.publish_on_off("fan_power", fan > 0);
        self.core
            .publish_property("fan_speed", PropertyValue::Int(fan as i64));
        self.core.publish_on_off("light_power", light > 0);
        self.core
            .publish_property("light_level", PropertyValue::Int(light as i64));
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 4 && buf[0] == 0x43 && buf[1] == 0x00 {
            return;
        }
        if buf.len() == 14 && buf[0] == 0x43 && buf[1] == 0xeb {
            self.process_status(&buf[2..14]);
            return;
        }
        if buf.len() == 26 && buf[0] == 0x43 && buf[1] == 0xec {
            self.process_status(&buf[14..26]);
        }
    }

    fn send_combined(&self, fan: u8, light: u8) {
        let fan_flag = if fan > 0 { 1u8 } else { 0 };
        let light_flag = if light > 0 { 1u8 } else { 0 };
        let body = [
            0xf0,
            0x43,
            0x22,
            0x05,
            fan_flag,
            fan,
            light_flag,
            light,
            0x00,
        ];
        self.core.send(&body);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core
            .send(&rethink_util::hex::decode("f0ed114101000000180403040000").unwrap());
    }
    fn drop_device(&self) {
        self.core.drop_device();
    }
    fn set_property(&self, prop: &str, value: &str) {
        let mut fan = *self.fan_speed.lock();
        let mut light = *self.light_level.lock();
        match prop {
            "fan_power" => {
                if value == "OFF" {
                    fan = 0;
                } else if fan == 0 {
                    fan = 1;
                }
            }
            "fan_speed" => {
                fan = value.parse::<u8>().unwrap_or(0).min(5);
            }
            "light_power" => {
                if value == "OFF" {
                    light = 0;
                } else if light == 0 {
                    light = 1;
                }
            }
            "light_level" => {
                light = value.parse::<u8>().unwrap_or(0).min(2);
            }
            _ => return,
        }
        *self.fan_speed.lock() = fan;
        *self.light_level.lock() = light;
        self.send_combined(fan, light);
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
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    const SAMPLE_DELTA_OFF_TO_LIGHT1: &str =
        "AA1E43EC00000000000000000000000701000000000100000000000752BB";
    const SAMPLE_DELTA_OFF_TO_FAN1: &str =
        "AA1E43EC0000000000000000000000070101040000000000000000075EBB";
    const SAMPLE_INITIAL: &str = "AA1243EB010104000001000000000007ADBB";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        let ha = MockHaConnection::new();
        let meta = Metadata::new("STUDIO_HOOD", "STUDIO_HOOD", "1.0");
        let thinq = MockThinq2Device::new(DEVICE_ID, meta.clone());
        (ha.clone(), thinq.clone(), Device::new(ha, thinq, meta))
    }

    #[test]
    fn config_has_fan_and_light() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fan_power"]["platform"], json!("fan"));
        assert_eq!(comps["light_power"]["platform"], json!("light"));
        assert_eq!(comps["fan_power"]["speed_range_max"], json!(5));
    }

    #[test]
    fn light_on_level_1() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_OFF_TO_LIGHT1));
        let p = ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(p.get("light_power").map(|x| x.as_string()).as_deref(), Some("ON"));
        assert_eq!(p.get("light_level").map(|x| x.as_string()).as_deref(), Some("1"));
        assert_eq!(p.get("fan_power").map(|x| x.as_string()).as_deref(), Some("OFF"));
    }

    #[test]
    fn fan_on_speed_1() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DELTA_OFF_TO_FAN1));
        let p = ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(p.get("fan_power").map(|x| x.as_string()).as_deref(), Some("ON"));
        assert_eq!(p.get("fan_speed").map(|x| x.as_string()).as_deref(), Some("1"));
    }

    #[test]
    fn start_sends_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert!(!thinq.outbox().is_empty());
        let h = hex_encode(&thinq.outbox()[0]).to_ascii_lowercase();
        assert!(h.contains("f0ed1141"));
    }

    #[test]
    fn initial_eb_status() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        let p = ha.device(DEVICE_ID).unwrap().properties;
        assert_eq!(p.get("fan_speed").map(|x| x.as_string()).as_deref(), Some("1"));
        assert_eq!(p.get("light_level").map(|x| x.as_string()).as_deref(), Some("1"));
    }

    #[test]
    fn set_light_sends_combined_frame() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("light_level", "2");
        assert_eq!(thinq.outbox().len(), 1);
        let h = hex_encode(&thinq.outbox()[0]).to_ascii_lowercase();
        assert!(h.contains("f04322"));
    }
}
