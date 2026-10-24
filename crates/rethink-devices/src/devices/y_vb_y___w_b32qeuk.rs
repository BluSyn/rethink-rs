//! Y_VB_Y___W.B32QEUK washer (LG F4W1175YW). AABB class 0x20.
//!
//! Port of unmerged upstream PR #148. 14-byte header + 39-byte status
//! (0xEB one block, 0xEC first of two). Offsets and 0x3a = "AI Wash" are
//! capture-confirmed; this is not an alias of Y_V8_Y___W.B32QEUK (53-byte).

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::request_status;
use crate::washer_common::{
    course_name, error_message, spin_value, state_name, temperature_value, DOSES,
};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::hex_decode;
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const HEADER_LENGTH: usize = 14;
const STATUS_LENGTH: usize = 39;

fn vb_course(code: u8) -> String {
    if code == 0x3a {
        "AI Wash".into()
    } else {
        course_name(code as u32).unwrap_or("unknown").into()
    }
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let mut cfg = default_config(&meta, Some(json!({"name": "LG Washer"})));
        let status_opts: Vec<&str> = crate::washer_common::STATES.iter().filter_map(|s| *s).collect();
        let error_opts: Vec<&str> = crate::washer_common::ERRORS.iter().filter_map(|s| *s).collect();
        cfg.components = [
            ("power".into(), json!({"platform":"button","unique_id":"$deviceid-power","command_topic":"$this/power/set","payload_press":"","name":"Power","icon":"mdi:power"})),
            ("start".into(), json!({"platform":"button","unique_id":"$deviceid-start","command_topic":"$this/start/set","payload_press":"","name":"Start","icon":"mdi:play-circle-outline"})),
            ("pause".into(), json!({"platform":"button","unique_id":"$deviceid-pause","command_topic":"$this/pause/set","payload_press":"","name":"Pause","icon":"mdi:pause-circle-outline"})),
            ("status".into(), json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine","device_class":"enum","options":status_opts})),
            ("error".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-error","state_topic":"$this/error","name":"Error","icon":"mdi:check-circle","device_class":"problem","entity_category":"diagnostic"})),
            ("error_message".into(), json!({"platform":"sensor","unique_id":"$deviceid-error-message","state_topic":"$this/error_message","name":"Error message","icon":"mdi:alert-circle-outline","device_class":"enum","entity_category":"diagnostic","options":error_opts})),
            ("course".into(), json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
            ("temp".into(), json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","device_class":"temperature","unit_of_measurement":"°C","suggested_display_precision":0,"value_template":"{{ value if value | is_number else 'None' }}"})),
            ("spin".into(), json!({"platform":"sensor","unique_id":"$deviceid-spin","state_topic":"$this/spin","name":"Spin","icon":"mdi:autorenew","unit_of_measurement":"RPM","value_template":"{{ value if value | is_number else 'None' }}"})),
            ("cycles".into(), json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycle count","icon":"mdi:counter"})),
            ("remote_start".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:play-circle-outline"})),
            ("door_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-door_lock","state_topic":"$this/door_lock","name":"Door lock","device_class":"lock"})),
            ("turbowash".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:speedometer"})),
            ("prewash".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"})),
            ("steam".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:weather-dust"})),
            ("delay_end".into(), json!({"platform":"sensor","unique_id":"$deviceid-delay_end","state_topic":"$this/delay_end","name":"Delay end","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"h"})),
            ("child_lock".into(), json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:account-lock"})),
            ("energy".into(), json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
            ("initial_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","device_class":"duration","unit_of_measurement":"min","name":"Initial time"})),
            ("detergent".into(), json!({"platform":"sensor","unique_id":"$deviceid-detergent","state_topic":"$this/detergent","name":"Detergent dose","icon":"mdi:cup","device_class":"enum","options":DOSES})),
            ("softener".into(), json!({"platform":"sensor","unique_id":"$deviceid-softener","state_topic":"$this/softener","name":"Softener dose","icon":"mdi:cup-outline","device_class":"enum","options":DOSES})),
            ("remaining_time".into(), json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","device_class":"duration","unit_of_measurement":"min","name":"Remaining time"})),
        ]
        .into_iter()
        .collect();
        core.set_config(cfg);

        let this = Arc::new(Self { core: core.clone() });
        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.first() != Some(&0x20) {
            return;
        }
        if buf.get(10) == Some(&0xeb) && buf.len() == HEADER_LENGTH + STATUS_LENGTH {
            self.process_status(&buf[HEADER_LENGTH..]);
        } else if buf.get(10) == Some(&0xec) && buf.len() == HEADER_LENGTH + STATUS_LENGTH * 2 {
            self.process_status(&buf[HEADER_LENGTH..HEADER_LENGTH + STATUS_LENGTH]);
        }
    }

    fn process_status(&self, buf: &[u8]) {
        if buf.len() < STATUS_LENGTH {
            return;
        }
        let status = buf[1];
        let time_remain = buf[2] as i64 * 60 + buf[3] as i64;
        let time_initial = buf[4] as i64 * 60 + buf[5] as i64;
        let course = buf[6];
        let error = buf[7];
        let spin = buf[9];
        let temp = buf[10];
        let delay_end = buf[13];
        let options = buf[15];
        let lock_status = buf[16];
        let cycles = buf[22];
        let energy = (buf[29] as i64) * 256 + buf[30] as i64;
        let detergent = buf[31] as usize;
        let softener = buf[32] as usize;
        let powered = status > 0 && !(course == 0 && spin == 0 && temp == 0);

        self.core
            .publish_property("error_message", error_message(error as usize).into());
        self.core.publish_on_off("error", error != 0);
        self.core.publish_property(
            "status",
            if powered {
                state_name(status as usize)
            } else {
                "Off"
            }
            .into(),
        );
        self.core.publish_property("course", vb_course(course).into());
        if !powered {
            self.core.publish_property("spin", "unknown".into());
            self.core.publish_property("temp", "unknown".into());
        } else {
            if spin != 0 {
                match spin_value(spin as usize) {
                    Some(label) => self.core.publish_property("spin", label.into()),
                    None => self.core.publish_property("spin", "None".into()),
                }
            }
            if temp != 0 {
                match temperature_value(temp as usize) {
                    Some(label) => self.core.publish_property("temp", label.into()),
                    None => self.core.publish_property("temp", "None".into()),
                }
            }
        }
        self.core.publish_property("cycles", i64::from(cycles).into());
        self.core
            .publish_on_off("remote_start", lock_status & 2 != 0);
        self.core
            .publish_on_off("door_lock", lock_status & 0x40 == 0);
        self.core
            .publish_on_off("child_lock", lock_status & 0x80 != 0);
        self.core.publish_property("initial_time", time_initial.into());
        self.core.publish_property("remaining_time", time_remain.into());
        self.core.publish_property("energy", energy.into());
        self.core.publish_property(
            "detergent",
            DOSES.get(detergent).copied().unwrap_or("unknown").into(),
        );
        self.core.publish_property(
            "softener",
            DOSES.get(softener).copied().unwrap_or("unknown").into(),
        );
        self.core.publish_on_off("turbowash", options & 0x01 != 0);
        self.core.publish_on_off("prewash", options & 0x40 != 0);
        self.core.publish_on_off("steam", options & 0x80 != 0);
        self.core
            .publish_property("delay_end", i64::from(delay_end).into());
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        if prop == "power" {
            self.core.send(&hex_decode("F02A0100"));
        }
        if prop == "pause" {
            self.core.send(&hex_decode("F024040100"));
        }
        if prop == "start" {
            let payload = if mqtt_value.is_empty() {
                hex_decode("F024050100")
            } else {
                hex_decode(mqtt_value)
            };
            self.core.send(&payload);
        }
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        request_status(&self.core);
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
        make_t2("Y_VB_Y___W.B32QEUK", Device::new)
    }

    const SAMPLE_EC_PANEL_OFF: &str =
        "AAFF200A006000479E000100EC004E000001000000000000000000000000000000000000000013006400000000000002022A1E000001000001000000000000000000000000000000000000000013006400000000000002022A1E0040016307BB";
    const SAMPLE_EB_PANEL_OFF: &str =
        "AAFF200A0039004A04000100EB0027000001002D002D0000000000000000000000000001000013006400000000000002022A1E000001017CBB";
    const SAMPLE_EC_COTTON_800_60: &str =
        "AAFF200A0060004BE6000100EC004E000001033103310100030506010000000000000004020013006400000400000002022A1E000001000001033103310100030506010000000000000004020013006400000400000002022A1E004001EE61BB";
    const SAMPLE_EC_CHILD_LOCK_ON: &str =
        "AAFF200A0060004D19000100EC004E000001003B003B3A00030A04010000000080010002020013006400000400000003002A1E000401000001003B003B3A00030A04010000000000010002020013006400000400000003002A1E0004014D9BBB";
    const SAMPLE_EC_CHILD_LOCK_OFF: &str =
        "AAFF200A0060004D3D000100EC004E000001003B003B3A00030A04010000000000010002020013006400000400000003002A1E004401000001003B003B3A00030A04010000000000010002020013006400000400000003002A1E0004010824BB";
    const SAMPLE_EC_STEAM: &str =
        "AAFF200A0060004F7A000100EC004E000001013201323A00030A06010000008000010003000013006400000400000003002A1E000001000001013201323A00030A06010000008000010003000013006400000400000003002A1E0040014397BB";
    const SAMPLE_EC_TURBOWASH: &str =
        "AAFF200A0060004F8D000100EC004E000001030E030E0100030704010000000100000002000013006400000400000002022A1E000001000001030E030E0100030704010000000100000002000013006400000400000002022A1E00400111A8BB";
    const SAMPLE_EC_PREWASH: &str =
        "AAFF200A0060004F94000100EC004E000001033B033B0100030704010000004000000003000013006400000500000002022A1E000001000001033B033B0100030704010000004000000003000013006400000500000002022A1E004001DA2CBB";
    const SAMPLE_EC_DELAY_4H: &str =
        "AAFF200A0060004FA6000100EC004E000001032A032A0100030704010004000000000003000013006400000400000002022A1E000001000001032A032A0100030704010000000000000003000013006400000400000002022A1E00000175AABB";
    const SAMPLE_EC_WASH_END_430WH: &str =
        "AAFF200A00600019F2000100EC004E00000A00000129050000000000000000004000000108001400640000030001AE02022A1E0002010000000000012905000000000000000000000000020A001400640000030001AE02022A1E0002016153BB";
    const SAMPLE_EC_STANDBY: &str =
        "AAFF200A0060004E65000100EC004E000001033803380000000000000000000000000004000013006400000000000002022A1E004001000001033803380000000000000000000000000004000013006400000000000002022A1E000001100EBB";

    #[test]
    fn config_and_power_is_button() {
        let (ha, _, _) = make();
        let device = ha.device(DEVICE_ID).unwrap();
        let comps = &device.config.as_ref().unwrap().components;
        for c in [
            "power", "start", "pause", "status", "error", "error_message", "course", "temp", "spin",
            "cycles", "remote_start", "door_lock", "child_lock", "detergent", "softener", "turbowash",
            "prewash", "steam", "delay_end", "energy", "initial_time", "remaining_time",
        ] {
            assert!(comps.contains_key(c), "missing {c}");
        }
        assert_eq!(comps["power"]["platform"], "button");
        assert!(comps["power"].get("state_topic").is_none());
    }

    #[test]
    fn panel_off_ec_and_eb() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_PANEL_OFF));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("19"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EB_PANEL_OFF));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("45"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("45"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("19"));
    }

    #[test]
    fn cotton_800_60_and_ai_wash() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_COTTON_800_60));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("800 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("229"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_CHILD_LOCK_ON));
        assert_eq!(prop(&ha, "course").as_deref(), Some("AI Wash"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn energy_wh_and_options() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_WASH_END_430WH));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("430"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_CHILD_LOCK_OFF));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_STEAM));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_TURBOWASH));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_PREWASH));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("ON"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_DELAY_4H));
        assert_eq!(prop(&ha, "delay_end").as_deref(), Some("4"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
    }

    #[test]
    fn standby_off_and_hold_temp_spin() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_STANDBY));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));

        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_EC_COTTON_800_60));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("800 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        let mut rinsing = hex_decode(SAMPLE_EC_COTTON_800_60);
        let block = 2 + HEADER_LENGTH;
        rinsing[block + 1] = 7;
        rinsing[block + 10] = 0;
        thinq.emit_data(&rinsing);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("800 rpm"));
        let mut ending = rinsing;
        ending[block + 1] = 10;
        ending[block + 9] = 0;
        thinq.emit_data(&ending);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("800 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
    }

    #[test]
    fn truncated_and_unframed_ignored() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(
            "AAFF200A0039000381000100EC0027000001032603260000000000000000000000000003000011007100000000000000000000000000974EBB",
        ));
        assert!(prop(&ha, "status").is_none());
        thinq.emit_data(&hex_decode("001122"));
        assert!(prop(&ha, "status").is_none());
    }

    #[test]
    fn power_press_and_status_query() {
        let (_, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA08F02A010098BB");
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1121010000001800B5BB");
    }
}
