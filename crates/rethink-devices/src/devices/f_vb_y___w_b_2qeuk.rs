//! F_VB_Y___W.B_2QEUK washer (LG F4WV710ATR). 39-byte class `0x20` status records.
//!
//! Same offsets as `F_V8_Y___W.B_2QEUK`, plus detergent and softener doses.
//! A `0xEB` frame carries one record, a `0xEC` frame the previous and the current
//! one. Child lock is not exposed. `auto_ack` is on; status records stay unacked.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{
    course_name, fy_base_components, install_components, soil_level, DOSES, SOIL_LABELS,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

const RECORD_LEN: usize = 39;

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self { core: core.clone() });

        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        install_components(&mut base, fy_base_components(crate::washer_common::FY_STEAM_PAUSE19));
        install_components(
            &mut base,
            [
                (
                    "cycles",
                    json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycle count","icon":"mdi:counter"}),
                ),
                (
                    "energy",
                    json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"}),
                ),
                (
                    "reserve_time",
                    json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","device_class":"duration","unit_of_measurement":"h","name":"Reserve time","icon":"mdi:timer-sand"}),
                ),
                (
                    "detergent",
                    json!({"platform":"sensor","unique_id":"$deviceid-detergent","state_topic":"$this/detergent","name":"Detergent dose","icon":"mdi:cup","device_class":"enum","options":DOSES}),
                ),
                (
                    "softener",
                    json!({"platform":"sensor","unique_id":"$deviceid-softener","state_topic":"$this/softener","name":"Softener dose","icon":"mdi:cup-outline","device_class":"enum","options":DOSES}),
                ),
                (
                    "extra_rinse",
                    json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra rinse","icon":"mdi:water-plus"}),
                ),
                (
                    "turbowash",
                    json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"}),
                ),
                (
                    "prewash",
                    json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"}),
                ),
                (
                    "soil",
                    json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:washing-machine","device_class":"enum","options":SOIL_LABELS}),
                ),
                ("intensive_wash", json!({"platform":"binary_sensor"})),
                (
                    "steam",
                    json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"}),
                ),
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
        let soil = buf[9];
        let spin = buf[10];
        let temp = buf[11];
        let extra_rinse = buf[12];
        let time_reserve_hour = buf[14] as i64;
        let options = buf[16];
        let lock_status = buf[17];
        let cycles = buf[23] as i64;
        let energy = buf[30] as i64 * 256 + buf[31] as i64;
        let detergent = buf[32];
        let softener = buf[33];

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_steam_pause19_name);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("None").into(),
        );
        pub_temp_spin(&self.core, temp, spin);
        self.core.publish_property("cycles", cycles.into());
        self.core
            .publish_on_off("remote_start", lock_status & 2 != 0);
        self.core
            .publish_on_off("door_lock", lock_status & 0x40 == 0);
        self.core
            .publish_property("initial_time", time_initial.into());
        self.core
            .publish_property("remaining_time", time_remain.into());
        self.core
            .publish_property("reserve_time", time_reserve_hour.into());
        self.core.publish_property("energy", energy.into());
        self.core
            .publish_property("detergent", dose_name(detergent).into());
        self.core
            .publish_property("softener", dose_name(softener).into());
        self.core.publish_on_off("extra_rinse", extra_rinse >= 2);
        self.core.publish_on_off("turbowash", options & 0x01 != 0);
        self.core.publish_on_off("prewash", options & 0x40 != 0);
        self.core.publish_on_off("steam", options & 0x80 != 0);
        self.core.publish_property(
            "soil",
            soil_level(soil as usize).unwrap_or("None").into(),
        );
    }

    pub fn set_property(&self, prop: &str, mqtt_value: &str) {
        set_power_start_pause(&self.core, prop, mqtt_value);
    }
}

fn dose_name(code: u8) -> &'static str {
    DOSES.get(code as usize).copied().unwrap_or("None")
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
    use crate::test_support::{eb_from_ec, prop, DEVICE_ID};
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device};

    // Status telegrams captured from a real unit (Vivace V750, firmware 2.10.123),
    // each a previous + current EC pair.
    const SAMPLE_IDLE: &str = "AA5420EC002600000000000000000000000000000000000000010008006400000000000002022A1E000201002601000000000000000000000000000000000000000008006400000000000002022A1E00000199BB";
    const SAMPLE_COTTON: &str = "AA5420EC002601002400241B00030503010000000000000001000008006400000300000001002A1E000001002601030203020100030A04010000000000010003000008006400000400000000002A1E0080018EBB";
    const SAMPLE_ALLERGY_STEAM: &str = "AA5420EC002601021C021C0900030504010000000000000002000008006400000500000002022A1E000201002601023002302D00030A06010000008000000004000008006400000500000002022A1E00020100BB";
    const SAMPLE_EASYCARE_RINSE_PREWASH: &str = "AA5420EC002601023502350200030A03020000000000010002000008006400000300000001002A1E000001002601030A030A0200030A03020000004000010003000008006400000400000001002A1E00000197BB";
    const SAMPLE_INTENSIVE: &str = "AA5420EC002601013A013A0200030A04010000000000010002000008006400000300000001002A1E000001002601032003200200040A04010000000000010003000008006400000300000001002A1E000001E3BB";
    const SAMPLE_DELAY_19H: &str = "AA5420EC002601000E000E0C00030202010012000100000000000008006400000100000001002A1E000001002601000E000E0C00030202010013000100000000000008006400000100000001002A1E00000118BB";
    const SAMPLE_DETERGENT_HIGH: &str = "AA5420EC002601031D031D0200030706020000008000010004000008006400000300000002002A1E008001002601031D031D0200030706020000008000010004000008006400000300000003002A1E000001AABB";
    const SAMPLE_SOFTENER_HIGH: &str = "AA5420EC002601000E000E0C00030202010000000100000000000008006400000100000001022A1E000201002601000E000E0C00030202010000000100000000000008006400000100000001032A1E00020164BB";
    const SAMPLE_RUNNING: &str = "AA5420EC002606000C000E0C00030202010000000142200001010008006400000100001301002A1E000001002606000C000E0C00030202010000000142200001010008006400000100001401002A1E00000148BB";
    const SAMPLE_PAUSED: &str = "AA5420EC0026070009000E0C00030200010000000142000001060008006400000100002301002A1E0000010026020009000E0C00030200010000000140000000070008006400000100002301002A1E000001A2BB";
    const SAMPLE_END: &str = "AA5420EC00260A0000000E0C00000000000000000040000001080008006400000100002901002A1E00000100260A0000000E0C00000000000000000000000001080008006400000100002901002A1E000001E5BB";
    const SAMPLE_POWER_OFF: &str = "AA5420EC002601010C010C1200030106010000000000000003000009006400000200002900002A1E000001002600010C010C1200000000000000000000000003010009006400000200002900002A1E000001D6BB";
    // Other frames of the machine, and the acks the cloud returned while bridged.
    const PAUSE_MARKER: &str = "AA09207200C80058BB";
    const ACK_PAUSE_MARKER: &str = "AA08F00072044DBB";
    const POWER_OFF_MARKER: &str = "AA0720D809E7BB";
    const ACK_POWER_OFF_MARKER: &str = "AA08F000D8042BBB";

    fn make() -> (
        std::sync::Arc<MockHaConnection>,
        std::sync::Arc<MockThinq2Device>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(
            DEVICE_ID,
            Metadata::new("F_VB_Y___W.B_2QEUK", "F_VB_Y___W.B_2QEUK", "2.10.123"),
        );
        let _dev = Device::new(ha.clone(), thinq.clone(), thinq.meta().clone());
        (ha, thinq)
    }

    fn decode(frame: &str) -> std::sync::Arc<MockHaConnection> {
        let (ha, thinq) = make();
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    fn acks(thinq: &MockThinq2Device) -> Vec<String> {
        thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn config_exposes_the_dosing_sensors_as_enums() {
        let (ha, _) = make();
        let info = ha.device(DEVICE_ID).unwrap();
        let components = &info.config.as_ref().unwrap().components;
        for name in ["detergent", "softener", "soil"] {
            assert_eq!(components[name]["platform"], "sensor");
            assert_eq!(components[name]["device_class"], "enum");
        }
        assert!(
            components.get("child_lock").is_none(),
            "child lock is not exposed until confirmed on this model"
        );
        assert_eq!(
            components["intensive_wash"],
            json!({"platform": "binary_sensor"})
        );
    }

    #[test]
    fn idle_ready_no_course() {
        let ha = decode(SAMPLE_IDLE);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Medium"));
    }

    #[test]
    fn cotton_1400_40_decodes_with_no_options_and_dosing_off() {
        let ha = decode(SAMPLE_COTTON);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("182"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("182"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));
        for o in ["extra_rinse", "turbowash", "prewash", "steam"] {
            assert_eq!(prop(&ha, o).as_deref(), Some("OFF"), "{o}");
        }
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Normal"));
    }

    #[test]
    fn allergy_care_with_steam() {
        let ha = decode(SAMPLE_ALLERGY_STEAM);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergy Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
    }

    #[test]
    fn rinse_plus_and_prewash() {
        let ha = decode(SAMPLE_EASYCARE_RINSE_PREWASH);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Ease Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("30 °C"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
    }

    #[test]
    fn intensive_wash() {
        let ha = decode(SAMPLE_INTENSIVE);
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Intensive"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("OFF"));
    }

    #[test]
    fn delayed_start_hours_are_published_as_reserve_time() {
        let ha = decode(SAMPLE_DELAY_19H);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick 14"));
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("19"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));
    }

    #[test]
    fn detergent_and_softener_doses() {
        let ha = decode(SAMPLE_DETERGENT_HIGH);
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));
        let ha = decode(SAMPLE_SOFTENER_HIGH);
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Low"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("High"));
    }

    #[test]
    fn running_door_lock_times_and_energy() {
        let ha = decode(SAMPLE_RUNNING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("14"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("12"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("20"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("8"));
    }

    #[test]
    fn paused() {
        let ha = decode(SAMPLE_PAUSED);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn end_of_cycle_unlocks_door_with_final_energy() {
        let ha = decode(SAMPLE_END);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("41"));
    }

    #[test]
    fn powered_off_cycle_counter_has_advanced() {
        let ha = decode(SAMPLE_POWER_OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("9"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("41"));
    }

    #[test]
    fn acks_marker_frames_and_not_status() {
        let (ha, thinq) = make();
        thinq.emit_data(&hex_decode(PAUSE_MARKER));
        thinq.emit_data(&hex_decode(POWER_OFF_MARKER));
        assert_eq!(acks(&thinq), vec![ACK_PAUSE_MARKER, ACK_POWER_OFF_MARKER]);
        thinq.emit_data(&hex_decode(SAMPLE_RUNNING));
        assert_eq!(acks(&thinq).len(), 2, "the cloud does not ack EC status packets");
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
    }

    #[test]
    fn eb_reply_is_decoded_and_not_acked() {
        let (ha, thinq) = make();
        thinq.emit_data(&eb_from_ec(&hex_decode(SAMPLE_DETERGENT_HIGH), RECORD_LEN));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert!(acks(&thinq).is_empty());
    }

    #[test]
    fn unmapped_codes_publish_none() {
        let (ha, thinq) = make();
        let mut frame = hex_decode(SAMPLE_COTTON);
        let rec = 2 + 2 + RECORD_LEN;
        frame[rec + 7] = 0xff; // course
        frame[rec + 8] = 0x0a; // error code without a label
        frame[rec + 9] = 0x05; // soil
        frame[rec + 32] = 0x04; // detergent dose
        thinq.emit_data(&frame);
        for p in ["course", "error_message", "soil", "detergent"] {
            assert_eq!(prop(&ha, p).as_deref(), Some("None"), "{p}");
        }
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("F_VB_Y___W.B_2QEUK").is_some());
    }
}
