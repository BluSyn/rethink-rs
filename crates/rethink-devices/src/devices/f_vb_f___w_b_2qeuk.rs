//! F_VB_F___W.B_2QEUK washer (AABB, 80-byte status + doses/options).

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, drying_mode, fy_base_components, install_components, DOSES};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::json;
use std::sync::Arc;

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
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() == 80 && buf[0] == 0x20 {
            let status = buf[43];
            let time_remain = buf[44] as i64 * 60 + buf[45] as i64;
            let time_initial = buf[46] as i64 * 60 + buf[47] as i64;
            let course = buf[48];
            let error = buf[49];
            let spin = buf[51];
            let temp = buf[52];
            let extra_rinse = buf[53];
            let drying_mode_b = buf[54];
            let delay_end = buf[55] as i64;
            let options = buf[57];
            let lock_status = buf[58];
            let cycles = buf[64] as i64;
            let energy = buf[71] as i64 * 256 + buf[72] as i64;
            let detergent = buf[73] as usize;
            let softener = buf[74] as usize;

            self.core.publish_on_off("power", status > 0);
            pub_error_status(&self.core, error, status, crate::washer_common::fy_refresh_pause19_name);
            self.core.publish_property(
                "course",
                course_name(course as u32).unwrap_or("unknown").into(),
            );
            pub_temp_spin(&self.core, temp, spin);
            self.core.publish_property(
                "drying_mode",
                drying_mode(drying_mode_b as u32)
                    .unwrap_or("unknown")
                    .into(),
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
                DOSES.get(detergent).copied().unwrap_or("unknown").into(),
            );
            self.core.publish_property(
                "softener",
                DOSES.get(softener).copied().unwrap_or("unknown").into(),
            );
            self.core.publish_on_off("extra_rinse", extra_rinse >= 2);
            self.core.publish_on_off("turbowash", options & 0x01 != 0);
            self.core.publish_on_off("eco_hybrid", options & 0x08 != 0);
            self.core.publish_on_off("prewash", options & 0x40 != 0);
            self.core.publish_on_off("steam", options & 0x80 != 0);
        }
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
    use rethink_core::{Thinq2Device, hex_decode, MockHaConnection, MockThinq2Device, Metadata};

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

    const SAMPLE: &str = "AA5420EC00250101000100180000000000020000000000000600001000640000000000000000000000000000250104380438130003090401020000000000000400001000640000040000000000000000000053BB";

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
}
