//! F_C__Y___W.A__QEUK front-load washer (LG F4WV709P1, AABB) — A-generation UK.
//!
//! Class 0x20 status comes as 30-byte records: 0xEB carries one, 0xEC the previous
//! then the current one. 0xE2 end-of-cycle alerts are ignored. Door lock comes from
//! 0xD8 while Off/Ready, else it is derived from the status. Port of upstream PR #136.

use crate::device_trait::DeviceHandler;
use crate::devices::washer_ctrl::{pub_error_status, pub_temp_spin, set_power_start_pause};
use crate::washer_common::{
    course_name, fy_base_components, fy_refresh_name, install_components, FY_REFRESH,
};
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use rethink_util::sync::Mutex;
use serde_json::json;
use std::sync::Arc;

const RECORD_LEN: usize = 30;

pub struct Device {
    core: Arc<AabbDeviceCore>,
    last_status: Mutex<i32>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::with_auto_ack(ha, thinq.clone(), true);
        let this = Arc::new(Self {
            core: core.clone(),
            last_status: Mutex::new(-1),
        });

        let mut base = default_config(&meta, Some(json!({"name": "LG Washer"})));
        install_components(&mut base, fy_base_components(FY_REFRESH));
        install_components(
            &mut base,
            [
                ("steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-steam","state_topic":"$this/steam","name":"Steam","icon":"mdi:kettle-steam"})),
                ("wrinkle_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle care","icon":"mdi:tshirt-crew-outline"})),
                ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:account-lock","device_class":"lock","entity_category":"diagnostic"})),
                ("active", json!({"platform":"binary_sensor","unique_id":"$deviceid-active","state_topic":"$this/active","name":"Active","icon":"mdi:washing-machine"})),
                ("tub_clean_count", json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})),
                ("delay_remaining", json!({"platform":"sensor","unique_id":"$deviceid-delay_remaining","state_topic":"$this/delay_remaining","device_class":"duration","unit_of_measurement":"min","name":"Delay remaining","icon":"mdi:clock-start"})),
                // Withdrawn in favour of upstream's entities (tub_clean is now tub_clean_count)
                ("pre_state", json!({"platform":"sensor"})),
                ("tub_clean", json!({"platform":"sensor"})),
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
        let is_e2 = buf.len() == 32 && buf[0] == 0x20 && buf[1] == 0xe2;
        let is_d8 = buf.len() == 3 && buf[0] == 0x20 && buf[1] == 0xd8;
        if is_e2 {
            return;
        }
        if is_d8 {
            // Only authoritative during Off (0) and Ready (1)
            if *self.last_status.lock() <= 1 {
                // HA lock: OFF=Locked, ON=Unlocked
                self.core.publish_on_off("door_lock", buf[2] == 0);
            }
            return;
        }
        if let Some(rec) = process_common_status(buf, 0x20, RECORD_LEN) {
            self.process_record(rec);
        }
    }

    fn process_record(&self, rec: &[u8]) {
        let status = rec[2];
        *self.last_status.lock() = status as i32;
        let remain_h = rec[3];
        let remain_m = rec[4];
        let initial_h = rec[5];
        let initial_m = rec[6];
        let lock_status = rec[7];
        let error_code = rec[8];
        let spin = rec[10];
        let temp = rec[11];
        let course = rec[12];
        let delay_h = rec[14];
        let delay_m = rec[15];
        let steam = rec[16] & 0x80;
        let active = rec[17] & 0x40;
        let wrinkle_care = rec[17] & 0x20;
        let child_lock = rec[17] & 0x80;
        let tub_clean_count = rec[23];

        self.core.publish_on_off("power", status > 0);
        pub_error_status(&self.core, error_code, status, fy_refresh_name);
        self.core.publish_property(
            "course",
            course_name(course as u32).unwrap_or("None").into(),
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
        // HA lock: OFF=Locked, ON=Unlocked
        self.core.publish_on_off("child_lock", child_lock == 0);
        self.core
            .publish_property("tub_clean_count", (tub_clean_count as i64).into());

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
    use rethink_core::device_base::wrap_aabb;
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};
    use std::collections::HashMap;

    const DEVICE_ID: &str = "test-id";
    fn meta() -> Metadata {
        Metadata::new("F_C__Y___W.A__QEUK", "F_C__Y___W.A__QEUK", "1.0.0")
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
    fn props(ha: &MockHaConnection) -> HashMap<String, rethink_core::PropertyValue> {
        ha.device(DEVICE_ID).unwrap().properties
    }
    fn emit(thinq: &MockThinq2Device, frame: &str) {
        thinq.emit_data(&hex_decode(frame));
    }

    // Real captures from upstream master's tests; 0xEC carries [previous][current].
    const SAMPLE_WASHING_EC: &str = "AA4220EC001C06012C02010100030A0601000000004000000306000A003400000500001C06012B02010100030A0601000000004000000306000A003400000500FEBB";
    const SAMPLE_STEAM_ON_EC: &str = "AA4220EC001C01033B033B0100030A0401000000000000000301002D003400000400001C01032803280100030A0601000000800000000401002D0034000004002ABB";
    const SAMPLE_WRINKLE_CARE_ON_EC: &str = "AA4220EC001C060126012B0100030A04010000000060000001060024003400000400001C060125012B0100030A04010000000060000001060024003400000400D8BB";
    const SAMPLE_CHILD_LOCK_ON_EC: &str = "AA4220EC001C060109010C0100030A04010000000040000001060023003400000400001C060109010C0100030A040100000000C000000106002300340000040001BB";
    const SAMPLE_REMOTE_START_ON_EC: &str = "AA4220EC001C01003B003B3200030904010000000100000001010027003400000000001C01011501150700030704010000000000000002010027003400000300B9BB";
    // Synthetic: Cotton/40 °C/1400 rpm delayed start, 4 h delay
    const SAMPLE_DELAYED_EC: &str = "AA4220EC001C03004800480100000A04010004000000000006030009003400000500001C03004700480100000A040100040000000000060300090034000005009BBB";
    const SAMPLE_SPINNING_EC: &str = "AA4220EC001C08000302010100000A0000000000004000000608000A003400000500001C08000202010100000A0000000000004000000608000A003400000500D6BB";
    const SAMPLE_END_EC: &str = "AA4220EC001C0A0000020101000000000000000000400000060A000A003400000500001C0A0000020101000000000000000000000000060A000A00340000050067BB";
    const SAMPLE_OFF_EC: &str = "AA4220EC001C000000020101000000000000000000000000030A000A003400000500001C0000000201010000000000000000000000000300000A0034000005009BBB";
    const SAMPLE_E2_IGNORED: &str =
        "AA2420E2091C04032603260100030A0601000000400000000604000A003400000500B8BB";
    // Synthetic: 0xEB single record
    const SAMPLE_WASHING_EB: &str =
        "AA2420EB001C06003200480100000A0601000000000000000606000A003400000500C4BB";
    const SAMPLE_DOOR_UNLOCKED: &str = "AA0720D800FCBB";
    const SAMPLE_DOOR_LOCKED: &str = "AA0720D80BE1BB";
    const SAMPLE_DOOR_LOCKED_30: &str = "AA0720D8308CBB";
    const SAMPLE_ERROR_EC: &str = "AA4220EC001C04031503150100030A06010007000000000001040032003400000400001C12031503150101030A060100070000000000041200320034000004009BBB";

    const WRITE_INIT: &str = "AA0EF0ED1121010000001800B5BB";
    const WRITE_POWER_ON: &str = "AA08F02A010098BB";
    const WRITE_POWER_OFF: &str = "AA09F0240101009CBB";
    const WRITE_PAUSE: &str = "AA09F02404010099BB";
    const WRITE_START: &str = "AA09F02405010098BB";

    #[test]
    fn config_exposes_expected_components() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        for c in [
            "power",
            "start",
            "pause",
            "status",
            "error",
            "error_message",
            "course",
            "temp",
            "spin",
            "remote_start",
            "door_lock",
            "steam",
            "wrinkle_care",
            "child_lock",
            "active",
            "tub_clean_count",
            "initial_time",
            "remaining_time",
            "delay_remaining",
        ] {
            assert!(comps.contains_key(c), "component {c} present");
        }
        let status_options = comps["status"]["options"].as_array().unwrap();
        assert!(status_options.contains(&json!("Washing")));
        assert!(status_options.contains(&json!("Error")));
        assert_eq!(comps.len(), 19 + 2);
        assert_eq!(
            comps["tub_clean_count"],
            json!({"platform":"sensor","unique_id":"$deviceid-tub_clean_count","state_topic":"$this/tub_clean_count","name":"Washes since drum clean","icon":"mdi:washing-machine-alert","entity_category":"diagnostic"})
        );
        assert_eq!(comps["steam"]["icon"], "mdi:kettle-steam");
        assert_eq!(comps["wrinkle_care"]["icon"], "mdi:tshirt-crew-outline");
        assert_eq!(
            comps["status"]["options"],
            json!([
                "Off",
                "Standby",
                "Paused",
                "Delayed",
                "Detecting",
                "Washing",
                "Rinsing",
                "Spinning",
                "Drying",
                "End",
                "Cool down",
                "Rinse hold",
                "Refreshing",
                "Demo",
                "Error"
            ])
        );
    }

    #[test]
    fn withdrawn_entities_are_tombstoned() {
        let (ha, thinq, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert_eq!(comps["pre_state"], json!({"platform": "sensor"}));
        assert_eq!(comps["tub_clean"], json!({"platform": "sensor"}));
        emit(&thinq, SAMPLE_WASHING_EC);
        let published = props(&ha);
        assert!(!published.contains_key("pre_state"));
        assert!(!published.contains_key("tub_clean"));
    }

    #[test]
    fn delayed_start_decodes_status_course_spin_temp_times_and_delay() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELAYED_EC);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Delayed"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("71"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("72"));
        assert_eq!(prop(&ha, "delay_remaining").as_deref(), Some("240"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "active").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("9"));
    }

    #[test]
    fn washing_decodes_status_course_spin_temp_and_times() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("103"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("121"));
        assert_eq!(prop(&ha, "delay_remaining").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "active").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("10"));
    }

    #[test]
    fn spinning_decodes_status_and_remaining_time() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_SPINNING_EC);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Spinning"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("2"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
    }

    #[test]
    fn end_keeps_power_on_and_clears_spin_temp_course() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_END_EC);
        assert_eq!(prop(&ha, "status").as_deref(), Some("End"));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Not selected"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("None"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("10"));
    }

    #[test]
    fn steam_off_when_bit7_clear() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
    }

    #[test]
    fn steam_on_in_ready_state() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_STEAM_ON_EC);
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("220"));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("OFF"));
    }

    #[test]
    fn steam_toggles_on_then_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_STEAM_ON_EC);
        assert_eq!(prop(&ha, "steam").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
    }

    #[test]
    fn wrinkle_care_off_when_bit5_clear() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("OFF"));
    }

    #[test]
    fn wrinkle_care_on_when_bit5_set() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WRINKLE_CARE_ON_EC);
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "active").as_deref(), Some("ON"));
    }

    #[test]
    fn wrinkle_care_toggles_on_then_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WRINKLE_CARE_ON_EC);
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("OFF"));
    }

    #[test]
    fn child_lock_unlocked_when_bit7_clear() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn child_lock_locked_when_bit7_set() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_CHILD_LOCK_ON_EC);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("69"));
        assert_eq!(prop(&ha, "active").as_deref(), Some("ON"));
    }

    #[test]
    fn child_lock_toggles_engaged_then_disengaged() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_CHILD_LOCK_ON_EC);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("OFF"));
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "child_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn remote_start_off_when_bit1_clear() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn remote_start_on_when_bit1_set() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_REMOTE_START_ON_EC);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Standby"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1000 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("81"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("81"));
        assert_eq!(prop(&ha, "steam").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "active").as_deref(), Some("OFF"));
    }

    #[test]
    fn remote_start_toggles_on_then_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_REMOTE_START_ON_EC);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_DELAYED_EC);
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("OFF"));
    }

    #[test]
    fn off_state_powers_off() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_OFF_EC);
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "active").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn eb_single_record_decodes_like_the_ec_current_record() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EB);
        assert_eq!(prop(&ha, "status").as_deref(), Some("Washing"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("50"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1400 rpm"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("60 °C"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("10"));
    }

    #[test]
    fn e2_end_of_cycle_alert_is_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        let before = props(&ha);
        emit(&thinq, SAMPLE_E2_IGNORED);
        assert_eq!(props(&ha), before);
    }

    #[test]
    fn door_lock_derived_from_status_without_d8() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DELAYED_EC);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        emit(&thinq, SAMPLE_OFF_EC);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn d8_unlocked_is_ignored_during_an_active_wash() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        emit(&thinq, SAMPLE_DOOR_UNLOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn d8_overrides_door_lock_during_startup() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DOOR_LOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        emit(&thinq, SAMPLE_DOOR_UNLOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn d8_zero_publishes_unlocked() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DOOR_UNLOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn d8_non_zero_publishes_locked() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DOOR_LOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn d8_cycle_start_lock_publishes_locked() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DOOR_LOCKED_30);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
    }

    #[test]
    fn door_lock_toggles_across_lock_unlock() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_DOOR_UNLOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_DOOR_LOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("OFF"));
        emit(&thinq, SAMPLE_DOOR_UNLOCKED);
        assert_eq!(prop(&ha, "door_lock").as_deref(), Some("ON"));
    }

    #[test]
    fn frames_without_aabb_envelope_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        let before = props(&ha);
        emit(&thinq, "001122");
        assert_eq!(props(&ha), before);
    }

    #[test]
    fn frames_with_unrecognised_inner_length_are_ignored() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        let before = props(&ha);
        emit(&thinq, "AA0820EC010203040506BB");
        assert_eq!(props(&ha), before);
    }

    #[test]
    fn d8_and_e2_are_acked_but_eb_ec_status_is_not() {
        let (_ha, thinq, _) = make();
        thinq.reset_recorder();
        emit(&thinq, SAMPLE_DOOR_UNLOCKED);
        emit(&thinq, SAMPLE_E2_IGNORED);
        emit(&thinq, SAMPLE_WASHING_EB);
        emit(&thinq, SAMPLE_WASHING_EC);
        let acks: Vec<String> = thinq
            .sent()
            .into_iter()
            .filter(|m| m.cmd == "ack")
            .map(|m| m.data.as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            acks,
            vec![
                hex_encode(&wrap_aabb(&hex_decode("f000d804"))),
                hex_encode(&wrap_aabb(&hex_decode("f000e204"))),
            ]
        );
        assert!(thinq.outbox().is_empty(), "acks are not sent as packets");
    }

    #[test]
    fn start_sends_the_status_query() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        DeviceHandler::start(dev.as_ref());
        assert_eq!(thinq.outbox().len(), 1);
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_INIT);
    }

    #[test]
    fn write_power_on() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "ON");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_ON);
    }

    #[test]
    fn write_power_off() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("power", "OFF");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_POWER_OFF);
    }

    #[test]
    fn write_pause() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("pause", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_PAUSE);
    }

    #[test]
    fn write_start_with_default_payload() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("start", "");
        assert_eq!(hex_encode(&thinq.outbox()[0]), WRITE_START);
    }

    #[test]
    fn write_to_unknown_property_sends_nothing() {
        let (_ha, thinq, dev) = make();
        thinq.reset_recorder();
        dev.set_property("does-not-exist", "whatever");
        assert!(thinq.outbox().is_empty());
    }

    #[test]
    fn no_error_publishes_ok() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
    }

    #[test]
    fn de2_door_lock_error_is_reported() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_ERROR_EC);
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        assert_eq!(
            prop(&ha, "error_message").as_deref(),
            Some("Door lock error (DE2)")
        );
        assert_eq!(prop(&ha, "status").as_deref(), Some("Error"));
        assert_eq!(prop(&ha, "tub_clean_count").as_deref(), Some("50"));
    }

    #[test]
    fn error_clears_when_the_machine_recovers() {
        let (ha, thinq, _) = make();
        emit(&thinq, SAMPLE_ERROR_EC);
        assert_eq!(prop(&ha, "error").as_deref(), Some("ON"));
        emit(&thinq, SAMPLE_WASHING_EC);
        assert_eq!(prop(&ha, "error").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "error_message").as_deref(), Some("OK"));
    }

    #[test]
    fn wv5_alias_decodes_the_a_gen_frames() {
        let canonical = crate::t2_factory("F_C__Y___W.A__QEUK").unwrap();
        let alias = crate::t2_factory("F_V7_Y___W.A__QEUK").unwrap();
        assert!(std::ptr::fn_addr_eq(canonical, alias));
        let ha = MockHaConnection::new();
        let alias_meta = Metadata::new("F_V7_Y___W.A__QEUK", "F_V7_Y___W.A__QEUK", "2.9.61");
        let thinq = MockThinq2Device::new(DEVICE_ID, alias_meta.clone());
        let _dev = alias(ha.clone(), thinq.clone(), alias_meta);
        emit(
            &thinq,
            "AA4220EC001C0100000000010000000000000000000000000001002C006400000000001C04023A023A010003090401000000000000000104002C006400000000B3BB",
        );
        assert_eq!(prop(&ha, "status").as_deref(), Some("Detecting"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Cotton"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("40 °C"));
        assert_eq!(prop(&ha, "spin").as_deref(), Some("1200 rpm"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("178"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("178"));
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("F_C__Y___W.A__QEUK").is_some());
    }
}
