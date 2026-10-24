//! F_C__Y___W.A__QEUK front-load washer (AABB) — A-generation UK.
//!
//! Port of upstream PR #136. 62-byte 0x20/0xEC dual-section status and 32-byte
//! 0xEB compact status share the first-section field layout. 0xE2 end-of-cycle
//! alerts are ignored. Door lock comes from 0xD8 when idle/ready, else status-derived.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{course_name, fy_base_components, install_components, state_name, state_options};
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

pub struct Device {
    core: Arc<AabbDeviceCore>,
    last_status: Mutex<i32>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self {
            core: core.clone(),
            last_status: Mutex::new(-1),
        });

        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        install_components(&mut base, fy_base_components());
        install_components(
            &mut base,
            [
                ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:weather-fog"})),
                ("wrinkle_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle care","icon":"mdi:iron-outline"})),
                ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:account-lock","device_class":"lock","entity_category":"diagnostic"})),
                ("active", json!({"platform":"binary_sensor","unique_id":"$deviceid-active","state_topic":"$this/active","name":"Active","icon":"mdi:washing-machine"})),
                ("pre_state", json!({"platform":"sensor","unique_id":"$deviceid-pre_state","state_topic":"$this/pre_state","name":"Pre state","icon":"mdi:state-machine","device_class":"enum","options":state_options()})),
                ("tub_clean", json!({"platform":"sensor","unique_id":"$deviceid-tub-clean","state_topic":"$this/tub_clean","name":"Tub clean counter","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
                ("delay_remaining", json!({"platform":"sensor","unique_id":"$deviceid-delay_remaining","state_topic":"$this/delay_remaining","device_class":"duration","unit_of_measurement":"min","name":"Delay remaining","icon":"mdi:clock-start"})),
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
        let is_ec = buf.len() == 62 && buf[0] == 0x20 && buf[1] == 0xec;
        let is_eb = buf.len() == 32 && buf[0] == 0x20 && buf[1] == 0xeb;
        let is_e2 = buf.len() == 32 && buf[0] == 0x20 && buf[1] == 0xe2;
        let is_d8 = buf.len() == 3 && buf[0] == 0x20 && buf[1] == 0xd8;
        if is_e2 {
            return;
        }
        if is_d8 {
            let last = *self.last_status.lock();
            // Only authoritative during Off (0) and Ready (1)
            if last <= 1 {
                // HA lock: OFF=Locked, ON=Unlocked
                self.core.publish_on_off("door_lock", buf[2] == 0);
            }
            return;
        }
        if !(is_ec || is_eb) {
            return;
        }
        if buf.len() < 26 {
            return;
        }
        let status = buf[4];
        *self.last_status.lock() = status as i32;
        let remain_h = buf[5];
        let remain_m = buf[6];
        let initial_h = buf[7];
        let initial_m = buf[8];
        let lock_status = buf[9];
        let error_code = buf[10];
        let spin = buf[12];
        let temp = buf[13];
        let course = buf[14];
        let delay_h = buf[16];
        let delay_m = buf[17];
        let steam = buf[18] & 0x80;
        let wrinkle_care = buf[18] & 0x20;
        let active = buf[19] & 0x40;
        let child_lock = buf[19] & 0x80;
        let pre_state = buf[23];
        let tub_clean = buf[25];

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error_code, status);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("unknown").into(),
        );
        pub_temp_spin(&self.core, temp, spin);
        self.core.publish_property(
            "remaining_time",
            (remain_h as i64 * 60 + remain_m as i64).into(),
        );
        self.core.publish_property(
            "initial_time",
            (initial_h as i64 * 60 + initial_m as i64).into(),
        );
        self.core.publish_property(
            "delay_remaining",
            (delay_h as i64 * 60 + delay_m as i64).into(),
        );
        self.core.publish_on_off("remote_start", lock_status & 2 != 0);
        self.core.publish_on_off("steam", steam != 0);
        self.core.publish_on_off("wrinkle_care", wrinkle_care != 0);
        self.core.publish_on_off("active", active != 0);
        // HA lock device_class: OFF=Locked, ON=Unlocked for child_lock mapping from TS
        self.core.publish_on_off("child_lock", child_lock == 0);
        self.core.publish_property(
            "pre_state",
            state_name(pre_state as usize).into(),
        );
        self.core
            .publish_property("tub_clean", (tub_clean as i64).into());

        // Off → unlocked ON; Ready → leave to 0xD8; else locked OFF
        if status == 0 {
            self.core.publish_property("door_lock", "ON".into());
        } else if status != 1 {
            self.core.publish_property("door_lock", "OFF".into());
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
    use rethink_core::{hex_decode, MockHaConnection, MockThinq2Device, Metadata, Thinq2Device};

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata {
        Metadata::new("F_C__Y___W.A__QEUK", "F_C__Y___W.A__QEUK", "1.0")
    }
    fn make() -> (
        std::sync::Arc<MockHaConnection>,
        std::sync::Arc<MockThinq2Device>,
        std::sync::Arc<Device>,
    ) {
        let ha = MockHaConnection::new();
        let thinq = MockThinq2Device::new(DEVICE_ID, meta());
        let dev = Device::new(ha.clone(), thinq.clone(), meta());
        (ha, thinq, dev)
    }
    fn prop(ha: &MockHaConnection, n: &str) -> Option<String> {
        ha.device(DEVICE_ID)?.properties.get(n).map(|p| p.as_string())
    }

    // Fixtures from upstream PR #136
    const SAMPLE_WASHING_EC: &str = "AA4220EC001C06012C02010100030A0601000000004000000306000A003400000500001C06012B02010100030A0601000000004000000306000A003400000500FEBB";
    const SAMPLE_STEAM_ON_EC: &str = "AA4220EC001C06012C02010100030A0601000000804000000306000A003400000500001C06012B02010100030A0601000000004000000306000A003400000500FEBB";
    const SAMPLE_WRINKLE_CARE_ON_EC: &str = "AA4220EC001C06012C02010100030A0601000000204000000306000A003400000500001C06012B02010100030A0601000000004000000306000A003400000500FEBB";
    const SAMPLE_CHILD_LOCK_ON_EC: &str = "AA4220EC001C06012C02010100030A060100000000C000000306000A003400000500001C06012B02010100030A0601000000004000000306000A003400000500FEBB";
    const SAMPLE_REMOTE_START_ON_EC: &str = "AA4220EC001C01003B003B3200030904010000000100000001010030003400000000001C01041B041B0400030A0601000000000000000201003000340000020044BB";
    const SAMPLE_OFF_EC: &str = "AA4220EC001C000000020101000000000000000000000000030A000A003400000500001C0000000201010000000000000000000000000300000A0034000005009BBB";
    const SAMPLE_END_EC: &str = "AA4220EC001C0A0000020101000000000000000000400000060A000A003400000500001C0A0000020101000000000000000000000000060A000A00340000050067BB";
    const SAMPLE_WASHING_EB: &str = "AA2420EB001C06003200480100000A0601000000000000000606000A003400000500C4BB";
    const SAMPLE_E2_IGNORED: &str = "AA2420E2091C04032603260100030A0601000000400000000604000A003400000500B8BB";
    const SAMPLE_DOOR_UNLOCKED: &str = "AA0720D800FCBB";
    const SAMPLE_DOOR_LOCKED: &str = "AA0720D80BE1BB";

    #[test]
    fn config_components() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("power"));
        assert!(comps.contains_key("status"));
        assert!(comps.contains_key("steam"));
        assert!(comps.contains_key("wrinkle_care"));
        assert!(comps.contains_key("tub_clean"));
        assert!(comps.contains_key("delay_remaining"));
    }

    #[test]
    fn washing_ec() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_WASHING_EC));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        // 1h44 remaining, 2h01 initial
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("104"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("121"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "active").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "tub_clean").as_deref(), Some("10"));
    }

    #[test]
    fn steam_wrinkle_child_lock() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_STEAM_ON_EC));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(SAMPLE_WRINKLE_CARE_ON_EC));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("ON"));

        thinq.emit_data(&hex_decode(SAMPLE_CHILD_LOCK_ON_EC));
        // HA lock: child lock engaged → OFF (Locked semantics in TS port)
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn remote_start_ready() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_REMOTE_START_ON_EC));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Ready"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        // Wire buf[14]=0x01 → Cotton (PR comment said Ease Care; byte is Cotton/0x01)
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1200 rpm"));
    }

    #[test]
    fn off_and_end() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_END_EC));
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF")); // locked during End

        thinq.emit_data(&hex_decode(SAMPLE_OFF_EC));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON")); // unlocked when off
    }

    #[test]
    fn eb_and_e2() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_WASHING_EB));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("50"));

        thinq.emit_data(&hex_decode(SAMPLE_E2_IGNORED));
        // still washing — E2 ignored
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
    }

    #[test]
    fn door_d8_when_ready() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(SAMPLE_REMOTE_START_ON_EC)); // Ready status=1
        thinq.emit_data(&hex_decode(SAMPLE_DOOR_LOCKED));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        thinq.emit_data(&hex_decode(SAMPLE_DOOR_UNLOCKED));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("F_C__Y___W.A__QEUK").is_some());
    }
}
