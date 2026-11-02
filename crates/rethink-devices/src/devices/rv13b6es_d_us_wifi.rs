//! RV13B6ES_D_US_WIFI — LG electric dryer (AABB, read-only).
//!
//! Same framing as RV13B6BSD_D_US_WIFI (class `0x30`, 29-byte `0xEB`/`0xEC` status blocks) but not
//! an alias: Wrinkle Care is rec[15] bit 0x10, Remote Start is rec[16] bit 0x01, and rec[23] is a
//! load-item count. Also decodes the Signal volume and the signed More/Less Time trim. The dryer
//! only reports on change, so `start` asks for a status snapshot.

use crate::device_trait::DeviceHandler;
use rethink_core::device_base::{default_config, process_common_status, AabbDeviceCore};
use rethink_core::{HaConnection, Metadata, PropertyValue, Thinq2Device};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const CLASS_BYTE: u8 = 0x30;
const STATUS_LENGTH: usize = 29;
const RECORD_MARKER: u8 = 0x1b;

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

const STATUS: &[(u8, &str)] = &[
    (0x00, "Off"),
    (0x01, "Initial"),
    (0x03, "Pause"),
    (0x32, "Drying"),
    (0x33, "Cooling"),
    (0x04, "End"),
];
const COURSE: &[(u8, &str)] = &[
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
];
// 0 (no dry level / no temperature) is left unmapped, so those courses publish "None".
const DRY_LEVEL: &[(u8, &str)] = &[
    (1, "Damp"),
    (2, "Less"),
    (3, "Normal"),
    (4, "More"),
    (5, "Very"),
];
const TEMP: &[(u8, &str)] = &[
    (1, "Ultra Low"),
    (2, "Low"),
    (3, "Medium"),
    (4, "Mid High"),
    (5, "High"),
];
// Read off the panel's indicator; the cloud reports no signal field for this dryer.
const SIGNAL: &[(u8, &str)] = &[(0x00, "Off"), (0x01, "Low"), (0x04, "High")];

fn named(table: &'static [(u8, &'static str)], code: u8) -> Option<&'static str> {
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

fn components() -> HashMap<String, Value> {
    [
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
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

pub struct Device {
    core: Arc<AabbDeviceCore>,
}

impl Device {
    pub fn new(
        ha: Arc<dyn HaConnection>,
        thinq: Arc<dyn Thinq2Device>,
        meta: Metadata,
    ) -> Arc<Self> {
        let core = AabbDeviceCore::new(ha, thinq.clone());
        let this = Arc::new(Self { core: core.clone() });
        let mut config = default_config(&meta, Some(json!({"name": "LG Dryer"})));
        config.components = components();
        core.set_config(config);
        let dev = this.clone();
        thinq.on_data(Box::new(move |data| {
            if let Some(inner) = dev.core.process_data_envelope(data) {
                dev.process_aabb(inner);
            }
        }));
        this
    }

    fn process_aabb(&self, buf: &[u8]) {
        if let Some(block) = process_common_status(buf, CLASS_BYTE, STATUS_LENGTH) {
            self.process_status(&block[1..]);
        }
    }

    fn process_status(&self, rec: &[u8]) {
        if rec[0] != RECORD_MARKER {
            return;
        }
        let phase = rec[PHASE_OFFSET];
        let is_off = phase == PHASE_OFF;
        let minutes = |hour: usize, min: usize| {
            if is_off {
                0
            } else {
                i64::from(rec[hour]) * 60 + i64::from(rec[min])
            }
        };

        self.core.publish_on_off("power", !is_off);
        self.core
            .publish_property("status", named(STATUS, phase).unwrap_or("Running").into());
        self.core.publish_property(
            "course",
            named(COURSE, rec[COURSE_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "remaining_time",
            PropertyValue::Int(minutes(TIME_HOUR_OFFSET, TIME_MIN_OFFSET)),
        );
        self.core.publish_property(
            "initial_time",
            PropertyValue::Int(minutes(INITIAL_TIME_HOUR_OFFSET, INITIAL_TIME_MIN_OFFSET)),
        );
        self.core.publish_property(
            "dry_level",
            named(DRY_LEVEL, rec[DRY_LEVEL_OFFSET])
                .unwrap_or("None")
                .into(),
        );
        self.core.publish_property(
            "temp",
            named(TEMP, rec[TEMP_OFFSET]).unwrap_or("None").into(),
        );
        self.core.publish_property(
            "load_item",
            PropertyValue::Int(i64::from(rec[LOAD_ITEM_OFFSET])),
        );
        self.core.publish_property(
            "signal",
            named(SIGNAL, rec[SIGNAL_OFFSET]).unwrap_or("None").into(),
        );
        // signed: negative trims the course default, positive extends it
        let more_less = if is_off {
            0
        } else {
            i64::from(rec[MORE_LESS_TIME_OFFSET] as i8)
        };
        self.core
            .publish_property("more_less_time", PropertyValue::Int(more_less));

        let flags = rec[FLAGS_OFFSET];
        self.core
            .publish_on_off("child_lock", flags & FLAG_CHILD_LOCK != 0);
        self.core
            .publish_on_off("reduce_static", flags & FLAG_REDUCE_STATIC != 0);
        self.core
            .publish_on_off("damp_dry_signal", flags & FLAG_DAMP_DRY_SIGNAL != 0);
        self.core
            .publish_on_off("wrinkle_care", flags & FLAG_WRINKLE_CARE != 0);

        let opt2 = rec[OPT2_OFFSET];
        self.core
            .publish_on_off("energy_saver", opt2 & OPT2_ENERGY_SAVER != 0);
        self.core
            .publish_on_off("turbo_steam", opt2 & OPT2_TURBO_STEAM != 0);
        self.core
            .publish_on_off("remote_start", opt2 & OPT2_REMOTE_START != 0);
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
    fn set_property(&self, _prop: &str, _value: &str) {}
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
    use crate::test_support::{make_t2, prop, DEVICE_ID};
    use rethink_core::{hex_decode, hex_encode, MockHaConnection, MockThinq2Device};

    const EB_IDLE: &str = "aa2330eb001b000029002900000000000100000000280000000100000064000000b6bb";
    const POWER_ON_NORMAL: &str =
        "aa4030ec001b01000a000a00000000000100000040280000000000000064000000001b0100290029030003040001000000402800000000000000640000001dbb";
    const ANTIBACTERIAL: &str =
        "aa4030ec001b01001f001f16000005000100000040280000000000000064000000001b01010a010a080005050001000000402800000000000000640000000cbb";
    const WRINKLE_CARE_ON: &str =
        "aa4030ec001b010029002903000304000100000040280000000000000064000000001b010029002903000304000100000050280000000000000064000000f5bb";
    const WRINKLE_CARE_OFF: &str =
        "aa4030ec001b010029002903000304000100000050280000000000000064000000001b010029002903000304000100000040280000000000000064000000f5bb";
    const CHILD_LOCK_ON: &str =
        "aa4030ec001b010029002903000304000100000040280000000000000064000000001b010029002903000304000100000041280000000000000064000000c4bb";
    const TURBO_STEAM_ON: &str =
        "aa4030ec001b010029002903000304000100000000280000000000000064000000001b0100290029030003040001000000002c000000000000006400000041bb";
    const ENERGY_SAVER_ON: &str =
        "aa4030ec001b010029002903000304000100000000280000000000000264000000001b0101030103030003040001000000002a000000000000026400000099bb";
    const REDUCE_STATIC_AND_LOAD_ITEM: &str =
        "aa4030ec001b010029002903000304000100000000280000000000000064000000001b01002700270300030400010000000228000000000000056400000046bb";
    const STARTS_DRYING: &str =
        "aa4030ec001b01000a000a100000020001f1000000280000000000000064000000001b32000a000a100000020001f1000000290000000100000064000000ecbb";
    const PAUSES: &str =
        "aa4030ec001b32000a000a100000020001f1000000290000000100000064000000001b03000a000a100000020001f10000402800000132000000640000007bbb";
    const RESUMES: &str =
        "aa4030ec001b03000a000a100000020001f1000040280000013200000064000000001b32000a000a100000020001f100000028000001030000006400000065bb";
    const COOLING: &str =
        "aa4030ec001b320006000a100000020001f10000002900000e0300000064000000001b330005000a100000020001f100000029000012320000006400000062bb";
    const ENDS: &str =
        "aa4030ec001b330001000b100000020001f1000000290000243200000064000000001b040001000b100000020001f1000040280000243300000064000000ccbb";
    const SETTLES_OFF: &str =
        "aa4030ec001b040001000b100000020001f1000040280000243300000064000000001b0000010001000000000001000000402800002404000000640000003fbb";
    const SIGNAL_OFF: &str =
        "aa4030ec001b010036003601000305000400000040280000000000000064000000001b01003600360100030500000000004028000000000000006400000091bb";
    const SIGNAL_LOW: &str =
        "aa4030ec001b010036003601000305000000000040280000000000000064000000001b01003600360100030500010000004028000000000000006400000094bb";
    const SIGNAL_HIGH: &str =
        "aa4030ec001b010029002903000304000100000040280000000000000064000000001b010029002903000304000400000040280000000000000064000000c6bb";

    fn make() -> (Arc<MockHaConnection>, Arc<MockThinq2Device>, Arc<Device>) {
        make_t2("RV13B6ES_D_US_WIFI", Device::new)
    }

    fn feed(frames: &[&str]) -> Arc<MockHaConnection> {
        let (ha, thinq, _) = make();
        for frame in frames {
            thinq.emit_data(&hex_decode(frame));
        }
        ha
    }

    fn expect(ha: &MockHaConnection, name: &str, value: &str) {
        assert_eq!(prop(ha, name).as_deref(), Some(value), "{name}");
    }

    #[test]
    fn config_has_es_entities() {
        let (ha, _, _) = make();
        let comps = ha.device(DEVICE_ID).unwrap().config.unwrap().components;
        for key in [
            "wrinkle_care",
            "load_item",
            "remote_start",
            "more_less_time",
            "signal",
        ] {
            assert!(comps.contains_key(key), "{key}");
        }
    }

    #[test]
    fn eb_single_record_decodes_with_the_ec_offsets() {
        let p = feed(&[EB_IDLE]);
        expect(&p, "power", "OFF");
        expect(&p, "status", "Off");
        expect(&p, "course", "None");
        expect(&p, "remaining_time", "0");
    }

    #[test]
    fn power_on_with_normal_selected() {
        let p = feed(&[POWER_ON_NORMAL]);
        expect(&p, "power", "ON");
        expect(&p, "status", "Initial");
        expect(&p, "course", "Normal");
        expect(&p, "dry_level", "Normal");
        expect(&p, "temp", "Mid High");
        expect(&p, "remaining_time", "41");
        expect(&p, "initial_time", "41");
    }

    #[test]
    fn hour_plus_estimate_folds_hour_and_minute_into_one_value() {
        let p = feed(&[ANTIBACTERIAL]);
        expect(&p, "course", "Antibacterial");
        expect(&p, "dry_level", "Very");
        expect(&p, "remaining_time", "70");
        expect(&p, "initial_time", "70");
    }

    #[test]
    fn wrinkle_care_is_read_from_rec15_not_rec16() {
        let on = feed(&[WRINKLE_CARE_ON]);
        expect(&on, "wrinkle_care", "ON");
        expect(&on, "energy_saver", "OFF");
        expect(&on, "turbo_steam", "OFF");

        let off = feed(&[WRINKLE_CARE_ON, WRINKLE_CARE_OFF]);
        expect(&off, "wrinkle_care", "OFF");
    }

    #[test]
    fn child_lock_reduce_static_and_damp_dry_share_rec15_without_collision() {
        let lock = feed(&[CHILD_LOCK_ON]);
        expect(&lock, "child_lock", "ON");
        expect(&lock, "wrinkle_care", "OFF");
        expect(&lock, "reduce_static", "OFF");

        let stat = feed(&[REDUCE_STATIC_AND_LOAD_ITEM]);
        expect(&stat, "reduce_static", "ON");
        expect(&stat, "child_lock", "OFF");
        expect(&stat, "wrinkle_care", "OFF");
    }

    #[test]
    fn load_item_count_is_reported_from_rec23() {
        expect(&feed(&[REDUCE_STATIC_AND_LOAD_ITEM]), "load_item", "5");
    }

    #[test]
    fn turbo_steam_and_energy_saver_share_rec16() {
        let steam = feed(&[TURBO_STEAM_ON]);
        expect(&steam, "turbo_steam", "ON");
        expect(&steam, "energy_saver", "OFF");

        let saver = feed(&[ENERGY_SAVER_ON]);
        expect(&saver, "energy_saver", "ON");
        expect(&saver, "turbo_steam", "OFF");
        expect(&saver, "remaining_time", "63");
    }

    #[test]
    fn more_less_time_is_a_signed_trim_of_the_course_default() {
        let p = feed(&[STARTS_DRYING]);
        expect(&p, "more_less_time", "-15");
        expect(&p, "remaining_time", "10");
    }

    #[test]
    fn remote_start_is_visible_in_rec16_bit0() {
        expect(&feed(&[STARTS_DRYING]), "remote_start", "ON");
        expect(&feed(&[STARTS_DRYING, PAUSES]), "remote_start", "OFF");
    }

    #[test]
    fn real_run_initial_drying_pause_drying_cooling() {
        let start = feed(&[STARTS_DRYING]);
        expect(&start, "status", "Drying");
        expect(&start, "course", "Speed Dry");
        expect(&start, "temp", "Low");
        expect(&start, "dry_level", "None");

        expect(&feed(&[STARTS_DRYING, PAUSES]), "status", "Pause");
        expect(&feed(&[STARTS_DRYING, PAUSES, RESUMES]), "status", "Drying");

        let cooling = feed(&[STARTS_DRYING, PAUSES, RESUMES, COOLING]);
        expect(&cooling, "status", "Cooling");
        expect(&cooling, "remaining_time", "5");
        expect(&cooling, "initial_time", "10");
    }

    #[test]
    fn run_finishes_cooling_end_off_and_off_clears_the_selection() {
        let ended = feed(&[STARTS_DRYING, RESUMES, COOLING, ENDS]);
        expect(&ended, "status", "End");
        expect(&ended, "power", "ON");

        let off = feed(&[STARTS_DRYING, RESUMES, COOLING, ENDS, SETTLES_OFF]);
        expect(&off, "status", "Off");
        expect(&off, "power", "OFF");
        expect(&off, "course", "None");
        expect(&off, "remaining_time", "0");
        expect(&off, "more_less_time", "0");
    }

    #[test]
    fn signal_beeper_setting_is_read_from_rec11() {
        expect(&feed(&[SIGNAL_OFF]), "signal", "Off");
        expect(&feed(&[SIGNAL_LOW]), "signal", "Low");
        expect(&feed(&[SIGNAL_HIGH]), "signal", "High");
        let p = feed(&[SIGNAL_HIGH]);
        expect(&p, "course", "Normal");
        expect(&p, "dry_level", "Normal");
    }

    #[test]
    fn start_requests_a_status_snapshot() {
        let (_, thinq, dev) = make();
        dev.start();
        let sent: Vec<String> = thinq.outbox().iter().map(|p| hex_encode(p)).collect();
        assert_eq!(sent, vec!["AA0EF0ED1121010000001800B5BB".to_string()]);
    }

    #[test]
    fn reply_to_start_decodes_as_an_ordinary_status_frame() {
        let p = feed(&[EB_IDLE]);
        expect(&p, "status", "Off");
        expect(&p, "power", "OFF");
    }

    #[test]
    fn frames_that_are_not_status_frames_publish_nothing() {
        for junk in [
            "aa0730d82b91bb",
            "aa09307200c80048bb",
            "aa0a30ec001b010029bb",
        ] {
            let props = feed(&[junk]).device(DEVICE_ID).unwrap().properties;
            assert!(props.is_empty(), "frame {junk} should publish nothing");
        }
    }

    #[test]
    fn registry_resolves() {
        assert!(crate::registry::t2_factory("RV13B6ES_D_US_WIFI").is_some());
    }
}
