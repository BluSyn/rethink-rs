//! F_V__F___W.B_1QEUK washer (AABB, 80-byte status).

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, drying_mode, fy_base_components, install_components};
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
            let temp = buf[52];
            let spin = buf[51];
            let drying_mode_b = buf[54];
            let lock_status = buf[58];
            let cycles = buf[64] as i64;
            let energy = buf[71] as i64 * 256 + buf[72] as i64;

            self.core.publish_on_off("power", status > 0);
            pub_error_status(&self.core, error, status, crate::washer_common::fy_refresh_name);
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
            self.core
                .publish_property("initial_time", time_initial.into());
            self.core
                .publish_property("remaining_time", time_remain.into());
            self.core.publish_property("energy", energy.into());
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
    use rethink_core::{Thinq2Device, hex_decode, hex_encode, MockHaConnection, MockThinq2Device, Metadata};
    use crate::device_trait::DeviceHandler;

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

    const SAMPLE_INITIAL: &str = "AA5420EC002500000000000000000000000000000000000000010036006400000000000000000000000000002501000000000000000000000000000000000000000036006400000000000000000000000000DFBB";
    const SAMPLE_WASH: &str = "AA5420EC00250101000100180000000000020000000000000600001000640000000000000000000000000000250104380438130003090401020000000000000400001000640000040000000000000000000053BB";
    const SAMPLE_RUNNING: &str = "AA5420EC002506001300140C00030204010000000142200001010036006400000100000E00000000000000002506001300140C00030204010000000142200001010036006400000100000F00000000000000A2BB";

    #[test]
    fn config_and_decode() {
        let (ha, thinq, dev) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        for c in ["power","start","pause","status","drying_mode","energy","remaining_time"] {
            assert!(comps.contains_key(c));
        }
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("54"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(SAMPLE_WASH));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Wash + Dry"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1200 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "drying_mode").as_deref(), Some("Auto"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("296")); // 4*60+56

        thinq.emit_data(&hex_decode(SAMPLE_RUNNING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));

        thinq.reset_recorder();
        dev.start();
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1121010000001800B5BB");
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA08F02A010098BB");
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F0240101009CBB");
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F02404010099BB");
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA09F02405010098BB");
    }
}
