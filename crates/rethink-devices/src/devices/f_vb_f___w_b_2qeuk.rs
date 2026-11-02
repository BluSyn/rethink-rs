//! F_VB_F___W.B_2QEUK washer (AABB, 39-byte class `0x20` status records + doses/options).
//!
//! A `0xEB` frame carries one record (the reply to the status query), a `0xEC`
//! frame the previous and the current one.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, drying_mode, fy_base_components, install_components, DOSES};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const RECORD_LEN: usize = 39;

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        install_components(&mut base, fy_base_components(crate::washer_common::FY_REFRESH_PAUSE19));
        install_components(
            &mut base,
            [
                ("drying_mode", json!({"platform":"sensor","unique_id":"$deviceid-drying-mode","state_topic":"$this/drying_mode","name":"Drying mode","icon":"mdi:tumble-dryer"})),
                ("cycles", json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycle count","icon":"mdi:counter"})),
                ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"})),
                ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
                ("delay_end", json!({"platform":"sensor","unique_id":"$deviceid-delay_end","state_topic":"$this/delay_end","name":"Delay end","icon":"mdi:timer-sand","device_class":"duration","unit_of_measurement":"h","suggested_display_precision":0})),
                ("detergent", json!({"platform":"sensor","unique_id":"$deviceid-detergent","state_topic":"$this/detergent","name":"Detergent dose","icon":"mdi:cup","device_class":"enum","options":DOSES})),
                ("softener", json!({"platform":"sensor","unique_id":"$deviceid-softener","state_topic":"$this/softener","name":"Softener dose","icon":"mdi:cup-outline","device_class":"enum","options":DOSES})),
                ("extra_rinse", json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra rinse","icon":"mdi:water-plus"})),
                ("turbowash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"})),
                ("eco_hybrid", json!({"platform":"binary_sensor","unique_id":"$deviceid-eco_hybrid","state_topic":"$this/eco_hybrid","name":"EcoHybrid","icon":"mdi:leaf"})),
                ("prewash", json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"})),
                ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
            ],
        );
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(rec) = process_common_status(buf, 0x20, RECORD_LEN) {
            self.process_status(rec);
        }
    }

    fn process_status(&self, buf: &[u8]) {
        let status = buf[2];
        let time_remain = buf[3] as i64 * 60 + buf[4] as i64;
        let time_initial = buf[5] as i64 * 60 + buf[6] as i64;
        let course = buf[7];
        let error = buf[8];
        let spin = buf[10];
        let temp = buf[11];
        let extra_rinse = buf[12];
        let drying_mode_b = buf[13];
        let delay_end = buf[14] as i64;
        let options = buf[16];
        let lock_status = buf[17];
        let cycles = buf[23] as i64;
        let energy = buf[30] as i64 * 256 + buf[31] as i64;
        let detergent = buf[32] as usize;
        let softener = buf[33] as usize;

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_refresh_pause19_name);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("None").into(),
        );
        pub_temp_spin(&self.core, temp, spin);
        self.core.publish_property(
            "drying_mode",
            drying_mode(drying_mode_b as u32).unwrap_or("None").into(),
        );
        self.core.publish_property("cycles", cycles.into());
        self.core.publish_on_off("remote_start", lock_status & 2 != 0);
        self.core.publish_on_off("door_lock", lock_status & 0x40 == 0);
        self.core.publish_on_off("child_lock", lock_status & 0x80 != 0);
        self.core
            .publish_property("initial_time", time_initial.into());
        self.core
            .publish_property("remaining_time", time_remain.into());
        self.core.publish_property("energy", energy.into());
        self.core.publish_property("delay_end", delay_end.into());
        self.core.publish_property(
            "detergent",
            DOSES.get(detergent).copied().unwrap_or("None").into(),
        );
        self.core.publish_property(
            "softener",
            DOSES.get(softener).copied().unwrap_or("None").into(),
        );
        self.core.publish_on_off("extra_rinse", extra_rinse >= 2);
        self.core.publish_on_off("turbowash", options & 0x01 != 0);
        self.core.publish_on_off("eco_hybrid", options & 0x08 != 0);
        self.core.publish_on_off("prewash", options & 0x40 != 0);
        self.core.publish_on_off("steam", options & 0x80 != 0);
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        set_power_start_pause(&self.core, prop, mqtt_value);
    }
}

impl DeviceHandler for Device {
    fn id(&self) -> &str {
        &self.core.id
    }
    fn start(&self) {
        crate::devices::washer_ctrl::request_status(&self.core);
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
    use crate::test_support::eb_from_ec;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata { Metadata::new("F_VB_F___W.B_2QEUK", "F_VB_F___W.B_2QEUK", "1.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }
    fn decode(frame: &str) -> std::sync::Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    const SAMPLE: &str = "AA5420EC00250101000100180000000000020000000000000600001000640000000000000000000000000000250104380438130003090401020000000000000400001000640000040000000000000000000053BB";

    // Powered on, idle (Ready), no course selected, no locks engaged.
    const SAMPLE_INITIAL: &str = "AA5420EC002500050F050F130000000000000000000000000401000A0064000004000CBD02022A1E00030100250100000000000000000000000000000000000000000A006400000000000002022A1E0040014EBB";
    // Idle with child lock engaged (0x80 is set when locked on this model).
    const SAMPLE_CHILD_LOCK: &str = "AA5420EC00250100000000000000000000000000000000000000000A006400000000000002022A1E00030100250100000000000000000000000000008000000000000A006400000000000002022A1E00030107BB";
    // Cotton, 1000 rpm, 40 °C, extra rinse + TurboWash + Pre-wash, detergent Off, softener High.
    const SAMPLE_COMBO_OPTIONS: &str = "AA5420EC00250104010401010003070402000000410001000200000A006400000500000000022A1E00020100250104010401010003070402000000410001000200000A006400000500000000032A1E000201C0BB";
    // Same combo with delay_end dialled to 3 hours.
    const SAMPLE_DELAY_END: &str = "AA5420EC00250104010401010003070402000000410001000200000A006400000500000000032A1E00020100250104010401010003070402000300410001000200000A006400000500000000032A1E000201CCBB";
    const SAMPLE_POWER_OFF: &str = "AA5420EC00250004010401010000000000000000000001000201000A006400000500000000032A1E00020100250004010401010000000000000000000001000201000A006400000500000000032A1E00420161BB";
    // Mid-Washing on Wash + Dry, 1400 rpm, 40 °C, drying Auto, door locked, remote start.
    const SAMPLE_RUNNING: &str = "AA5420EC002504050F050F1300030A0401020000004220000101000A006400000400000002022A1E000301002506021902191300030A0401020000004220000104000A006400000400000002022A1E0003010CBB";
    // Drying-only course just started.
    const SAMPLE_DRYING: &str = "AA5420EC00250100370037180000000000020000000000000600000B006400000000002D00002A1E00000100250900370037180000000000020000000220000101000B006400000000002D00002A1E004001F7BB";

    const WRITE_INIT: &str = "AA0EF0ED1121010000001800B5BB";
    const WRITE_POWER_ON: &str = "AA08F02A010098BB";
    const WRITE_POWER_OFF: &str = "AA09F0240101009CBB";
    const WRITE_PAUSE: &str = "AA09F02404010099BB";
    const WRITE_START: &str = "AA09F02405010098BB";

    #[test]
    fn basic() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("detergent"));
        assert!(comps.contains_key("eco_hybrid"));
        thinq.emit_data(&hex_decode(SAMPLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
    }

    #[test]
    fn config_exposes_expected_components() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
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
            "child_lock",
            "energy",
            "initial_time",
            "remaining_time",
            "delay_end",
            "detergent",
            "softener",
            "extra_rinse",
            "turbowash",
            "eco_hybrid",
            "prewash",
            "steam",
        ] {
            assert!(comps.contains_key(c), "component {c} present");
        }
        assert!(comps["status"]["options"].as_array().unwrap().contains(&json!("Washing")));
        assert!(comps["detergent"]["options"].as_array().unwrap().contains(&json!("Medium")));
        assert_eq!(comps["door_lock"]["device_class"], "lock");
        assert_eq!(comps["child_lock"]["device_class"], "lock");
        assert_eq!(comps["delay_end"]["device_class"], "duration");
    }

    #[test]
    fn initial_state_decodes_power_status_locks_and_error() {
        let ha = decode(SAMPLE_INITIAL);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "delay_end").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("10"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn child_lock_polarity_is_not_inverted() {
        let ha = decode(SAMPLE_CHILD_LOCK);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn combo_options_decode_rinse_turbowash_prewash_and_doses() {
        let ha = decode(SAMPLE_COMBO_OPTIONS);
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "eco_hybrid").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1000 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("241"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("241"));
    }

    #[test]
    fn delay_end_is_in_hours() {
        let ha = decode(SAMPLE_DELAY_END);
        assert_eq!(prop(&ha, "delay_end").as_deref(), Some("3"));
    }

    #[test]
    fn running_wash_and_dry_decodes_status_spin_temp_and_locks() {
        let ha = decode(SAMPLE_RUNNING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn drying_only_course_decodes_drying() {
        let ha = decode(SAMPLE_DRYING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("55"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("55"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("11"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Not selected"));
    }

    #[test]
    fn power_off_transition() {
        let ha = decode(SAMPLE_POWER_OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
    }

    #[test]
    fn frames_without_aabb_envelope_are_ignored() {
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
    fn start_sends_the_status_query() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_INIT);
    }

    #[test]
    fn write_power_on() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_ON);
    }

    #[test]
    fn write_power_off() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_OFF);
    }

    #[test]
    fn write_pause() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_PAUSE);
    }

    #[test]
    fn write_start() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_START);
    }

    #[test]
    fn write_to_unknown_property_sends_nothing() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("does-not-exist", "whatever");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn eb_reply_and_unmapped_codes() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&eb_from_ec(&hex_decode(SAMPLE_RUNNING), RECORD_LEN));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));

        let mut frame = hex_decode(SAMPLE_RUNNING);
        let rec = 2 + 2 + RECORD_LEN;
        frame[rec + 7] = 0xff; // course
        frame[rec + 8] = 0x0a; // error code without a label
        frame[rec + 13] = 0xff; // drying mode
        frame[rec + 32] = 0x04; // detergent dose
        frame[rec + 33] = 0x04; // softener dose
        thinq.emit_data(&frame);
        for p in ["course", "error_message", "drying_mode", "detergent", "softener"] {
            assert_eq!(prop(&ha, p).as_deref(), Some("None"), "{p}");
        }
    }
}
