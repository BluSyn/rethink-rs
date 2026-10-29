//! Y_V8_F___W.B_2QEUK washer/dryer combo (AABB dual status layout).

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
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.is_empty() || buf[0] != 0x20 {
            return;
        }
        if buf.len() > 8 && buf[8] == 0x02 && buf.len() == 79 {
            self.core
                .publish_property("cycles", (buf[19] as i64).into());
            return;
        }
        if buf.len() <= 8 || buf[8] != 0x01 {
            return;
        }

        let s = if buf.len() > 73 { 54 } else { 15 };
        if buf.len() <= s + 19 {
            return;
        }

        let status = buf[s];
        let remaining = buf[s + 1] as i64 * 60 + buf[s + 2] as i64;
        let initial = buf[s + 3] as i64 * 60 + buf[s + 4] as i64;
        let course = buf[s + 5];
        let error = buf[s + 6];
        let spin = buf[s + 8];
        let temp = buf[s + 9];
        let drying = buf[s + 11];

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error, status, crate::washer_common::fy_steam_name);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("unknown").into(),
        );
        pub_temp_spin(&self.core, temp, spin);
        self.core.publish_property(
            "drying_mode",
            drying_mode(drying as u32).unwrap_or("unknown").into(),
        );
        self.core.publish_on_off("remote_start", buf[s + 15] & 0x40 != 0);
        self.core.publish_on_off("door_lock", buf[s + 19] & 0x40 == 0);
        self.core.publish_property("initial_time", initial.into());
        self.core.publish_property("remaining_time", remaining.into());
        if buf.len() > s + 29 {
            let energy = buf[s + 28] as i64 * 256 + buf[s + 29] as i64;
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
    use rethink_core::{Thinq2Device, MockHaConnection, MockThinq2Device, Metadata};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata { Metadata::new("Y_V8_F___W.B_2QEUK", "Y_V8_F___W.B_2QEUK", "1.0") }
    fn make() -> (std::sync::Arc<MockHaConnection>, std::sync::Arc<MockThinq2Device>, std::sync::Arc<Device>) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    #[test]
    fn config_present() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("drying_mode"));
        assert!(comps.contains_key("status"));
    }

    // Minimal synthetic frame: 20, ..., buf[8]=0x01, length > 15+19, status at S=15
    #[test]
    fn synthetic_short_status() {
        let (ha, thinq, _) = make();
        // Build 53-byte-like body with type marker: length enough for S=15
        // buf[0]=0x20, buf[8]=0x01, status at 15 = 1 (Ready)
        let mut inner = vec![0u8; 40];
        inner[0] = 0x20;
        inner[8] = 0x01;
        inner[15] = 1; // Ready
        inner[16] = 0; inner[17] = 10; // remaining 10
        inner[18] = 0; inner[19] = 30; // initial 30
        // wrap AA BB
        let mut pkt = vec![0xaa, (inner.len()+4) as u8];
        pkt.extend(&inner);
        pkt.push(0); pkt.push(0xbb);
        thinq.emit_data(&pkt);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("10"));
    }
}
