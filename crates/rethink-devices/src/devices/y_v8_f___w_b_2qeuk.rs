//! Y_V8_F___W.B_2QEUK washer/dryer combo (LG W4WR70E61, AABB).
//!
//! 39-byte status records inside a class `0x20` envelope: a `0xEB` single record
//! or a `0xEC` previous + current pair. A 79-byte configuration frame carries
//! the wash cycle counter.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, drying_mode, fy_base_components, install_components};
use rethink_core::device_base::{default_config, process_enveloped_status, AabbDeviceCore};
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
        install_components(&mut base, fy_base_components(crate::washer_common::FY_STEAM));
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
        if let Some(rec) = process_enveloped_status(buf, 0x20, RECORD_LEN) {
            self.process_status(rec);
            return;
        }
        if buf.len() == 79 && buf[0] == 0x20 && buf[8] == 0x02 {
            self.core
                .publish_property("cycles", (buf[19] as i64).into());
        }
    }

    fn process_status(&self, buf: &[u8]) {
        let status = buf[2];
        let remaining = buf[3] as i64 * 60 + buf[4] as i64;
        let initial = buf[5] as i64 * 60 + buf[6] as i64;
        let course = buf[7];
        let error = buf[8];
        let spin = buf[10];
        let temp = buf[11];
        let drying = buf[13];

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_steam_name);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("None").into(),
        );
        pub_temp_spin(&self.core, temp, spin);
        self.core.publish_property(
            "drying_mode",
            drying_mode(drying as u32).unwrap_or("None").into(),
        );
        // Remote start is bit 0x40 of [17] here; F_V8_Y carries it in bit 0x02 of the lock byte.
        self.core.publish_on_off("remote_start", buf[17] & 0x40 != 0);
        self.core.publish_on_off("door_lock", buf[21] & 0x40 == 0);
        self.core.publish_property("initial_time", initial.into());
        self.core.publish_property("remaining_time", remaining.into());
        let energy = buf[30] as i64 * 256 + buf[31] as i64;
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
    fn start(&self) {}
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata { Metadata::new("Y_V8_F___W.B_2QEUK", "Y_V8_F___W.B_2QEUK", "0.0.0") }
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

    // Captures from an LG W4WR70E61 (clip_hna 2.11.207) over full wash+dry cycles,
    // upstream issue #98. Status frames are enveloped EC pairs in the long framing.
    const SAMPLE_ARMED_READY: &str = "aaff200a0060007f7f000100ec004e000001040a040a1300030a03010200000042000004000010000000000400000000000000000000000001040a040a1300030a0301020000004200000400001000000000040000000000000000c0007a96bb";
    const SAMPLE_RINSING: &str = "aaff200a006000823e000100ec004e000006022803131300030a03010200000042200001040010000000000400009800000000000000000007022703131300030a000102000000420000010600100000000004000098000000000000001066bb";
    const SAMPLE_DRYING: &str = "aaff200a0060008a6a000100ec004e000009011d031d130000000000020000004200000104001000000000040003e800000000000000000009011d031d130000000000020000004200000104001000000000040003e9000000000000005a45bb";
    const SAMPLE_OFF: &str = "aaff200a0060009437000100ec004e00000b0000031d13000000000002000000400000020a00100000000004000872000000000000000000000000031d13000000000000000000400000040b00100000000004000872000000000000008feabb";
    // 79-byte configuration frame (byte 8 = 0x02), wash cycle counter at byte 19.
    const SAMPLE_CONFIG: &str = "aaff200a005300c807000201030018120a0102120134000807001d00000000000000000000000001050025595f56385f465f5f5f572e425f325145554b00000102d51ced6f0104000000000000000000b596bb";

    #[test]
    fn config_present() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("drying_mode"));
        assert!(comps.contains_key("status"));
    }

    #[test]
    fn config_exposes_drying_mode_and_energy() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert_eq!(comps["drying_mode"]["platform"], "sensor");
        assert_eq!(comps["energy"]["platform"], "sensor");
        assert_eq!(comps["cycles"]["platform"], "sensor");
    }

    #[test]
    fn armed_ready_decodes_course_spin_temp_drying_and_remote_start() {
        let ha = decode(SAMPLE_ARMED_READY);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Wash + Dry"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("30 °C"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("250"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("250"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("0"));
    }

    #[test]
    fn rinse_phase_decodes_remaining_time_and_energy() {
        let ha = decode(SAMPLE_RINSING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Rinsing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Wash + Dry"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("159"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("199"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("152"));
    }

    #[test]
    fn drying_phase_decodes_drying_with_energy_advancing() {
        let ha = decode(SAMPLE_DRYING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("89"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("1001"));
    }

    #[test]
    fn powered_off_frame_decodes_power_off_and_final_energy() {
        let ha = decode(SAMPLE_OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("2162"));
    }

    #[test]
    fn configuration_frame_decodes_the_cycle_counter() {
        let ha = decode(SAMPLE_CONFIG);
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("52"));
    }

    #[test]
    fn eb_single_record_uses_the_same_offsets() {
        let ec = hex_decode(SAMPLE_DRYING);
        // 20 0A <len16> <seq16> .. .. .. EB <inner len16> <record> <checksum byte>
        let mut eb = ec[2..15].to_vec();
        eb[10] = 0xeb;
        eb[12] = RECORD_LEN as u8;
        eb.extend_from_slice(&ec[2 + 13 + RECORD_LEN..ec.len() - 2]);
        let (ha, thinq, _) = make();
        thinq.emit_data(&rethink_core::device_base::wrap_aabb(&eb));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("89"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("1001"));
    }

    #[test]
    fn unenveloped_and_unmapped_frames() {
        let (ha, thinq, _) = make();
        let mut frame = hex_decode(SAMPLE_RINSING);
        frame[3] = 0x00; // not an envelope
        thinq.emit_data(&frame);
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());

        let mut frame = hex_decode(SAMPLE_RINSING);
        let rec = 2 + 13 + RECORD_LEN;
        frame[rec + 7] = 0xff; // course
        frame[rec + 8] = 0x0a; // error code without a label
        frame[rec + 13] = 0xff; // drying mode
        thinq.emit_data(&frame);
        for p in ["course", "error_message", "drying_mode"] {
            assert_eq!(prop(&ha, p).as_deref(), Some("None"), "{p}");
        }
    }
}
