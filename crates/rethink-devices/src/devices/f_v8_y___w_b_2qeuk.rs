//! F_V8_Y___W.B_2QEUK washer (AABB, 80-byte status + more options).

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, fy_base_components, install_components, soil_level, SOIL_LABELS};
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
        install_components(&mut base, fy_base_components());
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
            pub_error_status(&self.core, error, status);
            self.core.publish_property(
                "course",
                course_name(course as u32).unwrap_or("unknown").into(),
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
                soil_level(wash_intensity as usize)
                    .unwrap_or("None")
                    .into(),
            );
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
    use crate::device_trait::DeviceHandler;
    use crate::devices::washer_ctrl::STATUS_QUERY;
    use crate::test_support::{make_t2, outbox_has_inner, prop, DEVICE_ID};
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

    #[test]
    fn basic() {
        let (ha, thinq, dev) = make();
        assert!(ha.device(DEVICE_ID).unwrap().config.is_some());
        thinq.emit_data(&hex_decode(SAMPLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        thinq.reset_recorder();
        dev.start();
        assert!(outbox_has_inner(&thinq, STATUS_QUERY));
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
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
    }
}
