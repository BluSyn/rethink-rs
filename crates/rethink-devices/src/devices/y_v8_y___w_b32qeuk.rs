//! Y_V8_Y___W.B32QEUK washer (AABB, 53-byte status).

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, fy_base_components, install_components};
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
        if buf.len() == 53 && buf[0] == 0x20 {
            let status = buf[15];
            let time_remain = buf[16] as i64 * 60 + buf[17] as i64;
            let time_initial = buf[18] as i64 * 60 + buf[19] as i64;
            let course = buf[20];
            let error = buf[21];
            let spin = buf[24];
            let temp = buf[25];
            let lock_status = buf[30];
            let cycles = buf[36] as i64;
            let energy = buf[47] as i64 * 256 + buf[48] as i64;

            self.core
                .publish_on_off("power", status > 0);
            pub_error_status(&self.core, error, status);
            self.core.publish_property(
                "course",
                course_name(course as u32).unwrap_or("unknown").into(),
            );
            pub_temp_spin(&self.core, temp, spin);
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
    fn meta() -> Metadata { Metadata::new("Y_V8_Y___W.B32QEUK", "Y_V8_Y___W.B32QEUK", "2.11.207") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    const SAMPLE_INITIAL: &str = "AAFF200A0039000381000100EB0027000001032603260000000000000000000000000003000011007100000000000000000000000000974EBB";
    const SAMPLE_RUNNING: &str = "AAFF200A0039000398000100EB0027000006000E000E0C0003020201000000014220000101001100710000010000000000000000000056D9BB";

    #[test]
    fn decode_and_writes() {
        let (ha, thinq, dev) = make();
        thinq.emit_data(&hex_decode(SAMPLE_INITIAL));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("218")); // 3*60+38
        assert_eq!(prop(&ha, "cycles").as_deref(), Some("17"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(SAMPLE_RUNNING));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Quick 14"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Cold"));

        thinq.reset_recorder();
        dev.start();
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA0EF0ED1121010000001800B5BB");
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), "AA08F02A010098BB");
    }
}
