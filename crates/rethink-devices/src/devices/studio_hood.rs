//! STUDIO_HOOD — LG range hood (deviceType 304), AABB 0x43 status.
//!
//! Port of upstream commit 98a1c4a (PR #120).

use crate::device_trait::DeviceHandler;
use crate::js;
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
    // Upstream does not round a commanded light level: 0.5 sends the on flag with level byte 0.
    light_level: Mutex<f64>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            fan_speed: Mutex::new(0),
            light_level: Mutex::new(0.0),
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
        *self.light_level.lock() = f64::from(light);
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

    fn send_combined(&self, fan: u8, light: f64) {
        let fan_flag = if fan > 0 { 1u8 } else { 0 };
        let light_flag = if light > 0.0 { 1u8 } else { 0 };
        let body = [
            0xf0,
            0x43,
            0x22,
            0x05,
            fan_flag,
            fan,
            light_flag,
            light as u8,
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
        // An unparseable number is NaN upstream, which every later use treats as 0.
        match prop {
            "fan_power" => fan = if value == "ON" { fan.max(1) } else { 0 },
            "fan_speed" => {
                fan = js::number(value).map_or(0, |v| v.round().clamp(0.0, 5.0) as u8);
            }
            "light_power" => {
                light = match value {
                    "ON" if light > 0.0 => light,
                    "ON" => 1.0,
                    _ => 0.0,
                };
            }
            "light_level" => light = js::number(value).map_or(0.0, |v| v.clamp(0.0, 2.0)),
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
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("STUDIO_HOOD", Device::new)
    }

    fn feed(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    /// `[fanFlag, fanSpeed, lightFlag, lightLevel]` of the one command sent.
    fn command_fields(thinq: &MockThinq2Device) -> [u8; 4] {
        let outbox = thinq.outbox();
        assert_eq!(outbox.len(), 1);
        let pkt = &outbox[0];
        assert_eq!(pkt[2], 0xf0);
        assert_eq!(pkt[3], 0x43);
        [pkt[2 + 4], pkt[2 + 5], pkt[2 + 6], pkt[2 + 7]]
    }

    // Wire captures from a live STUDIO_HOOD while cycling the light (off, 1, 2, off) and the fan
    // (off, 1..5, off). AA 1E 43 EC <12B previous> <12B current> <cksum> BB.
    const SAMPLE_ACK: &str = "AA08430043006DBB";
    const SAMPLE_DELTA_OFF_TO_LIGHT1: &str =
        "AA1E43EC00000000000000000000000701000000000100000000000752BB";
    const SAMPLE_DELTA_LIGHT1_TO_LIGHT2: &str =
        "AA1E43EC0100000000010000000000070100000000020000000000075FBB";
    const SAMPLE_DELTA_LIGHT2_TO_OFF: &str =
        "AA1E43EC0100000000020000000000070000000000000000000000075DBB";
    const SAMPLE_DELTA_OFF_TO_FAN1: &str =
        "AA1E43EC0000000000000000000000070101040000000000000000075EBB";
    // Fan at 3, then a light-only command (fan fields zeroed) turned the fan off and the light to 2.
    const SAMPLE_DELTA_FAN3_RESET_BY_LIGHT_CMD: &str =
        "AA1E43EC01030400000000000000000701000000000200000000000745BB";
    // The 43 EB reply to the start() query: fan 1, light 1.
    const SAMPLE_INITIAL_STATUS_FAN1_LIGHT1: &str = "AA1243EB010104000001000000000007ADBB";

    #[test]
    fn config_published_on_device_creation_with_fan_and_light_components() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        assert_eq!(comps["fan_power"]["platform"], "fan");
        assert_eq!(comps["fan_power"]["speed_range_min"], 1);
        assert_eq!(comps["fan_power"]["speed_range_max"], 5);
        assert_eq!(comps["light_power"]["platform"], "light");
        assert_eq!(comps["light_power"]["brightness_scale"], 2);
    }

    #[test]
    fn light_turned_on_to_level_1() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_LIGHT1);
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("0"));
    }

    #[test]
    fn light_level_1_to_2() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_DELTA_LIGHT1_TO_LIGHT2);
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("2"));
    }

    #[test]
    fn light_turned_off() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_DELTA_LIGHT2_TO_OFF);
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("0"));
    }

    #[test]
    fn fan_turned_on_to_speed_1() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_FAN1);
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("ON"));
        // raw device speed, not a percentage
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("OFF"));
    }

    #[test]
    fn a_light_only_command_resets_a_running_fan_to_off() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_DELTA_FAN3_RESET_BY_LIGHT_CMD);
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("2"));
    }

    #[test]
    fn generic_command_ack_frames_are_ignored() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_FAN1);
        let before = ha.device(DEVICE_ID).unwrap().properties;
        feed(&thinq, SAMPLE_ACK);
        assert_eq!(ha.device(DEVICE_ID).unwrap().properties, before);
    }

    #[test]
    fn initial_43_eb_status_decodes_correctly() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_INITIAL_STATUS_FAN1_LIGHT1);
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("1"));
    }

    #[test]
    fn start_sends_the_status_query_packet_on_the_wire() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.start();
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA12F0ED1141010000001804030400005ABB"
        );
    }

    #[test]
    fn frames_not_matching_the_aabb_envelope_are_ignored() {
        let (ha, thinq, _) = make();
        feed(&thinq, "001122");
        assert!(prop(&ha, "fan_power").is_none());
    }

    #[test]
    fn frames_with_unrecognised_inner_shape_are_ignored() {
        let (ha, thinq, _) = make();
        feed(&thinq, "AA08109901020304BB");
        assert!(prop(&ha, "fan_power").is_none());
    }

    #[test]
    fn ha_write_fan_power_on_defaults_to_speed_1_and_preserves_light_state() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_LIGHT1);
        thinq.reset_recorder();
        dev.set_property("fan_power", "ON");
        assert_eq!(command_fields(&thinq), [1, 1, 1, 1]);
    }

    #[test]
    fn ha_write_fan_speed_3_preserves_light_state() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_DELTA_LIGHT1_TO_LIGHT2);
        thinq.reset_recorder();
        dev.set_property("fan_speed", "3");
        assert_eq!(command_fields(&thinq), [1, 3, 1, 2]);
    }

    #[test]
    fn ha_write_fan_power_off_zeroes_only_the_fan_fields() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_FAN1);
        feed(&thinq, SAMPLE_DELTA_LIGHT1_TO_LIGHT2);
        thinq.reset_recorder();
        dev.set_property("fan_power", "OFF");
        assert_eq!(command_fields(&thinq), [0, 0, 1, 2]);
    }

    #[test]
    fn ha_write_light_power_on_defaults_to_level_1_and_preserves_fan_state() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_FAN1);
        thinq.reset_recorder();
        dev.set_property("light_power", "ON");
        assert_eq!(command_fields(&thinq), [1, 1, 1, 1]);
    }

    #[test]
    fn ha_write_light_level_2_preserves_fan_state() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_FAN1);
        thinq.reset_recorder();
        dev.set_property("light_level", "2");
        assert_eq!(command_fields(&thinq), [1, 1, 1, 2]);
    }

    #[test]
    fn ha_write_to_unknown_property_emits_no_packet() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_DELTA_OFF_TO_LIGHT1);
        thinq.reset_recorder();
        dev.set_property("nonsense", "whatever");
        assert_eq!(thinq.outbox().len(), 0);
    }

    #[test]
    fn ha_writes_follow_upstream_payload_handling() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_INITIAL_STATUS_FAN1_LIGHT1);
        for (prop, value, fields) in [
            // only "ON" turns a control on
            ("fan_power", "on", [0, 0, 1, 1]),
            ("fan_speed", " 2.5 ", [1, 3, 1, 1]),
            ("fan_speed", "abc", [0, 0, 1, 1]),
            ("fan_speed", "0x9", [1, 5, 1, 1]),
            ("light_power", "toggle", [1, 5, 0, 0]),
            // the light level is clamped but not rounded
            ("light_level", "0.5", [1, 5, 1, 0]),
            ("light_power", "ON", [1, 5, 1, 0]),
            ("light_level", "", [1, 5, 0, 0]),
            ("light_level", "7", [1, 5, 1, 2]),
        ] {
            thinq.reset_recorder();
            dev.set_property(prop, value);
            assert_eq!(command_fields(&thinq), fields, "{prop}={value:?}");
        }
    }
}
