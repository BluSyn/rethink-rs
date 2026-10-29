//! F_VB_Y___W.B_2QEUK washer (LG F4WV710ATR). 80-byte class `0x20` status.
//!
//! Same offsets as `F_V8_Y___W.B_2QEUK`, plus detergent and softener doses.
//! Child lock is not exposed. `auto_ack` is on; `EC` status stays unacked.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{
    course_name, fy_base_components, install_components, soil_level, DOSES, SOIL_LABELS,
};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

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
        if buf.len() != 80 || buf[0] != 0x20 {
            return;
        }
        let status = buf[43];
        let time_remain = buf[44] as i64 * 60 + buf[45] as i64;
        let time_initial = buf[46] as i64 * 60 + buf[47] as i64;
        let course = buf[48];
        let error = buf[49];
        let wash_intensity = buf[50];
        let spin = buf[51];
        let temp = buf[52];
        let extra_rinse = buf[53];
        let time_reserve_hour = buf[55] as i64;
        let options = buf[57];
        let lock_status = buf[58];
        let cycles = buf[64] as i64;
        let energy = buf[71] as i64 * 256 + buf[72] as i64;

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_steam_pause19_name);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("unknown").into(),
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
            .publish_property("detergent", dose_name(buf[73]).into());
        self.core
            .publish_property("softener", dose_name(buf[74]).into());
        self.core.publish_on_off("extra_rinse", extra_rinse >= 2);
        self.core.publish_on_off("turbowash", options & 0x01 != 0);
        self.core.publish_on_off("prewash", options & 0x40 != 0);
        self.core.publish_on_off("steam", options & 0x80 != 0);
        self.core.publish_property(
            "soil",
            soil_level(wash_intensity as usize).unwrap_or("None").into(),
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
    use crate::test_support::{prop, DEVICE_ID};
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device, Thinq2Device};

    const IDLE: &str = "aa5420ec002600000000000000000000000000000000000000010008006400000000000002022a1e000201002601000000000000000000000000000000000000000008006400000000000002022a1e00000199bb";
    const COTTON: &str = "aa5420ec002601002400241b00030503010000000000000001000008006400000300000001002a1e000001002601030203020100030a04010000000000010003000008006400000400000000002a1e0080018ebb";
    const ALLERGY: &str = "aa5420ec002601021c021c0900030504010000000000000002000008006400000500000002022a1e000201002601023002302d00030a06010000008000000004000008006400000500000002022a1e00020100bb";
    const EASY: &str = "aa5420ec002601023502350200030a03020000000000010002000008006400000300000001002a1e000001002601030a030a0200030a03020000004000010003000008006400000400000001002a1e00000197bb";
    const INTENSIVE: &str = "aa5420ec002601013a013a0200030a04010000000000010002000008006400000300000001002a1e000001002601032003200200040a04010000000000010003000008006400000300000001002a1e000001e3bb";
    const DELAY: &str = "aa5420ec002601000e000e0c00030202010012000100000000000008006400000100000001002a1e000001002601000e000e0c00030202010013000100000000000008006400000100000001002a1e00000118bb";
    const DET_HIGH: &str = "aa5420ec002601031d031d0200030706020000008000010004000008006400000300000002002a1e008001002601031d031d0200030706020000008000010004000008006400000300000003002a1e000001aabb";
    const SOFT_HIGH: &str = "aa5420ec002601000e000e0c00030202010000000100000000000008006400000100000001022a1e000201002601000e000e0c00030202010000000100000000000008006400000100000001032a1e00020164bb";
    const RUNNING: &str = "aa5420ec002606000c000e0c00030202010000000142200001010008006400000100001301002a1e000001002606000c000e0c00030202010000000142200001010008006400000100001401002a1e00000148bb";
    const PAUSED: &str = "aa5420ec0026070009000e0c00030200010000000142000001060008006400000100002301002a1e0000010026020009000e0c00030200010000000140000000070008006400000100002301002a1e000001a2bb";
    const END: &str = "aa5420ec00260a0000000e0c00000000000000000040000001080008006400000100002901002a1e00000100260a0000000e0c00000000000000000000000001080008006400000100002901002a1e000001e5bb";
    const POWER_OFF: &str = "aa5420ec002601010c010c1200030106010000000000000003000009006400000200002900002a1e000001002600010c010c1200000000000000000000000003010009006400000200002900002a1e000001d6bb";
    const PAUSE_MARKER: &str = "aa09207200c80058bb";
    const POWER_OFF_MARKER: &str = "aa0720d809e7bb";

    fn feed(
        frame: &str,
    ) -> (
        std::sync::Arc<MockHaConnection>,
        std::sync::Arc<MockThinq2Device>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(
            DEVICE_ID,
            Metadata::new("F_VB_Y___W.B_2QEUK", "F_VB_Y___W.B_2QEUK", "2.10.123"),
        );
        let _dev = Device::new(ha.clone(), thinq.clone(), thinq.meta().clone());
        thinq.emit_data(&hex_decode(frame));
        (ha, thinq)
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
    fn dosing_sensors_are_enums_and_child_lock_is_absent() {
        let (ha, _) = feed(IDLE);
        let info = ha.device(DEVICE_ID).unwrap();
        let components = &info.config.as_ref().unwrap().components;
        for name in ["detergent", "softener", "soil"] {
            assert_eq!(components[name]["platform"], "sensor");
            assert_eq!(components[name]["device_class"], "enum");
        }
        assert!(components.get("child_lock").is_none());
        assert_eq!(
            components["intensive_wash"],
            json!({"platform": "binary_sensor"})
        );
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Medium"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Medium"));
    }

    #[test]
    fn captured_courses_options_and_doses() {
        let (ha, _) = feed(COTTON);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("182"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("182"));
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Normal"));

        let (ha, _) = feed(ALLERGY);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Allergy Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("OFF"));

        let (ha, _) = feed(EASY);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Ease Care"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("30 °C"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "prewash").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));

        let (ha, _) = feed(INTENSIVE);
        assert_eq!(prop(&ha, "soil").as_deref(), Some("Intensive"));
        assert_eq!(prop(&ha, "extra_rinse").as_deref(), Some("OFF"));

        let (ha, _) = feed(DELAY);
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick 14"));
        assert_eq!(prop(&ha, "reserve_time").as_deref(), Some("19"));
        assert_eq!(prop(&ha, "turbowash").as_deref(), Some("ON"));

        let (ha, _) = feed(DET_HIGH);
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("High"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("Off"));
        let (ha, _) = feed(SOFT_HIGH);
        assert_eq!(prop(&ha, "detergent").as_deref(), Some("Low"));
        assert_eq!(prop(&ha, "softener").as_deref(), Some("High"));
    }

    #[test]
    fn running_pause_end_and_power_off() {
        let (ha, _) = feed(RUNNING);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("14"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("12"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("20"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("8"));

        let (ha, _) = feed(PAUSED);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Paused"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));

        let (ha, _) = feed(END);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("41"));

        let (ha, _) = feed(POWER_OFF);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("9"));
        assert_eq!(prop(&ha, "energy").as_deref(), Some("41"));
    }

    #[test]
    fn acks_marker_frames_and_not_status() {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, Metadata::new("F_VB_Y___W.B_2QEUK", "M", "1"));
        let _dev = Device::new(ha.clone(), thinq.clone(), thinq.meta().clone());
        thinq.emit_data(&hex_decode(PAUSE_MARKER));
        thinq.emit_data(&hex_decode(POWER_OFF_MARKER));
        assert_eq!(
            acks(&thinq),
            vec![
                "AA08F00072044DBB".to_string(),
                "AA08F000D8042BBB".to_string()
            ]
        );
        thinq.emit_data(&hex_decode(RUNNING));
        assert_eq!(acks(&thinq).len(), 2);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("F_VB_Y___W.B_2QEUK").is_some());
    }
}
