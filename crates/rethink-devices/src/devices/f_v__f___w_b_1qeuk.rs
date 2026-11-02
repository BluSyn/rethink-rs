//! F_V__F___W.B_1QEUK washer (AABB, 39-byte class `0x20` status records).
//!
//! A `0xEB` frame carries one record (the reply to the status query), a `0xEC`
//! frame the previous and the current one.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, drying_mode, fy_base_components, install_components};
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
        install_components(&mut base, fy_base_components(crate::washer_common::FY_REFRESH));
        install_components(
            &mut base,
            [
                ("drying_mode", json!({"platform":"sensor","unique_id":"$deviceid-drying-mode","state_topic":"$this/drying_mode","name":"Drying mode","icon":"mdi:tumble-dryer"})),
                ("cycles", json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycle count","icon":"mdi:counter"})),
                ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
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
        let drying_mode_b = buf[13];
        let lock_status = buf[17];
        let cycles = buf[23] as i64;
        let energy = buf[30] as i64 * 256 + buf[31] as i64;

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_refresh_name);
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
        self.core
            .publish_property("initial_time", time_initial.into());
        self.core
            .publish_property("remaining_time", time_remain.into());
        self.core.publish_property("energy", energy.into());
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
    fn meta() -> Metadata { Metadata::new("F_V__F___W.B_1QEUK", "F_V__F___W.B_1QEUK", "2.10.123") }
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

    const SAMPLE_INITIAL: &str = "AA5420EC002500000000000000000000000000000000000000010036006400000000000000000000000000002501000000000000000000000000000000000000000036006400000000000000000000000000DFBB";
    const SAMPLE_INITIAL_WASH_1200_40C_DRY_AUTO: &str = "AA5420EC00250101000100180000000000020000000000000600001000640000000000000000000000000000250104380438130003090401020000000000000400001000640000040000000000000000000053BB";
    const SAMPLE_INITIAL_COTTON_1000_40C_TURBO_WASH_DRY_1H: &str = "AA5420EC002501040C040C0100030904010400000000010002000010006400000400000000000000000000002501030603060100030704010400000100010002000010006400000400000000000000000000FCBB";
    const SAMPLE_RUNNING_QUICK_400_40C: &str = "AA5420EC002506001300140C00030204010000000142200001010036006400000100000E00000000000000002506001300140C00030204010000000142200001010036006400000100000F00000000000000A2BB";
    const SAMPLE_RINSING_QUICK_400_40C: &str = "AA5420EC002507000A00140C0003020001000000014200000106003600640000010000B900000000000000002507000A00140C0003020001000000014200000106003600640000010000BA00000000000000AABB";
    const SAMPLE_SPINNING_QUICK_400_40C: &str = "AA5420EC002507000600140C0003020001000000014200000106003600640000010000BC00000000000000002508000500140C0000020000000000004200000107003600640000010000BC00000000000000ADBB";
    const SAMPLE_END_QUICK_400_40C: &str = "AA5420EC002508000100140C0000020000000000004200000107003600640000010000BF0000000000000000250A000000140C0000000000000000000000000108003600640000010000BF00000000000000E5BB";
    const SAMPLE_POWER_OFF: &str = "AA5420EC00250A000000140C0000000000000000000000000108003600640000010000BF00000000000000002500000000140C000000000000000000000000010A003600640000010000BF0000000000000033BB";

    const WRITE_INIT: &str = "AA0EF0ED1121010000001800B5BB";
    const WRITE_POWER_ON: &str = "AA08F02A010098BB";
    const WRITE_POWER_OFF: &str = "AA09F0240101009CBB";
    const WRITE_PAUSE: &str = "AA09F02404010099BB";
    const WRITE_START: &str = "AA09F02405010098BB";

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
            "energy",
            "initial_time",
            "remaining_time",
        ] {
            assert!(comps.contains_key(c), "component {c} present");
        }
        let status_options = comps["status"]["options"].as_array().unwrap();
        assert!(status_options.contains(&json!("Washing")));
        assert!(status_options.contains(&json!("Error")));
    }

    #[test]
    fn initial_state_decodes_status_time_and_error() {
        let ha = decode(SAMPLE_INITIAL);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("54"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn initial_wash_and_dry_auto() {
        let ha = decode(SAMPLE_INITIAL_WASH_1200_40C_DRY_AUTO);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Wash + Dry"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("296"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("296"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1200 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("16"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn initial_cotton_turbowash_dry_1h() {
        let ha = decode(SAMPLE_INITIAL_COTTON_1000_40C_TURBO_WASH_DRY_1H);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("186"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("186"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1000 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("01:00"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("16"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn running_with_door_locked_and_remote_start() {
        let ha = decode(SAMPLE_RUNNING_QUICK_400_40C);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick 14"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("20"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("19"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("15"));
    }

    #[test]
    fn rinsing_state() {
        let ha = decode(SAMPLE_RINSING_QUICK_400_40C);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("10"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("186"));
    }

    #[test]
    fn spinning_state() {
        let ha = decode(SAMPLE_SPINNING_QUICK_400_40C);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Spinning"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("5"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("188"));
    }

    #[test]
    fn end_state() {
        let ha = decode(SAMPLE_END_QUICK_400_40C);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("191"));
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
        thinq.emit_data(&eb_from_ec(
            &hex_decode(SAMPLE_INITIAL_COTTON_1000_40C_TURBO_WASH_DRY_1H),
            RECORD_LEN,
        ));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("01:00"));

        let mut frame = hex_decode(SAMPLE_RUNNING_QUICK_400_40C);
        let rec = 2 + 2 + RECORD_LEN;
        frame[rec + 7] = 0xff; // course
        frame[rec + 8] = 0x0a; // error code without a label
        frame[rec + 13] = 0xff; // drying mode
        thinq.emit_data(&frame);
        for p in ["course", "error_message", "drying_mode"] {
            assert_eq!(prop(&ha, p).as_deref(), Some("None"), "{p}");
        }
    }
}
