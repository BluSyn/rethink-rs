//! F_V8_Y___W.B_2QEUK washer (AABB, 39-byte class `0x20` status records).
//!
//! A `0xEB` frame carries one record (the reply to the status query), a `0xEC`
//! frame the previous and the current one.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, fy_base_components, install_components, soil_level, SOIL_LABELS};
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
        install_components(&mut base, fy_base_components(crate::washer_common::FY_STEAM));
        install_components(
            &mut base,
            [
                ("cycles", json!({"platform":"sensor","unique_id":"$deviceid-cycles","state_topic":"$this/cycles","name":"Cycle count","icon":"mdi:counter"})),
                ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","device_class":"lock"})),
                ("energy", json!({"platform":"sensor","unique_id":"$deviceid-energy","state_topic":"$this/energy","name":"Energy","icon":"mdi:lightning-bolt","device_class":"energy","state_class":"total_increasing","unit_of_measurement":"Wh"})),
                ("reserve_time", json!({"platform":"sensor","unique_id":"$deviceid-reserve_time","state_topic":"$this/reserve_time","device_class":"duration","unit_of_measurement":"h","name":"Reserve time","icon":"mdi:timer-sand"})),
                ("extra_rinse", json!({"platform":"binary_sensor","unique_id":"$deviceid-extra_rinse","state_topic":"$this/extra_rinse","name":"Extra rinse","icon":"mdi:water-plus"})),
                ("turbowash", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbowash","state_topic":"$this/turbowash","name":"TurboWash","icon":"mdi:rocket-launch"})),
                ("prewash", json!({"platform":"binary_sensor","unique_id":"$deviceid-prewash","state_topic":"$this/prewash","name":"Pre-wash","icon":"mdi:water-sync"})),
                ("soil", json!({"platform":"sensor","unique_id":"$deviceid-soil","state_topic":"$this/soil","name":"Soil level","icon":"mdi:washing-machine","device_class":"enum","options":SOIL_LABELS})),
                ("intensive_wash", json!({"platform":"binary_sensor"})),
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
        let soil = buf[9];
        let spin = buf[10];
        let temp = buf[11];
        let extra_rinse = buf[12];
        let time_reserve_hour = buf[14] as i64;
        let options = buf[16];
        let lock_status = buf[17];
        let cycles = buf[23] as i64;
        let energy = buf[30] as i64 * 256 + buf[31] as i64;

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_steam_name);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("None").into(),
        );
        pub_temp_spin(&self.core, temp, spin);
        self.core.publish_property("cycles", cycles.into());
        self.core.publish_on_off("remote_start", lock_status & 2 != 0);
        self.core.publish_on_off("door_lock", lock_status & 0x40 == 0);
        self.core.publish_on_off("child_lock", lock_status & 0x80 == 0);
        self.core
            .publish_property("initial_time", time_initial.into());
        self.core
            .publish_property("remaining_time", time_remain.into());
        self.core
            .publish_property("reserve_time", time_reserve_hour.into());
        self.core.publish_property("energy", energy.into());
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
    use crate::device_trait::DeviceHandler;
    use crate::devices::washer_ctrl::STATUS_QUERY;
    use crate::test_support::{eb_from_ec, make_t2, outbox_has_inner, prop, DEVICE_ID};
    use rethink_core::{hex_decode, Thinq2Device};

    fn make() -> (
        std::sync::Arc<rethink_core::MockHaConnection>,
        std::sync::Arc<rethink_core::MockThinq2Device>,
        std::sync::Arc<Device>,
    ) {
        make_t2("F_V8_Y___W.B_2QEUK", Device::new)
    }

    // Reuse 80-byte frame layout from F_V__F for basic decode of shared offsets
    const SAMPLE: &str = "AA5420EC00250101000100180000000000020000000000000600001000640000000000000000000000000000250104380438130003090401020000000000000400001000640000040000000000000000000053BB";

    // Captured status telegrams from upstream issue #75 (F6WV710P2S.ABLQPDG).
    const SAMPLE_COTTON_STD: &str = "aa5420ec002501000000000000000000000000000000000000000001006400000000000000000000000000002501033a033a0100030b04010000000000010002000001006400000400000000000000000000e0bb";
    const SAMPLE_ALLERGY_STEAM: &str = "aa5420ec002501021c021c0900030504010000000000000002000001006400000500000000000000000000002501023002302d00030b06010000008000000003000001006400000500000000000000000000f2bb";
    const SAMPLE_COTTON_STEAM: &str = "aa5420ec002501031c031c0100030b03010000000000010002000001006400000400000000000000000000002501041104110100030b0601000000800001000300000100640000040000000000000000000015bb";
    const SAMPLE_TURBOWASH: &str = "aa5420ec002501041104110100030b06010000008000010003000001006400000400000000000000000000002501031d031d010003070601000000010001000300000100640000040000000000000000000016bb";
    const SAMPLE_RINSE_PLUS: &str = "aa5420ec002501033703370100030706010000000000010004000001006400000400000000000000000000002501040c040c0100030706020000000000010004000001006400000500000000000000000000b9bb";
    const SAMPLE_PREWASH: &str = "aa5420ec002501033703370100030706010000000000010004000001006400000400000000000000000000002501040c040c01000307060100000040000100040000010064000005000000000000000000007ebb";
    const SAMPLE_INTENSIVE: &str = "aa5420ec002501033703370100030706010000000000010004000001006400000400000000000000000000002501041604160100040706010000000000010004000001006400000400000000000000000000aabb";

    fn decode(frame: &str) -> std::sync::Arc<rethink_core::MockHaConnection> {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(frame));
        ha
    }

    fn options(ha: &rethink_core::MockHaConnection) -> Vec<Option<String>> {
        ["extra_rinse", "turbowash", "prewash", "steam", "soil"]
            .iter()
            .map(|p| prop(ha, p))
            .collect()
    }

    fn opts(v: [&str; 5]) -> Vec<Option<String>> {
        v.iter().map(|s| Some(s.to_string())).collect()
    }

    #[test]
    fn basic() {
        let (ha, thinq, dev) = make();
        assert!(ha.device(DEVICE_ID).unwrap().config.is_some());
        thinq.emit_data(&hex_decode(SAMPLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        thinq.reset_recorder();
        dev.start();
        assert!(outbox_has_inner(&thinq, STATUS_QUERY));
    }

    #[test]
    fn cotton_40_1600_decodes_with_no_options_set() {
        let ha = decode(SAMPLE_COTTON_STD);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1600 rpm"));
        assert_eq!(options(&ha), opts(["OFF", "OFF", "OFF", "OFF", "Normal"]));
    }

    #[test]
    fn allergy_care_60_steam_1600_decodes_steam_on() {
        let ha = decode(SAMPLE_ALLERGY_STEAM);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergy Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1600 rpm"));
        assert_eq!(options(&ha), opts(["OFF", "OFF", "OFF", "ON", "Normal"]));
    }

    #[test]
    fn cotton_60_steam_decodes_steam_on() {
        let ha = decode(SAMPLE_COTTON_STEAM);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
    }

    #[test]
    fn turbowash_decodes_turbowash_on_and_1000_rpm() {
        let ha = decode(SAMPLE_TURBOWASH);
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1000 rpm"));
        assert_eq!(options(&ha), opts(["OFF", "ON", "OFF", "OFF", "Normal"]));
    }

    #[test]
    fn rinse_plus_decodes_extra_rinse_on() {
        let ha = decode(SAMPLE_RINSE_PLUS);
        assert_eq!(options(&ha), opts(["ON", "OFF", "OFF", "OFF", "Normal"]));
    }

    #[test]
    fn prewash_decodes_prewash_on() {
        let ha = decode(SAMPLE_PREWASH);
        assert_eq!(options(&ha), opts(["OFF", "OFF", "ON", "OFF", "Normal"]));
    }

    #[test]
    fn intensive_wash_decodes_soil_intensive() {
        let ha = decode(SAMPLE_INTENSIVE);
        assert_eq!(options(&ha), opts(["OFF", "OFF", "OFF", "OFF", "Intensive"]));
    }

    #[test]
    fn eb_status_reply_decodes_like_the_current_ec_record() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&eb_from_ec(&hex_decode(SAMPLE_ALLERGY_STEAM), RECORD_LEN));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergy Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
    }

    #[test]
    fn other_frame_types_and_unmapped_codes() {
        let (ha, thinq, _) = make();
        let mut frame = hex_decode(SAMPLE_COTTON_STD);
        frame[3] = 0x0a;
        thinq.emit_data(&frame);
        assert!(ha.device(DEVICE_ID).unwrap().properties.is_empty());

        let mut frame = hex_decode(SAMPLE_COTTON_STD);
        frame[2 + 2 + RECORD_LEN + 7] = 0xff; // course
        frame[2 + 2 + RECORD_LEN + 8] = 0x0a; // error code without a label
        thinq.emit_data(&frame);
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
    }

    #[test]
    fn wv9_alias_uses_this_handler() {
        let factory = crate::registry::t2_factory("F_V__Y___W.B__QEUK").unwrap();
        let ha = rethink_core::MockHaConnection::new();
        let meta = Metadata::new("F_V__Y___W.B__QEUK", "WV9-1609B", "1");
        let thinq = rethink_core::MockThinq2Device::new(DEVICE_ID, meta.clone());
        let _dev = factory(ha.clone(), thinq.clone(), meta);
        thinq.emit_data(&hex_decode(SAMPLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
    }
}
