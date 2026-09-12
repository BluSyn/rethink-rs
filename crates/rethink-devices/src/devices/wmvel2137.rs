//! WMVEL2137 — LG MVEL2033F over-the-range microwave (AABB class 0x41).
//!
//! Port of upstream master `831f3a4`. Not an alias of WMVEM1825: this unit has
//! four vent speeds plus off (that one has two). Live captures while bridged to
//! the LG cloud; offsets confirmed against ovenState at the same timestamp.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x41;
const RECORD_LEN: usize = 46;
const FIELD_MARKER_OFFSET: usize = 35;
const FIELD_MARKER: u8 = 0x53;
const STATE_OFFSET: usize = 0;
const COOK_MODE_OFFSET: usize = 5;
const COOK_ITEM_OFFSET: usize = 6;
const POWER_LEVEL_OFFSET: usize = 10;
const REMAIN_TIME_OFFSET: usize = 16;
const COOK_TIME_OFFSET: usize = 18;
const TIMER_OFFSET: usize = 22;
const VENT_LAMP_OFFSET: usize = 36;

const CMD_SET: u8 = 0x01;
const CMD_OFF: u8 = 0x00;
const CMD_NO_CHANGE: u8 = 0x80;
const MAX_VENT_SPEED: u8 = 4;
const MAX_LAMP_LEVEL: u8 = 2;

const STATUS_QUERY: &str = "f0ed114101000000180e111718191a1a1b00000000000000";

const STATES: &[&str] = &["Idle", "Cooking", "Paused", "Done", "Settings"];
const COOK_MODES: &[&str] = &[
    "Off",
    "Microwave",
    "Inverter defrost",
    "Sensor cook",
    "Auto cook",
];

fn status_name(code: u8) -> &'static str {
    match code {
        0x00 => "Idle",
        0x02 => "Cooking",
        0x04 => "Paused",
        0x05 => "Done",
        0x07 => "Settings",
        _ => "unknown",
    }
}

fn cook_mode_name(code: u8) -> &'static str {
    match code {
        0x00 => "Off",
        0x01 => "Microwave",
        0x15 => "Inverter defrost",
        0x18 => "Sensor cook",
        0x1f => "Auto cook",
        _ => "unknown",
    }
}

/// Sparse 16-bit dish id (big-endian rec[6..7]). 0 is idle, not HA's `None`.
fn cook_item_name(id: u16) -> &'static str {
    match id {
        0 => "Not selected",
        211 => "Defrost meat",
        212 => "Defrost poultry",
        213 => "Defrost fish",
        214 => "Defrost bread",
        243 => "Potato",
        245 => "Boiling water",
        246 => "Canned vegetables",
        247 => "Casserole",
        248 => "Chicken pieces",
        249 => "Fish fillets",
        250 => "Fresh vegetables hard",
        251 => "Fresh vegetables soft",
        252 => "Frozen lasagna",
        253 => "Frozen vegetables",
        254 => "Popcorn",
        255 => "Rice",
        256 => "Shrimp",
        258 => "Reheat baked goods",
        259 => "Beverage",
        260 => "Reheat casserole",
        261 => "Reheat dinner plate",
        263 => "Reheat pizza",
        264 => "Reheat soup or sauce",
        3338 => "Kids corn dog",
        3346 => "Kids mac and cheese",
        3354 => "Soften butter",
        3355 => "Soften cream cheese",
        3357 => "Soften frozen juice",
        3358 => "Soften ice cream",
        3359 => "Melt butter",
        3361 => "Melt cheese",
        3362 => "Melt chocolate",
        3363 => "Melt marshmallow",
        3364 => "Steam asparagus",
        3365 => "Steam broccoli",
        3366 => "Steam brussels sprouts",
        3367 => "Steam carrots",
        3368 => "Steam chicken breast",
        3369 => "Steam fish",
        3370 => "Steam zucchini",
        3378 => "Kids chicken nuggets",
        _ => "unknown",
    }
}

fn cook_item_options() -> Vec<&'static str> {
    [
        0, 211, 212, 213, 214, 243, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 255, 256,
        258, 259, 260, 261, 263, 264, 3338, 3346, 3354, 3355, 3357, 3358, 3359, 3361, 3362, 3363,
        3364, 3365, 3366, 3367, 3368, 3369, 3370, 3378,
    ]
    .into_iter()
    .map(cook_item_name)
    .collect()
}

fn minutes_seconds(rec: &[u8], offset: usize) -> i64 {
    i64::from(rec[offset]) * 60 + i64::from(rec[offset + 1])
}

fn clamp_u8(value: f64, max: u8) -> u8 {
    value.round().clamp(0.0, f64::from(max)) as u8
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
    last_vent_speed: Mutex<u8>,
    last_lamp_level: Mutex<u8>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
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
                    "options": STATES,
                }),
            ),
            (
                "cook_mode".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-cook_mode",
                    "state_topic": "$this/cook_mode",
                    "name": "Cook mode",
                    "icon": "mdi:chef-hat",
                    "device_class": "enum",
                    "options": COOK_MODES,
                }),
            ),
            (
                "cook_item".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-cook_item",
                    "state_topic": "$this/cook_item",
                    "name": "Cook item",
                    "icon": "mdi:silverware-fork-knife",
                    "device_class": "enum",
                    "options": cook_item_options(),
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
                "cook_time".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-cook_time",
                    "state_topic": "$this/cook_time",
                    "name": "Cook time",
                    "icon": "mdi:timelapse",
                    "device_class": "duration",
                    "unit_of_measurement": "s",
                }),
            ),
            (
                "timer".into(),
                json!({
                    "platform": "sensor",
                    "unique_id": "$deviceid-timer",
                    "state_topic": "$this/timer",
                    "name": "Kitchen timer",
                    "icon": "mdi:timer-outline",
                    "device_class": "duration",
                    "unit_of_measurement": "s",
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
                    "speed_range_max": MAX_VENT_SPEED,
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
                    "brightness_scale": MAX_LAMP_LEVEL,
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
            last_vent_speed: Mutex::new(1),
            last_lamp_level: Mutex::new(1),
        });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_status(&self, rec: &[u8]) {
        if rec.len() != RECORD_LEN || rec[FIELD_MARKER_OFFSET] != FIELD_MARKER {
            return;
        }
        let item = u16::from(rec[COOK_ITEM_OFFSET]) << 8 | u16::from(rec[COOK_ITEM_OFFSET + 1]);
        self.core
            .publish_property("status", status_name(rec[STATE_OFFSET]).into());
        self.core
            .publish_property("cook_mode", cook_mode_name(rec[COOK_MODE_OFFSET]).into());
        self.core
            .publish_property("cook_item", cook_item_name(item).into());
        self.core.publish_property(
            "power_level",
            (i64::from(rec[POWER_LEVEL_OFFSET]) * 10).into(),
        );
        self.core.publish_property(
            "remaining_time",
            minutes_seconds(rec, REMAIN_TIME_OFFSET).into(),
        );
        self.core
            .publish_property("timer", minutes_seconds(rec, TIMER_OFFSET).into());
        let cook_time = i64::from(rec[COOK_TIME_OFFSET]) * 3600
            + i64::from(rec[COOK_TIME_OFFSET + 1]) * 60
            + i64::from(rec[COOK_TIME_OFFSET + 2]);
        self.core.publish_property("cook_time", cook_time.into());

        let vent_speed = rec[VENT_LAMP_OFFSET] & 0x0f;
        let lamp_level = rec[VENT_LAMP_OFFSET] >> 4;
        if vent_speed != 0 {
            *self.last_vent_speed.lock() = vent_speed;
        }
        if lamp_level != 0 {
            *self.last_lamp_level.lock() = lamp_level;
        }
        self.core
            .publish_property("fan_speed", i64::from(vent_speed).into());
        self.core.publish_on_off("fan_power", vent_speed != 0);
        self.core
            .publish_property("light_level", i64::from(lamp_level).into());
        self.core.publish_on_off("light_power", lamp_level != 0);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first().copied() != Some(CLASS_BYTE) {
            return;
        }
        if buf.len() == 48 && buf[1] == 0xeb {
            self.process_status(&buf[2..2 + RECORD_LEN]);
        } else if buf.len() == 94 && buf[1] == 0xec {
            self.process_status(&buf[48..48 + RECORD_LEN]);
        }
    }

    fn send_command(&self, fan_cmd: u8, fan_level: u8, light_cmd: u8, light_level: u8) {
        self.core.send(&[
            0xf0,
            0x43,
            0x22,
            0x04,
            fan_cmd,
            fan_level,
            light_cmd,
            light_level,
            0x80,
            0x80,
        ]);
    }

    fn set_vent(&self, speed: u8) {
        if speed == 0 {
            self.send_command(CMD_OFF, 0, CMD_NO_CHANGE, CMD_NO_CHANGE);
        } else {
            self.send_command(CMD_SET, speed, CMD_NO_CHANGE, CMD_NO_CHANGE);
        }
    }

    fn set_lamp(&self, level: u8) {
        if level == 0 {
            self.send_command(CMD_NO_CHANGE, CMD_NO_CHANGE, CMD_OFF, 0);
        } else {
            self.send_command(CMD_NO_CHANGE, CMD_NO_CHANGE, CMD_SET, level);
        }
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        match prop {
            "fan_power" => {
                let speed = if mqtt_value == "ON" {
                    *self.last_vent_speed.lock()
                } else {
                    0
                };
                self.set_vent(speed);
            }
            "fan_speed" => {
                let Ok(v) = mqtt_value.parse::<f64>() else {
                    return;
                };
                if !v.is_finite() {
                    return;
                }
                self.set_vent(clamp_u8(v, MAX_VENT_SPEED));
            }
            "light_power" => {
                let level = if mqtt_value == "ON" {
                    *self.last_lamp_level.lock()
                } else {
                    0
                };
                self.set_lamp(level);
            }
            "light_level" => {
                let Ok(v) = mqtt_value.parse::<f64>() else {
                    return;
                };
                if !v.is_finite() {
                    return;
                }
                self.set_lamp(clamp_u8(v, MAX_LAMP_LEVEL));
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
    use crate::registry::t2_factory;
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, Thinq2Device};

    fn make() -> (
        Arc<rethink_core::MockHaConnection>,
        Arc<rethink_core::MockThinq2Device>,
        Arc<Device>,
    ) {
        make_t2("WMVEL2137", Device::new)
    }

    // Live MVEL2033F captures from upstream master tests (831f3a4).
    const SAMPLE_VENT_TURBO_LAMP_HIGH: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000005320008080808001000000003000015500000000000000FF030D000000000000000000000000000000C300000000532400808080800100000080BB";
    const SAMPLE_VENT_OFF_LAMP_HIGH: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000005324008080808001000000003000015500000000000000FF030D000000000000000000000000000000C300000000532000808080800100000080BB";
    const SAMPLE_VENT_LOW_LAMP_HIGH: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000005324008080808001000000003000015500000000000000FF030D000000000000000000000000000000C300000000532100808080800100000083BB";
    const SAMPLE_LAMP_LOW: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000005300008080808001000000003000015500000000000000FF030D000000000000000000000000000000C3000000005310008080808001000000F4BB";
    const SAMPLE_COOKING_FULL_POWER_5MIN: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C3000000005300008080808001000000023000004401000000000A00FF030D000500000500000000000000000000C7000100005300018080808001000000C9BB";
    const SAMPLE_COOKING_4M59S: &str = "AA6241EC023000004401000000000A00FF030D000500000500000000000000000000C7000100005300018080808001000000023000004401000000000A00FF030D00043B000500000000000000000000C7000100005300018080808001000000B4BB";
    const SAMPLE_PAUSED: &str = "AA6241EC023000004401000000000A00FF030D00043B000500000000000000000000C7000100005300018080808001000000043000004401000000000A00FF030D00041E000500000000000000000000C700010000530001808080800100000055BB";
    const SAMPLE_COOKING_HALF_POWER_30S: &str = "AA6241EC073000004001000000000000FF030D000000000000000000000000000000C3000100005300008080808001000000023000004401000000000500FF030D00001E00001E000000000000000000C7000100005300018080808001000000E9BB";
    const SAMPLE_DONE: &str = "AA6241EC023000004401000000000A00FF030D00001E00001E000000000000000000C7000100005300018080808001000000053000015500000000000000FF030D000000000000000000000000000000C300000000530000808080800100000086BB";
    const SAMPLE_SENSOR_COOK_SETTINGS: &str = "AA6241EC003000015500000000000000FF030D000000000000000000000000000000C300000000530000808080800100000007300000401800FE00000000FF030D000000000000000000000000000000E3000000005300008080808001000000EDBB";
    const SAMPLE_AUTO_COOK_RUNNING: &str = "AA6241EC07300000401F000000000000FF030D000000000000000000000000000000C300000000530000808080800100000002300000441F0D1200000A00FF030D00010A00010A000000000000000000C7000000005300018080808001000000A1BB";
    const SAMPLE_SENSOR_COOK_RICE: &str = "AA6241EC07300000401800FF00000000FF030D000000000000000000000000000000E300000000530000808080800100000002300000441800FF00000000FF030D000000000000000000000000000000E3000000005300018080808001000000B4BB";
    const SAMPLE_SENSOR_REHEAT_SOUP: &str = "AA6241EC073000004018010800000000FF030D000000000000000000000000000000E3000000005300008080808001000000023000004418010800000000FF030D000000000000000000000000000000E3000000005300018080808001000000A0BB";
    const SAMPLE_SOFTEN_BUTTER: &str = "AA6241EC07300000401F000000000000FF030D000000000000000000000000000000C300000000530000808080800100000002300000441F0D1A00000200FF030D000032000032000000000000000000C700000000530001808080800100000017BB";
    const SAMPLE_DEFROST_RUNNING: &str = "AA6241EC07300000401500D300000000FF030D000000000000000000000000000000C300000000530000808080800100000002300000441500D400000700FF030D000A00000A00000000000000000000C700000000530001808080800100000031BB";
    const SAMPLE_INITIAL_STATUS: &str = "AA3441EB003000015500000000000000FF030D000000000000000000000000000000C300000000532000808080800100000083BB";
    const TIMER_90_MINUTES: &str = "aa6241ec003000014000000000000000ff030d000000000000000000000000000000c3000000005300008080808001000000003000015500000000000000ff030d000000000000005a00000000000000c300000000530000808080800100000083bb";
    const COOKING_90_MINUTES: &str = "aa6241ec073000004001000000000000ff030d000000000000000000000000000000c3000100005300008080808001000000023000004401000000000a00ff030d005a00011e00000000000000000000c7000100005300018080808001000000abbb";

    #[test]
    fn registry_resolves_separately_from_wmvem1825() {
        let a = t2_factory("WMVEL2137").expect("WMVEL2137");
        let b = t2_factory("WMVEM1825").expect("WMVEM1825");
        assert!(!std::ptr::fn_addr_eq(a, b));
    }

    #[test]
    fn config_published_on_device_creation() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        for k in [
            "status",
            "cook_mode",
            "cook_item",
            "power_level",
            "remaining_time",
            "cook_time",
            "timer",
            "fan_power",
            "light_power",
        ] {
            assert!(comps.contains_key(k), "missing {k}");
        }
        assert_eq!(comps["status"]["device_class"], "enum");
        assert_eq!(comps["status"]["options"], json!(STATES));
        assert_eq!(comps["remaining_time"]["unit_of_measurement"], "s");
        assert_eq!(comps["timer"]["device_class"], "duration");
        let items = comps["cook_item"]["options"].as_array().unwrap();
        assert!(!items.iter().any(|v| v.as_str() == Some("None")));
        assert_eq!(comps["fan_power"]["speed_range_max"], MAX_VENT_SPEED);
        assert_eq!(comps["light_power"]["brightness_scale"], MAX_LAMP_LEVEL);
    }

    #[test]
    fn start_sends_lg_cloud_status_query() {
        let (_, thinq, dev) = make();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA1CF0ED114101000000180E111718191A1A1B0000000000000091BB"
        );
    }

    #[test]
    fn vent_and_lamp_are_opposite_nibbles() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_VENT_TURBO_LAMP_HIGH));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "light_power").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(SAMPLE_VENT_LOW_LAMP_HIGH));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("1"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("2"));

        thinq.emit_data(&hex_decode(SAMPLE_VENT_OFF_LAMP_HIGH));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "fan_power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("2"));

        thinq.emit_data(&hex_decode(SAMPLE_LAMP_LOW));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("1"));
    }

    #[test]
    fn vent_set_sends_absolute_level_and_leaves_lamp() {
        let (_, thinq, dev) = make();
        dev.set_property("fan_speed", "1");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401018080808046BB"
        );
        thinq.reset_recorder();
        dev.set_property("fan_speed", "2");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401028080808041BB"
        );
    }

    #[test]
    fn lamp_set_leaves_vent_alone() {
        let (_, thinq, dev) = make();
        dev.set_property("light_level", "1");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220480800101808046BB"
        );
    }

    #[test]
    fn turning_a_control_off_is_one_sided() {
        let (_, thinq, dev) = make();
        dev.set_property("light_power", "OFF");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220480800000808044BB"
        );
        thinq.reset_recorder();
        dev.set_property("fan_power", "OFF");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220400008080808044BB"
        );
    }

    #[test]
    fn turning_on_restores_last_reported_level() {
        let (_, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_VENT_TURBO_LAMP_HIGH));
        thinq.reset_recorder();
        dev.set_property("fan_power", "ON");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401048080808043BB"
        );
        thinq.reset_recorder();
        dev.set_property("light_power", "ON");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220480800102808041BB"
        );
    }

    #[test]
    fn out_of_range_levels_are_clamped() {
        let (_, thinq, dev) = make();
        dev.set_property("fan_speed", "9");
        assert_eq!(
            hex_encode(&thinq.outbox()[0]),
            "AA0EF043220401048080808043BB"
        );
        thinq.reset_recorder();
        dev.set_property("light_level", "not a number");
        assert_eq!(thinq.outbox().len(), 0);
    }

    #[test]
    fn cooking_state_mode_power_and_remaining() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_COOKING_FULL_POWER_5MIN));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cooking"));
        assert_eq!(prop(&ha, "cook_mode").as_deref(), Some("Microwave"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("100"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("300"));
        assert_eq!(prop(&ha, "cook_time").as_deref(), Some("300"));

        thinq.emit_data(&hex_decode(SAMPLE_COOKING_4M59S));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("299"));
        assert_eq!(prop(&ha, "cook_time").as_deref(), Some("300"));

        thinq.emit_data(&hex_decode(SAMPLE_COOKING_HALF_POWER_30S));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("50"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("30"));
        assert_eq!(prop(&ha, "cook_time").as_deref(), Some("30"));
    }

    #[test]
    fn duration_over_an_hour_counts_minutes_without_borrow() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(TIMER_90_MINUTES));
        assert_eq!(prop(&ha, "timer").as_deref(), Some("5400"));
    }

    #[test]
    fn cook_length_stays_apart_from_remaining() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(COOKING_90_MINUTES));
        assert_eq!(prop(&ha, "cook_time").as_deref(), Some("5400"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("5400"));
    }

    #[test]
    fn paused_done_and_settings() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_PAUSED));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("270"));
        thinq.emit_data(&hex_decode(SAMPLE_DONE));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Done"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        thinq.emit_data(&hex_decode(SAMPLE_SENSOR_COOK_SETTINGS));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Settings"));
        assert_eq!(prop(&ha, "cook_mode").as_deref(), Some("Sensor cook"));
    }

    #[test]
    fn dish_id_is_big_endian_16bit() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_SENSOR_COOK_RICE));
        assert_eq!(prop(&ha, "cook_mode").as_deref(), Some("Sensor cook"));
        assert_eq!(prop(&ha, "cook_item").as_deref(), Some("Rice"));
        thinq.emit_data(&hex_decode(SAMPLE_SENSOR_REHEAT_SOUP));
        assert_eq!(
            prop(&ha, "cook_item").as_deref(),
            Some("Reheat soup or sauce")
        );
        thinq.emit_data(&hex_decode(SAMPLE_AUTO_COOK_RUNNING));
        assert_eq!(
            prop(&ha, "cook_item").as_deref(),
            Some("Kids mac and cheese")
        );
        thinq.emit_data(&hex_decode(SAMPLE_SOFTEN_BUTTER));
        assert_eq!(prop(&ha, "cook_mode").as_deref(), Some("Auto cook"));
        assert_eq!(prop(&ha, "cook_item").as_deref(), Some("Soften butter"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("20"));
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL_STATUS));
        assert_eq!(prop(&ha, "cook_item").as_deref(), Some("Not selected"));
    }

    #[test]
    fn defrost_reports_mode_and_chosen_power() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_DEFROST_RUNNING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Cooking"));
        assert_eq!(prop(&ha, "cook_mode").as_deref(), Some("Inverter defrost"));
        assert_eq!(prop(&ha, "power_level").as_deref(), Some("70"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("600"));
    }

    #[test]
    fn single_record_eb_decodes_like_ec_current() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL_STATUS));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Idle"));
        assert_eq!(prop(&ha, "cook_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "fan_speed").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "light_level").as_deref(), Some("2"));
    }

    #[test]
    fn wrong_length_or_class_publishes_nothing() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode("AA0B413E000D000D001BBB"));
        thinq.emit_data(&hex_decode("AA6141EC003000015500000000000000FFBB"));
        assert!(prop(&ha, "status").is_none());
    }
}
