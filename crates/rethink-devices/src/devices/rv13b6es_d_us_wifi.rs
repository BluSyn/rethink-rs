//! RV13B6ES_D_US_WIFI electric dryer (AABB).
//!
//! Port of upstream PR #134. Frame layout matches RV13B6BSD_D_US_WIFI but:
//! - Wrinkle Care is rec[15] bit 0x10 (not rec[16])
//! - rec[23] is load_item count
//! - remote_start is rec[16] bit 0x01
//! - also publishes signal and more_less_time

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, Thinq2Device};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::sync::Arc;

const STATUS_FRAME_TYPE: u8 = 0xec;
const STATUS_FRAME_LEN: usize = 60;
const RECORD_B_OFFSET: usize = 32;
const SINGLE_STATUS_FRAME_TYPE: u8 = 0xeb;
const SINGLE_STATUS_FRAME_LEN: usize = 31;
const SINGLE_RECORD_OFFSET: usize = 3;

const PHASE_OFFSET: usize = 1;
const TIME_HOUR_OFFSET: usize = 2;
const TIME_MIN_OFFSET: usize = 3;
const INITIAL_TIME_HOUR_OFFSET: usize = 4;
const INITIAL_TIME_MIN_OFFSET: usize = 5;
const COURSE_OFFSET: usize = 6;
const DRY_LEVEL_OFFSET: usize = 8;
const TEMP_OFFSET: usize = 9;
const SIGNAL_OFFSET: usize = 11;
const MORE_LESS_TIME_OFFSET: usize = 12;
const FLAGS_OFFSET: usize = 15;
const FLAG_CHILD_LOCK: u8 = 0x01;
const FLAG_REDUCE_STATIC: u8 = 0x02;
const FLAG_DAMP_DRY_SIGNAL: u8 = 0x08;
const FLAG_WRINKLE_CARE: u8 = 0x10;
const OPT2_OFFSET: usize = 16;
const OPT2_REMOTE_START: u8 = 0x01;
const OPT2_ENERGY_SAVER: u8 = 0x02;
const OPT2_TURBO_STEAM: u8 = 0x04;
const LOAD_ITEM_OFFSET: usize = 23;
const PHASE_OFF: u8 = 0x00;

fn status_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x00, "Off"),
        (0x01, "Initial"),
        (0x03, "Pause"),
        (0x32, "Drying"),
        (0x33, "Cooling"),
        (0x04, "End"),
    ])
}
fn course_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (0x01, "Heavy Duty"),
        (0x02, "Towels"),
        (0x03, "Normal"),
        (0x04, "Perm Press"),
        (0x05, "Delicates"),
        (0x07, "Bedding"),
        (0x08, "Antibacterial"),
        (0x10, "Speed Dry"),
        (0x11, "Air Dry"),
        (0x12, "Time Dry"),
        (0x15, "Steam Fresh"),
        (0x16, "Steam Sanitary"),
        (0x1a, "Super Dry"),
    ])
}
fn dry_level_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Damp"),
        (2, "Less"),
        (3, "Normal"),
        (4, "More"),
        (5, "Very"),
    ])
}
fn temp_map() -> HashMap<u8, &'static str> {
    HashMap::from([
        (1, "Ultra Low"),
        (2, "Low"),
        (3, "Medium"),
        (4, "Mid High"),
        (5, "High"),
    ])
}
fn signal_map() -> HashMap<u8, &'static str> {
    HashMap::from([(0x00, "Off"), (0x01, "Low"), (0x04, "High")])
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(ha: Arc<dyn HaConnection>, thinq: Arc<dyn Thinq2Device>, meta: Metadata) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });

        let mut base = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        let comps = [
            ("power", json!({"platform":"binary_sensor","unique_id":"$deviceid-power","state_topic":"$this/power","name":"Power","icon":"mdi:tumble-dryer","device_class":"running"})),
            ("status", json!({"platform":"sensor","unique_id":"$deviceid-status","state_topic":"$this/status","name":"Status","icon":"mdi:state-machine"})),
            ("course", json!({"platform":"sensor","unique_id":"$deviceid-course","state_topic":"$this/course","name":"Course","icon":"mdi:pin-outline"})),
            ("remaining_time", json!({"platform":"sensor","unique_id":"$deviceid-remaining_time","state_topic":"$this/remaining_time","name":"Remaining time","icon":"mdi:timer-outline","device_class":"duration","unit_of_measurement":"min"})),
            ("initial_time", json!({"platform":"sensor","unique_id":"$deviceid-initial_time","state_topic":"$this/initial_time","name":"Initial time estimate","icon":"mdi:clock-outline","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
            ("dry_level", json!({"platform":"sensor","unique_id":"$deviceid-dry_level","state_topic":"$this/dry_level","name":"Dry level","icon":"mdi:water-percent"})),
            ("temp", json!({"platform":"sensor","unique_id":"$deviceid-temp","state_topic":"$this/temp","name":"Temperature","icon":"mdi:thermometer"})),
            ("more_less_time", json!({"platform":"sensor","unique_id":"$deviceid-more_less_time","state_topic":"$this/more_less_time","name":"More/Less time","icon":"mdi:plus-minus-variant","device_class":"duration","unit_of_measurement":"min","entity_category":"diagnostic"})),
            ("remote_start", json!({"platform":"binary_sensor","unique_id":"$deviceid-remote_start","state_topic":"$this/remote_start","name":"Remote start","icon":"mdi:cellphone-wireless","entity_category":"diagnostic"})),
            ("load_item", json!({"platform":"sensor","unique_id":"$deviceid-load_item","state_topic":"$this/load_item","name":"Load items","icon":"mdi:tshirt-crew-outline","state_class":"measurement"})),
            ("signal", json!({"platform":"sensor","unique_id":"$deviceid-signal","state_topic":"$this/signal","name":"Signal","icon":"mdi:bell-outline","entity_category":"diagnostic"})),
            ("reduce_static", json!({"platform":"binary_sensor","unique_id":"$deviceid-reduce_static","state_topic":"$this/reduce_static","name":"Reduce static","icon":"mdi:flash-off-outline"})),
            ("damp_dry_signal", json!({"platform":"binary_sensor","unique_id":"$deviceid-damp_dry_signal","state_topic":"$this/damp_dry_signal","name":"Damp Dry Signal","icon":"mdi:water-alert-outline"})),
            ("child_lock", json!({"platform":"binary_sensor","unique_id":"$deviceid-child_lock","state_topic":"$this/child_lock","name":"Child lock","icon":"mdi:lock","entity_category":"diagnostic"})),
            ("energy_saver", json!({"platform":"binary_sensor","unique_id":"$deviceid-energy_saver","state_topic":"$this/energy_saver","name":"Energy Saver","icon":"mdi:leaf"})),
            ("turbo_steam", json!({"platform":"binary_sensor","unique_id":"$deviceid-turbo_steam","state_topic":"$this/turbo_steam","name":"Turbo Steam","icon":"mdi:kettle-steam"})),
            ("wrinkle_care", json!({"platform":"binary_sensor","unique_id":"$deviceid-wrinkle_care","state_topic":"$this/wrinkle_care","name":"Wrinkle Care","icon":"mdi:tshirt-crew-outline"})),
        ];
        let mut components = Map::new();
        for (k, v) in comps {
            components.insert(k.into(), v);
        }
        base.components = components.into_iter().collect();
        core.set_config(base);

        let t = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = t.core.process_data_envelope(data) {
                t.process_aabb(&inner);
            }
        }));
        this
    }

    fn process_status(&self, buf: &[u8], record_offset: usize, expected_len: usize) {
        if buf.len() != expected_len {
            return;
        }
        let rec = &buf[record_offset..];
        if rec.len() < LOAD_ITEM_OFFSET + 1 || rec[0] != 0x1b {
            return;
        }
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;

        self.core.publish_on_off("power", !is_off);
        self.core.publish_property(
            "status",
            status_map().get(&phase).copied().unwrap_or("Running").into(),
        );
        self.core.publish_property(
            "course",
            course_map()
                .get(&rec[COURSE_OFFSET])
                .copied()
                .unwrap_or("unknown")
                .into(),
        );
        self.core.publish_property(
            "remaining_time",
            if is_off {
                0i64
            } else {
                rec[TIME_HOUR_OFFSET] as i64 * 60 + rec[TIME_MIN_OFFSET] as i64
            }
            .into(),
        );
        self.core.publish_property(
            "initial_time",
            if is_off {
                0i64
            } else {
                rec[INITIAL_TIME_HOUR_OFFSET] as i64 * 60 + rec[INITIAL_TIME_MIN_OFFSET] as i64
            }
            .into(),
        );
        self.core.publish_property(
            "dry_level",
            dry_level_map()
                .get(&rec[DRY_LEVEL_OFFSET])
                .copied()
                .unwrap_or("unknown")
                .into(),
        );
        self.core.publish_property(
            "temp",
            temp_map()
                .get(&rec[TEMP_OFFSET])
                .copied()
                .unwrap_or("unknown")
                .into(),
        );
        self.core
            .publish_property("load_item", (rec[LOAD_ITEM_OFFSET] as i64).into());
        self.core.publish_property(
            "signal",
            signal_map()
                .get(&rec[SIGNAL_OFFSET])
                .copied()
                .unwrap_or("unknown")
                .into(),
        );
        let more_less = if is_off {
            0i64
        } else {
            rec[MORE_LESS_TIME_OFFSET] as i8 as i64
        };
        self.core
            .publish_property("more_less_time", more_less.into());

        let flags = rec[FLAGS_OFFSET];
        self.core.publish_on_off("child_lock", flags & FLAG_CHILD_LOCK != 0);
        self.core.publish_on_off("reduce_static", flags & FLAG_REDUCE_STATIC != 0);
        self.core.publish_on_off("damp_dry_signal", flags & FLAG_DAMP_DRY_SIGNAL != 0);
        self.core.publish_on_off("wrinkle_care", flags & FLAG_WRINKLE_CARE != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core.publish_on_off("energy_saver", opt2 & OPT2_ENERGY_SAVER != 0);
        self.core.publish_on_off("turbo_steam", opt2 & OPT2_TURBO_STEAM != 0);
        self.core.publish_on_off("remote_start", opt2 & OPT2_REMOTE_START != 0);
    }

    fn process_aabb(&self, buf: &[u8]) {
        if buf.len() < 2 || buf[0] != 0x30 {
            return;
        }
        if buf[1] == STATUS_FRAME_TYPE {
            self.process_status(buf, RECORD_B_OFFSET, STATUS_FRAME_LEN);
        } else if buf[1] == SINGLE_STATUS_FRAME_TYPE {
            self.process_status(buf, SINGLE_RECORD_OFFSET, SINGLE_STATUS_FRAME_LEN);
        }
    }

    pub fn set_property(&self, _prop: &str, _mqtt_value: &str) {}
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
        Metadata::new("RV13B6ES_D_US_WIFI", "RV13B6ES_D_US_WIFI", "1.0")
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

    // Real fixtures from upstream PR #134 tests
    const EB_IDLE: &str = "aa2330eb001b000029002900000000000100000000280000000100000064000000b6bb";
    const POWER_ON_NORMAL: &str =
        "aa4030ec001b01000a000a00000000000100000040280000000000000064000000001b0100290029030003040001000000402800000000000000640000001dbb";
    const WRINKLE_CARE_ON: &str =
        "aa4030ec001b010029002903000304000100000040280000000000000064000000001b010029002903000304000100000050280000000000000064000000f5bb";
    const WRINKLE_CARE_OFF: &str =
        "aa4030ec001b010029002903000304000100000050280000000000000064000000001b010029002903000304000100000040280000000000000064000000f5bb";
    const REDUCE_STATIC_AND_LOAD_ITEM: &str =
        "aa4030ec001b010029002903000304000100000000280000000000000064000000001b01002700270300030400010000000228000000000000056400000046bb";
    const STARTS_DRYING: &str =
        "aa4030ec001b01000a000a100000020001f1000000280000000000000064000000001b32000a000a100000020001f1000000290000000100000064000000ecbb";
    const ANTIBACTERIAL: &str =
        "aa4030ec001b01001f001f16000005000100000040280000000000000064000000001b01010a010a080005050001000000402800000000000000640000000cbb";

    #[test]
    fn config_has_es_entities() {
        let (ha, _, _) = make();
        let devinfo = ha.device(DEVICE_ID).unwrap();
        let comps = &devinfo.config.as_ref().unwrap().components;
        assert!(comps.contains_key("wrinkle_care"));
        assert!(comps.contains_key("load_item"));
        assert!(comps.contains_key("remote_start"));
        assert!(comps.contains_key("more_less_time"));
        assert!(comps.contains_key("signal"));
    }

    #[test]
    fn eb_idle() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(EB_IDLE));
        assert_eq!(prop(&ha, "power").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Off"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("0"));
    }

    #[test]
    fn power_on_normal() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(POWER_ON_NORMAL));
        assert_eq!(prop(&ha, "power").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Initial"));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Normal"));
        assert_eq!(prop(&ha, "temp").as_deref(), Some("Mid High"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("41"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("41"));
    }

    #[test]
    fn wrinkle_care_from_flags_not_opt2() {
        // Would fail if aliased to BSD (wrinkle on opt2 0x10)
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(WRINKLE_CARE_ON));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "energy_saver").as_deref(), Some("OFF"));
        assert_eq!(prop(&ha, "turbo_steam").as_deref(), Some("OFF"));

        thinq.emit_data(&hex_decode(WRINKLE_CARE_OFF));
        assert_eq!(prop(&ha, "wrinkle_care").as_deref(), Some("OFF"));
    }

    #[test]
    fn load_item_and_reduce_static() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(REDUCE_STATIC_AND_LOAD_ITEM));
        assert_eq!(prop(&ha, "reduce_static").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "load_item").as_deref(), Some("5"));
    }

    #[test]
    fn more_less_and_remote_start() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(STARTS_DRYING));
        assert_eq!(prop(&ha, "more_less_time").as_deref(), Some("-15"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("10"));
        assert_eq!(prop(&ha, "remote_start").as_deref(), Some("ON"));
        assert_eq!(prop(&ha, "status").as_deref(), Some("Drying"));
    }

    #[test]
    fn hour_plus_estimate() {
        let (ha, thinq, _) = make();
        thinq.emit_data(&hex_decode(ANTIBACTERIAL));
        assert_eq!(prop(&ha, "course").as_deref(), Some("Antibacterial"));
        assert_eq!(prop(&ha, "dry_level").as_deref(), Some("Very"));
        assert_eq!(prop(&ha, "remaining_time").as_deref(), Some("70"));
        assert_eq!(prop(&ha, "initial_time").as_deref(), Some("70"));
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("RV13B6ES_D_US_WIFI").is_some());
    }
}
