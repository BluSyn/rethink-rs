//! WMVEM1825 — LG MVEM1825-series microwave (AABB class 0x41).
//!
//! Port of upstream commit 7ad7f28 (PR #160). Status is a 46-byte record
//! (0x41EB / current half of 0x41EC). Hood fan/light share rec[36]; door uses
//! a short 0x41B2 event. Combined F0432204 writes were verified live on the
//! appliance.

use crate::device_trait::DeviceHandler;
use crate::js;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const STATUS_QUERY: &str =
    "F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000";
const DOOR_PREFIX: &[u8] = &[0x00, 0x0f, 0x07, 0x0a, 0x01, 0x03, 0x00, 0x00, 0x00];

fn status_name(code: u8) -> &'static str {
    match code {
        0x00 => "Idle",
        0x02 => "Cooking",
        0x04 => "Paused",
        0x05 => "Done",
        0x07 => "Ready to start",
        _ => "None",
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    fan_speed: Mutex<u8>,
    light_level: Mutex<u8>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Microwave"})));
        cfg.components = [
            (
                "status".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-status",
                    "state_topic": "$this/status",
                    "name": "Status",
                    "icon": "mdi:state-machine",
                    "device_class": "enum",
                    "options": ["Idle", "Cooking", "Paused", "Done", "Ready to start"],
                }),
            ),
            (
                "remaining_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-remaining_time",
                    "state_topic": "$this/remaining_time",
                    "name": "Remaining time",
                    "icon": "mdi:timer-outline",
                    "device_class": "duration",
                    "unit_of_measurement": "s",
                }),
            ),
            (
                "power_level".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-power_level",
                    "state_topic": "$this/power_level",
                    "name": "Power level",
                    "icon": "mdi:gauge",
                    "unit_of_measurement": "%",
                }),
            ),
            (
                "door".into(),
                json!({
                    "platform": "binary_sensor",
                    "unique_id": "$deviceid-door",
                    "state_topic": "$this/door",
                    "name": "Door",
                    "device_class": "door",
                }),
            ),
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
                    "speed_range_max": 2,
                    "name": "Vent fan",
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
                    "name": "Cooktop light",
                    "icon": "mdi:lightbulb",
                }),
            ),
        ]
        .into_iter()
        .collect();
        core.set_config(cfg);

        let this = Arc::new(Self {
            core: core.clone(),
            fan_speed: Mutex::new(0),
            light_level: Mutex::new(0),
        });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_status(&self, rec: &[u8]) {
        if rec.len() < 37 {
            return;
        }
        let state = rec[0];
        let power_level = rec[10];
        let remaining = rec[15] as i64 * 3600 + rec[16] as i64 * 60 + rec[17] as i64;
        let hood = rec[36];
        let fan_speed = hood & 0x0f;
        let light_level = (hood >> 4) & 0x07;

        self.core
            .publish_property("status", status_name(state).into());
        self.core.publish_property("remaining_time", remaining.into());
        if power_level <= 10 {
            self.core
                .publish_property("power_level", (i64::from(power_level) * 10).into());
        }
        if fan_speed <= 2 {
            *self.fan_speed.lock() = fan_speed;
            self.core.publish_on_off("fan_power", fan_speed > 0);
            self.core
                .publish_property("fan_speed", i64::from(fan_speed).into());
        }
        if light_level <= 2 {
            *self.light_level.lock() = light_level;
            self.core.publish_on_off("light_power", light_level > 0);
            self.core
                .publish_property("light_level", i64::from(light_level).into());
        }
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 48 && buf[0] == 0x41 && buf[1] == 0xeb {
            self.process_status(&buf[2..48]);
            return;
        }
        if buf.len() == 94 && buf[0] == 0x41 && buf[1] == 0xec {
            self.process_status(&buf[48..94]);
            return;
        }
        if buf.len() == 12
            && buf[0] == 0x41
            && buf[1] == 0xb2
            && buf.get(2..11) == Some(DOOR_PREFIX)
        {
            if buf[11] == 0 || buf[11] == 1 {
                self.core.publish_on_off("door", buf[11] == 1);
            }
        }
    }

    fn send_hood_command(&self) {
        let fan = *self.fan_speed.lock();
        let light = *self.light_level.lock();
        self.core.send(&[
            0xf0,
            0x43,
            0x22,
            0x04,
            if fan > 0 { 1 } else { 0 },
            fan,
            if light > 0 { 1 } else { 0 },
            light,
            0x80,
            0x80,
        ]);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "fan_power" => {
                let mut fan = self.fan_speed.lock();
                *fan = if mqtt_value == "ON" {
                    if *fan == 0 { 1 } else { *fan }
                } else {
                    0
                };
                drop(fan);
                self.send_hood_command();
            }
            "fan_speed" => {
                let Some(v) = js::number(mqtt_value).filter(|v| v.is_finite()) else {
                    return;
                };
                *self.fan_speed.lock() = v.round().clamp(0.0, 2.0) as u8;
                self.send_hood_command();
            }
            "light_power" => {
                let mut light = self.light_level.lock();
                *light = if mqtt_value == "ON" {
                    if *light == 0 { 1 } else { *light }
                } else {
                    0
                };
                drop(light);
                self.send_hood_command();
            }
            "light_level" => {
                let Some(v) = js::number(mqtt_value).filter(|v| v.is_finite()) else {
                    return;
                };
                *self.light_level.lock() = v.round().clamp(0.0, 2.0) as u8;
                self.send_hood_command();
            }
            _ => {}
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        self.core.send(&hex_decode(STATUS_QUERY));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_trait::DeviceHandler;
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("WMVEM1825", Device::new)
    }

    fn feed(thinq: &rethink_core::MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    // Real MVEM1825 captures, each matched to the physical appliance.
    const SAMPLE_INITIAL_OFF: &str = "AA3441EB003100015500000000000000FF030D000000000000000000000000000000C3000000004000008080808001000000F1BB";
    const SAMPLE_COOKING_10_SECONDS_HIGH: &str = "AA6241EC073000004001000000000000FF030D000000000000000000000000000000C3000100004000008080808001000000023000004401000000000A00FF030D00000A00000A000000000000000000C700010000400001808080800100000026BB";
    const SAMPLE_COOKING_1_05_POWER_5: &str = "AA6241EC073000004001000000000000FF030D000000000000000000000000000000C3000100004000008080808001000000023000004401000000000500FF030D000105000105000000000000000000C700010000400001808080800100000033BB";
    const SAMPLE_PAUSED_AT_57_SECONDS: &str = "AA6241EC023000004401000000000500FF030D000039000105000000000000000000C7000100004000018080808001000000043000004401000000000500FF030D000039000105000000000000000000C7000100004000018080808001000000B6BB";
    const SAMPLE_DONE: &str = "AA6241EC023000004401000000000A00FF030D00000100000A000000000000000000C7000100004000018080808001000000053000015500000000000000FF030D000000000000000000000000000000C300000000400000808080800100000029BB";
    const SAMPLE_DOOR_OPEN: &str = "AA0041B2000F070A01030000000197BB";
    const SAMPLE_DOOR_CLOSED: &str = "AA0041B2000F070A01030000000094BB";
    const SAMPLE_LIGHT_LOW: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000004000008080808001000000003000015500000000000000FF030D000000000000000000000000000000C30000000040100080808080010000002EBB";
    const SAMPLE_FAN_HIGH: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000004001008080808001000000003000015500000000000000FF030D000000000000000000000000000000C30000000040020080808080010000003BBB";
    const SAMPLE_FAN_HIGH_LIGHT_LOW: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000004010008080808001000000003000015500000000000000FF030D000000000000000000000000000000C3000000004012008080808001000000D8BB";

    #[test]
    fn power_differs_from_target_seconds_in_a_captured_20_second_level_5_cook() {
        // 20 seconds at power level 5: rec[10] = 05, rec[17] = 14 (remaining), rec[20] = 14 (target)
        let (ha, thinq, _) = make();
        feed(&thinq, "aa6241ec073000004001000000000000ff030d000000000000000000000000000000c3000100004000008080808001000000023000004401000000000500ff030d000014000014000000000000000000c7000100004000018080808001000000d7bb");
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cooking"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("50"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("20"));
    }

    #[test]
    fn config_exposes_microwave_status_and_controllable_hood_components() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        let mut keys: Vec<&str> = comps.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = vec![
            "status",
            "remaining_time",
            "power_level",
            "door",
            "fan_power",
            "light_power",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(comps["status"]["device_class"], "enum");
        assert_eq!(
            comps["status"]["options"],
            json!(["Idle", "Cooking", "Paused", "Done", "Ready to start"])
        );
        assert_eq!(comps["remaining_time"]["device_class"], "duration");
        assert_eq!(comps["remaining_time"]["unit_of_measurement"], "s");
        assert_eq!(comps["door"]["device_class"], "door");
        assert_eq!(comps["fan_power"]["platform"], "fan");
        assert_eq!(comps["fan_power"]["speed_range_max"], 2);
        assert_eq!(comps["light_power"]["platform"], "light");
        assert_eq!(comps["light_power"]["brightness_scale"], 2);
    }

    #[test]
    fn initial_status_reports_idle_with_both_hood_controls_off() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_INITIAL_OFF);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("0"));
    }

    #[test]
    fn ten_second_high_power_cook_reports_cooking_time_and_power() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_COOKING_10_SECONDS_HIGH);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cooking"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("10"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("100"));
    }

    #[test]
    fn one_minute_five_power_level_5_cook_converts_time_and_power_to_ha_units() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_COOKING_1_05_POWER_5);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cooking"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("65"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("50"));
    }

    #[test]
    fn pause_and_completion_states_decode_from_real_transitions() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_PAUSED_AT_57_SECONDS);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("57"));

        feed(&thinq, SAMPLE_DONE);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Done"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
    }

    #[test]
    fn door_events_publish_open_and_closed_states() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_DOOR_OPEN);
        assert_eq!(prop(&ha, "door").as_deref(), Some("ON"));
        feed(&thinq, SAMPLE_DOOR_CLOSED);
        assert_eq!(prop(&ha, "door").as_deref(), Some("OFF"));
    }

    #[test]
    fn packed_hood_status_decodes_fan_and_light_independently() {
        let (ha, thinq, _) = make();
        feed(&thinq, SAMPLE_FAN_HIGH_LIGHT_LOW);
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("1"));
    }

    #[test]
    fn fan_speed_command_preserves_the_reported_light_state() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_LIGHT_LOW);
        thinq.reset_recorder();
        dev.set_property("fan_speed", "2");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401020101808043BB"
        );
    }

    #[test]
    fn light_level_command_preserves_the_reported_fan_state() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_FAN_HIGH);
        thinq.reset_recorder();
        dev.set_property("light_level", "2");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401020102808042BB"
        );
    }

    #[test]
    fn turning_the_fan_off_preserves_the_reported_light_state() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_FAN_HIGH_LIGHT_LOW);
        thinq.reset_recorder();
        dev.set_property("fan_power", "OFF");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220400000101808046BB"
        );
    }

    #[test]
    fn invalid_numeric_control_values_are_ignored() {
        let (_, thinq, dev) = make();
        dev.set_property("fan_speed", "not-a-number");
        dev.set_property("light_level", "not-a-number");
        assert_eq!(thinq.outbox().len(), 0);
    }

    #[test]
    fn numeric_control_values_are_parsed_like_javascript_number() {
        let (_, thinq, dev) = make();
        feed(&thinq, SAMPLE_FAN_HIGH_LIGHT_LOW);
        thinq.reset_recorder();
        // "" is 0: the fan goes off and the light is preserved
        dev.set_property("fan_speed", "");
        dev.set_property("light_level", " 0x2 ");
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(
            sent,
            [
                "AA0EF043220400000101808046BB",
                "AA0EF043220400000102808041BB"
            ]
        );
    }

    #[test]
    fn start_sends_the_captured_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA28F0ED114101000000181A0207080C14191A1E262B30353A00000000000000000000000000F3BB"
        );
    }

    // an unmapped code goes out as the "None" payload, which HA shows as unknown
    #[test]
    fn unknown_status_uses_the_home_assistant_enum_fallback() {
        let (ha, thinq, _) = make();
        let mut unknown = hex_decode(SAMPLE_INITIAL_OFF);
        unknown[4] = 0xff;
        thinq.emit_data(&unknown);
        assert_eq!(prop(&ha, "status").as_deref(), Some("None"));
    }

    #[test]
    fn unrecognised_frame_shapes_are_ignored() {
        let (ha, thinq, _) = make();
        feed(&thinq, "AA084100430063BB");
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());
    }
}
